//! Proving which terminal is connecting.
//!
//! # What replaces the shared token
//!
//! `MOON_TOKEN` is a bearer credential: whoever has the string is the terminal. It cannot be
//! revoked for one laptop without changing it for all of them, it appears in shell history and
//! process listings, and anyone who reads it once holds it forever.
//!
//! Instead each terminal generates an Ed25519 key that never leaves it and proves possession
//! per connection. The core stores public keys, so a stolen core database yields nothing that
//! can be used to connect, and a lost laptop is revoked on its own without disturbing anyone
//! else.
//!
//! # The replay problem, and why the TLS exporter is in the signature
//!
//! A signature over a server nonce alone is enough to stop an attacker who has recorded an old
//! handshake — the nonce is fresh each time. It is **not** enough against an attacker sitting
//! in the middle of the current connection, who can take the signature out of the terminal's
//! `Hello` and present it on their own TLS session to the core.
//!
//! RFC 5705 exported keying material closes that. The exporter value is derived from the TLS
//! session's own secrets, so it differs between the terminal's connection and the attacker's,
//! and a signature bound to one is worthless on the other. This is why
//! [`Transcript`] takes an exporter and why `a_signature_does_not_transfer_between_sessions`
//! is the test that matters most in this file.
//!
//! # Enrolment
//!
//! A key has to be introduced once. The operator asks the core for a code, types it into the
//! terminal, and the terminal sends its public key with it. The code is single-use and expires
//! in ten minutes, because it is the one moment where possession of a short string is enough
//! to gain access — and unlike the token it replaces, that moment ends.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::{Duration, Instant};

pub use ed25519_dalek::{Signature, SigningKey, VerifyingKey, SIGNATURE_LENGTH};

/// How long an enrolment code is good for.
pub const ENROLL_CODE_TTL: Duration = Duration::from_secs(600);
/// Length of the exported keying material taken from TLS.
pub const EXPORTER_LEN: usize = 32;
/// RFC 5705 label. Namespaced so the same TLS session cannot be made to produce the same
/// material for two different purposes.
pub const EXPORTER_LABEL: &[u8] = b"EXPORTER-moon-own-device-auth";

/// Domain separator prefixed to everything signed.
///
/// Without it, a signature produced for one purpose could be presented as if it had been
/// produced for another — the classic cross-protocol attack. The version is inside the string
/// so that changing what is signed later cannot be made to look like the old format.
const TRANSCRIPT_DOMAIN: &[u8] = b"moon-own/device-auth/v1\0";

/// Stable identity of one terminal installation.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DeviceId(pub [u8; 16]);

impl DeviceId {
    /// Derived from the public key rather than assigned, so it cannot be claimed by a device
    /// that does not hold the matching private key.
    pub fn of_key(public: &VerifyingKey) -> Self {
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(public.as_bytes());
        let mut id = [0u8; 16];
        id.copy_from_slice(&digest[..16]);
        Self(id)
    }

    pub fn to_hex(self) -> String {
        hex::encode(self.0)
    }
}

impl std::fmt::Display for DeviceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl std::fmt::Debug for DeviceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "DeviceId({})", self.to_hex())
    }
}

/// Exactly what gets signed, assembled in one place.
///
/// A single constructor rather than a signing function and a verifying function that each
/// build their own bytes: two implementations of a transcript drift, and when they do the
/// failure is an authentication that succeeds when it should not.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Transcript(Vec<u8>);

