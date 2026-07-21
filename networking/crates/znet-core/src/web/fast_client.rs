//! Native fast-lane **client**, with transparent HTTP fallback.
//!
//! Given a Zap download link, this probes `GET /api/capabilities`; if a fast
//! lane is advertised and reachable it downloads over the custom TCP protocol
//! (see [`super::fast`] and `docs/fast-lane-protocol.md`), otherwise - or on any
//! fast-lane failure - it finishes over the existing HTTP path, resuming by
//! offset from whatever bytes are already on disk. The HTTP path is always the
//! safety net, so a fast-lane problem never fails a transfer HTTP could complete.
//!
//! v1 speaks **plain HTTP only** on the client side; HTTPS/pinned-cert support
//! arrives together with fast-lane TLS in a later phase. The server advertises
//! the fast lane only when it is on plain HTTP (see `web::capabilities_json`), so
//! this stays consistent.

use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};

use super::{crc32_file, fast};

/// How many times to (re)try the fast lane, re-handshaking from the current
/// on-disk offset each time, before giving up and falling back to HTTP.
const FAST_ATTEMPTS: u32 = 4;
/// Streaming buffer size.
const CHUNK: usize = 128 * 1024;

/// The outcome of a completed [`get`].
#[derive(Debug, Clone)]
pub struct Report {
    /// Final path the file was written to.
    pub path: PathBuf,
    /// Whole-file size in bytes.
    pub total: u64,
    /// True if the fast lane carried the transfer; false if it fell back to HTTP.
    pub used_fast: bool,
    /// True if a whole-file CRC-32 was checked and matched (fast lane only).
    pub verified: bool,
    /// Bytes already on disk when the download started (0 for a fresh download,
    /// non-zero when it resumed a partial `.zap-part-` file).
    pub resumed_from: u64,
}

/// A Zap download link, broken into its parts.
struct Target {
    host: String,
    port: u16,
    /// The file path relative to the share root, still percent-encoded (reused
    /// verbatim when rebuilding an HTTP `/download?path=` request).
    raw_path: String,
    /// The same path, percent-decoded (what the fast handshake and `resolve_within`
    /// expect, and where the filename is derived from).
    file_path: String,
    /// The pairing/session token (`?k=`), if the link carried one.
    token: Option<String>,
    /// The leaf filename to save as.
    filename: String,
}

/// Download the file named by a Zap link into `dest` (a file path or a
/// directory), using the fast lane when available and falling back to HTTP.
pub fn get(url: &str, dest: &Path) -> Result<Report> {
    let target = parse_target(url)?;
    let (folder, final_path) = resolve_dest(dest, &target.filename);
    fs::create_dir_all(&folder)
        .with_context(|| format!("creating destination folder {}", folder.display()))?;
    let dest_name = final_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(&target.filename)
        .to_string();
    // Reuse the exact same temp-file naming as the HTTP resumable path so the two
    // transports share one checkpoint and could even resume each other.
    let part = super::part_path(&folder, &dest_name);

    // Discover the fast lane over HTTP. A failure here just means "no fast lane".
    let fast_port = probe_fast_port(&target).unwrap_or(None);

    if let Some(fp) = fast_port {
        match fast_download(&target, fp, &part) {
            Ok((total, verified, resumed)) => {
                finalize(&part, &final_path)?;
                return Ok(Report {
                    path: final_path,
                    total,
                    used_fast: true,
                    verified,
                    resumed_from: resumed,
                });
            }
            Err(e) => {
                eprintln!("zap: fast lane failed ({e:#}); falling back to HTTP");
            }
        }
    }

    // HTTP fallback - resumes from whatever the fast lane already wrote.
    let (total, resumed) = http_download(&target, &part)?;
    finalize(&part, &final_path)?;
    Ok(Report {
        path: final_path,
        total,
        used_fast: false,
        verified: false,
        resumed_from: resumed,
    })
}

/// Atomically move the completed temp file into place. A partial file is never
/// exposed under the final name - the caller only reaches here after the size
/// (and, on the fast lane, the CRC) has been verified.
fn finalize(part: &Path, final_path: &Path) -> Result<()> {
    fs::rename(part, final_path)
        .with_context(|| format!("finalizing {}", final_path.display()))
}

