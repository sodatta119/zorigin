//! Web transport (server mode).
//!
//! Unlike the host-driven [`Transport`](crate::transport::Transport) trait,
//! the web transport inverts control: the Mac runs an HTTP server on the LAN
//! and the phone drives transfers from its browser. No app is needed on the
//! phone.
//!
//! Endpoints:
//!   GET  /                    -> the web UI
//!   GET  /api/files           -> JSON list of shareable files
//!   GET  /download/<name>     -> download a shared file (Mac -> phone)
//!   PUT  /upload?name=<name>  -> upload a file (phone -> Mac); body is raw bytes

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use socket2::{Domain, Protocol, Socket, Type};
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

const INDEX_HTML: &str = include_str!("index.html");
const LOGIN_HTML: &str = include_str!("login.html");

/// A username/password pair for HTTP Basic authentication.
#[derive(Debug, Clone)]
pub struct Credentials {
    pub user: String,
    pub pass: String,
}

/// Configuration for a web-serve session.
pub struct ServeConfig {
    /// Directory whose files are offered for download and where uploads land.
    pub dir: PathBuf,
    /// Port to listen on.
    pub port: u16,
    /// Address to bind (defaults to all interfaces so other devices can reach it).
    pub bind: IpAddr,
    /// If set, every request must authenticate with these credentials.
    pub auth: Option<Credentials>,
    /// If set, completed transfer records are persisted to this file and reloaded
    /// on start, so the activity history survives a server stop/start or an app
    /// restart. `None` disables persistence (e.g. the one-shot CLI).
    pub history: Option<PathBuf>,
}

/// Details about a running server, handed to the caller once it is bound and
/// listening. Callers (the CLI, an Android UI, …) use this to tell the user how
/// to connect - this crate itself performs no presentation.
#[derive(Debug, Clone)]
pub struct ServerInfo {
    /// Canonicalized directory being shared.
    pub dir: PathBuf,
    /// Port the server is listening on.
    pub port: u16,
    /// Best-guess LAN IP address of this host, if one could be determined.
    pub lan_ip: Option<IpAddr>,
    /// When the server is secured, the run's pairing key (session token). A URL
    /// carrying `?k=<key>` auto-authenticates, so scanning the QR skips the
    /// password. `None` when the server is open (no auth).
    pub auth_token: Option<String>,
}

impl ServerInfo {
    /// The URL another device on the same network should open. Falls back to
    /// `localhost` when no LAN IP could be determined.
    pub fn url(&self) -> String {
        let host = self
            .lan_ip
            .map(|ip| ip.to_string())
            .unwrap_or_else(|| "localhost".to_string());
        format!("http://{host}:{}/", self.port)
    }

    /// The URL to encode in a QR / share link: like [`url`](Self::url) but with
    /// the pairing key appended when the server is secured, so scanning it grants
    /// access without typing the password. Identical to `url()` when open.
    pub fn url_with_key(&self) -> String {
        match &self.auth_token {
            Some(token) => format!("{}?k={token}", self.url()),
            None => self.url(),
        }
    }
}

/// A running web server that can be stopped from another thread. Returned by
/// [`spawn`] for embedders (such as an Android foreground service) that start
/// the server and later need to shut it down. Dropping the handle also stops
/// the server and releases the port.
pub struct ServerHandle {
    server: Arc<Server>,
    acceptor: Option<thread::JoinHandle<()>>,
    stats: Arc<Stats>,
}

/// Direction of a transfer, from the server's point of view.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Direction {
    /// A client is sending a file to this server.
    Upload,
    /// A client is downloading a file from this server.
    Download,
}

/// Live state of one transfer, updated byte-by-byte on a worker thread.
struct TransferState {
    id: u64,
    /// Stable identity for a resumable upload (its destination). All chunks and
    /// resumes of the same file share one `TransferState` so they show as a
    /// single row, not one-per-chunk. Empty for downloads / one-shot uploads.
    key: String,
    name: String,
    /// Absolute path of the file on this host (for "open file location"). Empty
    /// if unknown.
    path: String,
    direction: Direction,
    total: Option<u64>,
    done: AtomicU64,
    finished: AtomicBool,
    ok: AtomicBool,
    /// True once a completed upload passed its crc32 integrity check.
    verified: AtomicBool,
    started: Instant,
}

/// A snapshot of one transfer for the UI.
#[derive(Clone, Debug)]
pub struct TransferInfo {
    pub id: u64,
    pub name: String,
    /// Absolute path of the file on this host; empty if unknown.
    pub path: String,
    pub direction: Direction,
    pub done: u64,
    pub total: Option<u64>,
    pub finished: bool,
    pub ok: bool,
    /// True once a completed upload passed its crc32 integrity check.
    pub verified: bool,
    pub elapsed_secs: f64,
}

/// Running totals + per-transfer activity, shared across worker threads.
#[derive(Default)]
pub struct Stats {
    bytes: AtomicU64,
    next_id: AtomicU64,
    /// Count of HTTP requests accepted since start. A non-zero value proves at
    /// least one client actually reached this host - used to distinguish a
    /// working setup from one where nothing can connect (wrong Wi-Fi, AP/client
    /// isolation, firewall). See [`ServerHandle::requests_seen`].
    requests: AtomicU64,
    transfers: Mutex<Vec<Arc<TransferState>>>,
    /// Where completed transfers are persisted (so history survives a restart).
    history_path: Option<PathBuf>,
}

impl Stats {
    /// Create stats, loading any previously-persisted transfer history so the
    /// activity list survives a server stop/start or app restart.
    fn new(history_path: Option<PathBuf>) -> Self {
        let stats = Stats { history_path, ..Default::default() };
        stats.load_history();
        stats
    }

    /// Register a new transfer and return its live state to update as bytes flow.
    fn begin(&self, name: &str, direction: Direction, total: Option<u64>, path: &str) -> Arc<TransferState> {
        self.create(String::new(), name, direction, total, path)
    }

    /// Persist the finished transfers (most recent, capped) as simple TSV so the
    /// history survives a restart. Atomic write via a temp file + rename.
    fn save_history(&self) {
        let Some(path) = &self.history_path else { return };
        let Ok(list) = self.transfers.lock() else { return };
        let mut out = String::new();
        for t in list.iter().filter(|t| t.finished.load(Ordering::Relaxed)) {
            let dir = match t.direction {
                Direction::Upload => "up",
                Direction::Download => "down",
            };
            let name = t.name.replace(['\t', '\n', '\r'], " ");
            let path = t.path.replace(['\t', '\n', '\r'], " ");
            let total = t.total.map(|n| n.to_string()).unwrap_or_else(|| "-".to_string());
            // path is last so it can hold anything except tab/newline (sanitized).
            out.push_str(&format!(
                "{dir}\t{name}\t{total}\t{}\t{}\t{}\t{path}\n",
                t.done.load(Ordering::Relaxed),
                t.ok.load(Ordering::Relaxed) as u8,
                t.verified.load(Ordering::Relaxed) as u8,
            ));
        }
        let tmp = path.with_extension("tmp");
        if fs::write(&tmp, out).is_ok() {
            let _ = fs::rename(&tmp, path);
        }
    }

    /// Load persisted transfer records as finished history entries.
    fn load_history(&self) {
        let Some(path) = &self.history_path else { return };
        let Ok(data) = fs::read_to_string(path) else { return };
        let Ok(mut list) = self.transfers.lock() else { return };
        for line in data.lines() {
            let f: Vec<&str> = line.splitn(7, '\t').collect();
            if f.len() < 6 {
                continue;
            }
            let direction = if f[0] == "down" { Direction::Download } else { Direction::Upload };
            let total = f[2].parse::<u64>().ok();
            let done = f[3].parse::<u64>().unwrap_or(0);
            let path = f.get(6).map(|s| s.to_string()).unwrap_or_default();
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            list.push(Arc::new(TransferState {
                id,
                key: String::new(),
                name: f[1].to_string(),
                path,
                direction,
                total,
                done: AtomicU64::new(done),
                finished: AtomicBool::new(true),
                ok: AtomicBool::new(f[4] == "1"),
                verified: AtomicBool::new(f[5] == "1"),
                started: Instant::now(),
            }));
        }
        let len = list.len();
        if len > 50 {
            list.drain(0..len - 50);
        }
    }

    /// Begin (or resume) a keyed resumable upload. If an unfinished transfer for
    /// the same `key` (destination) already exists - an earlier chunk or a prior
    /// attempt of the same file - reuse it so every chunk and every pause/resume
    /// shows as one continuous row instead of spawning a new one each time.
    fn begin_upload(&self, key: &str, name: &str, total: Option<u64>, path: &str) -> Arc<TransferState> {
        if let Ok(list) = self.transfers.lock() {
            if let Some(existing) = list
                .iter()
                .rev()
                .find(|t| t.key == key && !t.finished.load(Ordering::Relaxed))
            {
                return Arc::clone(existing);
            }
        }
        self.create(key.to_string(), name, Direction::Upload, total, path)
    }