impl Transcript {
    pub fn new(
        server_nonce: &[u8; 32],
        client_nonce: &[u8; 32],
        device: DeviceId,
        protocol: u16,
        tls_exporter: &[u8; EXPORTER_LEN],
    ) -> Self {
        let mut bytes = Vec::with_capacity(TRANSCRIPT_DOMAIN.len() + 32 + 32 + 16 + 2 + EXPORTER_LEN);
        bytes.extend_from_slice(TRANSCRIPT_DOMAIN);
        bytes.extend_from_slice(server_nonce);
        bytes.extend_from_slice(client_nonce);
        bytes.extend_from_slice(&device.0);
        bytes.extend_from_slice(&protocol.to_be_bytes());
        bytes.extend_from_slice(tls_exporter);
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

/// A terminal's private key.
///
/// No `Debug`, no `Clone`, no accessor for the secret bytes: the only things that can be done
/// with it are signing and exporting the public half. `ed25519_dalek::SigningKey` zeroises on
/// drop, so a copy is not left behind in freed memory.
pub struct DeviceKey {
    signing: SigningKey,
}

impl std::fmt::Debug for DeviceKey {
    /// Hand-written so a stray `{:?}` cannot put a private key in a log.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceKey")
            .field("device_id", &self.device_id())
            .field("secret", &"<redacted>")
            .finish()
    }
}

impl DeviceKey {
    /// Fresh key from the operating system's randomness.
    ///
    /// Bytes are taken directly rather than through a generic RNG parameter: the only correct
    /// source here is the OS, and letting a caller pass something else is an opportunity for
    /// a test double to become a production key.
    pub fn generate() -> Self {
        Self { signing: SigningKey::from_bytes(&rand::random::<[u8; 32]>()) }
    }

    /// Restore from stored bytes. The caller is responsible for where they were stored;
    /// see task 2.7 for the OS keystore.
    pub fn from_bytes(secret: &[u8; 32]) -> Self {
        Self { signing: SigningKey::from_bytes(secret) }
    }

    /// The bytes to hand to a keystore. Deliberately awkward to reach for.
    pub fn secret_to_store(&self) -> [u8; 32] {
        self.signing.to_bytes()
    }

    pub fn public(&self) -> VerifyingKey {
        self.signing.verifying_key()
    }

    pub fn device_id(&self) -> DeviceId {
        DeviceId::of_key(&self.public())
    }

    pub fn sign(&self, transcript: &Transcript) -> Signature {
        use ed25519_dalek::Signer;
        self.signing.sign(transcript.as_bytes())
    }
}

/// One enrolled terminal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Device {
    pub id: DeviceId,
    pub public: VerifyingKey,
    pub label: String,
    pub role: crate::command::Role,
    pub enrolled_at_ms: i64,
    pub revoked: bool,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AuthError {
    #[error("device {0} is not enrolled")]
    UnknownDevice(DeviceId),
    #[error("device {0} has been revoked")]
    Revoked(DeviceId),
    #[error("signature does not verify")]
    BadSignature,
    #[error("signature is {0} bytes, expected {SIGNATURE_LENGTH}")]
    MalformedSignature(usize),
    #[error("enrolment code is not recognised")]
    UnknownCode,
    #[error("enrolment code has expired")]
    ExpiredCode,
    #[error("public key is not a valid Ed25519 point")]
    BadPublicKey,
    #[error("device {0} is already enrolled")]
    AlreadyEnrolled(DeviceId),
}

/// What the core knows about the terminals allowed to connect.
#[derive(Debug, Default)]
pub struct DeviceRegistry {
    devices: HashMap<DeviceId, Device>,
    codes: HashMap<String, Pending>,
}

#[derive(Debug)]
struct Pending {
    issued_at: Instant,
    role: crate::command::Role,
}

impl DeviceRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Issue a one-time enrolment code.
    ///
    /// The code is what the operator reads off the core and types into the terminal. It grants
    /// the role it was issued for, so handing someone a code to watch the screen cannot make
    /// them able to trade.
    pub fn issue_code(&mut self, code: impl Into<String>, role: crate::command::Role, now: Instant) {
        self.codes.insert(code.into(), Pending { issued_at: now, role });
    }

    pub fn pending_codes(&self) -> usize {
        self.codes.len()
    }