// ---- Fast lane ----

/// Drive a fast-lane download, retrying (re-handshaking from the current on-disk
/// offset) on transient failure. Verifies whole-file size and CRC-32 before
/// returning; on a CRC mismatch it discards the temp file and retries clean.
fn fast_download(t: &Target, fast_port: u16, part: &Path) -> Result<(u64, bool, u64)> {
    let resumed_from = fs::metadata(part).map(|m| m.len()).unwrap_or(0);
    let mut last_err = None;

    for attempt in 1..=FAST_ATTEMPTS {
        let on_disk = fs::metadata(part).map(|m| m.len()).unwrap_or(0);
        match fast_download_once(t, fast_port, part, on_disk) {
            Ok((total, crc_opt)) => {
                let got = fs::metadata(part).map(|m| m.len()).unwrap_or(0);
                if got != total {
                    // Short read (connection dropped): resume on the next attempt.
                    last_err = Some(anyhow!("incomplete: {got}/{total} bytes"));
                    if attempt < FAST_ATTEMPTS {
                        thread::sleep(Duration::from_millis(200));
                        continue;
                    }
                    break;
                }
                let verified = match crc_opt {
                    Some(want) => {
                        let got_crc = crc32_file(part).map_err(|e| anyhow!("reading for CRC: {e}"))?;
                        if got_crc != want {
                            // Corrupt: discard so the retry starts from a clean slate.
                            let _ = fs::remove_file(part);
                            last_err = Some(anyhow!("integrity check failed (CRC mismatch)"));
                            if attempt < FAST_ATTEMPTS {
                                continue;
                            }
                            break;
                        }
                        true
                    }
                    None => false,
                };
                return Ok((total, verified, resumed_from));
            }
            Err(e) => {
                last_err = Some(e);
                if attempt < FAST_ATTEMPTS {
                    thread::sleep(Duration::from_millis(200));
                    continue;
                }
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow!("fast lane failed")))
}

/// One fast-lane handshake + range download starting at `offset`, writing into
/// `part`. Returns the whole-file size and optional CRC from the server reply.
fn fast_download_once(t: &Target, fast_port: u16, part: &Path, offset: u64) -> Result<(u64, Option<u32>)> {
    let mut stream = TcpStream::connect((t.host.as_str(), fast_port))
        .with_context(|| format!("connecting to fast lane {}:{fast_port}", t.host))?;
    stream.set_nodelay(true).ok();
    stream.set_read_timeout(Some(Duration::from_secs(60))).ok();

    // ---- Handshake ----
    let token = t.token.clone().unwrap_or_default();
    let path_bytes = t.file_path.as_bytes();
    let mut hs = Vec::with_capacity(27 + token.len() + path_bytes.len());
    hs.extend_from_slice(fast::MAGIC);
    hs.extend_from_slice(&fast::VERSION.to_le_bytes());
    hs.push(fast::OP_GET);
    hs.extend_from_slice(&(token.len() as u16).to_le_bytes());
    hs.extend_from_slice(token.as_bytes());
    hs.extend_from_slice(&(path_bytes.len() as u32).to_le_bytes());
    hs.extend_from_slice(path_bytes);
    hs.extend_from_slice(&offset.to_le_bytes());
    hs.extend_from_slice(&0u64.to_le_bytes()); // range_len 0 = to EOF
    stream.write_all(&hs)?;
    stream.flush()?;

    // ---- Reply ----
    let mut status = [0u8; 1];
    stream.read_exact(&mut status)?;
    if status[0] != fast::ST_OK {
        let mut lenb = [0u8; 2];
        stream.read_exact(&mut lenb)?;
        let mlen = u16::from_le_bytes(lenb) as usize;
        let mut msg = vec![0u8; mlen];
        stream.read_exact(&mut msg).ok();
        bail!("server said: {}", String::from_utf8_lossy(&msg));
    }
    let total = {
        let mut b = [0u8; 8];
        stream.read_exact(&mut b)?;
        u64::from_le_bytes(b)
    };
    let has_crc = {
        let mut b = [0u8; 1];
        stream.read_exact(&mut b)?;
        b[0] == 1
    };
    let crc = if has_crc {
        let mut b = [0u8; 4];
        stream.read_exact(&mut b)?;
        Some(u32::from_le_bytes(b))
    } else {
        None
    };

    // ---- Body: write [offset, total) at the right position ----
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .open(part)
        .with_context(|| format!("opening {}", part.display()))?;
    file.seek(SeekFrom::Start(offset))?;
    // Drop anything past our resume point so a re-handshake never leaves stale
    // trailing bytes from a prior attempt.
    file.set_len(offset)?;

    let mut remaining = total.saturating_sub(offset);
    let mut buf = [0u8; CHUNK];
    while remaining > 0 {
        let want = remaining.min(buf.len() as u64) as usize;
        let n = stream.read(&mut buf[..want])?;
        if n == 0 {
            break; // connection dropped; caller verifies size and resumes/falls back
        }
        file.write_all(&buf[..n])?;
        remaining -= n as u64;
    }
    file.flush()?;
    Ok((total, crc))
}

// ---- HTTP fallback ----

/// Finish (or perform) the download over the HTTP path, resuming from any bytes
/// already on disk via a `Range` request. Returns the whole-file size and the
/// offset it resumed from.
fn http_download(t: &Target, part: &Path) -> Result<(u64, u64)> {
    let on_disk = fs::metadata(part).map(|m| m.len()).unwrap_or(0);
    let mut headers = auth_headers(t);
    if on_disk > 0 {
        headers.push(("Range".to_string(), format!("bytes={on_disk}-")));
    }
    let target = format!("/download?path={}", t.raw_path);
    let mut resp = http_request(&t.host, t.port, "GET", &target, &headers)?;
    if resp.status != 200 && resp.status != 206 {
        bail!("HTTP download failed: status {}", resp.status);
    }
    let body_len = resp
        .content_length
        .ok_or_else(|| anyhow!("HTTP response had no Content-Length"))?;

    // 206 => the server honored our Range and resumed; 200 => it sent the whole
    // file (Range ignored or none requested), so restart from zero.
    let start = if resp.status == 206 { on_disk } else { 0 };
    let total = if resp.status == 206 { start + body_len } else { body_len };

    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .open(part)
        .with_context(|| format!("opening {}", part.display()))?;
    file.seek(SeekFrom::Start(start))?;
    file.set_len(start)?;

    let mut remaining = body_len;
    let mut buf = [0u8; CHUNK];
    while remaining > 0 {
        let want = remaining.min(buf.len() as u64) as usize;
        let n = resp.reader.read(&mut buf[..want])?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])?;
        remaining -= n as u64;
    }
    file.flush()?;

    let got = fs::metadata(part).map(|m| m.len()).unwrap_or(0);
    if got != total {
        bail!("HTTP download incomplete: {got}/{total} bytes");
    }
    Ok((total, start))
}