    fn create(
        &self,
        key: String,
        name: &str,
        direction: Direction,
        total: Option<u64>,
        path: &str,
    ) -> Arc<TransferState> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let state = Arc::new(TransferState {
            id,
            key,
            name: name.to_string(),
            path: path.to_string(),
            direction,
            total,
            done: AtomicU64::new(0),
            finished: AtomicBool::new(false),
            ok: AtomicBool::new(false),
            verified: AtomicBool::new(false),
            started: Instant::now(),
        });
        if let Ok(mut list) = self.transfers.lock() {
            list.push(Arc::clone(&state));
            // Keep only the most recent transfers.
            let len = list.len();
            if len > 50 {
                list.drain(0..len - 50);
            }
        }
        state
    }
}

impl ServerHandle {
    /// Total bytes transferred (uploads + downloads) since the server started.
    /// Poll this over time to compute live throughput.
    pub fn bytes_transferred(&self) -> u64 {
        self.stats.bytes.load(Ordering::Relaxed)
    }

    /// Number of HTTP requests any client has made since the server started.
    /// While this stays 0, no device has managed to reach the host - a front-end
    /// can wait a few seconds and, if it's still 0, surface an AP/client-isolation
    /// or wrong-network hint instead of leaving the user staring at a dead link.
    pub fn requests_seen(&self) -> u64 {
        self.stats.requests.load(Ordering::Relaxed)
    }

    /// Snapshot of recent transfers (newest last), for an activity view.
    pub fn transfers(&self) -> Vec<TransferInfo> {
        let Ok(list) = self.stats.transfers.lock() else {
            return Vec::new();
        };
        list.iter()
            .map(|t| TransferInfo {
                id: t.id,
                name: t.name.clone(),
                path: t.path.clone(),
                direction: t.direction,
                done: t.done.load(Ordering::Relaxed),
                total: t.total,
                finished: t.finished.load(Ordering::Relaxed),
                ok: t.ok.load(Ordering::Relaxed),
                verified: t.verified.load(Ordering::Relaxed),
                elapsed_secs: t.started.elapsed().as_secs_f64(),
            })
            .collect()
    }

    /// Stop the server and wait until the listening socket is released, so the
    /// port is free to bind again immediately (e.g. an app restart).
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        // `unblock` wakes exactly one thread blocked in `incoming_requests`,
        // and there is exactly one - the acceptor - so a single call is enough.
        self.server.unblock();
        if let Some(acceptor) = self.acceptor.take() {
            let _ = acceptor.join();
        }
        // The acceptor has now dropped its `Arc<Server>`. Once this handle's own
        // `Arc` drops too, `Server`'s destructor closes the listening socket.
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Bind the web server, invoke `on_ready` exactly once with the live
/// [`ServerInfo`], then serve requests until the process is stopped (blocking).
///
/// `on_ready` is where the caller presents connection details - the core does
/// no printing of its own, so the same server can back a terminal or a GUI.
pub fn serve(config: ServeConfig, on_ready: impl FnOnce(&ServerInfo)) -> Result<()> {
    let (server, dir, mut info) = bind(&config)?;
    let auth = build_auth(config.auth.as_ref());
    info.auth_token = auth.as_ref().map(|a| a.token.clone());
    on_ready(&info);
    let auth = Arc::new(auth);
    let stats = Arc::new(Stats::new(config.history.clone()));
    // Use the calling thread as the acceptor; blocks until the process exits.
    accept_loop(&server, &dir, &auth, &stats);
    Ok(())
}

/// Bind and start the web server, returning immediately with the connection
/// details and a [`ServerHandle`] to stop it later. A single acceptor thread
/// runs in the background. This is the entry point embedders (e.g. the Android
/// app via JNI) use, since they can't block the calling thread.
pub fn spawn(config: ServeConfig) -> Result<(ServerInfo, ServerHandle)> {
    let (server, dir, mut info) = bind(&config)?;
    let auth = build_auth(config.auth.as_ref());
    info.auth_token = auth.as_ref().map(|a| a.token.clone());
    let auth = Arc::new(auth);
    let stats = Arc::new(Stats::new(config.history.clone()));
    let acceptor = {
        let server = Arc::clone(&server);
        let dir = Arc::clone(&dir);
        let auth = Arc::clone(&auth);
        let stats = Arc::clone(&stats);
        thread::spawn(move || accept_loop(&server, &dir, &auth, &stats))
    };
    Ok((
        info,
        ServerHandle {
            server,
            acceptor: Some(acceptor),
            stats,
        },
    ))
}

/// Create the share directory, bind the listener, and assemble [`ServerInfo`].
fn bind(config: &ServeConfig) -> Result<(Arc<Server>, Arc<PathBuf>, ServerInfo)> {
    fs::create_dir_all(&config.dir)
        .with_context(|| format!("creating share directory {}", config.dir.display()))?;
    let dir = config
        .dir
        .canonicalize()
        .with_context(|| format!("resolving share directory {}", config.dir.display()))?;

    let addr = SocketAddr::new(config.bind, config.port);
    let listener = bind_listener(addr)
        .with_context(|| format!("failed to start server on {addr}"))?;
    let server = Server::from_listener(listener, None)
        .map_err(|e| anyhow::anyhow!("failed to start server on {addr}: {e}"))?;

    let dir = Arc::new(dir);
    let info = ServerInfo {
        dir: (*dir).clone(),
        port: config.port,
        lan_ip: lan_ip(),
        auth_token: None, // filled in by serve/spawn once auth is built
    };
    Ok((Arc::new(server), dir, info))
}

/// Create the listening socket with `SO_REUSEADDR` set *before* bind, then start
/// listening. `SO_REUSEADDR` lets a fresh run bind the port even if the previous
/// socket is still lingering in `TIME_WAIT` - belt-and-suspenders on top of the
/// single-acceptor clean shutdown, so a quick stop→start never hits `EADDRINUSE`.
fn bind_listener(addr: SocketAddr) -> Result<TcpListener> {
    let domain = Domain::for_address(addr);
    let socket = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))
        .context("creating socket")?;
    socket.set_reuse_address(true).context("setting SO_REUSEADDR")?;
    // Larger socket buffers help throughput on higher-latency Wi-Fi. Best-effort
    // and set before listen so accepted connections inherit them where the OS
    // supports it; the OS clamps to its own max, so failures are harmless.
    let _ = socket.set_send_buffer_size(1 << 20); // 1 MiB
    let _ = socket.set_recv_buffer_size(1 << 20);
    socket.bind(&addr.into()).context("binding socket")?;
    socket.listen(128).context("listening on socket")?;
    Ok(socket.into())
}

/// Accept requests until the server is unblocked, handling each on its own
/// thread so that transfers run concurrently. Only this loop blocks on the
/// server, which is what makes a single `unblock()` a clean shutdown.
fn accept_loop(
    server: &Arc<Server>,
    dir: &Arc<PathBuf>,
    auth: &Arc<Option<Auth>>,
    stats: &Arc<Stats>,
) {
    for request in server.incoming_requests() {
        // A request arriving at all proves a client reached this host.
        stats.requests.fetch_add(1, Ordering::Relaxed);
        let dir = Arc::clone(dir);
        let auth = Arc::clone(auth);
        let stats = Arc::clone(stats);
        thread::spawn(move || {
            if let Err(e) = handle(request, &dir, (*auth).as_ref(), &stats) {
                eprintln!("zap: request error: {e:#}");
            }
        });
    }
}

