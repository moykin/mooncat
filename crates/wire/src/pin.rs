//! Trusting the core's key instead of a certificate authority.
//!
//! # Why not a normal certificate
//!
//! The usual model answers "is this really `example.com`?" by asking a certificate authority.
//! A core has no domain name worth buying a certificate for, often no name at all — just an
//! address on a VPS — and the operator and the person connecting are the same human. There is
//! nobody a CA could vouch to.
//!
//! What actually needs answering is narrower: **is this the same core I set up?** A pin
//! answers exactly that. The terminal records the hash of the core's public key the first time
//! it is configured, and refuses anything else afterwards.
//!
//! # Why the public key and not the certificate
//!
//! A certificate expires; a self-signed one for ten years still eventually does. Re-issuing it
//! from the same private key produces different certificate bytes and the same key, so pinning
//! the key means a renewal costs nothing, while pinning the certificate would mean visiting
//! every terminal.
//!
//! # What this does not defend against
//!
//! The first connection. If the operator pins a hash handed to them by an attacker, everything
//! afterwards faithfully trusts the attacker. The pin is transferred out of band, by the same
//! person who generated the certificate — which is the whole reason this model fits here and
//! would not fit a public service.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tokio_rustls::rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use tokio_rustls::rustls::{DigitallySignedStruct, Error as TlsError, SignatureScheme};

/// SHA-256 of a certificate's `SubjectPublicKeyInfo`, DER-encoded.
///
/// The same value `openssl x509 -pubkey | openssl pkey -pubin -outform der | sha256sum`
/// produces, so an operator can check a pin without running this program.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SpkiPin(#[serde(with = "hex_bytes")] pub [u8; 32]);

impl SpkiPin {
    /// Compute the pin of a DER-encoded certificate.
    pub fn of_certificate(der: &[u8]) -> Result<Self, PinError> {
        let (_, cert) =
            x509_parser::parse_x509_certificate(der).map_err(|e| PinError::NotACertificate(e.to_string()))?;
        Ok(Self::of_spki(cert.tbs_certificate.subject_pki.raw))
    }

    /// Compute the pin of an already-extracted `SubjectPublicKeyInfo`.
    pub fn of_spki(spki_der: &[u8]) -> Self {
        Self(Sha256::digest(spki_der).into())
    }

    pub fn to_hex(self) -> String {
        hex::encode(self.0)
    }

    pub fn from_hex(s: &str) -> Result<Self, PinError> {
        let bytes = hex::decode(s.trim()).map_err(|e| PinError::BadHex(e.to_string()))?;
        let array: [u8; 32] = bytes.as_slice().try_into().map_err(|_| PinError::WrongLength(bytes.len()))?;
        Ok(Self(array))
    }
}

/// Prints as hex, so a pin in a log or an error message is one an operator can compare.
impl std::fmt::Display for SpkiPin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl std::fmt::Debug for SpkiPin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SpkiPin({})", self.to_hex())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PinError {
    #[error("not a parsable X.509 certificate: {0}")]
    NotACertificate(String),
    #[error("pin is not valid hex: {0}")]
    BadHex(String),
    #[error("pin is {0} bytes, expected 32")]
    WrongLength(usize),
}

/// Serialises a pin as a hex string rather than an array of 32 numbers, so a config file holds
/// something a human can copy.
mod hex_bytes {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8; 32], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&hex::encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 32], D::Error> {
        let text = String::deserialize(d)?;
        let bytes = hex::decode(text.trim()).map_err(serde::de::Error::custom)?;
        bytes
            .as_slice()
            .try_into()
            .map_err(|_| serde::de::Error::custom(format!("pin is {} bytes, expected 32", bytes.len())))
    }
}

/// Accepts exactly the pinned keys and nothing else.
///
/// A list rather than one, so a key rotation can be staged: the terminal is given the new pin
/// alongside the old, the core switches over, and the old one is removed afterwards. Without
/// that overlap a rotation means every terminal is offline until someone visits it.
#[derive(Debug)]
pub struct PinnedVerifier {
    accepted: Vec<SpkiPin>,
    /// Signature schemes to advertise. Taken from the process-wide crypto provider so this
    /// cannot drift from what the connection can actually negotiate.
    schemes: Vec<SignatureScheme>,
}

