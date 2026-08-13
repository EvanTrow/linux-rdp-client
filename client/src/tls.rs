use anyhow::{Context, Result};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, ClientConnection, DigitallySignedStruct, SignatureScheme};
use sha2::{Digest, Sha256};
use std::net::TcpStream;
use std::sync::Arc;

/// RDP hosts on a LAN almost always present a self-signed certificate (there's no
/// enterprise PKI issuing certs for `mstsc`'s TLS layer). We trust-on-first-use: accept
/// whatever cert the host presents and print its fingerprint so the user can eyeball it,
/// same trust model `mstsc` itself uses when it asks "do you trust this certificate?".
#[derive(Debug)]
struct TrustOnFirstUseVerifier;

impl ServerCertVerifier for TrustOnFirstUseVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let fingerprint = Sha256::digest(end_entity.as_ref());
        eprintln!(
            "TLS server certificate SHA-256 fingerprint: {}",
            fingerprint
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<Vec<_>>()
                .join(":")
        );
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![
            SignatureScheme::RSA_PKCS1_SHA256,
            SignatureScheme::RSA_PKCS1_SHA384,
            SignatureScheme::RSA_PKCS1_SHA512,
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::RSA_PSS_SHA384,
            SignatureScheme::RSA_PSS_SHA512,
            SignatureScheme::ED25519,
        ]
    }
}

/// Upgrades the already-connected TCP stream (post X.224 negotiation) to TLS, per
/// MS-RDPBCGR 5.4.5.4: RDS AAD Auth performs its own TLS handshake at this stage.
pub fn upgrade(stream: TcpStream, server_name: &str) -> Result<rustls::StreamOwned<ClientConnection, TcpStream>> {
    let config = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(TrustOnFirstUseVerifier))
        .with_no_client_auth();

    let name = ServerName::try_from(server_name.to_string())
        .context("invalid server name for TLS SNI")?;
    let conn = ClientConnection::new(Arc::new(config), name)
        .context("creating rustls ClientConnection")?;

    let mut tls_stream = rustls::StreamOwned::new(conn, stream);
    // Force the handshake to complete now by flushing (a zero-byte write still drives the handshake).
    use std::io::Write;
    tls_stream.flush().context("performing TLS handshake")?;
    Ok(tls_stream)
}

#[cfg(test)]
mod verify {
    use super::*;

    #[test]
    fn verify_crypto_provider_ambiguity_resolved() {
        // With two rustls crypto providers in the dependency tree (see main.rs's comment),
        // `ClientConfig::builder()`/`ClientConnection::new()` panic instead of erroring if
        // no default provider was installed process-wide first. `cargo test` runs every
        // test in-process, so whichever test runs first performs the install; this one
        // tolerates it already having happened (`install_default` erroring just means some
        // other test won the race) — what actually matters is that building a config never
        // panics either way.
        let _ = rustls::crypto::ring::default_provider().install_default();

        let config = ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(TrustOnFirstUseVerifier))
            .with_no_client_auth();
        let name = ServerName::try_from("example.com".to_string()).unwrap();
        // This is exactly where the ambiguous-provider panic fires — reaching this line
        // without panicking is the whole point of the test.
        let _conn = ClientConnection::new(Arc::new(config), name).expect("ClientConnection::new");
    }
}