fn handle(request: Request, dir: &Path, auth: Option<&Auth>, stats: &Arc<Stats>) -> Result<()> {
    let method = request.method().clone();
    let raw_url = request.url().to_string();
    let (path, query) = split_query(&raw_url);

    // Session gate: serve a custom login page (no browser Basic-auth popup).
    if let Some(a) = auth {
        // Pairing key from the QR / share link: `?k=<token>` auto-authenticates
        // (set the cookie, then redirect to the clean path so the key doesn't
        // linger in the address bar / history).
        if let Some(k) = query_param(query, "k") {
            if decode_percent(k) == a.token {
                let cookie = session_cookie(&a.token);
                return respond(
                    request,
                    Response::from_string("")
                        .with_status_code(303)
                        .with_header(header("Set-Cookie", &cookie))
                        .with_header(header("Location", &path)),
                );
            }
        }
        if method == Method::Post && path == "/login" {
            return handle_login(request, a);
        }
        if !has_valid_session(&request, a) {
            return match (&method, path.as_str()) {
                (Method::Get, "/") | (Method::Get, "/login") => {
                    respond(request, html_response(LOGIN_HTML))
                }
                _ => respond(
                    request,
                    Response::from_string("Unauthorized").with_status_code(401),
                ),
            };
        }
    }

    match (&method, path.as_str()) {
        (Method::Get, "/") => respond(request, html_response(INDEX_HTML)),
        (Method::Get, "/api/list") => {
            let rel = query_param(query, "path").map(decode_percent).unwrap_or_default();
            respond(request, json_response(&list_dir_json(dir, &rel)))
        }
        (Method::Get, "/api/search") => {
            let q = query_param(query, "q").map(decode_percent).unwrap_or_default();
            respond(request, json_response(&search_json(dir, &q)))
        }
        // In-progress uploads (temp files) so a refreshed sender can see what's
        // resumable and continue by re-selecting the file.
        (Method::Get, "/api/incoming") => respond(request, json_response(&incoming_json(dir))),
        (Method::Get, "/download") => {
            let rel = query_param(query, "path").map(decode_percent).unwrap_or_default();
            serve_download(request, dir, &rel, stats)
        }
        (Method::Get, "/download-folder") => {
            let rel = query_param(query, "path").map(decode_percent).unwrap_or_default();
            serve_folder_zip(request, dir, &rel, stats)
        }
        // Resume handshake: how many bytes of this file does the host already hold?
        (Method::Head, "/upload") => {
            let rel = query_param(query, "path").map(decode_percent).unwrap_or_default();
            let name = query_param(query, "name").map(decode_percent);
            handle_upload_head(request, dir, &rel, name.as_deref())
        }
        (Method::Put, "/upload") => {
            let rel = query_param(query, "path").map(decode_percent).unwrap_or_default();
            let name = query_param(query, "name").map(decode_percent);
            // Presence of `offset` selects the resumable path; without it we keep
            // the original one-shot behavior for any non-resumable client.
            let offset = query_param(query, "offset").and_then(|s| s.parse::<u64>().ok());
            handle_upload(request, dir, &rel, name.as_deref(), offset, stats)
        }
        // Discard an interrupted upload's partial temp file (client "clear").
        (Method::Delete, "/upload") => {
            let rel = query_param(query, "path").map(decode_percent).unwrap_or_default();
            let name = query_param(query, "name").map(decode_percent);
            discard_partial(request, dir, &rel, name.as_deref())
        }
        _ => respond(request, Response::from_string("Not found").with_status_code(404)),
    }
}

fn serve_download(request: Request, root: &Path, rel: &str, stats: &Stats) -> Result<()> {
    let Some(path) = resolve_within(root, rel) else {
        return respond(request, Response::from_string("Bad path").with_status_code(400));
    };
    let meta = match fs::metadata(&path) {
        Ok(m) if m.is_file() => m,
        _ => return respond(request, Response::from_string("Not found").with_status_code(404)),
    };
    let mut file = match File::open(&path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("zap: opening {} failed: {e:#}", path.display());
            return respond(request, Response::from_string("read error").with_status_code(500));
        }
    };
    let filename = path
        .file_name()
        .map(|n| n.to_string_lossy().replace('"', ""))
        .unwrap_or_else(|| "download".to_string());
    let disposition = format!("attachment; filename=\"{filename}\"");
    let total = meta.len();

    // Honor a `Range: bytes=start-[end]` request so downloads can resume and
    // seek. Everything else falls through to a normal 200 that advertises
    // `Accept-Ranges: bytes`.
    let range = header_value(&request, "Range").and_then(|h| parse_range(h, total));

    let transfer = stats.begin(
        &filename,
        Direction::Download,
        Some(range.map_or(total, |(s, e)| e - s + 1)),
        &path.to_string_lossy(),
    );

    let (status, body_len, content_range) = match range {
        Some((start, end)) => {
            if file.seek(SeekFrom::Start(start)).is_err() {
                return respond(request, Response::from_string("seek error").with_status_code(500));
            }
            (206u16, end - start + 1, Some(format!("bytes {start}-{end}/{total}")))
        }
        None => (200, total, None),
    };

    let reader = CountingReader { inner: file.take(body_len), stats, transfer: &transfer };
    let mut headers = vec![
        header("Content-Type", "application/octet-stream"),
        header("Content-Disposition", &disposition),
        header("Accept-Ranges", "bytes"),
    ];
    if let Some(cr) = &content_range {
        headers.push(header("Content-Range", cr));
    }
    // tiny_http defaults to chunked transfer for any body >= 32 KB, which drops
    // the Content-Length header - so the browser can't show download progress
    // ("Downloading… 0 KB" with no bar). Raise the threshold so a known-length
    // file is always sent with Content-Length (identity), streamed not buffered.
    let response = Response::new(StatusCode(status), headers, reader, Some(body_len as usize), None)
        .with_chunked_threshold(usize::MAX);
    let result = respond(request, response);
    transfer.finished.store(true, Ordering::Relaxed);
    transfer.ok.store(true, Ordering::Relaxed);
    stats.save_history();
    result
}

/// Parse an HTTP `Range` header of the form `bytes=start-[end]` against a known
/// total size. Returns an inclusive `(start, end)` clamped to the file, or
/// `None` for absent/unsatisfiable/multi-range values (caller then sends the
/// whole file). Only the common single open/closed range is supported.
fn parse_range(h: &str, total: u64) -> Option<(u64, u64)> {
    if total == 0 {
        return None;
    }
    let spec = h.trim().strip_prefix("bytes=")?;
    if spec.contains(',') {
        return None; // multi-range not supported
    }
    let (a, b) = spec.split_once('-')?;
    let start: u64 = a.trim().parse().ok()?;
    let end: u64 = if b.trim().is_empty() {
        total - 1
    } else {
        b.trim().parse::<u64>().ok()?.min(total - 1)
    };
    if start > end || start >= total {
        return None;
    }
    Some((start, end))
}

// ---- Folder download (streaming ZIP) ----

/// Stream a folder as a ZIP archive (store / no compression, ZIP64 when a file
/// or the archive exceeds 4 GB). Built dependency-free and streamed via a
/// background producer thread, so arbitrarily large folders never buffer in
/// memory or on disk.
fn serve_folder_zip(request: Request, root: &Path, rel: &str, stats: &Arc<Stats>) -> Result<()> {
    let Some(folder) = resolve_within(root, rel) else {
        return respond(request, Response::from_string("Bad path").with_status_code(400));
    };
    match fs::metadata(&folder) {
        Ok(m) if m.is_dir() => {}
        _ => return respond(request, Response::from_string("Not found").with_status_code(404)),
    }

    let base = folder.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "folder".to_string());
    let mut files: Vec<(PathBuf, String, u64)> = Vec::new();
    collect_files(&folder, &format!("{base}/"), &mut files);
    let total: u64 = files.iter().map(|(_, _, s)| *s).sum();

    let zip_name = format!("{}.zip", base.replace('"', ""));
    let transfer = stats.begin(&zip_name, Direction::Download, Some(total), &folder.to_string_lossy());

    // Producer thread generates the ZIP and pushes chunks; the response reader
    // pulls them. A small bounded channel gives natural backpressure.
    let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(4);
    let stats_t = Arc::clone(stats);
    let transfer_t = Arc::clone(&transfer);
    thread::spawn(move || {
        let mut w = SenderWriter { tx, stats: stats_t, transfer: transfer_t };
        if let Err(e) = write_zip(&mut w, &files) {
            eprintln!("zap: zipping {base} stopped: {e:#}");
        }
    });

    let reader = ChannelReader { rx, cur: Vec::new(), pos: 0 };
    let headers = vec![
        header("Content-Type", "application/zip"),
        header("Content-Disposition", &format!("attachment; filename=\"{zip_name}\"")),
    ];
    // No Content-Length (size unknown up front) → tiny_http uses chunked encoding.
    let response = Response::new(StatusCode(200), headers, reader, None, None);
    let result = respond(request, response);
    transfer.finished.store(true, Ordering::Relaxed);
    transfer.ok.store(true, Ordering::Relaxed);
    result
}

/// Recursively collect `(absolute path, name-in-zip, size)` for every file under
/// `dir`. `prefix` is the archive path prefix (e.g. `"MyAlbum/"`).
fn collect_files(dir: &Path, prefix: &str, out: &mut Vec<(PathBuf, String, u64)>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(PART_PREFIX) {
            continue; // skip in-progress upload temp files
        }
        let zip_name = format!("{prefix}{name}");
        if meta.is_dir() {
            collect_files(&entry.path(), &format!("{zip_name}/"), out);
        } else if meta.is_file() {
            out.push((entry.path(), zip_name, meta.len()));
        }
    }
}

/// A `Write` that forwards each write as a chunk to the response reader and
/// counts bytes into the global + per-transfer stats.
struct SenderWriter {
    tx: mpsc::SyncSender<Vec<u8>>,
    stats: Arc<Stats>,
    transfer: Arc<TransferState>,
}
impl Write for SenderWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.tx
            .send(buf.to_vec())
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "download closed"))?;
        self.stats.bytes.fetch_add(buf.len() as u64, Ordering::Relaxed);
        self.transfer.done.fetch_add(buf.len() as u64, Ordering::Relaxed);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The response side: serves bytes from chunks produced by [`SenderWriter`].
struct ChannelReader {
    rx: mpsc::Receiver<Vec<u8>>,
    cur: Vec<u8>,
    pos: usize,
}
impl Read for ChannelReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        loop {
            if self.pos < self.cur.len() {
                let n = (self.cur.len() - self.pos).min(out.len());
                out[..n].copy_from_slice(&self.cur[self.pos..self.pos + n]);
                self.pos += n;
                return Ok(n);
            }
            match self.rx.recv() {
                Ok(chunk) => {
                    self.cur = chunk;
                    self.pos = 0;
                }
                Err(_) => return Ok(0), // producer finished → EOF
            }
        }
    }
}

