//! Server-side TLS for the wire.
//!
//! The core holds the API keys, and the token that lets a terminal talk to it is a bearer
//! credential: whoever reads it off the network becomes the terminal. So the moment the core
//! leaves loopback, the socket has to be encrypted — [`crate::config::Config::validate`]
//! refuses to start otherwise, and this module is what makes the allowed case possible.
//!
//! Self-signed certificates are expected and fine here. There is no browser and no public
//! name to validate: the terminal pins the certificate's public key (task 2.1) rather than
//! trusting a chain. That makes a CA an operational cost with no security return.

use std::path::Path;
use std::sync::Arc;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::TlsAcceptor;

/// Build an acceptor from PEM files on disk.
///
/// Every failure names the file and what was wrong with it. This runs on a VPS during a
/// deploy, where the only diagnostic anyone will see is this string.
pub fn acceptor(cert_path: &Path, key_path: &Path) -> Result<TlsAcceptor, String> {
    install_crypto_provider();
    let certs = load_certs(cert_path)?;
    let key = load_key(key_path)?;

    let config = ServerConfig::builder().with_no_client_auth().with_single_cert(certs, key).map_err(|e| {
        format!(
            "certificate {} and key {} do not form a usable pair: {e}",
            cert_path.display(),
            key_path.display()
        )
    })?;

    Ok(TlsAcceptor::from(Arc::new(config)))
}

/// Pin the crypto backend explicitly.
///
/// Both `ring` and `aws-lc-rs` end up in the dependency graph — different crates in the tree
/// ask for different ones — and rustls refuses to guess when it sees two. Left to chance this
/// surfaces as a panic on the first TLS handshake, meaning at the first connection on the VPS
/// rather than at startup here. `install_default` is idempotent by contract; a second call
/// returns `Err` and that is not a problem.
fn install_crypto_provider() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
    });
}

fn load_certs(path: &Path) -> Result<Vec<CertificateDer<'static>>, String> {
    let pem = std::fs::read(path).map_err(|e| format!("cannot read certificate {}: {e}", path.display()))?;
    let certs: Vec<_> = rustls_pemfile::certs(&mut pem.as_slice())
        .collect::<Result<_, _>>()
        .map_err(|e| format!("certificate {} is not valid PEM: {e}", path.display()))?;

    if certs.is_empty() {
        return Err(format!(
            "certificate {} contains no CERTIFICATE block — a key file passed as the certificate \
             looks exactly like this",
            path.display()
        ));
    }
    Ok(certs)
}

fn load_key(path: &Path) -> Result<PrivateKeyDer<'static>, String> {
    let pem = std::fs::read(path).map_err(|e| format!("cannot read private key {}: {e}", path.display()))?;

    // Accepts PKCS#8, PKCS#1 and SEC1. Which one a tool emits is not something an operator
    // chooses on purpose, so refusing any of them would only produce a confusing error.
    rustls_pemfile::private_key(&mut pem.as_slice())
        .map_err(|e| format!("private key {} is not valid PEM: {e}", path.display()))?
        .ok_or_else(|| {
            format!(
                "private key {} contains no PRIVATE KEY block — if this is the certificate, the \
                 two paths are swapped",
                path.display()
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `TlsAcceptor` has no `Debug`, so `unwrap_err` will not compile on this Result.
    fn error(result: Result<TlsAcceptor, String>) -> String {
        match result {
            Ok(_) => panic!("expected an error, got a working acceptor"),
            Err(e) => e,
        }
    }

    /// A throwaway self-signed pair, generated the way an operator would.
    ///
    /// Shelling out to openssl rather than pulling in a certificate-generation crate: this is
    /// the only place in the tree that needs one, and the dependency would ship to the VPS.
    fn self_signed() -> Option<(tempdir::Dir, std::path::PathBuf, std::path::PathBuf)> {
        let dir = tempdir::Dir::new("tls");
        let (cert, key) = (dir.path().join("cert.pem"), dir.path().join("key.pem"));
        let ok = std::process::Command::new("openssl")
            .args(["req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1", "-subj", "/CN=localhost"])
            .arg("-keyout")
            .arg(&key)
            .arg("-out")
            .arg(&cert)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        ok.then_some((dir, cert, key))
    }

    #[test]
    fn a_self_signed_pair_builds_an_acceptor() {
        let Some((_dir, cert, key)) = self_signed() else {
            eprintln!("openssl unavailable — skipping");
            return;
        };
        assert!(acceptor(&cert, &key).is_ok());
    }

    #[test]
    fn swapping_the_two_paths_says_so() {
        // The single most likely deployment mistake, and the default rustls error for it
        // ("no private key found") does not hint at the cause.
        let Some((_dir, cert, key)) = self_signed() else {
            return;
        };
        let err = error(acceptor(&key, &cert));
        assert!(err.contains("swapped") || err.contains("no CERTIFICATE block"), "got: {err}");
    }

    #[test]
    fn a_missing_certificate_names_the_path() {
        let err = error(acceptor(Path::new("/nope/cert.pem"), Path::new("/nope/key.pem")));
        assert!(err.contains("/nope/cert.pem"), "got: {err}");
    }

    #[test]
    fn garbage_is_rejected_as_pem_not_accepted_as_empty() {
        let dir = tempdir::Dir::new("garbage");
        let (cert, key) = (dir.path().join("cert.pem"), dir.path().join("key.pem"));
        std::fs::write(&cert, "this is not a certificate").unwrap();
        std::fs::write(&key, "this is not a key").unwrap();

        let err = error(acceptor(&cert, &key));
        assert!(err.contains("no CERTIFICATE block"), "got: {err}");
    }

    /// A directory that removes itself, so a failing assertion leaves no litter.
    pub mod tempdir {
        use std::path::{Path, PathBuf};

        pub struct Dir(PathBuf);

        impl Dir {
            pub fn new(tag: &str) -> Self {
                static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
                let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let path = std::env::temp_dir().join(format!("moon-tls-{}-{n}-{tag}", std::process::id()));
                std::fs::create_dir_all(&path).expect("temp dir is creatable");
                Self(path)
            }

            pub fn path(&self) -> &Path {
                &self.0
            }
        }

        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }
}
