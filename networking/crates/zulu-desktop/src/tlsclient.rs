//! The join side of encrypted sync: a TCP connection that is either plaintext
//! or TLS with **fingerprint pinning**. There's no CA (the host is self-signed),
//! so we trust exactly the cert whose SHA-256 matches the fingerprint carried in
//! the pairing link (`&fp=<hex>`), and nothing else.
//!
//! [`Conn`] hides the plain/TLS split behind `Read`/`Write` so `sync.rs` uses
//! one code path. The handshake is completed up front (blocking) so a wrong
//! fingerprint fails fast; the socket's read timeout is set afterward for the
//! poll-and-check-stop streaming loop.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::aws_lc_rs::default_provider;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, ClientConnection, DigitallySignedStruct, Error as TlsError, SignatureScheme, StreamOwned};
use sha2::{Digest, Sha256};

/// A connection to the host: plaintext, or TLS pinned to a fingerprint.
pub enum Conn {
    Plain(TcpStream),
    Tls(Box<StreamOwned<ClientConnection, TcpStream>>),
}

impl Read for Conn {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Conn::Plain(s) => s.read(buf),
            Conn::Tls(t) => t.read(buf),
        }
    }
}
impl Write for Conn {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Conn::Plain(s) => s.write(buf),
            Conn::Tls(t) => t.write(buf),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Conn::Plain(s) => s.flush(),
            Conn::Tls(t) => t.flush(),
        }
    }
}

/// Connect to `host:port`. With `fp = Some(fingerprint)` the connection is TLS,
/// trusting only the cert with that SHA-256; `read_timeout` applies to the
/// streaming phase after the handshake.
pub fn connect(host: &str, port: u16, fp: Option<&str>, read_timeout: Duration) -> io::Result<Conn> {
    let sock = TcpStream::connect((host, port))?;
    match fp {
        None => {
            sock.set_read_timeout(Some(read_timeout))?;
            Ok(Conn::Plain(sock))
        }
        Some(fp) => {
            let config = pinned_config(fp);
            // The verifier ignores the name, but rustls needs a syntactically
            // valid one; fall back to a dummy DNS name for odd hosts.
            let name: ServerName = ServerName::try_from(host.to_string())
                .unwrap_or_else(|_| ServerName::try_from("zulu.local".to_string()).unwrap());
            let mut conn = ClientConnection::new(Arc::new(config), name).map_err(io_err)?;
            let mut sock = sock;
            // Drive the handshake to completion now, so a bad fingerprint errors
            // here rather than mid-stream.
            while conn.is_handshaking() {
                conn.complete_io(&mut sock)?;
            }
            sock.set_read_timeout(Some(read_timeout))?;
            Ok(Conn::Tls(Box::new(StreamOwned::new(conn, sock))))
        }
    }
}

fn pinned_config(fp: &str) -> ClientConfig {
    ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(Pinned { pin: fp.to_lowercase() }))
        .with_no_client_auth()
}

fn io_err(e: TlsError) -> io::Error {
    io::Error::other(e)
}

/// Trusts a server cert solely by its SHA-256 fingerprint (self-signed pinning).
#[derive(Debug)]
struct Pinned {
    pin: String,
}

impl Pinned {
    fn fingerprint(der: &[u8]) -> String {
        let digest = Sha256::digest(der);
        let mut out = String::with_capacity(64);
        for b in digest {
            use std::fmt::Write as _;
            let _ = write!(out, "{b:02x}");
        }
        out
    }
}

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        if Pinned::fingerprint(end_entity) == self.pin {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(TlsError::General("certificate fingerprint mismatch".into()))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &default_provider().signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &default_provider().signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        default_provider().signature_verification_algorithms.supported_schemes()
    }
}
