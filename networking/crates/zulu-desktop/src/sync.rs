//! The Zulu sync engine.
//!
//! It talks to a host's `znet-core` server over plain HTTP - the *same* server
//! Zap uses, reusing its pairing/transport plus the new clip + SSE endpoints.
//! Both a host (pointed at its own `127.0.0.1`) and a joiner (pointed at a peer)
//! run the identical loop, so there's one code path:
//!
//! - **receiver** holds `GET /events` open and applies each incoming `clip`
//!   frame to the OS clipboard (and tracks `presence`);
//! - **sender** polls the OS clipboard and `POST`s any local change to `/clip`.
//!
//! A single content-based guard ([`last_synced`](Shared::last)) breaks the echo
//! loop: a clip we just applied is never re-sent, and a clip we just sent (which
//! the host broadcasts back to us) is never re-applied.

use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use arboard::Clipboard;

use crate::tlsclient::{self, Conn};

const POLL: Duration = Duration::from_millis(500);
const READ_TIMEOUT: Duration = Duration::from_millis(500);
const RECONNECT_BACKOFF: Duration = Duration::from_secs(1);
const MAX_RECENT: usize = 20;

/// GUI-readable snapshot of the sync engine, updated by the worker threads.
#[derive(Default)]
pub struct SyncState {
    /// True while the event stream is connected.
    pub connected: bool,
    /// Devices currently paired (from the host's `presence` events).
    pub presence: usize,
    /// Recent clips seen (sent or received), newest last.
    pub recent: Vec<ClipLine>,
    /// The last error, if the engine is having trouble.
    pub error: Option<String>,
    pub sent: u64,
    pub received: u64,
}

/// One entry in the activity list.
#[derive(Clone)]
pub struct ClipLine {
    pub text: String,
    pub incoming: bool,
}

/// State shared between the two worker threads.
struct Shared {
    host: String,
    port: u16,
    /// When Some, connect over TLS pinning this cert fingerprint.
    tls_fp: Option<String>,
    stop: AtomicBool,
    /// The content we consider already synced (the echo-loop guard).
    last: Mutex<String>,
    /// Highest clip id applied so far (-1 = none). Skips clips we've already
    /// seen, so a reconnect's backfill replay isn't re-applied. (Resets only if
    /// the host restarts and its ids reset - rare; restart Zulu to recover.)
    seen_id: AtomicI64,
    /// Until this instant, the sender skips the clipboard - set briefly after we
    /// apply a received image, because the OS can hand an image back in a
    /// slightly different encoding and we don't want to echo that back.
    mute_until: Mutex<Instant>,
    state: Arc<Mutex<SyncState>>,
}

impl Shared {
    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }
    fn set_error(&self, msg: Option<String>) {
        if let Ok(mut s) = self.state.lock() {
            s.error = msg;
        }
    }
    fn set_connected(&self, on: bool) {
        if let Ok(mut s) = self.state.lock() {
            s.connected = on;
        }
    }
    /// Record a clip in the activity list (newest last, capped).
    fn record(&self, text: &str, incoming: bool) {
        if let Ok(mut s) = self.state.lock() {
            s.recent.push(ClipLine { text: text.to_string(), incoming });
            let len = s.recent.len();
            if len > MAX_RECENT {
                s.recent.drain(0..len - MAX_RECENT);
            }
            if incoming {
                s.received += 1;
            } else {
                s.sent += 1;
            }
        }
    }
}

/// A running sync engine. Dropping or calling [`stop`](Self::stop) tears down
/// both threads and lets any held connection close.
pub struct SyncHandle {
    shared: Arc<Shared>,
    threads: Vec<JoinHandle<()>>,
}

impl SyncHandle {
    /// Start syncing against `base` (e.g. `http://192.168.1.9:8080`, or just
    /// `192.168.1.9:8080`). Returns `None` if the address can't be parsed.
    pub fn start(base: &str, state: Arc<Mutex<SyncState>>) -> Option<SyncHandle> {
        let (host, port, tls_fp) = parse_base(base)?;
        let shared = Arc::new(Shared {
            host,
            port,
            tls_fp,
            stop: AtomicBool::new(false),
            last: Mutex::new(String::new()),
            seen_id: AtomicI64::new(-1),
            mute_until: Mutex::new(Instant::now()),
            state,
        });
        let receiver = {
            let sh = Arc::clone(&shared);
            thread::spawn(move || run_receiver(sh))
        };
        let sender = {
            let sh = Arc::clone(&shared);
            thread::spawn(move || run_sender(sh))
        };
        Some(SyncHandle { shared, threads: vec![receiver, sender] })
    }