const Z64_THRESHOLD: u64 = 0xFFFF_FFFF;

/// Write a streaming ZIP of `files` (store method). Reads each file twice - once
/// for its crc32, once to stream the data - so headers carry real crc/sizes and
/// no data descriptors are needed (maximally compatible). ZIP64 fields kick in
/// per-entry when a size or offset crosses 4 GB.
fn write_zip(w: &mut impl Write, files: &[(PathBuf, String, u64)]) -> io::Result<()> {
    let mut central: Vec<u8> = Vec::new();
    let mut count: u64 = 0;
    let mut offset: u64 = 0;

    for (abs, name, size) in files {
        let crc = match crc32_file(abs) {
            Ok(c) => c,
            Err(_) => continue, // file vanished/unreadable - skip it
        };
        let nb = name.as_bytes();
        let name_z64 = *size >= Z64_THRESHOLD;

        // Local file header.
        let mut lh = Vec::with_capacity(30 + nb.len() + 20);
        push_u32(&mut lh, 0x0403_4b50);
        push_u16(&mut lh, if name_z64 { 45 } else { 20 }); // version needed
        push_u16(&mut lh, if name.is_ascii() { 0 } else { 0x0800 }); // UTF-8 flag
        push_u16(&mut lh, 0); // store
        push_u16(&mut lh, 0); // mod time
        push_u16(&mut lh, 0x21); // mod date (1980-01-01)
        push_u32(&mut lh, crc);
        push_u32(&mut lh, if name_z64 { 0xFFFF_FFFF } else { *size as u32 });
        push_u32(&mut lh, if name_z64 { 0xFFFF_FFFF } else { *size as u32 });
        push_u16(&mut lh, nb.len() as u16);
        push_u16(&mut lh, if name_z64 { 20 } else { 0 }); // extra len
        lh.extend_from_slice(nb);
        if name_z64 {
            push_u16(&mut lh, 0x0001); // ZIP64 extra tag
            push_u16(&mut lh, 16);
            push_u64(&mut lh, *size); // uncompressed
            push_u64(&mut lh, *size); // compressed
        }
        w.write_all(&lh)?;

        // File data.
        let mut f = File::open(abs)?;
        let mut buf = [0u8; 65536];
        loop {
            let n = f.read(&mut buf)?;
            if n == 0 {
                break;
            }
            w.write_all(&buf[..n])?;
        }

        // Central directory record for this entry.
        let off_z64 = offset >= Z64_THRESHOLD;
        let need_z64 = name_z64 || off_z64;
        let mut z64extra: Vec<u8> = Vec::new();
        if name_z64 {
            push_u64(&mut z64extra, *size);
            push_u64(&mut z64extra, *size);
        }
        if off_z64 {
            push_u64(&mut z64extra, offset);
        }
        push_u32(&mut central, 0x0201_4b50);
        push_u16(&mut central, 45); // version made by
        push_u16(&mut central, if need_z64 { 45 } else { 20 });
        push_u16(&mut central, if name.is_ascii() { 0 } else { 0x0800 });
        push_u16(&mut central, 0);
        push_u16(&mut central, 0);
        push_u16(&mut central, 0x21);
        push_u32(&mut central, crc);
        push_u32(&mut central, if name_z64 { 0xFFFF_FFFF } else { *size as u32 });
        push_u32(&mut central, if name_z64 { 0xFFFF_FFFF } else { *size as u32 });
        push_u16(&mut central, nb.len() as u16);
        push_u16(&mut central, z64extra.len() as u16);
        push_u16(&mut central, 0); // comment len
        push_u16(&mut central, 0); // disk start
        push_u16(&mut central, 0); // internal attrs
        push_u32(&mut central, 0); // external attrs
        push_u32(&mut central, if off_z64 { 0xFFFF_FFFF } else { offset as u32 });
        central.extend_from_slice(nb);
        if !z64extra.is_empty() {
            let mut hdr = Vec::new();
            push_u16(&mut hdr, 0x0001);
            push_u16(&mut hdr, z64extra.len() as u16);
            central.extend_from_slice(&hdr);
            central.extend_from_slice(&z64extra);
        }

        offset += lh.len() as u64 + size;
        count += 1;
    }

    // Central directory, then end-of-central-directory records.
    let cd_offset = offset;
    let cd_size = central.len() as u64;
    w.write_all(&central)?;

    let need_z64 = count >= 0xFFFF || cd_size >= Z64_THRESHOLD || cd_offset >= Z64_THRESHOLD;
    if need_z64 {
        let z64_eocd_offset = cd_offset + cd_size;
        let mut z = Vec::new();
        push_u32(&mut z, 0x0606_4b50); // ZIP64 EOCD
        push_u64(&mut z, 44); // size of remaining record
        push_u16(&mut z, 45);
        push_u16(&mut z, 45);
        push_u32(&mut z, 0);
        push_u32(&mut z, 0);
        push_u64(&mut z, count);
        push_u64(&mut z, count);
        push_u64(&mut z, cd_size);
        push_u64(&mut z, cd_offset);
        push_u32(&mut z, 0x0706_4b50); // ZIP64 EOCD locator
        push_u32(&mut z, 0);
        push_u64(&mut z, z64_eocd_offset);
        push_u32(&mut z, 1);
        w.write_all(&z)?;
    }

    let mut e = Vec::new();
    push_u32(&mut e, 0x0605_4b50); // EOCD
    push_u16(&mut e, 0);
    push_u16(&mut e, 0);
    push_u16(&mut e, count.min(0xFFFF) as u16);
    push_u16(&mut e, count.min(0xFFFF) as u16);
    push_u32(&mut e, cd_size.min(Z64_THRESHOLD) as u32);
    push_u32(&mut e, cd_offset.min(Z64_THRESHOLD) as u32);
    push_u16(&mut e, 0); // comment len
    w.write_all(&e)?;
    Ok(())
}

fn push_u16(v: &mut Vec<u8>, n: u16) {
    v.extend_from_slice(&n.to_le_bytes());
}
fn push_u32(v: &mut Vec<u8>, n: u32) {
    v.extend_from_slice(&n.to_le_bytes());
}
fn push_u64(v: &mut Vec<u8>, n: u64) {
    v.extend_from_slice(&n.to_le_bytes());
}

/// Temp file that accumulates a resumable upload until it's complete and
/// verified, then gets atomically renamed to the final name. Hidden from
/// directory listings (see `list_dir_json`) so half-done uploads don't show.
fn part_path(folder: &Path, name: &str) -> PathBuf {
    folder.join(format!("{PART_PREFIX}{name}"))
}

/// `HEAD /upload?path&name` → `X-Zap-Offset: <bytes already received>`. The
/// client resumes its `PUT` from that offset (0 if nothing is here yet).
fn handle_upload_head(request: Request, root: &Path, rel_dir: &str, name: Option<&str>) -> Result<()> {
    let Some(name) = name.filter(|n| is_plain_filename(n)) else {
        return respond(request, Response::from_string("").with_status_code(400));
    };
    let Some(folder) = resolve_within(root, rel_dir) else {
        return respond(request, Response::from_string("").with_status_code(400));
    };
    let offset = fs::metadata(part_path(&folder, name)).map(|m| m.len()).unwrap_or(0);
    respond(
        request,
        Response::from_string("")
            .with_header(header("X-Zap-Offset", &offset.to_string()))
            .with_header(header("Accept-Ranges", "bytes")),
    )
}

/// `DELETE /upload?path&name` → remove the interrupted upload's `.zap-part-<name>`
/// temp file (the client "clear" action). No-op (still 200) if it's already gone.
fn discard_partial(request: Request, root: &Path, rel_dir: &str, name: Option<&str>) -> Result<()> {
    let Some(name) = name.filter(|n| is_plain_filename(n)) else {
        return respond(request, Response::from_string("").with_status_code(400));
    };
    let Some(folder) = resolve_within(root, rel_dir) else {
        return respond(request, Response::from_string("").with_status_code(400));
    };
    let _ = fs::remove_file(part_path(&folder, name));
    respond(request, Response::from_string("ok"))
}

fn handle_upload(
    request: Request,
    root: &Path,
    rel_dir: &str,
    name: Option<&str>,
    offset: Option<u64>,
    stats: &Stats,
) -> Result<()> {
    let Some(name) = name.filter(|n| is_plain_filename(n)) else {
        return respond(request, Response::from_string("Bad or missing name").with_status_code(400));
    };
    let Some(folder) = resolve_within(root, rel_dir) else {
        return respond(request, Response::from_string("Bad path").with_status_code(400));
    };
    // Create the target directory tree so folder uploads (which carry a relative
    // subpath per file) recreate their structure on the host.
    if let Err(e) = fs::create_dir_all(&folder) {
        eprintln!("zap: creating {} failed: {e:#}", folder.display());
        return respond(request, Response::from_string("mkdir failed").with_status_code(500));
    }
    match offset {
        Some(off) => resumable_upload(request, &folder, name, off, stats),
        None => legacy_upload(request, &folder, name, stats),
    }
}