/// Probe `GET /api/capabilities` and extract the advertised fast-lane port, if
/// any. Returns `Ok(None)` when there is no fast lane (older server, HTTPS, or
/// the field is `null`).
fn probe_fast_port(t: &Target) -> Result<Option<u16>> {
    let headers = auth_headers(t);
    let mut resp = http_request(&t.host, t.port, "GET", "/api/capabilities", &headers)?;
    if resp.status != 200 {
        return Ok(None);
    }
    let mut body = String::new();
    resp.reader.read_to_string(&mut body).ok();
    Ok(parse_fast_port(&body))
}

/// Extract `fast.port` from the capabilities JSON. Deliberately tiny and
/// dependency-free (the crate hand-writes JSON everywhere else too).
fn parse_fast_port(json: &str) -> Option<u16> {
    let compact: String = json.chars().filter(|c| !c.is_whitespace()).collect();
    if compact.contains("\"fast\":null") {
        return None;
    }
    let idx = compact.find("\"port\":")?;
    let after = &compact[idx + "\"port\":".len()..];
    let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// The auth header a native client sends: the pairing token as the session
/// cookie the HTTP server already understands (`has_valid_session`).
fn auth_headers(t: &Target) -> Vec<(String, String)> {
    match &t.token {
        Some(tok) => vec![("Cookie".to_string(), format!("zap_session={tok}"))],
        None => Vec::new(),
    }
}

/// A minimal HTTP/1.1 response: status, headers, a reader positioned at the body
/// start, and the parsed `Content-Length`.
struct HttpResponse {
    status: u16,
    #[allow(dead_code)]
    headers: Vec<(String, String)>,
    reader: BufReader<TcpStream>,
    content_length: Option<u64>,
}

/// Perform a minimal plain-HTTP/1.1 request (`Connection: close`, one request per
/// connection) and return the response with its body reader ready.
fn http_request(
    host: &str,
    port: u16,
    method: &str,
    target: &str,
    extra: &[(String, String)],
) -> Result<HttpResponse> {
    let stream = TcpStream::connect((host, port))
        .with_context(|| format!("connecting to {host}:{port}"))?;
    stream.set_read_timeout(Some(Duration::from_secs(60))).ok();

    let mut req = format!("{method} {target} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n");
    for (k, v) in extra {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    {
        let mut w = &stream;
        w.write_all(req.as_bytes())?;
        w.flush()?;
    }

    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    reader.read_line(&mut status_line)?;
    let status = parse_status(&status_line)?;

    let mut headers = Vec::new();
    let mut content_length = None;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            break;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            let k = k.trim().to_string();
            let v = v.trim().to_string();
            if k.eq_ignore_ascii_case("content-length") {
                content_length = v.parse().ok();
            }
            headers.push((k, v));
        }
    }
    Ok(HttpResponse {
        status,
        headers,
        reader,
        content_length,
    })
}

