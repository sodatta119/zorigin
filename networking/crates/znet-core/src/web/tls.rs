//! Self-signed TLS material for optional native <-> native encryption
//! (feature `tls`). We don't use a CA - there's no domain and no internet - so
//! a peer trusts the host by pinning its certificate's SHA-256 **fingerprint**,
//! which travels in the pairing link (`&fp=<hex>`) next to the session key.
//!
//! This is the "trust" primitive from the roadmap (H1.5): encrypt the LAN hop
//! for sensitive clips on hostile Wi-Fi, verified by a fingerprint you can also
//! read off the two screens. Plain HTTP stays the default (the no-app browser
//! receiver can't trust a self-signed cert without a scary warning).

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

use super::TlsMaterial;

/// Generate a fresh self-signed certificate covering `sans` (the host's LAN IP
/// and `localhost`), returning the PEM cert + key and the cert's fingerprint.
pub fn self_signed(sans: Vec<String>) -> Result<TlsMaterial> {
    let key = rcgen::generate_simple_self_signed(sans).context("generating self-signed cert")?;
    let fingerprint = fingerprint_hex(key.cert.der());
    Ok(TlsMaterial {
        cert_pem: key.cert.pem().into_bytes(),
        key_pem: key.key_pair.serialize_pem().into_bytes(),
        fingerprint,
    })
}

/// Lowercase hex SHA-256 of a DER certificate - the value a client pins.
pub fn fingerprint_hex(der: &[u8]) -> String {
    let digest = Sha256::digest(der);
    let mut out = String::with_capacity(64);
    for b in digest {
        use std::fmt::Write as _;
        let _ = write!(out, "{b:02x}");
    }
    out
}