/// Resumable upload: append to a temp file at the client-declared `offset`,
/// verify the whole-file crc32 once complete, then atomically rename into place.
fn resumable_upload(
    mut request: Request,
    folder: &Path,
    name: &str,
    offset: u64,
    stats: &Stats,
) -> Result<()> {
    let part = part_path(folder, name);
    let cur = fs::metadata(&part).map(|m| m.len()).unwrap_or(0);

    // The client's idea of the offset must match what we actually hold; if not,
    // tell it the real offset so it can re-sync instead of corrupting the file.
    if offset != cur {
        return respond(
            request,
            Response::from_string("offset mismatch")
                .with_status_code(409)
                .with_header(header("X-Zap-Offset", &cur.to_string())),
        );
    }

    let total = header_value(&request, "X-Zap-Total").and_then(|s| s.parse::<u64>().ok());
    let expected_crc = header_value(&request, "X-Zap-Crc32")
        .and_then(|s| u32::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok());

    // Key by the destination temp path so all chunks + resumes of this file
    // coalesce into one transfer row instead of one row per chunk.
    let key = part.to_string_lossy().into_owned();
    let dest_path = folder.join(name).to_string_lossy().into_owned();
    let transfer = stats.begin_upload(&key, name, total, &dest_path);
    // Reflect bytes already on disk so the progress bar resumes, not restarts.
    transfer.done.store(cur, Ordering::Relaxed);

    let write_res = append_upload(&mut request, &part, stats, &transfer);
    let newsize = fs::metadata(&part).map(|m| m.len()).unwrap_or(cur);

    // Complete once the temp file has reached the declared total size.
    if let Some(total) = total {
        if newsize >= total {
            transfer.finished.store(true, Ordering::Relaxed);
            let verified = match expected_crc {
                Some(want) => matches!(crc32_file(&part), Ok(got) if got == want),
                None => true,
            };
            if verified {
                let dest = folder.join(name);
                if let Err(e) = fs::rename(&part, &dest) {
                    eprintln!("zap: finalizing {name} failed: {e:#}");
                    return respond(request, Response::from_string("finalize failed").with_status_code(500));
                }
                transfer.ok.store(true, Ordering::Relaxed);
                // Only flag as "verified" when the client actually sent a crc to
                // check against; a bare completion is done, not verified.
                if expected_crc.is_some() {
                    transfer.verified.store(true, Ordering::Relaxed);
                }
                println!("received {name} ({total} bytes, verified) into {}", folder.display());
                stats.save_history();
                return respond(
                    request,
                    Response::from_string("ok").with_header(header("X-Zap-Verified", "true")),
                );
            } else {
                // Corrupt - discard the temp file so a retry starts clean.
                let _ = fs::remove_file(&part);
                eprintln!("zap: integrity check failed for {name} (crc mismatch)");
                stats.save_history();
                return respond(
                    request,
                    Response::from_string("integrity check failed")
                        .with_status_code(422)
                        .with_header(header("X-Zap-Verified", "false")),
                );
            }
        }
    }

    // Not complete yet: report how far we got. Either the client is chunking, or
    // the connection dropped mid-PUT - either way the temp file survives for the
    // next resume, keyed off this offset.
    match write_res {
        Ok(_) => respond(
            request,
            Response::from_string("partial").with_header(header("X-Zap-Offset", &newsize.to_string())),
        ),
        Err(e) => {
            transfer.finished.store(true, Ordering::Relaxed);
            eprintln!("zap: upload of {name} interrupted at {newsize} bytes: {e:#}");
            stats.save_history();
            respond(
                request,
                Response::from_string("interrupted")
                    .with_status_code(500)
                    .with_header(header("X-Zap-Offset", &newsize.to_string())),
            )
        }
    }
}

/// One-shot upload (no resume) - the original behavior, kept for any client that
/// doesn't send an `offset`.
fn legacy_upload(mut request: Request, folder: &Path, name: &str, stats: &Stats) -> Result<()> {
    let dest = folder.join(name);
    let total = request.body_length().map(|n| n as u64);
    let transfer = stats.begin(name, Direction::Upload, total, &dest.to_string_lossy());

    // On failure we must still send a response, otherwise the client hangs and
    // reports a confusing generic error instead of a clean failure.
    let result = write_upload(&mut request, &dest, stats, &transfer);
    transfer.finished.store(true, Ordering::Relaxed);
    match result {
        Ok(bytes) => {
            transfer.ok.store(true, Ordering::Relaxed);
            println!("received {name} ({bytes} bytes) into {}", folder.display());
            stats.save_history();
            respond(request, Response::from_string("ok"))
        }
        Err(e) => {
            eprintln!("zap: upload of {name} failed: {e:#}");
            stats.save_history();
            respond(request, Response::from_string("upload failed").with_status_code(500))
        }
    }
}

/// Append the request body to the temp file (create if new). Append mode keeps
/// the write at the current end, matching the verified `offset`.
fn append_upload(
    request: &mut Request,
    part: &Path,
    stats: &Stats,
    transfer: &TransferState,
) -> Result<u64> {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(part)
        .with_context(|| format!("opening {}", part.display()))?;
    let mut writer = CountingWriter { inner: file, stats, transfer };
    io::copy(request.as_reader(), &mut writer).with_context(|| format!("writing {}", part.display()))
}

/// CRC-32 (IEEE / zlib, table-driven) over a whole file. Matches the streaming
/// crc32 the web client computes, so the two can be compared to verify an upload
/// arrived intact. Detects truncation, gaps, overlaps and bit-flips - the real
/// risks on flaky Wi-Fi (this is a corruption check, not a security primitive).
fn crc32_file(path: &Path) -> io::Result<u32> {
    let table = crc32_table();
    let mut f = File::open(path)?;
    let mut buf = [0u8; 65536];
    let mut crc = 0xFFFF_FFFFu32;
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        for &b in &buf[..n] {
            crc = (crc >> 8) ^ table[((crc ^ b as u32) & 0xff) as usize];
        }
    }
    Ok(!crc)
}

/// The 256-entry CRC-32 lookup table, built once on first use.
fn crc32_table() -> &'static [u32; 256] {
    static TABLE: OnceLock<[u32; 256]> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        let mut i = 0usize;
        while i < 256 {
            let mut c = i as u32;
            let mut k = 0;
            while k < 8 {
                c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
                k += 1;
            }
            t[i] = c;
            i += 1;
        }
        t
    })
}

/// Stream the request body to `dest`, returning the number of bytes written.
fn write_upload(
    request: &mut Request,
    dest: &Path,
    stats: &Stats,
    transfer: &TransferState,
) -> Result<u64> {
    let file = File::create(dest).with_context(|| format!("creating {}", dest.display()))?;
    let mut writer = CountingWriter { inner: file, stats, transfer };
    io::copy(request.as_reader(), &mut writer).with_context(|| format!("writing {}", dest.display()))
}

/// A reader that counts bytes into the global total and this transfer.
struct CountingReader<'a, R> {
    inner: R,
    stats: &'a Stats,
    transfer: &'a TransferState,
}
impl<R: Read> Read for CountingReader<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.stats.bytes.fetch_add(n as u64, Ordering::Relaxed);
        self.transfer.done.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
}

/// A writer that counts bytes into the global total and this transfer.
struct CountingWriter<'a, W> {
    inner: W,
    stats: &'a Stats,
    transfer: &'a TransferState,
}
impl<W: Write> Write for CountingWriter<'_, W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.stats.bytes.fetch_add(n as u64, Ordering::Relaxed);
        self.transfer.done.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

// ---- Directory listing ----

/// List the directory at `root`/`rel` as JSON:
/// `{"path":"<normalized rel>","entries":[{"name","dir","size"?}...]}`.
/// Folders sort before files; both alphabetically. Returns an `error` object
/// if the path escapes the root or can't be read.
fn list_dir_json(root: &Path, rel: &str) -> String {
    let Some(dir) = resolve_within(root, rel) else {
        return r#"{"error":"bad path"}"#.to_string();
    };

    let mut dirs: Vec<(String, String)> = Vec::new();
    let mut files: Vec<(String, String)> = Vec::new();
    if let Ok(entries) = fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with(PART_PREFIX) {
                continue; // hide in-progress upload temp files
            }
            if meta.is_dir() {
                dirs.push((
                    name.to_lowercase(),
                    format!("{{\"name\":{},\"dir\":true}}", json_string(&name)),
                ));
            } else if meta.is_file() {
                files.push((
                    name.to_lowercase(),
                    format!(
                        "{{\"name\":{},\"dir\":false,\"size\":{}}}",
                        json_string(&name),
                        meta.len()
                    ),
                ));
            }
        }
    }
    dirs.sort_by(|a, b| a.0.cmp(&b.0));
    files.sort_by(|a, b| a.0.cmp(&b.0));

    let entries: Vec<String> = dirs
        .into_iter()
        .chain(files)
        .map(|(_, json)| json)
        .collect();

    format!(
        "{{\"path\":{},\"entries\":[{}]}}",
        json_string(&normalize_rel(rel)),
        entries.join(",")
    )
}

