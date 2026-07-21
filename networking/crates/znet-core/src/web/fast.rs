//! Fast lane: the optional native-to-native transport over a custom protocol on
//! TCP. It is **additive** - the HTTP(S) browser path stays the default and is
//! never removed or gated. A browser cannot speak this protocol, so the fast
//! lane only runs app-to-app; native clients discover it via
//! `GET /api/capabilities` and always fall back to HTTP when it is unavailable.
//!
//! This module is the **server** side (listener + protocol). The **client** side
//! (plus HTTP fallback) lives in [`super::fast_client`]. The wire format is
//! specified in `docs/fast-lane-protocol.md`.
//!
//! It is a submodule of [`web`](super) on purpose: it reuses `web`'s existing
//! primitives directly (`crc32_file`, `resolve_within`, the `Stats` /
//! `begin_download` transfer bookkeeping) instead of reinventing them, matching
//! their semantics so the Transfers UI, resume, and integrity behave identically.

use std::collections::HashMap;
use std::fs::{File, Metadata, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, UNIX_EPOCH};

use super::{crc32_file, resolve_within, Auth, Stats};

/// Wire-format magic every handshake starts with.
pub(crate) const MAGIC: &[u8; 4] = b"ZAPX";
/// Protocol version this build speaks.
pub(crate) const VERSION: u16 = 1;
/// Handshake op: download a byte range.
pub(crate) const OP_GET: u8 = 1;
/// Handshake op: upload a file (resumable, CRC-verified).
pub(crate) const OP_PUT: u8 = 2;

// Reply status codes (see docs/fast-lane-protocol.md §4).
pub(crate) const ST_OK: u8 = 0;
pub(crate) const ST_BAD_REQUEST: u8 = 1;
pub(crate) const ST_UNAUTHORIZED: u8 = 2;
pub(crate) const ST_NOT_FOUND: u8 = 3;
pub(crate) const ST_UNSUPPORTED: u8 = 4;
pub(crate) const ST_SERVER_ERROR: u8 = 5;
pub(crate) const ST_INTEGRITY: u8 = 6;

/// Cap on the handshake `path` length, to bound allocation from a hostile or
/// buggy client (paths are short; this is generous headroom).
const MAX_PATH_LEN: usize = 64 * 1024;
/// Streaming buffer size for serving a range.
const CHUNK: usize = 128 * 1024;

/// A running fast-lane listener. Dropping it (or calling [`stop`](Self::stop))
/// shuts the acceptor down and releases the port, mirroring how the HTTP
/// [`ServerHandle`](super::ServerHandle) manages its own listener.
pub(crate) struct FastHandle {
    /// The OS-assigned port the listener bound to (advertised in capabilities).
    pub(crate) port: u16,
    stop: Arc<AtomicBool>,
    local_addr: SocketAddr,
    acceptor: Option<thread::JoinHandle<()>>,
}

impl FastHandle {
    /// Stop the acceptor and wait for it to exit, freeing the port.
    fn shutdown(&mut self) {
        if self.stop.swap(true, Ordering::SeqCst) {
            return; // already stopped
        }
        // `accept()` is blocking; wake it once by connecting to ourselves. The
        // loop then sees the stop flag and exits.
        let _ = TcpStream::connect(wake_addr(self.local_addr));
        if let Some(j) = self.acceptor.take() {
            let _ = j.join();
        }
    }

    #[allow(dead_code)]
    pub(crate) fn stop(mut self) {
        self.shutdown();
    }
}

impl Drop for FastHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// The address to connect to in order to wake a blocked `accept()`. A listener
/// bound to the unspecified address is woken via loopback.
fn wake_addr(listen: SocketAddr) -> SocketAddr {
    let ip = match listen.ip() {
        IpAddr::V4(v4) if v4.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(v6) if v6.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        other => other,
    };
    SocketAddr::new(ip, listen.port())
}

/// TLS material for the fast-lane listener: a rustls server config under the
/// `tls` feature, and an uninhabited-in-practice `()` otherwise (the fast lane is
/// only ever asked to run TLS when the feature is on, since the HTTP server that
/// carries the same cert cannot bind HTTPS without it either).
#[cfg(feature = "tls")]
pub(crate) type FastServerTls = std::sync::Arc<rustls::ServerConfig>;
#[cfg(not(feature = "tls"))]
pub(crate) type FastServerTls = ();