    pub fn stop(self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        for t in self.threads {
            let _ = t.join();
        }
    }
}

/// Parse `http(s)://host:port/...?fp=<hex>` into `(host, port, tls_fingerprint)`.
/// `https` selects TLS and the `fp` query param is the pinned cert fingerprint;
/// plain `http` ignores any `fp`. Scheme, path and query are all optional; port
/// defaults to 8080.
fn parse_base(url: &str) -> Option<(String, u16, Option<String>)> {
    let s = url.trim();
    let https = s.starts_with("https://");
    let s = s
        .strip_prefix("http://")
        .or_else(|| s.strip_prefix("https://"))
        .unwrap_or(s);
    let authority = s.split(['/', '?']).next().unwrap_or(s);
    if authority.is_empty() {
        return None;
    }
    let query = s.split_once('?').map(|(_, q)| q);
    let fp = query.and_then(|q| {
        q.split('&').find_map(|kv| kv.strip_prefix("fp=")).map(|v| v.to_string())
    });
    let tls_fp = if https { Some(fp.unwrap_or_default()) } else { None };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().ok()?),
        None => (authority.to_string(), 8080),
    };
    Some((host, port, tls_fp))
}

// ---- receiver: hold GET /events, apply incoming clips ----

fn run_receiver(sh: Arc<Shared>) {
    // One clipboard handle for writing received clips. If the OS won't give us
    // one, we can still show activity; we just can't auto-paste.
    let mut clip = Clipboard::new().ok();
    while !sh.stopped() {
        match open_events(&sh) {
            Ok(stream) => {
                sh.set_connected(true);
                sh.set_error(None);
                stream_events(stream, &sh, &mut clip);
                sh.set_connected(false);
            }
            Err(e) => sh.set_error(Some(format!("Can't reach {}:{} - {e}", sh.host, sh.port))),
        }
        // Reconnect after a short backoff (unless we're shutting down).
        let mut waited = Duration::ZERO;
        while !sh.stopped() && waited < RECONNECT_BACKOFF {
            thread::sleep(Duration::from_millis(100));
            waited += Duration::from_millis(100);
        }
    }
}

fn open_events(sh: &Arc<Shared>) -> io::Result<Conn> {
    let mut s = tlsclient::connect(&sh.host, sh.port, sh.tls_fp.as_deref(), READ_TIMEOUT)?;
    write!(
        s,
        "GET /events HTTP/1.1\r\nHost: {}\r\nAccept: text/event-stream\r\nConnection: keep-alive\r\n\r\n",
        sh.host
    )?;
    s.flush()?;
    Ok(s)
}

/// Read the SSE stream, dispatching `clip` and `presence` events until the
/// connection drops or we're told to stop. Timeouts are expected (they let us
/// check the stop flag), so they don't end the loop.
fn stream_events(mut stream: Conn, sh: &Arc<Shared>, clip: &mut Option<Clipboard>) {
    let mut buf: Vec<u8> = Vec::new();
    let mut headers_done = false;
    let mut event: Option<String> = None;
    let mut id: Option<String> = None;
    let mut data: Vec<String> = Vec::new();
    let mut tmp = [0u8; 4096];

    while !sh.stopped() {
        match stream.read(&mut tmp) {
            Ok(0) => return, // server closed the stream
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
            Err(e) if is_timeout(&e) => continue, // no data this tick; check stop, retry
            Err(_) => return,
        }

        if !headers_done {
            match find(&buf, b"\r\n\r\n") {
                Some(pos) => {
                    buf.drain(0..pos + 4);
                    headers_done = true;
                }
                None => continue, // headers still arriving
            }
        }

        // Process every complete line (SSE fields are newline-delimited).
        while let Some(nl) = buf.iter().position(|&b| b == b'\n') {
            let raw: Vec<u8> = buf.drain(0..=nl).collect();
            let line = String::from_utf8_lossy(&raw[..raw.len() - 1]);
            let line = line.strip_suffix('\r').unwrap_or(&line);
            on_sse_line(line, &mut event, &mut id, &mut data, sh, clip);
        }
    }
}