/// Collapse a relative path to clean `a/b/c` form (no empty or `.` segments).
fn normalize_rel(rel: &str) -> String {
    rel.split('/')
        .filter(|s| !s.is_empty() && *s != ".")
        .collect::<Vec<_>>()
        .join("/")
}

/// Result and scan caps for recursive search - keeps a huge tree from stalling.
const SEARCH_MAX_RESULTS: usize = 300;
const SEARCH_MAX_SCANNED: usize = 30_000;

/// Recursively search `root` for entries whose name contains `query`
/// (case-insensitive). Returns `{"query":..,"entries":[{path,name,dir,size?}..]}`.
fn search_json(root: &Path, query: &str) -> String {
    let needle = query.trim().to_lowercase();
    let mut hits: Vec<String> = Vec::new();
    if !needle.is_empty() {
        let mut budget = SEARCH_MAX_SCANNED;
        search_walk(root, root, &needle, &mut hits, &mut budget);
    }
    format!(
        "{{\"query\":{},\"entries\":[{}]}}",
        json_string(query),
        hits.join(",")
    )
}

fn search_walk(root: &Path, dir: &Path, needle: &str, hits: &mut Vec<String>, budget: &mut usize) {
    if *budget == 0 || hits.len() >= SEARCH_MAX_RESULTS {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        if *budget == 0 || hits.len() >= SEARCH_MAX_RESULTS {
            return;
        }
        *budget -= 1;
        let Ok(meta) = entry.metadata() else { continue };
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(PART_PREFIX) {
            continue; // hide in-progress upload temp files
        }
        let path = entry.path();

        if name.to_lowercase().contains(needle) {
            let rel = path
                .strip_prefix(root)
                .ok()
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default();
            hits.push(if meta.is_dir() {
                format!("{{\"path\":{},\"name\":{},\"dir\":true}}", json_string(&rel), json_string(&name))
            } else {
                format!(
                    "{{\"path\":{},\"name\":{},\"dir\":false,\"size\":{}}}",
                    json_string(&rel),
                    json_string(&name),
                    meta.len()
                )
            });
        }
        if meta.is_dir() {
            search_walk(root, &path, needle, hits, budget);
        }
    }
}

/// List in-progress upload temp files (`.zap-part-*`) so a client that reloaded
/// mid-upload can see what's unfinished. `{entries:[{path,name,done}]}` where
/// `name` is the real filename and `done` is the bytes already received.
fn incoming_json(root: &Path) -> String {
    let mut hits: Vec<String> = Vec::new();
    incoming_walk(root, root, &mut hits);
    format!("{{\"entries\":[{}]}}", hits.join(","))
}

fn incoming_walk(root: &Path, dir: &Path, hits: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        let name = entry.file_name().to_string_lossy().into_owned();
        if meta.is_dir() {
            incoming_walk(root, &entry.path(), hits);
        } else if let Some(real) = name.strip_prefix(PART_PREFIX) {
            let rel = dir
                .strip_prefix(root)
                .ok()
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default();
            hits.push(format!(
                "{{\"path\":{},\"name\":{},\"done\":{}}}",
                json_string(&rel),
                json_string(real),
                meta.len()
            ));
        }
    }
}

// ---- Response helpers ----

fn respond<R: io::Read>(request: Request, response: Response<R>) -> Result<()> {
    request.respond(response).ok();
    Ok(())
}

fn html_response(body: &str) -> Response<io::Cursor<Vec<u8>>> {
    Response::from_string(body).with_header(header("Content-Type", "text/html; charset=utf-8"))
}

fn json_response(body: &str) -> Response<io::Cursor<Vec<u8>>> {
    Response::from_string(body).with_header(header("Content-Type", "application/json"))
}

fn header(name: &str, value: &str) -> Header {
    Header::from_bytes(name.as_bytes(), value.as_bytes())
        .expect("static header name/value are valid")
}

/// Read a request header value by name (case-insensitive), if present.
fn header_value<'a>(request: &'a Request, name: &str) -> Option<&'a str> {
    request
        .headers()
        .iter()
        .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(name))
        .map(|h| h.value.as_str())
}

/// Prefix of the temp file used for in-progress resumable uploads. Hidden from
/// directory listings and search so half-done transfers never show up.
const PART_PREFIX: &str = ".zap-part-";

// ---- Authentication (custom login page + session cookie) ----

/// Runtime auth state: the expected credentials plus an unguessable session
/// token minted at startup. A client only receives the token (as a cookie)
/// after posting the correct credentials to `/login`.
struct Auth {
    user: String,
    pass: String,
    token: String,
}

const SESSION_COOKIE: &str = "zap_session";

/// The `Set-Cookie` value that grants a session for `token` (shared by the login
/// form and the `?k=` pairing-key auto-login).
fn session_cookie(token: &str) -> String {
    format!("{SESSION_COOKIE}={token}; Path=/; SameSite=Strict; Max-Age=86400")
}

/// Build the auth state, generating a fresh session token per run.
fn build_auth(creds: Option<&Credentials>) -> Option<Auth> {
    creds.map(|c| {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        Auth {
            user: c.user.clone(),
            pass: c.pass.clone(),
            token: format!("{nanos:032x}"),
        }
    })
}

/// Handle a login form POST (`user=..&pass=..`). On success, set the session
/// cookie; on failure, 401.
fn handle_login(mut request: Request, auth: &Auth) -> Result<()> {
    let mut body = String::new();
    request.as_reader().read_to_string(&mut body).ok();

    let user = query_param(Some(&body), "user").map(decode_percent);
    let pass = query_param(Some(&body), "pass").map(decode_percent);

    if user.as_deref() == Some(auth.user.as_str()) && pass.as_deref() == Some(auth.pass.as_str()) {
        let cookie = session_cookie(&auth.token);
        respond(
            request,
            Response::from_string("ok").with_header(header("Set-Cookie", &cookie)),
        )
    } else {
        respond(
            request,
            Response::from_string("Incorrect username or password").with_status_code(401),
        )
    }
}

/// True if the request carries a `zap_session` cookie matching the token.
fn has_valid_session(request: &Request, auth: &Auth) -> bool {
    request
        .headers()
        .iter()
        .find(|h| h.field.equiv("Cookie"))
        .and_then(|h| cookie_value(h.value.as_str(), SESSION_COOKIE))
        .map(|v| v == auth.token)
        .unwrap_or(false)
}

/// Pull a single cookie value out of a `Cookie:` header.
fn cookie_value<'a>(cookies: &'a str, name: &str) -> Option<&'a str> {
    cookies.split(';').find_map(|pair| {
        let (k, v) = pair.trim().split_once('=')?;
        (k == name).then_some(v)
    })
}

// ---- URL / path utilities ----

fn split_query(url: &str) -> (String, Option<&str>) {
    match url.split_once('?') {
        Some((path, query)) => (path.to_string(), Some(query)),
        None => (url.to_string(), None),
    }
}

fn query_param<'a>(query: Option<&'a str>, key: &str) -> Option<&'a str> {
    query?.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key).then_some(v)
    })
}