/// Bind the fast-lane listener on `bind:0` (OS-assigned port) and start its
/// acceptor thread. Each connection is handled on its own thread, exactly like
/// the HTTP accept loop. When `tls` is set, connections are wrapped in the shared
/// rustls server session. Returns a handle carrying the chosen port.
pub(crate) fn spawn_listener(
    bind: IpAddr,
    dir: Arc<PathBuf>,
    auth: Arc<Option<Auth>>,
    stats: Arc<Stats>,
    tls: Option<FastServerTls>,
) -> io::Result<FastHandle> {
    let listener = TcpListener::bind(SocketAddr::new(bind, 0))?;
    let local_addr = listener.local_addr()?;
    let port = local_addr.port();
    let stop = Arc::new(AtomicBool::new(false));

    let acceptor = {
        let stop = Arc::clone(&stop);
        thread::spawn(move || {
            for stream in listener.incoming() {
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                match stream {
                    Ok(s) => {
                        // Socket options are set here (before any TLS wrap) so the
                        // connection handler can stay generic over the stream type.
                        s.set_nodelay(true).ok();
                        s.set_read_timeout(Some(Duration::from_secs(60))).ok();
                        let dir = Arc::clone(&dir);
                        let auth = Arc::clone(&auth);
                        let stats = Arc::clone(&stats);
                        let tls = tls.clone();
                        thread::spawn(move || {
                            if let Err(e) = serve_connection(s, tls.as_ref(), &dir, (*auth).as_ref(), &stats) {
                                eprintln!("zap: fast-lane connection error: {e:#}");
                            }
                        });
                    }
                    Err(_) => {
                        if stop.load(Ordering::SeqCst) {
                            break;
                        }
                    }
                }
            }
        })
    };

    Ok(FastHandle {
        port,
        stop,
        local_addr,
        acceptor: Some(acceptor),
    })
}

/// Take one accepted connection, optionally wrap it in the shared rustls server
/// session, and hand it to the (stream-generic) protocol handler. Keeping the
/// TLS wrap here lets [`handle_conn`] stay generic over the stream type.
fn serve_connection(
    s: TcpStream,
    tls: Option<&FastServerTls>,
    dir: &Path,
    auth: Option<&Auth>,
    stats: &Stats,
) -> io::Result<()> {
    #[cfg(feature = "tls")]
    {
        if let Some(cfg) = tls {
            let conn = rustls::ServerConnection::new(std::sync::Arc::clone(cfg))
                .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("tls setup: {e}")))?;
            let stream = rustls::StreamOwned::new(conn, s);
            return handle_conn(stream, dir, auth, stats);
        }
    }
    #[cfg(not(feature = "tls"))]
    let _ = tls;
    handle_conn(s, dir, auth, stats)
}

/// Read the handshake, authenticate, and dispatch a GET (download) or PUT
/// (upload). Generic over the stream so it serves both plain TCP and TLS.
fn handle_conn<S: Read + Write>(
    mut stream: S,
    dir: &Path,
    auth: Option<&Auth>,
    stats: &Stats,
) -> io::Result<()> {
    // ---- Handshake (client -> server) ----
    let mut magic = [0u8; 4];
    stream.read_exact(&mut magic)?;
    if &magic != MAGIC {
        return reply_err(&mut stream, ST_BAD_REQUEST, "bad magic");
    }
    let version = read_u16(&mut stream)?;
    if version != VERSION {
        return reply_err(&mut stream, ST_BAD_REQUEST, "unsupported version");
    }
    let op = read_u8(&mut stream)?;
    let token_len = read_u16(&mut stream)? as usize;
    let token = read_string(&mut stream, token_len)?;
    let path_len = read_u32(&mut stream)? as usize;
    if path_len > MAX_PATH_LEN {
        return reply_err(&mut stream, ST_BAD_REQUEST, "path too long");
    }
    let path = read_string(&mut stream, path_len)?;

    // ---- Auth (same token as the HTTP `?k=` / zap_session cookie) ----
    if let Some(a) = auth {
        if token != a.token {
            return reply_err(&mut stream, ST_UNAUTHORIZED, "unauthorized");
        }
    }

    match op {
        OP_GET => {
            let offset = read_u64(&mut stream)?;
            let range_len = read_u64(&mut stream)?;
            handle_get(&mut stream, dir, &path, offset, range_len, stats)
        }
        OP_PUT => {
            let _client_offset = read_u64(&mut stream)?;
            let total = read_u64(&mut stream)?;
            let has_crc = read_u8(&mut stream)?;
            let expected_crc = if has_crc == 1 {
                Some(read_u32(&mut stream)?)
            } else {
                None
            };
            handle_put(&mut stream, dir, &path, total, expected_crc, stats)
        }
        _ => reply_err(&mut stream, ST_UNSUPPORTED, "unsupported op"),
    }
}

