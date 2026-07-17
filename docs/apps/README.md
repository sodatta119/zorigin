# The Z-family — local-first networking utilities

> Direction for anyone (human or agent) building the next apps after **Zap**.
> Read this first, then the per-app file (`zap.md`, `zulu.md`, `zod.md`, `zeus.md`).

## Vision

A small family of **local-first, privacy-first networking utilities**, all built in
Rust, all cross-platform, each doing one job really well and **reliably**. The
brand promise is the same one Zap sells:

> "It just works when the discovery-based tools don't."

Every app: **no cloud, no accounts, data never leaves the LAN, explicit pairing
(QR / URL / token) instead of mDNS / BLE / multicast discovery.** That last point
is the whole thesis — home routers quietly break discovery (multicast snooping,
band-steering, client/AP isolation), so we never rely on it.

## The line-up

| App | Job | Status |
| --- | --- | --- |
| **Zap** | Move files & folders across devices over Wi-Fi | shipped; more features in `zap.md` |
| **Zulu** | Continuous clipboard / link / snippet **sync** across devices | spec: `zulu.md` |
| **Zod** | LAN **recon** — scan & inspect devices, ping, ports, speed | spec: `zod.md` |
| **Zeus** | **Wake / power-control** your PCs from your phone (WoL + SSH) | spec: `zeus.md` |

Names are thematic: **Zap** (the bolt), **Zulu** ("Zulu time" = synchronized),
**Zod** (surveils the whole network), **Zeus** (throws the bolt that wakes a
machine). All start with **Z** by design.

## Build order (recommended)

1. **Zap** — finish the fold-in features (`zap.md`).
2. **Zulu** — highest daily-use, reuses Zap's core most.
3. **Zeus** — small, reliable, ~weekend; quick credibility win.
4. **Zod** — diagnostics; different core, do when the mood fits.

## Shared core (do this as the family grows)

Extract Zap's reusable networking into an open-source crate — working name
**`zero`** (zero-config LAN core) or `znet`:

- **Pairing**: QR / URL / one-time token (see Zap's `ServerInfo::url_with_key()`
  and the `?k=<token>` auto-login in `crates/zap-core/src/web/mod.rs`).
- **LAN HTTP transport** + the **no-app browser client** pattern.
- **Presence** (which paired devices are currently connected — needs SSE/long-poll,
  **not yet in Zap**; add it to the core when building Zulu).
- **Resumable, crc32-verified transfer** (Zap's `.zap-part-` + `HEAD` offset +
  `X-Zap-Crc32` protocol).

Zap and Zulu reuse this heavily; Zod and Zeus barely touch it (they're a separate
"diagnostics/control" family). Don't force everything through one core.

## Reuse Zap's design & code — everywhere it fits

**Do not reinvent what Zap already solved.** When building any Z-app, refer to:

- **Core server / protocol** — `crates/zap-core/src/web/mod.rs` (serve/spawn,
  `ServerHandle`, auth/session cookie, pairing key, resumable upload, `Range`
  download, folder zip, transfer stats + history).
- **Web client** — `crates/zap-core/src/web/index.html` (SPA patterns: upload with
  progress/pause/resume, listing, search, dark theme).
- **Desktop shell** — `crates/zap-desktop/src/main.rs` (egui, `tune_theme` light/dark,
  `ZAP_SHOT` headless screenshot harness for verification).
- **Android shell** — `android/` + `crates/zap-android` (JNI `NativeBridge`,
  foreground `ZapService`, `MainActivity`; MIUI gotchas in `docs/HANDOFF.md` §7).
- **Visual language** — amber accent (`#f5a623` / deeper `#D98A1E`), dark base
  `#0D0D0F`, the bolt mark, cards, `site/favicon.svg`. Each app may pick its own
  accent variant but keep the family look. Brand names are Title-case ("Zap",
  "Zulu"); identifiers stay lowercase.
- **Distribution** — `scripts/build-dist.sh` (universal macOS via lipo),
  `.github/workflows/release.yml` (tag `v*` → macOS/Windows/Linux installers),
  the gradle APK flow, and the **landing-page pattern** in `site/` (static, dark,
  amber, per-platform download buttons, Cloudflare Pages, git-connected).
- **Full technical handoff** — `docs/HANDOFF.md` (local-only), plus `docs/roadmap.md`
  and `docs/backlog.md` for how Zap itself is planned.

## Non-negotiable principles

1. **No cloud, no accounts.** Everything stays on the LAN.
2. **No reliance on mDNS / BLE / multicast discovery.** Explicit pairing only.
3. **Honesty over hype** (the audience includes networking pros): speed = normal
   Wi-Fi/link throughput, not magic; "resume" = after a reconnect, not if the
   router dies; and **state OS limits plainly** (Android background restrictions,
   iOS Local-Network / no-Wi-Fi-scan). Never overclaim — it's the fastest way to
   lose credibility with this crowd.
4. **One job, done reliably.** Reliability is the product, not feature count.
5. **Open-core**: core crate + desktop/CLI can be MIT/Apache; keep the commercial
   piece (paid mobile apps) separate — see `docs/HANDOFF.md` §2.

## Verification expectation

Each app must be **driven end-to-end and observed**, not just unit-tested. Reuse
Zap's habits: core unit tests for protocol, a headless screenshot harness for the
GUI (`ZAP_SHOT`-style), and real cross-device runs. Rebuild the installer/APK after
changes so the owner can test on real hardware.
