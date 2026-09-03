//! TLS for node-to-node connections.
//!
//! Mining fleets are private, so there is no CA to lean on and no public
//! hostname to validate. The model is SSH's: each machine generates a
//! self-signed certificate once, publishes its SHA-256 fingerprint, and the
//! dashboard pins that fingerprint when it adds the node. A man in the middle
//! would have to present a different certificate, which fails the pin.
//!
//! Encryption is not optional for remote connections — the auth token travels
//! inside the TLS session, so an observer on the network learns neither the
//! token nor what the fleet is doing.

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, ServerConfig, SignatureScheme};
use sha2::{Digest, Sha256};

use crate::paths;

/// Fingerprints are shown and stored as `sha256:<hex>`.
pub fn fingerprint(cert: &CertificateDer<'_>) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(cert.as_ref())))
}

/// Normalise user input: accept a bare hex digest, tolerate colons and case.
pub fn normalize_fingerprint(input: &str) -> String {
    let trimmed = input.trim().to_ascii_lowercase();
    let hex: String = trimmed
        .trim_start_matches("sha256:")
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .collect();
    format!("sha256:{hex}")
}

fn cert_path() -> std::path::PathBuf {
    paths::data_dir().join("node.crt")
}

fn key_path() -> std::path::PathBuf {
    paths::data_dir().join("node.key")
}

/// Load this machine's certificate, generating one on first use.
///
/// The key never leaves the machine and is written owner-readable only.
pub fn ensure_identity() -> Result<(CertificateDer<'static>, PrivateKeyDer<'static>)> {
    paths::ensure_dirs()?;
    let (cert_file, key_file) = (cert_path(), key_path());

    if cert_file.exists() && key_file.exists() {
        let cert = std::fs::read(&cert_file)
            .with_context(|| format!("reading {}", cert_file.display()))?;
        let key =
            std::fs::read(&key_file).with_context(|| format!("reading {}", key_file.display()))?;
        return Ok((
            CertificateDer::from(cert),
            PrivateKeyDer::try_from(key).map_err(|e| anyhow::anyhow!("bad node key: {e}"))?,
        ));
    }

    // The name is cosmetic: verification is by fingerprint, not by hostname,
    // because a mining box rarely has a name a certificate could attest to.
    let generated = rcgen::generate_simple_self_signed(vec![
        "cryptocli-node".to_string(),
        "localhost".to_string(),
    ])
    .context("generating a node certificate")?;

    let cert_der = generated.cert.der().to_vec();
    let key_der = generated.signing_key.serialize_der();
    std::fs::write(&cert_file, &cert_der)
        .with_context(|| format!("writing {}", cert_file.display()))?;
    std::fs::write(&key_file, &key_der)
        .with_context(|| format!("writing {}", key_file.display()))?;
    restrict(&key_file)?;

    Ok((
        CertificateDer::from(cert_der),
        PrivateKeyDer::try_from(key_der).map_err(|e| anyhow::anyhow!("bad generated key: {e}"))?,
    ))
}

#[cfg(unix)]
fn restrict(path: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("restricting {}", path.display()))
}

/// This machine's fingerprint, for pasting into another machine's `node add`.
pub fn local_fingerprint() -> Result<String> {
    let (cert, _) = ensure_identity()?;
    Ok(fingerprint(&cert))
}

/// Server side, for the daemon's remote listener.
pub fn server_config() -> Result<Arc<ServerConfig>> {
    let (cert, key) = ensure_identity()?;
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .context("building the TLS server config")?;
    Ok(Arc::new(config))
}

/// Client side, pinned to one certificate fingerprint.
pub fn client_config(expected: &str) -> Result<Arc<ClientConfig>> {
    let expected = normalize_fingerprint(expected);
    if expected == "sha256:" {
        bail!("no certificate fingerprint to pin");
    }
    let mut config = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedVerifier { expected }))
        .with_no_client_auth();
    // Nothing here speaks HTTP; skip ALPN entirely.
    config.alpn_protocols.clear();
    Ok(Arc::new(config))
}