/// Minimal percent-decoding (also turns '+' into space, matching form encoding).
fn decode_percent(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                match u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    Ok(b) => {
                        out.push(b);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Resolve a client-supplied relative path against `root`, refusing anything
/// that would escape it. Because no `..` segment is ever accepted, the result
/// is always inside `root`. Empty / `.` segments are skipped, so `""` maps to
/// `root` itself.
fn resolve_within(root: &Path, rel: &str) -> Option<PathBuf> {
    let mut path = root.to_path_buf();
    for seg in rel.split('/') {
        if seg.is_empty() || seg == "." {
            continue;
        }
        if seg == ".." || seg.contains('\\') {
            return None;
        }
        path.push(seg);
    }
    Some(path)
}

/// True if `name` is a single path component safe to create inside a folder.
fn is_plain_filename(name: &str) -> bool {
    !name.is_empty()
        && !name.contains('/')
        && !name.contains('\\')
        && name != "."
        && name != ".."
}

/// JSON-encode a string (quotes + escapes). Enough for filenames.
fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Determine this machine's LAN IP by asking the OS which local address it
/// would use to reach an external host. No packets are actually sent.
pub fn lan_ip() -> Option<IpAddr> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    socket.local_addr().ok().map(|addr| addr.ip())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;
    use std::sync::MutexGuard;

    /// Serializes tests that bind a server. They pick an ephemeral port via
    /// [`free_port`]; run in parallel, two could grab the same freed port and
    /// race on bind. Holding this for the duration of each such test removes the
    /// race without forcing `--test-threads=1` globally.
    static PORT_GUARD: Mutex<()> = Mutex::new(());
    fn port_guard() -> MutexGuard<'static, ()> {
        PORT_GUARD.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Ask the OS for an unused TCP port, then release it for the caller to reuse.
    fn free_port() -> u16 {
        TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    /// A stop→start on the same port in quick succession must not fail with
    /// `EADDRINUSE`. Guards the single-acceptor clean shutdown *and* SO_REUSEADDR.
    #[test]
    fn restart_same_port_does_not_hit_eaddrinuse() {
        let _g = port_guard();
        let dir = std::env::temp_dir().join(format!("zap-restart-test-{}", std::process::id()));
        let port = free_port();
        let make_config = || ServeConfig {
            dir: dir.clone(),
            port,
            bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
            auth: None,
            history: None,
        };

        let (_info, handle) = spawn(make_config()).expect("first bind should succeed");
        handle.stop();

        // Immediately rebind the same port; without a clean shutdown + reuse this
        // would return "address already in use".
        let (_info2, handle2) = spawn(make_config()).expect("rebind on same port should succeed");
        handle2.stop();

        let _ = fs::remove_dir_all(&dir);
    }

    /// A client request must bump `requests_seen()` - the signal the front-ends
    /// use to decide whether any device has reached the host (AP-isolation hint).
    #[test]
    fn requests_seen_counts_client_requests() {
        use std::io::Write as _;
        use std::net::TcpStream;

        let _g = port_guard();
        let dir = std::env::temp_dir().join(format!("zap-req-test-{}", std::process::id()));
        let port = free_port();
        let (_info, handle) = spawn(ServeConfig {
            dir: dir.clone(),
            port,
            bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
            auth: None,
            history: None,
        })
        .expect("bind");

        assert_eq!(handle.requests_seen(), 0, "no requests before any client");

        let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        stream
            .write_all(b"GET /api/list?path= HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
            .expect("send request");
        let mut resp = Vec::new();
        let _ = stream.read_to_end(&mut resp);

        assert!(handle.requests_seen() >= 1, "a client request should be counted");

        handle.stop();
        let _ = fs::remove_dir_all(&dir);
    }

    // ---- resumable upload / verify / range download ----

    /// Send a raw HTTP request and return the full response (small bodies only).
    /// `Connection: close` makes the server close so `read_to_end` returns.
    fn send_raw(port: u16, head: &str, body: &[u8]) -> Vec<u8> {
        use std::net::TcpStream;
        let mut s = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        s.write_all(head.as_bytes()).expect("write head");
        if !body.is_empty() {
            s.write_all(body).expect("write body");
        }
        let mut resp = Vec::new();
        s.read_to_end(&mut resp).expect("read response");
        resp
    }

    fn spawn_test_server(dir: &Path) -> (u16, ServerHandle) {
        let port = free_port();
        let (_info, handle) = spawn(ServeConfig {
            dir: dir.to_path_buf(),
            port,
            bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
            auth: None,
            history: None,
        })
        .expect("bind");
        (port, handle)
    }

    /// Both a received (upload) and a served (download) transfer must expose a
    /// real, existing `path` so "Open location" can show for each.
    #[test]
    fn open_location_path_set_for_upload_and_download() {
        let _g = port_guard();
        let dir = std::env::temp_dir().join(format!("zap-openloc-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("existing.bin"), b"already here").unwrap();
        let (port, handle) = spawn_test_server(&dir);

        // Upload (received by host).
        let body = b"ABCDEFGHIJ";
        let crc_src = dir.join("crc-src");
        fs::write(&crc_src, body).unwrap();
        let crc = crc32_file(&crc_src).unwrap();
        let head = format!(
            "PUT /upload?path=&name=recv.bin&offset=0 HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\
             X-Zap-Total: 10\r\nX-Zap-Crc32: {crc:08x}\r\nContent-Length: 10\r\n\r\n"
        );
        let _ = send_raw(port, &head, body);

        // Download (served out by host).
        let _ = send_raw(
            port,
            "GET /download?path=existing.bin HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
            b"",
        );

        let list = handle.transfers();
        let up = list.iter().find(|t| t.name == "recv.bin").expect("upload transfer present");
        let down = list.iter().find(|t| t.name == "existing.bin").expect("download transfer present");
        for (label, t) in [("upload", up), ("download", down)] {
            assert!(t.finished && t.ok, "{label} should be finished+ok");
            assert!(!t.path.is_empty(), "{label} must carry a path for Open location");
            assert!(Path::new(&t.path).exists(), "{label} path must exist: {}", t.path);
        }

        handle.stop();
        let _ = fs::remove_dir_all(&dir);
    }

    /// A finished upload's persisted history row must carry its file path (last
    /// TSV field), so "Open location" works after a restart - same as downloads.
    #[test]
    fn history_saves_upload_path() {
        let _g = port_guard();
        let dir = std::env::temp_dir().join(format!("zap-histpath-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let histfile = dir.join("transfers.tsv");
        let port = free_port();
        let (_info, handle) = spawn(ServeConfig {
            dir: dir.clone(),
            port,
            bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
            auth: None,
            history: Some(histfile.clone()),
        })
        .expect("bind");

        let body = b"ABCDEFGHIJ";
        let crc_src = dir.join("crc-src");
        fs::write(&crc_src, body).unwrap();
        let crc = crc32_file(&crc_src).unwrap();
        let head = format!(
            "PUT /upload?path=&name=up.bin&offset=0 HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\
             X-Zap-Total: 10\r\nX-Zap-Crc32: {crc:08x}\r\nContent-Length: 10\r\n\r\n"
        );
        let _ = send_raw(port, &head, body);
        // Give the finalize's save_history a moment to hit disk.
        let tsv = fs::read_to_string(&histfile).unwrap_or_default();
        let up_line = tsv.lines().find(|l| l.starts_with("up\t")).expect("upload row in history");
        let fields: Vec<&str> = up_line.split('\t').collect();
        assert_eq!(fields.len(), 7, "row has all 7 fields incl. path: {up_line:?}");
        assert!(!fields[6].is_empty(), "upload history row must carry a path: {up_line:?}");
        assert!(fields[6].ends_with("up.bin"), "path points at the file: {}", fields[6]);

        handle.stop();
        let _ = fs::remove_dir_all(&dir);
    }

    /// DELETE /upload removes an interrupted upload's `.zap-part-` temp file.
    #[test]
    fn discard_removes_partial() {
        let _g = port_guard();
        let dir = std::env::temp_dir().join(format!("zap-discard-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let (port, handle) = spawn_test_server(&dir);

        // Leave a partial behind (interrupted upload).
        let head = "PUT /upload?path=&name=big.bin&offset=0 HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\
             X-Zap-Total: 100\r\nContent-Length: 5\r\n\r\n";
        let _ = send_raw(port, head, b"abcde");
        assert!(dir.join(".zap-part-big.bin").exists(), "partial should exist after interrupted PUT");

        let resp = send_raw(
            port,
            "DELETE /upload?path=&name=big.bin HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
            b"",
        );
        let r = String::from_utf8_lossy(&resp);
        assert!(r.contains("200"), "discard should succeed: {r}");
        assert!(!dir.join(".zap-part-big.bin").exists(), "partial should be gone after discard");

        handle.stop();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn crc32_matches_known_check_value() {
        let dir = std::env::temp_dir().join(format!("zap-crc-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("check");
        fs::write(&p, b"123456789").unwrap();
        // The canonical CRC-32/ISO-HDLC check value for "123456789".
        assert_eq!(crc32_file(&p).unwrap(), 0xCBF4_3926);
        let _ = fs::remove_dir_all(&dir);
    }

    /// Full resume round-trip: HEAD → partial PUT (drop) → HEAD sees the new
    /// offset → PUT the remainder with a crc → server verifies and finalizes.
    #[test]
    fn resumable_upload_resumes_and_verifies() {
        let _g = port_guard();
        let dir = std::env::temp_dir().join(format!("zap-resume-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let (port, handle) = spawn_test_server(&dir);

        let full = b"ABCDEFGHIJ"; // 10 bytes
        let total = full.len();

        // Compute the whole-file crc the client would send.
        let crc_src = dir.join("crc-src");
        fs::write(&crc_src, full).unwrap();
        let crc = crc32_file(&crc_src).unwrap();

        // HEAD → nothing yet.
        let r = String::from_utf8_lossy(&send_raw(
            port,
            "HEAD /upload?path=&name=foo.bin HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
            b"",
        ))
        .to_lowercase();
        assert!(r.contains("x-zap-offset: 0"), "fresh HEAD should report offset 0: {r}");

        // First PUT: only the first 4 bytes, then "drop" (total not reached).
        let head = format!(
            "PUT /upload?path=&name=foo.bin&offset=0 HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\
             X-Zap-Total: {total}\r\nContent-Length: 4\r\n\r\n"
        );
        let r = String::from_utf8_lossy(&send_raw(port, &head, &full[..4])).to_lowercase();
        assert!(r.contains("x-zap-offset: 4"), "partial PUT should report offset 4: {r}");

        // HEAD again → resume point advanced.
        let r = String::from_utf8_lossy(&send_raw(
            port,
            "HEAD /upload?path=&name=foo.bin HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
            b"",
        ))
        .to_lowercase();
        assert!(r.contains("x-zap-offset: 4"), "HEAD should see the 4 bytes on disk: {r}");

        // Second PUT: the remaining 6 bytes from offset 4, with the crc → verify.
        let head = format!(
            "PUT /upload?path=&name=foo.bin&offset=4 HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\
             X-Zap-Total: {total}\r\nX-Zap-Crc32: {crc:08x}\r\nContent-Length: 6\r\n\r\n"
        );
        let r = String::from_utf8_lossy(&send_raw(port, &head, &full[4..])).to_lowercase();
        assert!(r.contains("x-zap-verified: true"), "final PUT should verify: {r}");

        // The assembled file exists, byte-exact; the temp file is gone.
        assert_eq!(fs::read(dir.join("foo.bin")).unwrap(), full);
        assert!(!dir.join(".zap-part-foo.bin").exists(), "temp file should be renamed away");

        handle.stop();
        let _ = fs::remove_dir_all(&dir);
    }

    /// Every chunk (and resume) of one file must share a single transfer row,
    /// not spawn a new one per PUT.
    #[test]
    fn resumable_upload_coalesces_chunks_into_one_transfer() {
        let _g = port_guard();
        let dir = std::env::temp_dir().join(format!("zap-coalesce-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let (port, handle) = spawn_test_server(&dir);

        let full = b"ABCDEFGHIJ"; // 10 bytes, sent as three chunks
        let crc_src = dir.join("crc-src");
        fs::write(&crc_src, full).unwrap();
        let crc = crc32_file(&crc_src).unwrap();

        let put = |off: usize, len: usize, last: bool| {
            let crc_hdr = if last { format!("X-Zap-Crc32: {crc:08x}\r\n") } else { String::new() };
            let head = format!(
                "PUT /upload?path=&name=f.bin&offset={off} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\
                 X-Zap-Total: 10\r\n{crc_hdr}Content-Length: {len}\r\n\r\n"
            );
            send_raw(port, &head, &full[off..off + len]);
        };

        put(0, 4, false);
        assert_eq!(handle.transfers().len(), 1, "first chunk starts one transfer");
        put(4, 4, false);
        assert_eq!(handle.transfers().len(), 1, "second chunk must reuse the same row");
        put(8, 2, true);
        let list = handle.transfers();
        assert_eq!(list.len(), 1, "still one row after finalize");
        assert!(list[0].finished && list[0].ok && list[0].verified, "final row verified");
        assert_eq!(fs::read(dir.join("f.bin")).unwrap(), full);

        handle.stop();
        let _ = fs::remove_dir_all(&dir);
    }

    /// The generated ZIP must be well-formed: local-file magic at the start, the
    /// end-of-central-directory record at the end, the right entry count, and all
    /// file names (with their subfolder prefixes) present.
    #[test]
    fn folder_zip_is_well_formed() {
        let dir = std::env::temp_dir().join(format!("zap-zip-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("a.txt"), b"hello world").unwrap();
        fs::write(dir.join("sub/b.bin"), vec![7u8; 5000]).unwrap();

        let mut files = Vec::new();
        collect_files(&dir, "root/", &mut files);
        assert_eq!(files.len(), 2);

        let mut out = Vec::new();
        write_zip(&mut out, &files).unwrap();

        assert_eq!(&out[0..4], &[0x50, 0x4b, 0x03, 0x04], "starts with local file header");
        // EOCD is the trailing 22 bytes (no archive comment).
        let eocd = &out[out.len() - 22..];
        assert_eq!(&eocd[0..4], &[0x50, 0x4b, 0x05, 0x06], "ends with EOCD");
        let entries = u16::from_le_bytes([eocd[10], eocd[11]]);
        assert_eq!(entries, 2, "EOCD reports two entries");

        let contains = |needle: &[u8]| out.windows(needle.len()).any(|w| w == needle);
        assert!(contains(b"root/a.txt"), "names the top-level file");
        assert!(contains(b"root/sub/b.bin"), "names the nested file with its subfolder");

        let _ = fs::remove_dir_all(&dir);
    }

    /// A completed transfer must be persisted and reappear after a stop/start.
    #[test]
    fn transfer_history_survives_restart() {
        let _g = port_guard();
        let dir = std::env::temp_dir().join(format!("zap-hist-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let hist = dir.join("history.tsv");
        let port = free_port();
        let cfg = || ServeConfig {
            dir: dir.clone(),
            port,
            bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
            auth: None,
            history: Some(hist.clone()),
        };

        let (_i, h) = spawn(cfg()).expect("bind");
        let full = b"hello";
        let crc_src = dir.join("crc-src");
        fs::write(&crc_src, full).unwrap();
        let crc = crc32_file(&crc_src).unwrap();
        let head = format!(
            "PUT /upload?path=&name=h.bin&offset=0 HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\
             X-Zap-Total: 5\r\nX-Zap-Crc32: {crc:08x}\r\nContent-Length: 5\r\n\r\n"
        );
        send_raw(port, &head, full);
        h.stop();

        // Restart with the same history file - the record must be reloaded.
        let (_i2, h2) = spawn(cfg()).expect("rebind");
        let list = h2.transfers();
        assert!(
            list.iter().any(|t| t.name == "h.bin" && t.finished && t.ok && t.verified),
            "completed transfer should survive a restart: {list:?}"
        );
        h2.stop();
        let _ = fs::remove_dir_all(&dir);
    }

    /// A wrong offset must be rejected with 409 + the real offset, not corrupt
    /// the file.
    #[test]
    fn resumable_upload_rejects_offset_mismatch() {
        let _g = port_guard();
        let dir = std::env::temp_dir().join(format!("zap-offset-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let (port, handle) = spawn_test_server(&dir);

        // Nothing uploaded, but client claims offset 5 → 409 with real offset 0.
        let head = "PUT /upload?path=&name=x.bin&offset=5 HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\
             X-Zap-Total: 3\r\nContent-Length: 3\r\n\r\n";
        let r = String::from_utf8_lossy(&send_raw(port, head, b"abc")).to_lowercase();
        assert!(r.contains("409"), "offset mismatch should be 409: {r}");
        assert!(r.contains("x-zap-offset: 0"), "should report the real offset: {r}");

        handle.stop();
        let _ = fs::remove_dir_all(&dir);
    }

    /// A `?k=<token>` URL must auto-authenticate (303 + session cookie); the
    /// resulting cookie must unlock the SPA; a wrong key must not.
    #[test]
    fn pairing_key_auto_logins() {
        let _g = port_guard();
        let dir = std::env::temp_dir().join(format!("zap-pair-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let port = free_port();
        let (info, handle) = spawn(ServeConfig {
            dir: dir.clone(),
            port,
            bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
            auth: Some(Credentials { user: "zap".into(), pass: "zap".into() }),
            history: None,
        })
        .expect("bind");
        let token = info.auth_token.clone().expect("secured server exposes a token");

        let get = |extra: &str| {
            String::from_utf8_lossy(&send_raw(
                port,
                &format!("GET /{extra} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"),
                b"",
            ))
            .to_lowercase()
        };

        // No key, no cookie → the login page, not the app.
        assert!(!get("").contains("tap to choose files"), "gated without auth");

        // Correct key → 303 + Set-Cookie with the session token.
        let keyed = get(&format!("?k={token}"));
        assert!(keyed.contains("303"), "pairing key should redirect: {keyed}");
        assert!(keyed.contains(&format!("set-cookie: zap_session={token}")), "sets session cookie: {keyed}");

        // That cookie unlocks the app.
        let with_cookie = String::from_utf8_lossy(&send_raw(
            port,
            &format!("GET / HTTP/1.1\r\nHost: x\r\nCookie: zap_session={token}\r\nConnection: close\r\n\r\n"),
            b"",
        ))
        .to_lowercase();
        assert!(with_cookie.contains("tap to choose files"), "cookie unlocks the app");

        // Wrong key → still gated.
        assert!(!get("?k=deadbeef").contains("set-cookie: zap_session"), "wrong key grants nothing");

        handle.stop();
        let _ = fs::remove_dir_all(&dir);
    }

    /// `Range: bytes=2-5` must return 206 with just that slice and a
    /// `Content-Range` header.
    #[test]
    fn download_supports_range() {
        let _g = port_guard();
        let dir = std::env::temp_dir().join(format!("zap-range-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("data.txt"), b"0123456789").unwrap();
        let (port, handle) = spawn_test_server(&dir);

        let resp = send_raw(
            port,
            "GET /download?path=data.txt HTTP/1.1\r\nHost: x\r\nRange: bytes=2-5\r\nConnection: close\r\n\r\n",
            b"",
        );
        let text = String::from_utf8_lossy(&resp);
        let lower = text.to_lowercase();
        assert!(lower.contains("206"), "range request should be 206: {text}");
        assert!(lower.contains("content-range: bytes 2-5/10"), "should set Content-Range: {text}");
        // Body is the last thing after the header block.
        assert!(text.ends_with("2345"), "body should be the requested slice: {text:?}");

        handle.stop();
        let _ = fs::remove_dir_all(&dir);
    }
}
