# Zulu - build brief (hand this to a build agent)

> Self-contained. You should be able to start building Zulu from **this file
> alone** + the Zap source it points to. Zulu is app #2 in the zOrigin family
> (after Zap). It reuses Zap's core heavily - the whole point is to NOT reinvent
> pairing / LAN transport / the no-app browser client.
>
> Deeper context if you want it: `docs/apps/zulu.md` (original spec),
> `docs/apps/README.md` (family vision + reuse map), `docs/HANDOFF.md` (full Zap
> technical handoff). But everything essential is inlined below.

---

## 1. What Zulu is

**Zulu = continuous clipboard / link / snippet sync across your paired devices.**
Copy on one device, it's instantly available on the others. "Zulu time" =
everything stays in sync.

- Syncs **text, links, code snippets, and small (size-capped) images**.
- Plus a short **clipboard history** and **pinned snippets**.
- **Not** file transfer (that's Zap - link to it). Zulu is "my clipboard follows me".
- Same promise as the whole family: **no cloud, no accounts, data stays on the LAN,
  explicit pairing (QR / URL / one-time token) - never mDNS / BLE / multicast
  discovery** (home routers quietly break discovery; that's the differentiator).

---

## 2. The Zap core you reuse (the important part)

Zap's engine lives in **`networking/crates/znet-core/src/web/mod.rs`** (presentation-free Rust;
deps are tiny: `tiny_http` + `anyhow` + `socket2` only). One device runs an HTTP
server bound to `0.0.0.0:<port>`; other devices connect from **any browser** (or a
native app). **Reuse this crate - do not rebuild the transport or pairing.**

### The idea in one line
Explicit-URL HTTP server on the LAN + a browser/native client, with QR/token
pairing and a session cookie. No discovery layer, so it works when Quick
Share / LocalSend can't.

### Public API you'll call (from `znet-core::web`)
- `struct ServeConfig { dir, port, bind, auth: Option<Credentials>, history: Option<PathBuf> }`
- `fn serve(config, on_ready: impl FnOnce(&ServerInfo)) -> Result<()>` - blocks; the
  caller presents connection info in `on_ready` (core prints nothing).
- `fn spawn(config) -> Result<(ServerInfo, ServerHandle)>` - non-blocking; embedders
  (Android foreground service, desktop GUI) use this. `ServerHandle::stop()` frees
  the port cleanly.
- `struct ServerInfo { dir, port, lan_ip }` with `.url()` and **`.url_with_key()`**
  (appends `?k=<token>` - the QR/share link).
- `pub fn lan_ip() -> Option<IpAddr>` - best-guess LAN IP (UDP-connect trick, no
  packets sent).

### Pairing (reuse verbatim - this is the family primitive)
- Auth = **session cookie**, not HTTP Basic (no ugly browser popup). Custom
  `login.html`. Token minted per run.
- The QR / share link carries **`?k=<token>`**. A request with a matching `?k=` is
  **auto-authenticated**: the server sets the cookie and 303-redirects to the clean
  path. So scanning a secured server's QR skips password typing entirely.

### Transport patterns worth copying
- **Single acceptor thread** + one thread per request; `Server::unblock()` = clean
  shutdown. Listener built with **`SO_REUSEADDR`** (via `socket2`) so quick
  restarts never hit `EADDRINUSE`.
- **Path safety**: `resolve_within(root, rel)` (rejects `..`), `is_plain_filename`.
- **No-app browser client**: `networking/crates/znet-core/src/web/index.html` is a single-file
  SPA (`include_str!`) - dark theme, live progress, listing, search. Copy its
  patterns for Zulu's receiver page.

### Live push / presence - BUILT in the core (2026-07-19)
**Server-Sent Events (SSE).** Zap is request/response; Zulu needs the host to
*push* a new clip to every connected device the moment it's copied. This primitive
now exists in the shared core - use it, don't rebuild it:

- **`GET /events`** - the client opens it and holds it; the host streams
  `data: <json>` frames. Behind the same session/`?k=` pairing gate as everything
  else. Served by `web::serve_events`, which flushes each frame immediately (plain
  `Request::respond` buffers, so it takes the raw writer and flushes per frame).
- **`web::EventHub`** (`web/events.rs`) - clone-cheap handle over the connected
  clients. `hub.broadcast(&Event::named("clip", json))` fans a frame out to all.
  Get the server's hub via **`ServerHandle::events()`** and broadcast a clip from
  wherever your app captures one.
- **Presence is automatic**: connect/disconnect broadcasts an `event: presence`
  frame with `{"count":N}`; `EventHub::client_count()` reads it directly.
- **`web::Event`** encodes the SSE wire format (multi-line `data:`, optional
  `event:`/`id:`). A 15s heartbeat comment keeps NAT/proxies from dropping idle
  connections and lets the server notice a departed client (drop -> presence
  cleanup). Reconnects automatically in browsers via `EventSource`.
- This is the **presence/push primitive the whole family reuses** (Zap's future
  "trusted devices / presence" feature too). No new dependency - plain HTTP over
  the existing `tiny_http` server. Covered by unit + end-to-end socket tests in
  `znet-core` (`cargo test -p znet-core --lib`).

---

## 3. The hard constraint - read before designing (mobile clipboard OS limits)

This is make-or-break. **Design around it; don't pretend it isn't there.**

- **Reading the clipboard in the background is blocked** on modern mobile: Android
  10+ only lets the **foreground** app read the clipboard; iOS is stricter. So
  "auto-capture whatever you copy" is **impossible in the background on phones**.
- **Writing the clipboard from a web page needs a user gesture** (Clipboard API
  requires user activation) → on the web receiver it's **tap-to-copy**, not silent.

Resulting honest design:

| Direction | Desktop (native app) | Mobile (Android / iOS) |
| --- | --- | --- |
| **Send** (copy → push) | Auto: native app watches clipboard, pushes | Assisted: **Share-sheet → "Send to Zulu"** or an in-app / quick-tile "push clipboard" button (foreground) |
| **Receive** (push → paste) | Auto: native app writes the OS clipboard | **Tap-to-copy** in the open Zulu page/app (Android may allow foreground auto-write) |

Net: **desktop ↔ desktop is truly seamless; mobile is assisted.** That's fine - say
so plainly. It still beats every "just works" tool that silently fails. **Never
overclaim** - the audience includes networking pros.

---

## 4. Architecture

- **Transport & pairing**: reuse `znet-core::web` - one device hosts, others connect
  (native app or a browser tab). Same QR / URL / `?k=` token pairing.
- **Live delivery**: the new **SSE** primitive (§2) - host → every connected device.
- **State**: an in-memory ring of recent clips on the host + optional small on-disk
  history (cap the count; downscale/size-limit images). Reuse Zap's `history`
  (TSV) idea if useful.
- **Receiver**: a foreground browser tab (mobile) or the native app (desktop),
  listening on SSE, rendering the clip list with one-tap copy.

---

## 5. Features (prioritized - build in this order)

1. **Text + link sync** - the core loop (copy on A → appears on B).
2. **Clipboard history** - last N items, searchable.
3. **One-tap copy / Share-sheet send** - the mobile-safe path.
4. **Small image sync** - size-capped, downscaled.
5. **Pinned snippets** - frequently-pasted text.
6. **End-to-end encryption** (later; native↔native first - mirrors Zap's planned TLS).

## 6. Platforms

- **Desktop (macOS / Windows / Linux)** - egui shell like `zap-desktop`; full auto sync.
- **Android** - Kotlin shell like the Zap app; **assisted** (share-target + tap-to-copy).
- **iOS** - later; very limited (document the gap honestly).

## 7. Design / visual language

Reuse the family look: dark base, cards, the concentric "origin" mark / bolt-adjacent
identity. Give Zulu its own **accent tint** (the landing site uses a muted blue,
`#6f8fc4`, for Zulu) but keep it recognizably part of the set. Reuse
`zap-desktop`'s `tune_theme` (light/dark) and the **`ZAP_SHOT`-style headless
screenshot harness** for GUI verification. Brand name Title-case ("Zulu");
identifiers lowercase.

## 8. Non-goals

No cloud, no accounts, no background-clipboard hacks that fight the OS, no file
transfer (that's Zap - link to it).

## 9. First milestone - DONE (2026-07-19)

**Desktop ↔ desktop text/link sync over the LAN via SSE + pairing:** copy on A →
appears on B, auto-pasted. ✅ **Shipped and verified.**

- **`zulu-desktop`** (`networking/crates/zulu-desktop`) - egui shell (Zulu-blue
  theme, `ZULU_SHOT` screenshot harness mirroring Zap's). Two modes: **Host**
  (runs `web::spawn`, shows URL + QR) and **Join** (paste the host URL). Both run
  the same `sync.rs` engine.
- **`sync.rs`** - talks to the host's `znet-core` server over plain HTTP (std
  only, no HTTP dep). A **receiver** thread holds `GET /events` open and writes
  incoming `clip` frames to the OS clipboard (`arboard`); a **sender** thread
  polls the clipboard and `POST`s changes to `/clip`. A content-based guard
  breaks the echo loop. Presence count comes from the `presence` frames.
- **Core additions:** `POST /clip` (store + broadcast) and `GET /clips`
  (backfill) in `znet-core::web`, on top of the SSE `EventHub`.
- **Verified end-to-end on the real macOS clipboard:** a remote `POST /clip`
  landed in `pbpaste` (receive), and a local `pbcopy` was pushed to the host and
  showed in `GET /clips` (send). Plus 25 core tests + the app's unit tests, all
  green.

### Shipped since the milestone (2026-07-19)

- **History backfill on connect** - `/events` replays the recent clips (oldest
  first) to a freshly-connected device as its first frames, so a late joiner
  lands in sync with no separate fetch and no client-side JSON parsing
  (`EventHub::subscribe_with_backfill`). The desktop app id-dedups so a
  reconnect's replay isn't re-applied.
- **No-app web receiver** - `ServeConfig.index_html` lets `znet-core` serve an
  app-supplied SPA at `/`; Zulu ships `zulu.html`: a live EventSource clip list
  (+ backfill), presence, a paste-and-send box, and tap-to-copy with an
  `execCommand` fallback for plain-http LAN (the async Clipboard API needs a
  secure context). Verified in a real browser.
- **Small images** - clips can be a `data:image/png;base64,…` URL, so images
  ride the same `/clip` + SSE path as text. The desktop sends/receives clipboard
  images (`arboard` + `image`, downscaled to ≤1600px, capped ~700 KB); the web
  receiver renders them inline. Echo is broken by a content guard plus a short
  post-apply mute (the OS can re-encode an image on the clipboard round-trip).
  Verified: a posted image rendered once in the browser, no echo.
- **Pinned snippets** - pin any recent clip; pins persist to
  `<app-data>/zulu/pins.txt` (newline-escaped) and reappear next run. Clicking a
  pin puts it back on the clipboard (and syncs it when connected).

**Next:** a **native Android app** (share-target sender + tap-to-copy) - the web
receiver already covers Android *browsers*, but a true system share-sheet needs
either TLS (for a PWA Web Share Target - service workers require a secure
context) or a Kotlin shell. Then end-to-end **encryption** (`rustls`, H1.5).

> Run it: `cargo run -p zulu-desktop` on two machines on the same Wi-Fi - one
> Host, one Join with the shown URL. Any phone/laptop **browser** can also open
> the host URL for the no-app receiver. (Milestone server is open/no-auth; the
> `?k=` pairing key and TLS are later phases.)

---

## 10. Where the code / patterns live (in this repo)

| You want... | Look at |
| --- | --- |
| Server, pairing, session auth, endpoints | `networking/crates/znet-core/src/web/mod.rs` |
| No-app browser client (SPA) patterns | `networking/crates/znet-core/src/web/index.html` |
| Desktop egui shell + `tune_theme` + `ZAP_SHOT` harness | `networking/crates/zap-desktop/src/main.rs` |
| **Zulu desktop app (shell + sync engine)** | `networking/crates/zulu-desktop/src/{main,sync}.rs` |
| **Clip publish/history + SSE** (core) | `networking/crates/znet-core/src/web/{clips,events}.rs` + `mod.rs` routes |
| Android JNI shell (NativeBridge, foreground service) | `networking/android/zap/` + `networking/crates/zap-android/src/lib.rs` |
| Build / dist (universal macOS, CI installers) | `scripts/build-dist.sh`, `.github/workflows/release.yml` |
| Landing-page pattern (static, dark, per-accent) | `site/` (see `site/zulu/index.html` for Zulu's page) |
| Full Zap technical handoff | `docs/HANDOFF.md` |
| Family vision + reuse rules + principles | `docs/apps/README.md` |

## 11. Non-negotiable principles (family)

1. **No cloud, no accounts** - everything on the LAN.
2. **No mDNS / BLE / multicast discovery** - explicit pairing only.
3. **Honesty over hype** - state OS limits plainly (esp. §3); speed = real Wi-Fi
   throughput, not magic.
4. **One job, done reliably** - reliability is the product.
5. **Open-core** - core crate + desktop/CLI can be MIT/Apache; keep any paid mobile
   app separate.
6. **Verify end-to-end** (drive the real flow across two devices), not just unit
   tests. Rebuild the installer/APK after changes so it's testable on real hardware.
