//! Server-Sent Events (SSE) - the zOrigin family's live-delivery + presence
//! primitive.
//!
//! Zap is request/response: a client asks, the host answers. Zulu (and Zap's
//! future "trusted devices / presence" view) needs the inverse - the host must
//! *push* to every connected device the moment something changes (a new clip, a
//! device joining or leaving). This module is that push channel, built once here
//! so every Z-app reuses it instead of bolting its own onto the app layer.
//!
//! The design mirrors the existing streaming-download machinery in
//! [`super`] (a producer feeds encoded frames through a channel; a `Read`
//! implementation drains them into the HTTP response), so it needs no new
//! dependency - just `std` plus the `tiny_http` server already in use.
//!
//! # Shape
//! - [`EventHub`] - a cheap-to-clone handle over the set of connected clients.
//!   The embedder holds one (see [`super::ServerHandle::events`]) and calls
//!   [`EventHub::broadcast`] to fan an [`Event`] out to everyone listening.
//! - [`Event`] - one SSE message; [`Event::encode`] renders the wire format.
//! - [`SseReader`] - the body of a `GET /events` response. It blocks until the
//!   next frame, emits a heartbeat comment on idle so NAT/proxies never drop the
//!   connection, and - crucially - **unregisters the client on drop**, which is
//!   how presence stays honest when a browser tab closes.
//!
//! Presence is automatic: connecting or disconnecting broadcasts an
//! `event: presence` frame carrying the live client count, so every device sees
//! who is currently paired without any extra bookkeeping at the app layer.

use std::io::{self, Read};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

/// How long a listener waits for a frame before emitting a heartbeat comment.
/// A periodic write is also what lets `tiny_http` notice a vanished client (the
/// failing write drops the response, which drops the [`SseReader`], which
/// unregisters the subscriber) - so this doubles as the presence-cleanup pulse.
const HEARTBEAT: Duration = Duration::from_secs(15);

/// One Server-Sent Event.
///
/// The `data` may contain newlines; [`encode`](Self::encode) splits it into the
/// one-`data:`-line-per-newline form the SSE spec requires. An optional `event`
/// name lets the browser dispatch typed listeners
/// (`source.addEventListener("clip", ...)`); an optional `id` populates
/// `EventSource`'s `lastEventId` for reconnection.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Event {
    /// The `event:` field - a type name the client can filter on. `None` => the
    /// browser's default `message` event.
    pub event: Option<String>,
    /// The `data:` payload (typically a JSON string). May span multiple lines.
    pub data: String,
    /// The `id:` field, surfaced to the client as `lastEventId`.
    pub id: Option<String>,
}

impl Event {
    /// An unnamed event (dispatched as the default `message` type) carrying
    /// `data`.
    pub fn new(data: impl Into<String>) -> Self {
        Event { event: None, data: data.into(), id: None }
    }

    /// A named event: the client can listen for `name` specifically. Zulu uses
    /// e.g. `Event::named("clip", json)` and `"presence"` (emitted for you).
    pub fn named(name: impl Into<String>, data: impl Into<String>) -> Self {
        Event { event: Some(name.into()), data: data.into(), id: None }
    }

    /// Set the `id:` field (chainable), surfaced to the client as `lastEventId`.
    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = Some(id.into());
        self
    }

    /// Render this event as an SSE wire frame: an optional `event:`/`id:` line,
    /// one `data:` line per line of the payload, then the blank line that
    /// terminates the message. A lone `\r` (from `\r\n` payloads) is trimmed so
    /// it can't smuggle a premature record separator into the stream.
    pub fn encode(&self) -> String {
        let mut out = String::with_capacity(self.data.len() + 16);
        if let Some(name) = &self.event {
            // A name can't contain a newline without corrupting the frame.
            out.push_str("event: ");
            out.push_str(&name.replace(['\n', '\r'], " "));
            out.push('\n');
        }
        if let Some(id) = &self.id {
            out.push_str("id: ");
            out.push_str(&id.replace(['\n', '\r'], " "));
            out.push('\n');
        }
        for line in self.data.split('\n') {
            let line = line.strip_suffix('\r').unwrap_or(line);
            out.push_str("data: ");
            out.push_str(line);
            out.push('\n');
        }
        out.push('\n');
        out
    }
}

/// One connected client: the id we prune by, and the channel we push frames to.
struct Subscriber {
    id: u64,
    tx: Sender<Vec<u8>>,
}

/// Shared registry of the currently-connected clients.
#[derive(Default)]
struct Hub {
    next_id: u64,
    subs: Vec<Subscriber>,
}

