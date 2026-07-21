//! rustls glue for the fast lane (feature `tls`).
//!
//! The server side builds a [`rustls::ServerConfig`] from the same self-signed
//! [`TlsMaterial`](super::TlsMaterial) the HTTPS path uses. The client side pins
//! the server's certificate by its SHA-256 fingerprint (the `&fp=` value from
//! the pairing link) instead of trusting a CA - there is no domain and no
//! internet on the LAN, so fingerprint pinning is the trust primitive.

use std::sync::Arc;

use anyhow::{anyhow, Context, Result};

use super::{tls::fingerprint_hex, TlsMaterial};

/// Build the fast-lane rustls server config from the self-signed cert + key PEM.
pub(crate) fn server_config(material: &TlsMaterial) -> Result<Arc<rustls::ServerConfig>> {
    let certs = rustls_pemfile::certs(&mut &material.cert_pem[..])
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("parsing fast-lane certificate PEM")?;
    if certs.is_empty() {
        return Err(anyhow!("no certificate found in TLS material"));
    }
    let key = rustls_pemfile::private_key(&mut &material.key_pem[..])
        .context("parsing fast-lane private key PEM")?
        .ok_or_else(|| anyhow!("no private key found in TLS material"))?;

    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .context("selecting TLS protocol versions")?
    .with_no_client_auth()
    .with_single_cert(certs, key)
    .context("building fast-lane rustls server config")?;
    Ok(Arc::new(config))
}

/// Build a client config that trusts the server solely by its cert fingerprint.
pub(crate) fn client_config(pin: &str) -> Arc<rustls::ClientConfig> {
    let verifier = Arc::new(PinnedCert { pin: pin.to_string() });
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .expect("ring provider supports the default protocol versions")
    .dangerous()
    .with_custom_certificate_verifier(verifier)
    .with_no_client_auth();
    Arc::new(config)
}

/// A rustls verifier that trusts a certificate solely by its SHA-256 fingerprint
/// - the same value the client learned from the QR/pairing link (`&fp=`). This is
/// deliberate certificate pinning, not a CA bypass: there is no CA on the LAN.
#[derive(Debug)]
struct PinnedCert {
    pin: String,
}

impl rustls::client::danger::ServerCertVerifier for PinnedCert {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        if fingerprint_hex(end_entity) == self.pin {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General("fingerprint mismatch".into()))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
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
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}