/// Handle one SSE line. A blank line ends an event and dispatches it.
fn on_sse_line(
    line: &str,
    event: &mut Option<String>,
    id: &mut Option<String>,
    data: &mut Vec<String>,
    sh: &Arc<Shared>,
    clip: &mut Option<Clipboard>,
) {
    if line.is_empty() {
        dispatch(event.take(), id.take(), std::mem::take(data), sh, clip);
    } else if let Some(v) = line.strip_prefix("event:") {
        *event = Some(v.trim().to_string());
    } else if let Some(v) = line.strip_prefix("id:") {
        *id = Some(v.trim().to_string());
    } else if let Some(v) = line.strip_prefix("data:") {
        // Exactly one optional leading space is part of the SSE framing.
        data.push(v.strip_prefix(' ').unwrap_or(v).to_string());
    }
    // ":" comments (heartbeats) and "retry:" need no action here.
}

fn dispatch(
    event: Option<String>,
    id: Option<String>,
    data: Vec<String>,
    sh: &Arc<Shared>,
    clip: &mut Option<Clipboard>,
) {
    match event.as_deref() {
        Some("clip") => {
            // Skip clips we've already applied (a reconnect replays the backfill
            // with the same, non-increasing ids).
            if let Some(n) = id.as_deref().and_then(|s| s.parse::<i64>().ok()) {
                if n <= sh.seen_id.load(Ordering::Relaxed) {
                    return;
                }
                sh.seen_id.store(n, Ordering::Relaxed);
            }
            apply_clip(data.join("\n"), sh, clip);
        }
        Some("presence") => {
            if let Some(n) = parse_count(&data.join("")) {
                if let Ok(mut s) = sh.state.lock() {
                    s.presence = n;
                }
            }
        }
        _ => {}
    }
}

/// Write a received clip to the OS clipboard, unless it's one we already have
/// (the echo of something we just sent, or an unchanged repeat).
fn apply_clip(text: String, sh: &Arc<Shared>, clip: &mut Option<Clipboard>) {
    if text.is_empty() {
        return;
    }
    {
        let mut guard = match sh.last.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        if *guard == text {
            return; // echo / duplicate - do not re-apply
        }
        *guard = text.clone();
    }
    if let Some(c) = clip {
        if crate::imageclip::is_image(&text) {
            if crate::imageclip::apply_image_data_url(c, &text) {
                // Mute the sender briefly so it can't read this image back and
                // re-post it before we canonicalize the guard below.
                if let Ok(mut m) = sh.mute_until.lock() {
                    *m = Instant::now() + Duration::from_millis(900);
                }
                // The OS may hand an image back in a slightly different encoding
                // than we sent. Canonicalize the guard to what THIS device will
                // now read, so the sender doesn't mistake it for a fresh copy.
                if let Some(canon) = crate::imageclip::read_image_data_url(c) {
                    if let Ok(mut g) = sh.last.lock() {
                        *g = canon;
                    }
                }
            }
        } else {
            let _ = c.set_text(text.clone());
        }
    }
    sh.record(label(&text), true);
}

/// What to show for a clip in the activity list - a data-URL image is a huge
/// string, so show a short placeholder instead of dumping base64.
fn label(content: &str) -> &str {
    if crate::imageclip::is_image(content) {
        "[image]"
    } else {
        content
    }
}

// ---- sender: poll the OS clipboard, POST local changes ----