fn parse_status(line: &str) -> Result<u16> {
    line.split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| anyhow!("malformed HTTP status line: {line:?}"))
}

// ---- URL + destination parsing ----

/// Parse a Zap download link. Must be `http://host[:port]/...?path=<file>` with
/// an optional `&k=<token>`. HTTPS is rejected in v1 (plain HTTP only).
fn parse_target(url: &str) -> Result<Target> {
    let rest = url.strip_prefix("http://").ok_or_else(|| {
        if url.starts_with("https://") {
            anyhow!("`zap get` speaks plain HTTP only for now; open this https link in a browser instead")
        } else {
            anyhow!("expected an http:// URL, got: {url}")
        }
    })?;

    let (authority, pathq) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = split_authority(authority);

    let (_, query) = super::split_query(pathq);
    let raw_path = super::query_param(query, "path")
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow!("the URL must include ?path=<file> (a Zap download link)"))?;
    let file_path = super::decode_percent(&raw_path);
    let token = super::query_param(query, "k").map(super::decode_percent);

    let filename = file_path
        .rsplit('/')
        .find(|s| !s.is_empty())
        .unwrap_or("download")
        .to_string();
    if filename.is_empty() {
        bail!("could not determine a filename from the URL");
    }

    Ok(Target {
        host,
        port,
        raw_path,
        file_path,
        token,
        filename,
    })
}

/// Split `host`, `host:port`, or `[ipv6]:port` into (host, port), defaulting to
/// port 80.
fn split_authority(authority: &str) -> (String, u16) {
    if let Some(after) = authority.strip_prefix('[') {
        // [ipv6]:port
        let (h, tail) = after.split_once(']').unwrap_or((after, ""));
        let port = tail.trim_start_matches(':').parse().unwrap_or(80);
        (h.to_string(), port)
    } else if let Some((h, p)) = authority.rsplit_once(':') {
        (h.to_string(), p.parse().unwrap_or(80))
    } else {
        (authority.to_string(), 80)
    }
}