/// Serve a GET: reply with total_size + whole-file CRC, then stream the requested
/// byte range `[offset, offset+range_len)` (range_len 0 = to EOF). Integrity and
/// resume mirror the HTTP path; a zero-length request is a stat (header only).
fn handle_get<S: Read + Write>(
    stream: &mut S,
    dir: &Path,
    path: &str,
    offset: u64,
    range_len: u64,
    stats: &Stats,
) -> io::Result<()> {
    let Some(fpath) = resolve_within(dir, path) else {
        return reply_err(stream, ST_NOT_FOUND, "bad path");
    };
    let meta = match std::fs::metadata(&fpath) {
        Ok(m) if m.is_file() => m,
        _ => return reply_err(stream, ST_NOT_FOUND, "not found"),
    };
    let total = meta.len();
    let start = offset.min(total);
    let len = if range_len == 0 {
        total - start
    } else {
        range_len.min(total - start)
    };

    // Whole-file CRC-32 so the client can prove the assembled file is byte-exact.
    // Cached per (path, size, mtime) so the many handshakes of a multi-stream
    // download read the file once, not once per connection (the client's upfront
    // stat warms the cache). Best-effort: no CRC on read error.
    let crc = cached_crc(&fpath, &meta);

    // ---- Reply: status + total_size + optional CRC ----
    let mut head = Vec::with_capacity(14);
    head.push(ST_OK);
    head.extend_from_slice(&total.to_le_bytes());
    match crc {
        Some(c) => {
            head.push(1);
            head.extend_from_slice(&c.to_le_bytes());
        }
        None => head.push(0),
    }
    stream.write_all(&head)?;

    // Zero-length request = stat: header only, no data, no transfer row.
    if len == 0 {
        stream.flush()?;
        return Ok(());
    }

    // Coalesce into one transfer row keyed by path, like the HTTP download.
    let filename = fpath
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "download".to_string());
    let path_key = fpath.to_string_lossy().into_owned();
    let transfer = stats.begin_download(&path_key, &filename, Some(total), &path_key);
    transfer.done.fetch_max(start, Ordering::Relaxed);

    // ---- Data: raw bytes for [start, start+len) ----
    let mut file = File::open(&fpath)?;
    file.seek(SeekFrom::Start(start))?;
    let mut remaining = len;
    let mut buf = [0u8; CHUNK];
    let mut sent_abs = start;
    while remaining > 0 {
        let want = remaining.min(buf.len() as u64) as usize;
        let n = file.read(&mut buf[..want])?;
        if n == 0 {
            break;
        }
        stream.write_all(&buf[..n])?;
        remaining -= n as u64;
        sent_abs += n as u64;
        stats.bytes.fetch_add(n as u64, Ordering::Relaxed);
        transfer.done.fetch_max(sent_abs, Ordering::Relaxed);
    }
    stream.flush()?;

    if transfer.done.load(Ordering::Relaxed) >= total {
        transfer.finished.store(true, Ordering::Relaxed);
        transfer.ok.store(true, Ordering::Relaxed);
        stats.save_history();
    }
    Ok(())
}