impl PinnedVerifier {
    pub fn new(accepted: Vec<SpkiPin>) -> Arc<Self> {
        let schemes = tokio_rustls::rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes();
        Arc::new(Self { accepted, schemes })
    }

    pub fn accepts(&self, pin: SpkiPin) -> bool {
        self.accepted.contains(&pin)
    }
}

impl ServerCertVerifier for PinnedVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        // The name is deliberately not checked. A pin identifies the key, and the core is
        // reached by address; requiring a matching name would mean re-issuing the certificate
        // every time the VPS moves, for no gain — an attacker who could present the pinned
        // key would not be stopped by also having to claim a name.
        let pin = SpkiPin::of_certificate(end_entity)
            .map_err(|e| TlsError::General(format!("cannot pin this certificate: {e}")))?;

        if self.accepts(pin) {
            return Ok(ServerCertVerified::assertion());
        }
        // Naming both sides: on a real mismatch the operator needs to see what arrived in
        // order to decide whether they are being attacked or simply forgot a rotation.
        Err(TlsError::General(format!(
            "certificate key {pin} is not pinned; expected one of [{}]",
            self.accepted.iter().map(|p| p.to_hex()).collect::<Vec<_>>().join(", ")
        )))
    }

    /// Signature checking is left to rustls: the pin decides *whose* key, and rustls still has
    /// to confirm the peer actually holds it. Skipping this would let anyone replay a copy of
    /// the certificate.
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        tokio_rustls::rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &tokio_rustls::rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        tokio_rustls::rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &tokio_rustls::rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.schemes.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A self-signed pair, generated the way an operator would. Returns the DER certificate
    /// and the directory holding it.
    fn self_signed(tag: &str) -> Option<(TempDir, Vec<u8>)> {
        let dir = TempDir::new(tag);
        let (cert, key) = (dir.0.join("cert.pem"), dir.0.join("key.pem"));
        let ok = std::process::Command::new("openssl")
            .args(["req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1", "-subj", "/CN=core"])
            .arg("-keyout")
            .arg(&key)
            .arg("-out")
            .arg(&cert)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !ok {
            return None;
        }
        let pem = std::fs::read(&cert).ok()?;
        let der = rustls_pemfile::certs(&mut pem.as_slice()).next()?.ok()?;
        Some((dir, der.to_vec()))
    }

    /// Same private key, a second certificate issued from it — a renewal.
    fn reissue(dir: &TempDir) -> Option<Vec<u8>> {
        let (key, cert2) = (dir.0.join("key.pem"), dir.0.join("cert2.pem"));
        let ok = std::process::Command::new("openssl")
            .args(["req", "-x509", "-days", "3650", "-subj", "/CN=core-renewed"])
            .arg("-key")
            .arg(&key)
            .arg("-out")
            .arg(&cert2)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !ok {
            return None;
        }
        let pem = std::fs::read(&cert2).ok()?;
        let der = rustls_pemfile::certs(&mut pem.as_slice()).next()?.ok()?;
        Some(der.to_vec())
    }

    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!("moon-pin-{}-{n}-{tag}", std::process::id()));
            std::fs::create_dir_all(&path).expect("temp dir");
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_pin_is_stable_for_the_same_certificate() {
        let Some((_dir, der)) = self_signed("stable") else {
            eprintln!("openssl unavailable — skipping");
            return;
        };
        let a = SpkiPin::of_certificate(&der).unwrap();
        let b = SpkiPin::of_certificate(&der).unwrap();
        assert_eq!(a, b);
        assert_eq!(a.to_hex().len(), 64, "a SHA-256 is 32 bytes, 64 hex characters");
    }

    #[test]
    fn two_different_keys_pin_differently() {
        let (Some((_d1, a)), Some((_d2, b))) = (self_signed("k1"), self_signed("k2")) else {
            return;
        };
        assert_ne!(
            SpkiPin::of_certificate(&a).unwrap(),
            SpkiPin::of_certificate(&b).unwrap(),
            "distinct keys must not collide"
        );
    }

    #[test]
    fn reissuing_the_certificate_from_the_same_key_keeps_the_pin() {
        // The reason the public key is pinned rather than the certificate: a renewal must not
        // require visiting every terminal.
        let Some((dir, original)) = self_signed("renew") else {
            return;
        };
        let Some(renewed) = reissue(&dir) else {
            return;
        };

        assert_ne!(original, renewed, "the certificates differ, as they must");
        assert_eq!(
            SpkiPin::of_certificate(&original).unwrap(),
            SpkiPin::of_certificate(&renewed).unwrap(),
            "the same key must produce the same pin"
        );
    }

    #[test]
    fn a_valid_certificate_for_a_different_key_is_refused() {
        // The attack this exists to stop: a proxy that presents a perfectly valid certificate
        // — its own. Chain validation would accept it; the pin does not.
        let (Some((_d1, ours)), Some((_d2, theirs))) = (self_signed("ours"), self_signed("mitm")) else {
            return;
        };
        let verifier = PinnedVerifier::new(vec![SpkiPin::of_certificate(&ours).unwrap()]);

        assert!(verifier.accepts(SpkiPin::of_certificate(&ours).unwrap()));
        assert!(
            !verifier.accepts(SpkiPin::of_certificate(&theirs).unwrap()),
            "a valid certificate for the wrong key must be rejected"
        );
    }

    #[test]
    fn a_rotation_can_be_staged_with_both_pins_accepted() {
        // Without an overlap window, rotating the core's key means every terminal is offline
        // until someone visits it.
        let (Some((_d1, old)), Some((_d2, new))) = (self_signed("old"), self_signed("new")) else {
            return;
        };
        let (old_pin, new_pin) =
            (SpkiPin::of_certificate(&old).unwrap(), SpkiPin::of_certificate(&new).unwrap());

        let during = PinnedVerifier::new(vec![old_pin, new_pin]);
        assert!(during.accepts(old_pin) && during.accepts(new_pin));

        let after = PinnedVerifier::new(vec![new_pin]);
        assert!(!after.accepts(old_pin), "once retired, the old key must stop working");
    }

    #[test]
    fn garbage_is_not_a_certificate() {
        assert!(SpkiPin::of_certificate(b"not a certificate").is_err());
        assert!(SpkiPin::of_certificate(&[]).is_err());
    }

    #[test]
    fn a_pin_survives_hex_and_config_round_trips() {
        // It lives in a config file and gets pasted between machines, so both directions have
        // to be exact.
        let pin = SpkiPin::of_spki(b"some key material");
        assert_eq!(SpkiPin::from_hex(&pin.to_hex()).unwrap(), pin);
        assert_eq!(SpkiPin::from_hex(&format!("  {}\n", pin.to_hex())).unwrap(), pin);

        let encoded = rmp_serde::to_vec_named(&pin).unwrap();
        assert_eq!(rmp_serde::from_slice::<SpkiPin>(&encoded).unwrap(), pin);

        let as_value: rmpv::Value = rmp_serde::from_slice(&encoded).unwrap();
        assert!(as_value.is_str(), "a pin must serialise as a string an operator can read");
    }

    #[test]
    fn a_malformed_pin_is_refused_rather_than_padded() {
        assert!(SpkiPin::from_hex("zz").is_err());
        assert!(SpkiPin::from_hex("aabb").is_err(), "too short must not be zero-extended");
        assert!(SpkiPin::from_hex(&"aa".repeat(33)).is_err(), "too long must not be truncated");
    }

    #[test]
    fn a_pin_prints_as_hex_everywhere_it_could_be_compared() {
        // An operator comparing a pin from a log against one from `openssl` should not have to
        // translate between representations.
        let pin = SpkiPin([0xab; 32]);
        assert_eq!(pin.to_string(), "ab".repeat(32));
        assert!(format!("{pin:?}").contains(&"ab".repeat(32)));
    }
}