/// Resolve the destination folder and final path. If `dest` is an existing
/// directory (or empty), the file lands inside it under its own name; otherwise
/// `dest` is treated as the target file path.
fn resolve_dest(dest: &Path, filename: &str) -> (PathBuf, PathBuf) {
    if dest.as_os_str().is_empty() || dest.is_dir() {
        let folder = if dest.as_os_str().is_empty() {
            PathBuf::from(".")
        } else {
            dest.to_path_buf()
        };
        let final_path = folder.join(filename);
        (folder, final_path)
    } else {
        let folder = match dest.parent() {
            Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
            _ => PathBuf::from("."),
        };
        (folder, dest.to_path_buf())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::net::{IpAddr, Ipv4Addr, TcpListener};
    use std::sync::Mutex;

    use crate::web::{spawn, ServeConfig};

    /// Serialize the tests that stand up a real server (they pick an ephemeral
    /// port and rebind it), so two do not race on the same freed port.
    static GUARD: Mutex<()> = Mutex::new(());

    fn free_port() -> u16 {
        TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    #[test]
    fn parse_target_reads_all_parts() {
        let t = parse_target("http://192.168.1.5:8080/download?path=sub%2Fmovie.mp4&k=abc123")
            .expect("parse");
        assert_eq!(t.host, "192.168.1.5");
        assert_eq!(t.port, 8080);
        assert_eq!(t.raw_path, "sub%2Fmovie.mp4", "raw path kept encoded for HTTP reuse");
        assert_eq!(t.file_path, "sub/movie.mp4", "decoded path for the handshake");
        assert_eq!(t.token.as_deref(), Some("abc123"));
        assert_eq!(t.filename, "movie.mp4");
    }

    #[test]
    fn parse_target_defaults_port_and_rejects_bad_input() {
        let t = parse_target("http://host/download?path=x.txt").expect("default port");
        assert_eq!(t.port, 80);
        assert!(t.token.is_none());
        assert!(parse_target("https://host/download?path=x.txt").is_err(), "https rejected in v1");
        assert!(parse_target("http://host/").is_err(), "missing ?path rejected");
    }

    #[test]
    fn parse_fast_port_handles_null_and_value() {
        assert_eq!(parse_fast_port("{\"fast\":null}"), None);
        assert_eq!(
            parse_fast_port("{\"fast\":{\"port\":50370,\"tls\":false,\"version\":1}}"),
            Some(50370)
        );
        // Whitespace-tolerant.
        assert_eq!(parse_fast_port("{ \"fast\": { \"port\": 41000 } }"), Some(41000));
    }

    /// The HTTP fallback transport must download byte-exact and resume from a
    /// partial temp file via a Range request - the safety net when the fast lane
    /// is unavailable.
    #[test]
    fn http_fallback_downloads_and_resumes() {
        let _g = GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let src = std::env::temp_dir().join(format!("zap-fbk-src-{}", std::process::id()));
        let dst = std::env::temp_dir().join(format!("zap-fbk-dst-{}", std::process::id()));
        let _ = fs::remove_dir_all(&src);
        let _ = fs::remove_dir_all(&dst);
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dst).unwrap();
        let data: Vec<u8> = (0..250_000u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8 ^ (i as u8))
            .collect();
        fs::write(src.join("f.bin"), &data).unwrap();

        let port = free_port();
        let (_info, handle) = spawn(ServeConfig {
            dir: src.clone(),
            port,
            bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
            auth: None,
            history: None,
            index_html: None,
            tls: None,
        })
        .expect("bind");

        let target = Target {
            host: "127.0.0.1".to_string(),
            port,
            raw_path: "f.bin".to_string(),
            file_path: "f.bin".to_string(),
            token: None,
            filename: "f.bin".to_string(),
        };
        let part = dst.join(".zap-part-f.bin");

        // Fresh download over HTTP.
        let (total, resumed) = http_download(&target, &part).expect("http download");
        assert_eq!(total, data.len() as u64);
        assert_eq!(resumed, 0);
        assert_eq!(fs::read(&part).unwrap(), data, "HTTP download byte-exact");

        // Pre-seed a partial and confirm it resumes via Range (206).
        fs::write(&part, &data[..80_000]).unwrap();
        let (total2, resumed2) = http_download(&target, &part).expect("http resume");
        assert_eq!(total2, data.len() as u64);
        assert_eq!(resumed2, 80_000, "should resume from the on-disk offset");
        assert_eq!(fs::read(&part).unwrap(), data, "HTTP resume byte-exact");

        handle.stop();
        let _ = fs::remove_dir_all(&src);
        let _ = fs::remove_dir_all(&dst);
    }
}