/// Serve a PUT: reply with the authoritative resume offset (bytes already on
/// disk), receive `[offset, total)` into the `.zap-part-<name>` temp file, verify
/// the whole-file CRC-32, atomically rename into place, and send a final status.
/// This mirrors the HTTP resumable upload exactly (same temp file, same
/// coalesced transfer row, same integrity + atomic-rename rules).
fn handle_put<S: Read + Write>(
    stream: &mut S,
    dir: &Path,
    path: &str,
    total: u64,
    expected_crc: Option<u32>,
    stats: &Stats,
) -> io::Result<()> {
    let Some(dest) = resolve_within(dir, path) else {
        return reply_err(stream, ST_NOT_FOUND, "bad path");
    };
    let Some(name) = dest.file_name().map(|n| n.to_string_lossy().into_owned()) else {
        return reply_err(stream, ST_BAD_REQUEST, "bad destination");
    };
    let folder = dest.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| dir.to_path_buf());
    if std::fs::create_dir_all(&folder).is_err() {
        return reply_err(stream, ST_SERVER_ERROR, "mkdir failed");
    }

    let part = folder.join(format!("{}{name}", super::PART_PREFIX));
    let mut cur = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
    if cur > total {
        // Stale/oversized partial: discard and restart clean.
        let _ = std::fs::remove_file(&part);
        cur = 0;
    }

    // ---- Reply: status + authoritative offset the client should send from ----
    let mut head = Vec::with_capacity(9);
    head.push(ST_OK);
    head.extend_from_slice(&cur.to_le_bytes());
    stream.write_all(&head)?;
    stream.flush()?;

    // Coalesce chunks/resumes into one upload row keyed by the temp path.
    let key = part.to_string_lossy().into_owned();
    let dest_str = dest.to_string_lossy().into_owned();
    let transfer = stats.begin_upload(&key, &name, Some(total), &dest_str);
    transfer.done.fetch_max(cur, Ordering::Relaxed);

    // ---- Receive [cur, total) and append to the temp file ----
    let mut file = OpenOptions::new().create(true).append(true).open(&part)?;
    let mut received = cur;
    let mut remaining = total.saturating_sub(cur);
    let mut buf = [0u8; CHUNK];
    let mut write_err = false;
    while remaining > 0 {
        let want = remaining.min(buf.len() as u64) as usize;
        let n = match stream.read(&mut buf[..want]) {
            Ok(0) => break, // client dropped
            Ok(n) => n,
            Err(_) => break,
        };
        if file.write_all(&buf[..n]).is_err() {
            write_err = true;
            break;
        }
        received += n as u64;
        remaining -= n as u64;
        stats.bytes.fetch_add(n as u64, Ordering::Relaxed);
        transfer.done.fetch_max(received, Ordering::Relaxed);
    }
    let _ = file.flush();

    let newsize = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(received);
    if write_err || newsize < total {
        // Incomplete: leave the temp file for the client to resume from. Best
        // effort to tell the client (the socket may already be broken).
        return put_final(stream, ST_SERVER_ERROR);
    }

    transfer.finished.store(true, Ordering::Relaxed);
    let verified = match expected_crc {
        Some(want) => matches!(crc32_file(&part), Ok(got) if got == want),
        None => true,
    };
    if !verified {
        let _ = std::fs::remove_file(&part); // corrupt: discard so a retry is clean
        stats.save_history();
        return put_final(stream, ST_INTEGRITY);
    }
    if std::fs::rename(&part, &dest).is_err() {
        return put_final(stream, ST_SERVER_ERROR);
    }
    transfer.ok.store(true, Ordering::Relaxed);
    if expected_crc.is_some() {
        transfer.verified.store(true, Ordering::Relaxed);
    }
    stats.save_history();
    put_final(stream, ST_OK)
}

/// Send the single-byte final status of a PUT (0 = ok/verified, non-zero = fail).
fn put_final<S: Write>(stream: &mut S, status: u8) -> io::Result<()> {
    stream.write_all(&[status])?;
    let _ = stream.flush();
    Ok(())
}

/// Write an error reply (`status`, then a length-prefixed UTF-8 message) and
/// return `Ok` - the connection then closes. The client maps this to a fast-lane
/// failure and falls back to HTTP.
fn reply_err<S: Write>(stream: &mut S, status: u8, msg: &str) -> io::Result<()> {
    let bytes = msg.as_bytes();
    let len = bytes.len().min(u16::MAX as usize);
    let mut out = Vec::with_capacity(3 + len);
    out.push(status);
    out.extend_from_slice(&(len as u16).to_le_bytes());
    out.extend_from_slice(&bytes[..len]);
    stream.write_all(&out)?;
    let _ = stream.flush();
    Ok(())
}

/// Whole-file CRC-32, cached per `(path, size, mtime)`. A multi-stream download
/// opens many connections to the same file; without a cache each handshake would
/// re-read the whole file to compute the CRC (N full reads). The first caller for
/// a given file computes it, the rest reuse the cached value. When the file
/// changes (size or mtime), the key changes and it is recomputed. Returns `None`
/// only if the read itself fails.
fn cached_crc(path: &Path, meta: &Metadata) -> Option<u32> {
    static CACHE: OnceLock<Mutex<HashMap<(PathBuf, u64, u128), Arc<OnceLock<u32>>>>> =
        OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let key = (path.to_path_buf(), meta.len(), mtime);
    let slot = {
        let mut map = cache.lock().ok()?;
        Arc::clone(map.entry(key).or_insert_with(|| Arc::new(OnceLock::new())))
    };
    if let Some(v) = slot.get() {
        return Some(*v);
    }
    // Compute outside the map lock. A rare concurrent first-compute may read
    // twice; the client's upfront stat means workers normally see a warm cache.
    let crc = crc32_file(path).ok()?;
    let _ = slot.set(crc);
    Some(crc)
}

// ---- Little-endian framing helpers (server side) ----

fn read_u8(r: &mut impl Read) -> io::Result<u8> {
    let mut b = [0u8; 1];
    r.read_exact(&mut b)?;
    Ok(b[0])
}

fn read_u16(r: &mut impl Read) -> io::Result<u16> {
    let mut b = [0u8; 2];
    r.read_exact(&mut b)?;
    Ok(u16::from_le_bytes(b))
}

fn read_u32(r: &mut impl Read) -> io::Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}

fn read_u64(r: &mut impl Read) -> io::Result<u64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}

fn read_string(r: &mut impl Read, len: usize) -> io::Result<String> {
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}