/// A cheap-to-clone handle over the set of connected SSE clients. Clone it
/// freely - every clone points at the same registry. The server keeps one to
/// route `GET /events`, and the embedder (Zulu) keeps one to
/// [`broadcast`](Self::broadcast) clips.
#[derive(Clone)]
pub struct EventHub {
    inner: Arc<Mutex<Hub>>,
}

impl Default for EventHub {
    fn default() -> Self {
        Self::new()
    }
}

impl EventHub {
    /// Create an empty hub with no connected clients.
    pub fn new() -> Self {
        EventHub { inner: Arc::new(Mutex::new(Hub::default())) }
    }

    /// Register a new listener and return the [`SseReader`] that becomes the
    /// `GET /events` response body. Connecting broadcasts a `presence` frame so
    /// every device (including this one) learns the fresh client count.
    pub fn subscribe(&self) -> SseReader {
        self.subscribe_with_backfill(&[])
    }

    /// Like [`subscribe`](Self::subscribe), but first queues `initial` events
    /// for *this client only* - a private backfill (e.g. the recent clips) that
    /// a freshly-connected device receives before any live frames. The frames
    /// are enqueued before registration, so they can't interleave with a
    /// concurrent broadcast and always arrive first.
    pub fn subscribe_with_backfill(&self, initial: &[Event]) -> SseReader {
        let (tx, rx) = mpsc::channel();
        // Backfill goes only into this subscriber's own channel.
        for ev in initial {
            let _ = tx.send(ev.encode().into_bytes());
        }
        let (id, count) = {
            let mut hub = self.lock();
            let id = hub.next_id;
            hub.next_id += 1;
            hub.subs.push(Subscriber { id, tx });
            (id, hub.subs.len())
        };
        self.broadcast_presence(count);
        // The guard holds only a *weak* handle: a live reader must not keep the
        // hub (and thus its own channel sender) alive, or the stream could never
        // see the EOF that a server shutdown should produce.
        SseReader::new(rx, SubGuard { hub: Arc::downgrade(&self.inner), id })
    }

    /// Fan `event` out to every connected client. Clients whose channel has
    /// closed (tab gone but not yet pruned) are dropped in passing, so the
    /// registry self-heals even between heartbeats.
    pub fn broadcast(&self, event: &Event) {
        self.send_bytes(event.encode().into_bytes());
    }

    /// Number of clients currently connected. Reflects real open connections:
    /// an [`SseReader`] removes itself on drop.
    pub fn client_count(&self) -> usize {
        self.lock().subs.len()
    }

    /// Send pre-encoded bytes to all clients, pruning any dead channels.
    fn send_bytes(&self, bytes: Vec<u8>) {
        let mut hub = self.lock();
        hub.subs.retain(|s| s.tx.send(bytes.clone()).is_ok());
    }

    /// Remove a client by id (called from [`SubGuard`] on drop) and tell the
    /// survivors the new count.
    fn unregister(&self, id: u64) {
        let count = {
            let mut hub = self.lock();
            hub.subs.retain(|s| s.id != id);
            hub.subs.len()
        };
        self.broadcast_presence(count);
    }

    /// Broadcast the current client count as a `presence` event.
    fn broadcast_presence(&self, count: usize) {
        self.broadcast(&Event::named("presence", format!("{{\"count\":{count}}}")));
    }

