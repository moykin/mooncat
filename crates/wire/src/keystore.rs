//! Where a terminal keeps its private key.
//!
//! # What is being protected, and from what
//!
//! The device key is what lets a terminal connect. It is not an exchange credential — the core
//! holds those and the terminal never sees them — so losing it costs a revocation and an
//! enrolment, not money. That shapes the trade-off: the protection has to be good enough that
//! a stolen laptop is not an immediate compromise, and cheap enough that a terminal starting
//! up does not ask for a password every time.
//!
//! # Two places, in order of preference
//!
//! The operating system's own store — Keychain on macOS, the Credential Manager on Windows,
//! the Secret Service on Linux — is the right answer where it exists. It is unlocked with the
//! user's login, so the terminal starts without prompting, and the secret is not sitting in a
//! file that a backup or a synced folder will copy somewhere else.
//!
//! Where it does not exist, and it often does not on a headless Linux box, the fallback is a
//! passphrase-encrypted file with mode `0600`. Not as good: the file can be copied and
//! attacked offline. The key derivation is deliberately slow to make that expensive.
//!
//! # No plaintext option
//!
//! There is deliberately no "just write the bytes to a file" path, however convenient it would
//! be for development. Something added for convenience is exactly what ends up in production,
//! and a private key in a plaintext file is the failure this module exists to prevent.

use crate::auth::DeviceKey;
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM, NONCE_LEN};
use ring::pbkdf2;
use ring::rand::{SecureRandom, SystemRandom};
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};

/// Identifies the file format, so a wrong file is refused rather than decrypted into nonsense.
const MAGIC: &[u8; 8] = b"MOONKEY1";
const SALT_LEN: usize = 16;
/// Ed25519 secret plus the AEAD tag.
const SEALED_LEN: usize = 32 + 16;

/// PBKDF2 iterations.
///
/// Six hundred thousand is the OWASP figure for PBKDF2-HMAC-SHA256, and costs a few hundred
/// milliseconds once at startup. The whole point of the fallback is that the file can be
/// stolen and attacked offline, so the cost per guess is the only defence there is.
const PBKDF2_ITERATIONS: u32 = 600_000;

/// Enforced at compile time rather than by a test.
///
/// This is the only defence the file fallback has against an offline attack on a copied key,
/// so lowering it for speed must not be possible without deleting this line and explaining why.
const _: () = assert!(PBKDF2_ITERATIONS >= 600_000, "PBKDF2 is below the OWASP figure");

#[derive(Debug, thiserror::Error)]
pub enum KeystoreError {
    #[error("no key stored")]
    NotFound,
    #[error("wrong passphrase, or the file has been tampered with")]
    CannotDecrypt,
    #[error("not a moon-own key file")]
    WrongFormat,
    #[error("key file is {0} bytes, expected {expected}", expected = MAGIC.len() + SALT_LEN + NONCE_LEN + SEALED_LEN)]
    WrongLength(usize),
    #[error("passphrase is empty")]
    EmptyPassphrase,
    #[error("io: {0}")]
    Io(String),
    #[error("os keystore: {0}")]
    Os(String),
}

/// Somewhere a device key can live.
pub trait Keystore {
    fn store(&self, key: &DeviceKey) -> Result<(), KeystoreError>;
    fn load(&self) -> Result<DeviceKey, KeystoreError>;
    fn delete(&self) -> Result<(), KeystoreError>;
    /// For logs and diagnostics. Must never include the secret.
    fn describe(&self) -> String;
}

/// A passphrase-encrypted file, mode `0600`.
pub struct EncryptedFile {
    path: PathBuf,
    passphrase: String,
}

impl std::fmt::Debug for EncryptedFile {
    /// Hand-written: a passphrase in a log is the same failure as a key in a log.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EncryptedFile").field("path", &self.path).field("passphrase", &"<redacted>").finish()
    }
}

impl EncryptedFile {
    pub fn new(path: impl Into<PathBuf>, passphrase: impl Into<String>) -> Result<Self, KeystoreError> {
        let passphrase = passphrase.into();
        if passphrase.is_empty() {
            // An empty passphrase derives a key from nothing and turns this into the plaintext
            // file the module exists to avoid.
            return Err(KeystoreError::EmptyPassphrase);
        }
        Ok(Self { path: path.into(), passphrase })
    }

    fn derive(&self, salt: &[u8]) -> [u8; 32] {
        let mut derived = [0u8; 32];
        pbkdf2::derive(
            pbkdf2::PBKDF2_HMAC_SHA256,
            NonZeroU32::new(PBKDF2_ITERATIONS).expect("iterations is a non-zero literal"),
            salt,
            self.passphrase.as_bytes(),
            &mut derived,
        );
        derived
    }

