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
use std::fs::{File, Metadata};
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
/// Handshake op: upload (reserved for a later phase; v1 rejects it).
#[allow(dead_code)]
pub(crate) const OP_PUT: u8 = 2;

// Reply status codes (see docs/fast-lane-protocol.md §4).
pub(crate) const ST_OK: u8 = 0;
pub(crate) const ST_BAD_REQUEST: u8 = 1;
pub(crate) const ST_UNAUTHORIZED: u8 = 2;
pub(crate) const ST_NOT_FOUND: u8 = 3;
pub(crate) const ST_UNSUPPORTED: u8 = 4;

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

/// Bind the fast-lane listener on `bind:0` (OS-assigned port) and start its
/// acceptor thread. Each connection is handled on its own thread, exactly like
/// the HTTP accept loop. Returns a handle carrying the chosen port.
pub(crate) fn spawn_listener(
    bind: IpAddr,
    dir: Arc<PathBuf>,
    auth: Arc<Option<Auth>>,
    stats: Arc<Stats>,
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
                        let dir = Arc::clone(&dir);
                        let auth = Arc::clone(&auth);
                        let stats = Arc::clone(&stats);
                        thread::spawn(move || {
                            if let Err(e) = handle_conn(s, &dir, (*auth).as_ref(), &stats) {
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

/// Handle one fast-lane connection: read the handshake, authenticate, and (for a
/// GET) stream the requested byte range. Integrity and resume mirror the HTTP
/// path - a whole-file CRC-32 is sent so the client can verify byte-exactness,
/// and the client drives resume by re-handshaking with a new `offset`.
fn handle_conn(mut stream: TcpStream, dir: &Path, auth: Option<&Auth>, stats: &Stats) -> io::Result<()> {
    stream.set_nodelay(true).ok();
    // A dead peer must not hang a worker thread forever. Generous so a slow but
    // alive link is not falsely cut; the client resumes on any drop anyway.
    stream.set_read_timeout(Some(Duration::from_secs(60))).ok();

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
    let offset = read_u64(&mut stream)?;
    let range_len = read_u64(&mut stream)?;

    // ---- Auth (same token as the HTTP `?k=` / zap_session cookie) ----
    if let Some(a) = auth {
        if token != a.token {
            return reply_err(&mut stream, ST_UNAUTHORIZED, "unauthorized");
        }
    }
    if op != OP_GET {
        return reply_err(&mut stream, ST_UNSUPPORTED, "op not supported in v1");
    }

    // ---- Resolve the file (same sandbox rule as the HTTP download) ----
    let Some(fpath) = resolve_within(dir, &path) else {
        return reply_err(&mut stream, ST_NOT_FOUND, "bad path");
    };
    let meta = match std::fs::metadata(&fpath) {
        Ok(m) if m.is_file() => m,
        _ => return reply_err(&mut stream, ST_NOT_FOUND, "not found"),
    };
    let total = meta.len();
    let start = offset.min(total);
    let len = if range_len == 0 {
        total - start
    } else {
        range_len.min(total - start)
    };

    // Whole-file CRC-32 so the client can prove the assembled file is byte-exact
    // before renaming it into place - the same integrity primitive the HTTP
    // upload path uses. Cached per (path, size, mtime) so the many handshakes of
    // a multi-stream download read the file just once, not once per connection.
    // The client stats the file first (one connection), which warms the cache
    // before the parallel workers hand-shake. Best-effort: no CRC on read error.
    let crc = cached_crc(&fpath, &meta);

    // ---- Reply (server -> client): status + total_size + optional CRC ----
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

    // A zero-length request is a "stat": the client only wanted total_size + CRC
    // (e.g. to plan a multi-stream download). Send the header, no data, and skip
    // the transfer bookkeeping so it never shows as a spurious transfer.
    if len == 0 {
        stream.flush()?;
        return Ok(());
    }

    // Coalesce into one transfer row keyed by path (whole-file total), exactly
    // like the HTTP download does for its many Range chunks.
    let filename = fpath
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "download".to_string());
    let path_key = fpath.to_string_lossy().into_owned();
    let transfer = stats.begin_download(&path_key, &filename, Some(total), &path_key);
    // Reflect bytes the client already holds so the bar resumes, not restarts.
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

    // Only finish the coalesced row once the whole file has been delivered;
    // otherwise this was one range of a transfer still in progress.
    if transfer.done.load(Ordering::Relaxed) >= total {
        transfer.finished.store(true, Ordering::Relaxed);
        transfer.ok.store(true, Ordering::Relaxed);
        stats.save_history();
    }
    Ok(())
}

/// Write an error reply (`status`, then a length-prefixed UTF-8 message) and
/// return `Ok` - the connection then closes. The client maps this to a fast-lane
/// failure and falls back to HTTP.
fn reply_err(stream: &mut TcpStream, status: u8, msg: &str) -> io::Result<()> {
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