    /// Lock the registry, recovering from a poisoned mutex (a panicking worker
    /// thread must not wedge the whole hub).
    fn lock(&self) -> std::sync::MutexGuard<'_, Hub> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Unregisters its subscriber from the hub when dropped. Held by [`SseReader`],
/// so the client is removed the instant `tiny_http` drops the response (client
/// disconnect or server shutdown) - this is what keeps presence honest. Holds a
/// [`Weak`] so the reader never keeps the hub alive on its own.
struct SubGuard {
    hub: Weak<Mutex<Hub>>,
    id: u64,
}

impl Drop for SubGuard {
    fn drop(&mut self) {
        // If the hub is already gone (server shut down), there is nothing to
        // unregister from and no one left to notify.
        if let Some(inner) = self.hub.upgrade() {
            EventHub { inner }.unregister(self.id);
        }
    }
}

/// The body of a `GET /events` response: a blocking [`Read`] that drains encoded
/// SSE frames from the hub. `tiny_http` reads it on the connection's worker
/// thread and writes each frame to the socket as it arrives.
///
/// On idle it returns a heartbeat comment every [`HEARTBEAT`] so the connection
/// stays warm *and* so a departed client is noticed promptly (the next write
/// fails, the response is dropped, and [`SubGuard`] unregisters). When the hub
/// is dropped (server shutdown) the channel closes and this reports EOF.
pub struct SseReader {
    rx: Receiver<Vec<u8>>,
    /// The current frame being handed out, and how far we've read into it.
    buf: Vec<u8>,
    pos: usize,
    /// Unregisters the subscriber on drop. Never read; exists for its `Drop`.
    _guard: SubGuard,
}

impl SseReader {
    fn new(rx: Receiver<Vec<u8>>, guard: SubGuard) -> Self {
        // Preamble: hint the browser's reconnect delay and confirm the stream is
        // open (a comment line - ignored by EventSource - so `onopen` fires
        // without a spurious message).
        let preamble = b"retry: 3000\n: connected\n\n".to_vec();
        SseReader { rx, buf: preamble, pos: 0, _guard: guard }
    }
}

impl Read for SseReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        loop {
            if self.pos < self.buf.len() {
                let n = (self.buf.len() - self.pos).min(out.len());
                out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
                self.pos += n;
                return Ok(n);
            }
            match self.rx.recv_timeout(HEARTBEAT) {
                Ok(frame) => {
                    self.buf = frame;
                    self.pos = 0;
                }
                // Idle: emit a comment so the write path stays exercised.
                Err(RecvTimeoutError::Timeout) => {
                    self.buf = b": ping\n\n".to_vec();
                    self.pos = 0;
                }
                // Hub dropped (server stopped) => clean end of stream.
                Err(RecvTimeoutError::Disconnected) => return Ok(0),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_named_event_with_terminator() {
        let ev = Event::named("clip", "hello");
        assert_eq!(ev.encode(), "event: clip\ndata: hello\n\n");
    }

    #[test]
    fn encodes_unnamed_event() {
        assert_eq!(Event::new("hi").encode(), "data: hi\n\n");
    }

    #[test]
    fn multiline_data_becomes_multiple_data_lines() {
        // Each line of the payload gets its own `data:` line; a `\r\n` payload's
        // stray `\r` is trimmed so it can't inject a blank-line record break.
        let ev = Event::new("line1\r\nline2");
        assert_eq!(ev.encode(), "data: line1\ndata: line2\n\n");
    }

    #[test]
    fn id_is_emitted_before_data() {
        let ev = Event::new("x").with_id("7");
        assert_eq!(ev.encode(), "id: 7\ndata: x\n\n");
    }

    #[test]
    fn broadcast_reaches_a_subscriber() {
        let hub = EventHub::new();
        let mut reader = hub.subscribe();

        // Drain the preamble + the presence frame the subscribe itself emitted.
        let _ = drain_available(&mut reader);

        hub.broadcast(&Event::named("clip", "copied text"));
        let got = read_one_frame(&mut reader);
        assert!(got.contains("event: clip"), "frame was: {got:?}");
        assert!(got.contains("data: copied text"), "frame was: {got:?}");
    }

    #[test]
    fn presence_tracks_connect_and_disconnect() {
        let hub = EventHub::new();
        assert_eq!(hub.client_count(), 0);

        let a = hub.subscribe();
        assert_eq!(hub.client_count(), 1);
        let b = hub.subscribe();
        assert_eq!(hub.client_count(), 2);

        drop(a);
        assert_eq!(hub.client_count(), 1, "dropping a reader unregisters it");
        drop(b);
        assert_eq!(hub.client_count(), 0);
    }

    #[test]
    fn subscribe_emits_a_presence_frame_with_the_count() {
        let hub = EventHub::new();
        let mut reader = hub.subscribe();
        let _preamble = read_one_frame(&mut reader); // "retry:"/": connected"
        let frame = read_one_frame(&mut reader);
        assert!(frame.contains("event: presence"), "frame was: {frame:?}");
        assert!(frame.contains("\"count\":1"), "frame was: {frame:?}");
    }

    #[test]
    fn reader_reports_eof_when_hub_is_dropped() {
        let hub = EventHub::new();
        let mut reader = hub.subscribe();
        let _ = drain_available(&mut reader);
        drop(hub); // last hub handle gone => the subscriber's Sender is dropped
        let mut buf = [0u8; 32];
        assert_eq!(reader.read(&mut buf).unwrap(), 0, "closed hub => EOF");
    }

    // ---- test helpers ----

    /// Read one non-empty chunk (a single frame; frames are sent whole).
    fn read_one_frame(reader: &mut SseReader) -> String {
        let mut buf = [0u8; 512];
        let n = reader.read(&mut buf).expect("read");
        String::from_utf8_lossy(&buf[..n]).into_owned()
    }

    /// Pull the preamble and any queued frames out of the way, so a following
    /// broadcast is read in isolation. Reads exactly the frames already queued.
    fn drain_available(reader: &mut SseReader) -> String {
        // The preamble is always present; the subscribe-time presence frame is
        // queued immediately after, so two reads clear both.
        let a = read_one_frame(reader);
        let b = read_one_frame(reader);
        format!("{a}{b}")
    }
}