fn run_sender(sh: Arc<Shared>) {
    let mut clip = match Clipboard::new() {
        Ok(c) => c,
        Err(e) => {
            sh.set_error(Some(format!("No clipboard access: {e}")));
            return;
        }
    };
    // Seed the guard with whatever is already on the clipboard (text OR a
    // leftover image), so connecting doesn't immediately broadcast the user's
    // pre-existing clipboard.
    if let Some(cur) = read_clipboard(&mut clip) {
        if let Ok(mut g) = sh.last.lock() {
            *g = cur;
        }
    }

    while !sh.stopped() {
        sleep_interruptible(&sh, POLL);
        if sh.stopped() {
            break;
        }
        // Skip while muted (just applied a received image - see apply_clip).
        if sh.mute_until.lock().map(|m| Instant::now() < *m).unwrap_or(false) {
            continue;
        }
        // Prefer text; fall back to a size-capped image if the clipboard holds
        // a picture instead. Both travel as a plain string over /clip.
        let cur = match read_clipboard(&mut clip) {
            Some(c) => c,
            None => continue,
        };
        {
            let mut guard = match sh.last.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            if *guard == cur {
                continue; // nothing new since last sync
            }
            *guard = cur.clone();
        }
        match post_clip(&sh, &cur) {
            Ok(()) => sh.record(label(&cur), false),
            Err(e) => sh.set_error(Some(format!("Send failed: {e}"))),
        }
    }
}

/// Read the current clipboard as a sync payload: the text if present, else a
/// small image as a PNG data URL. `None` when there's nothing (or the image is
/// too large to send).
fn read_clipboard(clip: &mut Clipboard) -> Option<String> {
    if let Ok(t) = clip.get_text() {
        if !t.is_empty() {
            return Some(t);
        }
    }
    crate::imageclip::read_image_data_url(clip)
}

fn post_clip(sh: &Arc<Shared>, text: &str) -> io::Result<()> {
    let mut s = tlsclient::connect(&sh.host, sh.port, sh.tls_fp.as_deref(), Duration::from_secs(3))?;
    write!(
        s,
        "POST /clip HTTP/1.1\r\nHost: {}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        sh.host,
        text.len()
    )?;
    s.write_all(text.as_bytes())?;
    s.flush()?;
    // Drain and discard the response so the server sees a clean close. Over TLS
    // a `Connection: close` shows up as UnexpectedEof after the body - fine.
    let mut resp = Vec::new();
    let _ = s.read_to_end(&mut resp);
    Ok(())
}

// ---- small helpers ----

fn sleep_interruptible(sh: &Arc<Shared>, total: Duration) {
    let step = Duration::from_millis(100);
    let mut waited = Duration::ZERO;
    while !sh.stopped() && waited < total {
        thread::sleep(step);
        waited += step;
    }
}

fn is_timeout(e: &io::Error) -> bool {
    matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut)
}

/// Find the first occurrence of `needle` in `hay`.
fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Pull the integer out of a `{"count":N}` presence payload.
fn parse_count(s: &str) -> Option<usize> {
    let after = s.split("\"count\":").nth(1)?;
    let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_base_handles_forms() {
        assert_eq!(parse_base("http://192.168.1.9:8080"), Some(("192.168.1.9".into(), 8080, None)));
        assert_eq!(parse_base("192.168.1.9:8080/"), Some(("192.168.1.9".into(), 8080, None)));
        assert_eq!(parse_base("http://host:9000/path?k=abc"), Some(("host".into(), 9000, None)));
        assert_eq!(parse_base("10.0.0.2"), Some(("10.0.0.2".into(), 8080, None)));
        assert_eq!(parse_base(""), None);
    }

    #[test]
    fn parse_base_extracts_tls_fingerprint() {
        // https selects TLS and pulls the pinned fingerprint from `fp`.
        assert_eq!(
            parse_base("https://192.168.1.9:8080/?k=tok&fp=abcd1234"),
            Some(("192.168.1.9".into(), 8080, Some("abcd1234".into())))
        );
        // https without fp still means TLS (empty pin -> will fail to verify).
        assert_eq!(parse_base("https://host:9000"), Some(("host".into(), 9000, Some(String::new()))));
        // plain http ignores any fp.
        assert_eq!(parse_base("http://host:9000?fp=xx"), Some(("host".into(), 9000, None)));
    }

    #[test]
    fn parse_count_reads_presence_payload() {
        assert_eq!(parse_count("{\"count\":3}"), Some(3));
        assert_eq!(parse_count("{\"count\":0}"), Some(0));
        assert_eq!(parse_count("garbage"), None);
    }

    #[test]
    fn find_locates_header_terminator() {
        assert_eq!(find(b"abc\r\n\r\nbody", b"\r\n\r\n"), Some(3));
        assert_eq!(find(b"no terminator", b"\r\n\r\n"), None);
    }
}