    /// Redeem a code, enrolling the key that came with it.
    ///
    /// The code is consumed whether or not the rest succeeds, because a code that survives a
    /// failed attempt can be brute-forced by repeating that failure.
    pub fn enroll(
        &mut self,
        code: &str,
        public: &[u8; 32],
        label: impl Into<String>,
        now: Instant,
        now_ms: i64,
    ) -> Result<Device, AuthError> {
        let Some(pending) = self.codes.remove(code) else {
            return Err(AuthError::UnknownCode);
        };
        if now.duration_since(pending.issued_at) > ENROLL_CODE_TTL {
            return Err(AuthError::ExpiredCode);
        }

        let public = VerifyingKey::from_bytes(public).map_err(|_| AuthError::BadPublicKey)?;
        let id = DeviceId::of_key(&public);
        if self.devices.contains_key(&id) {
            return Err(AuthError::AlreadyEnrolled(id));
        }

        let device = Device {
            id,
            public,
            label: label.into(),
            role: pending.role,
            enrolled_at_ms: now_ms,
            revoked: false,
        };
        self.devices.insert(id, device.clone());
        Ok(device)
    }

    /// Verify a `Hello` signature against the enrolled key.
    ///
    /// Returns the device so the caller can take its role from the registry rather than from
    /// anything the terminal claimed.
    pub fn verify(
        &self,
        device: DeviceId,
        transcript: &Transcript,
        signature: &[u8],
    ) -> Result<&Device, AuthError> {
        let Some(entry) = self.devices.get(&device) else {
            return Err(AuthError::UnknownDevice(device));
        };
        // Checked before the signature: a revoked device that presents a perfect signature
        // must still be refused, and doing the cheap check first also avoids spending a
        // verification on it.
        if entry.revoked {
            return Err(AuthError::Revoked(device));
        }

        let bytes: [u8; SIGNATURE_LENGTH] =
            signature.try_into().map_err(|_| AuthError::MalformedSignature(signature.len()))?;

        use ed25519_dalek::Verifier;
        entry
            .public
            .verify(transcript.as_bytes(), &Signature::from_bytes(&bytes))
            .map_err(|_| AuthError::BadSignature)?;
        Ok(entry)
    }

    /// Stop accepting a device. Kept rather than deleted so an audit trail still resolves its
    /// identity, and so re-enrolling the same key is a deliberate act.
    pub fn revoke(&mut self, device: DeviceId) -> bool {
        match self.devices.get_mut(&device) {
            Some(entry) => {
                entry.revoked = true;
                true
            }
            None => false,
        }
    }

    pub fn get(&self, device: DeviceId) -> Option<&Device> {
        self.devices.get(&device)
    }

    pub fn len(&self) -> usize {
        self.devices.len()
    }

    pub fn is_empty(&self) -> bool {
        self.devices.is_empty()
    }