    fn seal_key(&self, salt: &[u8]) -> LessSafeKey {
        let derived = self.derive(salt);
        LessSafeKey::new(UnboundKey::new(&AES_256_GCM, &derived).expect("a 32-byte key fits AES-256-GCM"))
    }
}

impl Keystore for EncryptedFile {
    fn store(&self, key: &DeviceKey) -> Result<(), KeystoreError> {
        let rng = SystemRandom::new();
        let mut salt = [0u8; SALT_LEN];
        let mut nonce_bytes = [0u8; NONCE_LEN];
        rng.fill(&mut salt).map_err(|_| KeystoreError::Io("no system randomness".into()))?;
        rng.fill(&mut nonce_bytes).map_err(|_| KeystoreError::Io("no system randomness".into()))?;

        // The salt and nonce are authenticated as associated data, so a file whose header has
        // been edited fails to open rather than decrypting under a different derived key.
        let mut sealed = key.secret_to_store().to_vec();
        self.seal_key(&salt)
            .seal_in_place_append_tag(
                Nonce::assume_unique_for_key(nonce_bytes),
                Aad::from(&salt),
                &mut sealed,
            )
            .map_err(|_| KeystoreError::Io("sealing failed".into()))?;

        let mut out = Vec::with_capacity(MAGIC.len() + SALT_LEN + NONCE_LEN + SEALED_LEN);
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&salt);
        out.extend_from_slice(&nonce_bytes);
        out.extend_from_slice(&sealed);

        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| KeystoreError::Io(e.to_string()))?;
        }
        write_private(&self.path, &out)
    }

    fn load(&self) -> Result<DeviceKey, KeystoreError> {
        let bytes = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(KeystoreError::NotFound),
            Err(e) => return Err(KeystoreError::Io(e.to_string())),
        };

        if !bytes.starts_with(MAGIC) {
            return Err(KeystoreError::WrongFormat);
        }
        if bytes.len() != MAGIC.len() + SALT_LEN + NONCE_LEN + SEALED_LEN {
            return Err(KeystoreError::WrongLength(bytes.len()));
        }

        let salt = &bytes[MAGIC.len()..MAGIC.len() + SALT_LEN];
        let nonce_start = MAGIC.len() + SALT_LEN;
        let nonce: [u8; NONCE_LEN] =
            bytes[nonce_start..nonce_start + NONCE_LEN].try_into().expect("length was checked above");

        let mut sealed = bytes[nonce_start + NONCE_LEN..].to_vec();
        let opened = self
            .seal_key(salt)
            .open_in_place(Nonce::assume_unique_for_key(nonce), Aad::from(salt), &mut sealed)
            .map_err(|_| KeystoreError::CannotDecrypt)?;

        let secret: [u8; 32] = opened.try_into().map_err(|_| KeystoreError::CannotDecrypt)?;
        Ok(DeviceKey::from_bytes(&secret))
    }

    fn delete(&self) -> Result<(), KeystoreError> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(KeystoreError::NotFound),
            Err(e) => Err(KeystoreError::Io(e.to_string())),
        }
    }

    fn describe(&self) -> String {
        format!("encrypted file at {}", self.path.display())
    }
}

/// Write with owner-only permissions, and set them **before** the bytes go in.
///
/// Creating the file and then tightening it leaves a window in which it is world-readable, and
/// on a shared machine that window is all an attacker needs.
fn write_private(path: &Path, bytes: &[u8]) -> Result<(), KeystoreError> {
    use std::io::Write;

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    let mut file = options.open(path).map_err(|e| KeystoreError::Io(e.to_string()))?;
    file.write_all(bytes).map_err(|e| KeystoreError::Io(e.to_string()))?;
    // Durability matters here: a key half-written across a crash is a terminal that cannot
    // connect and cannot say why.
    file.sync_all().map_err(|e| KeystoreError::Io(e.to_string()))?;
    Ok(())
}

/// The operating system's own credential store.
#[cfg(feature = "os-keystore")]
pub struct OsKeystore {
    service: String,
    account: String,
}

#[cfg(feature = "os-keystore")]
impl OsKeystore {
    pub fn new(service: impl Into<String>, account: impl Into<String>) -> Self {
        Self { service: service.into(), account: account.into() }
    }

    fn entry(&self) -> Result<keyring::Entry, KeystoreError> {
        keyring::Entry::new(&self.service, &self.account).map_err(|e| KeystoreError::Os(e.to_string()))
    }
}

#[cfg(feature = "os-keystore")]
impl Keystore for OsKeystore {
    fn store(&self, key: &DeviceKey) -> Result<(), KeystoreError> {
        self.entry()?.set_secret(&key.secret_to_store()).map_err(|e| KeystoreError::Os(e.to_string()))
    }