/// Accepts exactly one certificate: the one whose fingerprint we were given.
///
/// This deliberately replaces the usual chain-and-hostname checks. Those verify
/// "a CA vouches for this name"; we instead verify "this is the exact machine
/// the user pinned", which is the stronger statement for a private fleet.
#[derive(Debug)]
struct PinnedVerifier {
    expected: String,
}

impl ServerCertVerifier for PinnedVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let presented = fingerprint(end_entity);
        if presented == self.expected {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(format!(
                "certificate fingerprint mismatch: expected {}, got {presented} — \
                 either the node was reinstalled, or something is intercepting \
                 the connection",
                self.expected
            )))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Fetch a peer's certificate fingerprint without trusting it, so `node add`
/// can show the user what they are about to pin.
pub fn peek_fingerprint(address: &str, timeout: std::time::Duration) -> Result<String> {
    #[derive(Debug)]
    struct Peek(std::sync::Mutex<Option<String>>);

    impl ServerCertVerifier for Peek {
        fn verify_server_cert(
            &self,
            end_entity: &CertificateDer<'_>,
            _i: &[CertificateDer<'_>],
            _n: &ServerName<'_>,
            _o: &[u8],
            _t: UnixTime,
        ) -> Result<ServerCertVerified, rustls::Error> {
            *self.0.lock().unwrap() = Some(fingerprint(end_entity));
            Ok(ServerCertVerified::assertion())
        }
        fn verify_tls12_signature(
            &self,
            m: &[u8],
            c: &CertificateDer<'_>,
            d: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls12_signature(
                m,
                c,
                d,
                &rustls::crypto::ring::default_provider().signature_verification_algorithms,
            )
        }
        fn verify_tls13_signature(
            &self,
            m: &[u8],
            c: &CertificateDer<'_>,
            d: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls13_signature(
                m,
                c,
                d,
                &rustls::crypto::ring::default_provider().signature_verification_algorithms,
            )
        }
        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            rustls::crypto::ring::default_provider()
                .signature_verification_algorithms
                .supported_schemes()
        }
    }

    let seen = Arc::new(Peek(std::sync::Mutex::new(None)));
    let config = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(seen.clone())
        .with_no_client_auth();

    let address = crate::net::normalize_address(address)?;
    let mut socket = crate::net::connect(&address, timeout)?;

    let server_name = ServerName::try_from("cryptocli-node").expect("static name");
    let mut connection = rustls::ClientConnection::new(Arc::new(config), server_name)
        .context("starting the TLS handshake")?;
    // Drive the handshake far enough for the certificate to arrive.
    while connection.is_handshaking() {
        if connection.wants_write() {
            connection.write_tls(&mut socket)?;
            continue;
        }
        if connection.wants_read() {
            if connection.read_tls(&mut socket)? == 0 {
                break;
            }
            connection
                .process_new_packets()
                .context("TLS handshake failed")?;
            continue;
        }
        break;
    }

    let captured = seen.0.lock().unwrap().clone();
    captured.context("the peer did not present a certificate")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprints_are_stable_and_prefixed() {
        let cert = CertificateDer::from(vec![1, 2, 3, 4]);
        let printed = fingerprint(&cert);
        assert!(printed.starts_with("sha256:"));
        assert_eq!(printed.len(), "sha256:".len() + 64);
        assert_eq!(printed, fingerprint(&cert), "must be deterministic");
    }

    #[test]
    fn fingerprint_input_is_forgiving() {
        let canonical = format!("sha256:{}", "ab".repeat(32));
        assert_eq!(normalize_fingerprint(&canonical), canonical);
        assert_eq!(normalize_fingerprint(&"AB".repeat(32)), canonical);
        assert_eq!(
            normalize_fingerprint(&"ab:".repeat(32)),
            canonical,
            "colon-separated pastes should work"
        );
        assert_eq!(
            normalize_fingerprint(&format!("  SHA256:{}  ", "Ab".repeat(32))),
            canonical
        );
    }

    #[test]
    fn a_pinned_verifier_needs_a_fingerprint() {
        assert!(client_config("").is_err());
        assert!(client_config(&format!("sha256:{}", "aa".repeat(32))).is_ok());
    }
}