    /// Drop codes nobody redeemed. Called on a timer; without it an unattended core
    /// accumulates them.
    pub fn expire_codes(&mut self, now: Instant) -> usize {
        let before = self.codes.len();
        self.codes.retain(|_, p| now.duration_since(p.issued_at) <= ENROLL_CODE_TTL);
        before - self.codes.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::Role;

    fn now() -> Instant {
        static ORIGIN: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
        *ORIGIN.get_or_init(Instant::now)
    }

    fn at(secs: u64) -> Instant {
        now() + Duration::from_secs(secs)
    }

    /// The exported keying material of one TLS session. Different sessions differ; that is the
    /// entire property being relied on.
    fn exporter(session: u8) -> [u8; EXPORTER_LEN] {
        [session; EXPORTER_LEN]
    }

    fn transcript(device: DeviceId, session: u8) -> Transcript {
        Transcript::new(&[1; 32], &[2; 32], device, 2, &exporter(session))
    }

    /// Thirty-two bytes that are not a valid compressed Edwards point.
    ///
    /// Found by probing rather than assumed: the obvious candidate, `[0xff; 32]`, decompresses
    /// perfectly well, and a test built on that assumption was passing for the wrong reason.
    fn not_a_curve_point() -> [u8; 32] {
        let mut bytes = [0u8; 32];
        bytes[0] = 1;
        bytes[31] = 3;
        debug_assert!(VerifyingKey::from_bytes(&bytes).is_err(), "probe value stopped being invalid");
        bytes
    }

    /// A registry with one device enrolled, and its key.
    fn enrolled(role: Role) -> (DeviceRegistry, DeviceKey) {
        let mut registry = DeviceRegistry::new();
        let key = DeviceKey::generate();
        registry.issue_code("123456", role, now());
        registry
            .enroll("123456", key.public().as_bytes(), "laptop", now(), 1_700_000_000_000)
            .expect("enrolment succeeds");
        (registry, key)
    }

    // --- the invariant this module exists for ---------------------------------------------

    #[test]
    fn a_signature_does_not_transfer_between_sessions() {
        // **The test that matters most here.** An attacker in the middle of the live
        // connection can lift the signature out of the terminal's Hello. Binding it to the
        // TLS exporter is what makes that copy worthless on their own session.
        let (registry, key) = enrolled(Role::Trader);
        let device = key.device_id();

        let theirs = transcript(device, 1);
        let signature = key.sign(&theirs);
        assert!(registry.verify(device, &theirs, &signature.to_bytes()).is_ok());

        // The same bytes, replayed on a different TLS session.
        let attackers = transcript(device, 2);
        assert_eq!(
            registry.verify(device, &attackers, &signature.to_bytes()),
            Err(AuthError::BadSignature),
            "a signature lifted from one session must not authenticate another"
        );
    }

    #[test]
    fn every_part_of_the_transcript_is_load_bearing() {
        // If any component could be changed without breaking the signature, an attacker could
        // vary it. Checked one at a time rather than assumed from how the bytes are laid out.
        let (registry, key) = enrolled(Role::Trader);
        let device = key.device_id();

        let base = Transcript::new(&[1; 32], &[2; 32], device, 2, &exporter(1));
        let signature = key.sign(&base).to_bytes();
        assert!(registry.verify(device, &base, &signature).is_ok());

        let variations = [
            ("server nonce", Transcript::new(&[9; 32], &[2; 32], device, 2, &exporter(1))),
            ("client nonce", Transcript::new(&[1; 32], &[9; 32], device, 2, &exporter(1))),
            ("protocol", Transcript::new(&[1; 32], &[2; 32], device, 3, &exporter(1))),
            ("exporter", Transcript::new(&[1; 32], &[2; 32], device, 2, &exporter(9))),
            ("device id", Transcript::new(&[1; 32], &[2; 32], DeviceId([7; 16]), 2, &exporter(1))),
        ];
        for (what, altered) in variations {
            assert!(
                registry.verify(device, &altered, &signature).is_err(),
                "changing the {what} must invalidate the signature"
            );
        }
    }

    #[test]
    fn the_transcript_is_domain_separated() {
        // So that a signature made here cannot be presented as one made for another purpose.
        let t = transcript(DeviceId([0; 16]), 1);
        assert!(t.as_bytes().starts_with(TRANSCRIPT_DOMAIN));
        assert!(
            TRANSCRIPT_DOMAIN.ends_with(b"\0"),
            "the separator must terminate, or a longer domain could be confused with it"
        );
    }

    // --- enrolment ---------------------------------------------------------------------------

    #[test]
    fn a_code_works_once_and_only_once() {
        // A code that survives its use is a token by another name.
        let mut registry = DeviceRegistry::new();
        registry.issue_code("abc", Role::Trader, now());

        let first = DeviceKey::generate();
        assert!(registry.enroll("abc", first.public().as_bytes(), "one", now(), 1).is_ok());

        let second = DeviceKey::generate();
        assert_eq!(
            registry.enroll("abc", second.public().as_bytes(), "two", now(), 1),
            Err(AuthError::UnknownCode),
            "the code must not enrol a second device"
        );
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn an_expired_code_is_refused() {
        let mut registry = DeviceRegistry::new();
        registry.issue_code("abc", Role::Trader, now());
        let key = DeviceKey::generate();

        let just_inside = at(ENROLL_CODE_TTL.as_secs());
        let mut r2 = DeviceRegistry::new();
        r2.issue_code("abc", Role::Trader, now());
        assert!(r2.enroll("abc", key.public().as_bytes(), "l", just_inside, 1).is_ok());

        let past = at(ENROLL_CODE_TTL.as_secs() + 1);
        assert_eq!(
            registry.enroll("abc", key.public().as_bytes(), "l", past, 1),
            Err(AuthError::ExpiredCode)
        );
    }

    #[test]
    fn a_failed_enrolment_still_consumes_the_code() {
        // Otherwise a wrong guess costs nothing and the code can be brute-forced by repeating
        // the failure.
        let mut registry = DeviceRegistry::new();
        registry.issue_code("abc", Role::Trader, now());

        assert_eq!(
            registry.enroll("abc", &not_a_curve_point(), "bad key", now(), 1),
            Err(AuthError::BadPublicKey)
        );
        assert_eq!(registry.pending_codes(), 0, "the code must be spent either way");

        let key = DeviceKey::generate();
        assert_eq!(
            registry.enroll("abc", key.public().as_bytes(), "l", now(), 1),
            Err(AuthError::UnknownCode)
        );
    }

    #[test]
    fn a_code_carries_the_role_it_was_issued_for() {
        // Handing someone a code so they can watch the screen must not make them able to trade.
        let mut registry = DeviceRegistry::new();
        registry.issue_code("watch", Role::Viewer, now());
        let key = DeviceKey::generate();

        let device = registry.enroll("watch", key.public().as_bytes(), "guest", now(), 1).unwrap();
        assert_eq!(device.role, Role::Viewer);
    }

    #[test]
    fn unredeemed_codes_are_swept_up() {
        let mut registry = DeviceRegistry::new();
        registry.issue_code("a", Role::Trader, now());
        registry.issue_code("b", Role::Trader, at(300));
        assert_eq!(registry.pending_codes(), 2);

        // Past the first code's life but not the second's.
        let removed = registry.expire_codes(at(ENROLL_CODE_TTL.as_secs() + 1));
        assert_eq!(removed, 1);
        assert_eq!(registry.pending_codes(), 1);
    }

    #[test]
    fn the_same_key_cannot_be_enrolled_twice() {
        let mut registry = DeviceRegistry::new();
        let key = DeviceKey::generate();
        registry.issue_code("a", Role::Trader, now());
        registry.issue_code("b", Role::Trader, now());

        assert!(registry.enroll("a", key.public().as_bytes(), "first", now(), 1).is_ok());
        assert!(matches!(
            registry.enroll("b", key.public().as_bytes(), "again", now(), 1),
            Err(AuthError::AlreadyEnrolled(_))
        ));
    }

    // --- identity and revocation ----------------------------------------------------------------

    #[test]
    fn a_device_id_is_derived_from_its_key_not_claimed() {
        // A terminal cannot pick an id, so it cannot claim another device's identity and hope
        // the signature check is skipped somewhere.
        let key = DeviceKey::generate();
        assert_eq!(DeviceId::of_key(&key.public()), key.device_id());

        let other = DeviceKey::generate();
        assert_ne!(key.device_id(), other.device_id());
    }

    #[test]
    fn an_unknown_device_is_refused_before_any_verification() {
        let (registry, _) = enrolled(Role::Trader);
        let stranger = DeviceKey::generate();
        assert_eq!(
            registry.verify(
                stranger.device_id(),
                &transcript(stranger.device_id(), 1),
                &stranger.sign(&transcript(stranger.device_id(), 1)).to_bytes()
            ),
            Err(AuthError::UnknownDevice(stranger.device_id()))
        );
    }

    #[test]
    fn a_revoked_device_is_refused_even_with_a_perfect_signature() {
        // The reason revocation exists: a lost laptop still holds a working key.
        let (mut registry, key) = enrolled(Role::Trader);
        let device = key.device_id();
        let t = transcript(device, 1);
        assert!(registry.verify(device, &t, &key.sign(&t).to_bytes()).is_ok());

        assert!(registry.revoke(device));
        assert_eq!(registry.verify(device, &t, &key.sign(&t).to_bytes()), Err(AuthError::Revoked(device)));
    }

    #[test]
    fn revocation_keeps_the_record_so_an_audit_trail_still_resolves() {
        let (mut registry, key) = enrolled(Role::Trader);
        registry.revoke(key.device_id());
        let entry = registry.get(key.device_id()).expect("the record survives revocation");
        assert!(entry.revoked);
        assert_eq!(entry.label, "laptop");
    }

    #[test]
    fn revoking_something_that_was_never_enrolled_is_not_an_error_that_lies() {
        let mut registry = DeviceRegistry::new();
        assert!(!registry.revoke(DeviceId([0; 16])), "must report that nothing was revoked");
    }

    #[test]
    fn the_role_comes_from_the_registry_not_from_the_terminal() {
        // A terminal that could name its own role would simply name Admin.
        let (registry, key) = enrolled(Role::Viewer);
        let t = transcript(key.device_id(), 1);
        let device = registry.verify(key.device_id(), &t, &key.sign(&t).to_bytes()).unwrap();
        assert_eq!(device.role, Role::Viewer);
    }

    // --- malformed input -------------------------------------------------------------------------

    #[test]
    fn a_signature_of_the_wrong_length_is_rejected_by_length_not_by_maths() {
        let (registry, key) = enrolled(Role::Trader);
        let t = transcript(key.device_id(), 1);
        for bad in [vec![], vec![0u8; 32], vec![0u8; 65]] {
            let len = bad.len();
            assert_eq!(registry.verify(key.device_id(), &t, &bad), Err(AuthError::MalformedSignature(len)));
        }
    }

    #[test]
    fn a_signature_from_another_key_does_not_verify() {
        let (registry, key) = enrolled(Role::Trader);
        let impostor = DeviceKey::generate();
        let t = transcript(key.device_id(), 1);
        assert_eq!(
            registry.verify(key.device_id(), &t, &impostor.sign(&t).to_bytes()),
            Err(AuthError::BadSignature)
        );
    }

    // --- key handling ------------------------------------------------------------------------------

    #[test]
    fn a_private_key_never_renders_itself() {
        // The same rule as `Auth` and the exchange credentials: a stray `{:?}` must not put a
        // key in a log.
        let key = DeviceKey::generate();
        let rendered = format!("{key:?}");
        assert!(rendered.contains("<redacted>"));

        let secret = hex::encode(key.secret_to_store());
        assert!(!rendered.contains(&secret), "the secret leaked through Debug");
    }

    #[test]
    fn a_key_survives_being_stored_and_restored() {
        // What task 2.7 will put in the OS keystore.
        let key = DeviceKey::generate();
        let restored = DeviceKey::from_bytes(&key.secret_to_store());

        assert_eq!(restored.device_id(), key.device_id());
        let t = transcript(key.device_id(), 1);
        assert_eq!(restored.sign(&t).to_bytes(), key.sign(&t).to_bytes());
    }

    #[test]
    fn two_generated_keys_differ() {
        // Cheap, but a broken source of randomness here would make every terminal the same
        // device and would otherwise show up only as a confusing enrolment failure.
        let a = DeviceKey::generate();
        let b = DeviceKey::generate();
        assert_ne!(a.secret_to_store(), b.secret_to_store());
        assert_ne!(a.public().as_bytes(), b.public().as_bytes());
    }
}