    fn load(&self) -> Result<DeviceKey, KeystoreError> {
        let secret = match self.entry()?.get_secret() {
            Ok(secret) => secret,
            Err(keyring::Error::NoEntry) => return Err(KeystoreError::NotFound),
            Err(e) => return Err(KeystoreError::Os(e.to_string())),
        };
        let bytes: [u8; 32] = secret.as_slice().try_into().map_err(|_| KeystoreError::CannotDecrypt)?;
        Ok(DeviceKey::from_bytes(&bytes))
    }

    fn delete(&self) -> Result<(), KeystoreError> {
        match self.entry()?.delete_credential() {
            Ok(()) => Ok(()),
            Err(keyring::Error::NoEntry) => Err(KeystoreError::NotFound),
            Err(e) => Err(KeystoreError::Os(e.to_string())),
        }
    }

    fn describe(&self) -> String {
        format!("os keystore, service `{}`, account `{}`", self.service, self.account)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!("moon-ks-{}-{n}-{tag}", std::process::id()));
            std::fs::create_dir_all(&path).expect("temp dir");
            Self(path)
        }

        fn file(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn store(dir: &TempDir, name: &str, passphrase: &str) -> EncryptedFile {
        EncryptedFile::new(dir.file(name), passphrase).expect("a non-empty passphrase")
    }

    // --- the round trip -------------------------------------------------------------------

    #[test]
    fn a_key_survives_being_stored_and_loaded() {
        let dir = TempDir::new("round");
        let ks = store(&dir, "device.key", "correct horse battery staple");
        let key = DeviceKey::generate();

        ks.store(&key).expect("stores");
        let loaded = ks.load().expect("loads");

        assert_eq!(loaded.device_id(), key.device_id());
        assert_eq!(loaded.secret_to_store(), key.secret_to_store());
    }

    #[test]
    fn loading_from_nothing_says_so_rather_than_failing_obscurely() {
        // First run of a fresh terminal. It has to be distinguishable from a corrupt file, or
        // the terminal cannot tell "enrol me" from "something is wrong".
        let dir = TempDir::new("absent");
        assert!(matches!(store(&dir, "missing.key", "pw").load(), Err(KeystoreError::NotFound)));
    }

    #[test]
    fn a_stored_key_can_be_deleted_and_then_is_gone() {
        let dir = TempDir::new("delete");
        let ks = store(&dir, "device.key", "pw");
        ks.store(&DeviceKey::generate()).unwrap();

        assert!(ks.delete().is_ok());
        assert!(matches!(ks.load(), Err(KeystoreError::NotFound)));
        assert!(matches!(ks.delete(), Err(KeystoreError::NotFound)), "a second delete is honest");
    }

    // --- what the encryption is for ------------------------------------------------------------

    #[test]
    fn the_wrong_passphrase_does_not_open_it() {
        let dir = TempDir::new("wrongpw");
        let key = DeviceKey::generate();
        store(&dir, "device.key", "right").store(&key).unwrap();

        assert!(matches!(store(&dir, "device.key", "wrong").load(), Err(KeystoreError::CannotDecrypt)));
    }

    #[test]
    fn the_secret_is_not_in_the_file() {
        // The obvious check, and worth making: an encryption bug that wrote the plaintext
        // alongside the ciphertext would pass every other test here.
        let dir = TempDir::new("plaintext");
        let key = DeviceKey::generate();
        let ks = store(&dir, "device.key", "pw");
        ks.store(&key).unwrap();

        let raw = std::fs::read(dir.file("device.key")).unwrap();
        let secret = key.secret_to_store();
        assert!(
            !raw.windows(secret.len()).any(|w| w == secret),
            "the private key appears verbatim in the file"
        );
    }

    #[test]
    fn tampering_with_the_file_is_detected_rather_than_decrypted_into_nonsense() {
        // AES-GCM authenticates, so a flipped bit anywhere fails to open. Without that, a
        // corrupted file would yield a key that is silently wrong and a terminal that cannot
        // connect for no visible reason.
        let dir = TempDir::new("tamper");
        let ks = store(&dir, "device.key", "pw");
        ks.store(&DeviceKey::generate()).unwrap();

        let path = dir.file("device.key");
        let original = std::fs::read(&path).unwrap();

        // Every region: header, salt, nonce, ciphertext.
        for offset in [8, MAGIC.len() + 2, MAGIC.len() + SALT_LEN + 1, original.len() - 1] {
            let mut corrupted = original.clone();
            corrupted[offset] ^= 0x01;
            std::fs::write(&path, &corrupted).unwrap();
            assert!(ks.load().is_err(), "a flipped bit at offset {offset} was not detected");
        }
    }

    #[test]
    fn the_salt_and_nonce_are_authenticated_not_merely_carried() {
        // Swapping the salt for another valid one must fail. If it were not authenticated, the
        // file would decrypt under a different derived key — to garbage, silently.
        let dir = TempDir::new("aad");
        let ks = store(&dir, "device.key", "pw");
        ks.store(&DeviceKey::generate()).unwrap();

        let path = dir.file("device.key");
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[MAGIC.len()..MAGIC.len() + SALT_LEN].copy_from_slice(&[0xAA; SALT_LEN]);
        std::fs::write(&path, &bytes).unwrap();

        assert!(matches!(ks.load(), Err(KeystoreError::CannotDecrypt)));
    }

    #[test]
    fn two_stores_of_the_same_key_produce_different_files() {
        // A fresh salt and nonce each time. Reusing either would leak that the key has not
        // changed, and reusing a nonce under the same derived key breaks GCM outright.
        let dir = TempDir::new("fresh");
        let key = DeviceKey::generate();
        let ks = store(&dir, "device.key", "pw");

        ks.store(&key).unwrap();
        let first = std::fs::read(dir.file("device.key")).unwrap();
        ks.store(&key).unwrap();
        let second = std::fs::read(dir.file("device.key")).unwrap();

        assert_ne!(first, second, "salt and nonce must be fresh on every write");
        assert_eq!(ks.load().unwrap().secret_to_store(), key.secret_to_store());
    }

    #[test]
    fn an_empty_passphrase_is_refused_at_construction() {
        // It would derive a key from nothing and turn this into the plaintext file the module
        // exists to avoid. Refused where it is chosen, not where it is used.
        let dir = TempDir::new("empty");
        assert!(matches!(
            EncryptedFile::new(dir.file("device.key"), ""),
            Err(KeystoreError::EmptyPassphrase)
        ));
    }

    // --- file hygiene ------------------------------------------------------------------------------

    #[cfg(unix)]
    #[test]
    fn the_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new("perms");
        let ks = store(&dir, "device.key", "pw");
        ks.store(&DeviceKey::generate()).unwrap();

        let mode = std::fs::metadata(dir.file("device.key")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "mode is {:o}, expected 600", mode & 0o777);
    }

    #[test]
    fn a_missing_directory_is_created_rather_than_reported() {
        // The terminal's config directory does not exist on a first run, and failing there
        // would make enrolment fail for a reason that has nothing to do with enrolment.
        let dir = TempDir::new("mkdir");
        let ks = EncryptedFile::new(dir.file("a/b/c/device.key"), "pw").unwrap();
        assert!(ks.store(&DeviceKey::generate()).is_ok());
        assert!(ks.load().is_ok());
    }

    #[test]
    fn a_foreign_file_is_refused_by_its_header() {
        // Pointed at the wrong path — a config file, someone else's key — it must say so
        // rather than spend six hundred thousand iterations failing to decrypt.
        let dir = TempDir::new("foreign");
        std::fs::write(dir.file("device.key"), b"this is not a key file at all").unwrap();
        assert!(matches!(store(&dir, "device.key", "pw").load(), Err(KeystoreError::WrongFormat)));
    }

    #[test]
    fn a_truncated_file_is_refused_by_length() {
        let dir = TempDir::new("truncated");
        let ks = store(&dir, "device.key", "pw");
        ks.store(&DeviceKey::generate()).unwrap();

        let path = dir.file("device.key");
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.truncate(bytes.len() - 4);
        std::fs::write(&path, &bytes).unwrap();

        assert!(matches!(ks.load(), Err(KeystoreError::WrongLength(_))));
    }

    // --- secrecy in diagnostics ----------------------------------------------------------------------

    #[test]
    fn neither_the_key_nor_the_passphrase_appears_in_diagnostics() {
        // The acceptance criterion for task 2.7, extended to the passphrase: a passphrase in a
        // log is the same failure as a key in a log.
        let dir = TempDir::new("debug");
        let ks = store(&dir, "device.key", "super secret passphrase");
        let key = DeviceKey::generate();

        for text in [format!("{ks:?}"), ks.describe()] {
            assert!(!text.contains("super secret passphrase"), "the passphrase leaked: {text}");
            assert!(!text.contains(&hex::encode(key.secret_to_store())));
        }
        assert!(format!("{ks:?}").contains("<redacted>"));
    }

    #[test]
    fn the_description_is_useful_without_being_revealing() {
        // It goes in a startup log line so an operator can see which store is in use.
        let dir = TempDir::new("describe");
        let described = store(&dir, "device.key", "pw").describe();
        assert!(described.contains("device.key"), "must name the path: {described}");
    }

    // --- the derivation --------------------------------------------------------------------------------

    #[test]
    fn different_salts_derive_different_keys() {
        let dir = TempDir::new("kdf");
        let ks = store(&dir, "device.key", "same passphrase");
        assert_ne!(ks.derive(&[1; SALT_LEN]), ks.derive(&[2; SALT_LEN]));
        assert_eq!(ks.derive(&[1; SALT_LEN]), ks.derive(&[1; SALT_LEN]), "and it is deterministic");
    }
}
