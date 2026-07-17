# START HERE — kickoff brief for a new agent conversation

> Paste-and-go context for continuing the **Zap / Z-family** project in a fresh
> chat. Read this first, then the docs it points to. Written 2026-07-17.

## 1. Orientation — read in this order

1. `docs/HANDOFF.md` — full Zap functional + technical handoff (local-only,
   gitignored; it's on disk). **The single best source of truth for how Zap works.**
2. `docs/roadmap.md` + `docs/backlog.md` — how Zap itself is planned (Horizon 0/1).
3. `docs/apps/README.md` — the **Z-family** vision, shared-core plan, design-reuse
   rules, principles, build order.
4. `docs/apps/{zap,zulu,zod,zeus}.md` — per-app specs for the next phase.
5. `git log --oneline -15` — what shipped recently.

## 2. What Zap is (one paragraph)

**Zap** = lightning-fast, private **file/folder transfer over local Wi-Fi**. One
device runs an HTTP server (Rust `zap-core`); the other opens a URL / scans a QR in
**any browser — no app on the receiver**. The differentiator is **reliability**: it
uses **explicit URL/QR pairing, never mDNS/BLE/multicast discovery** (which home
routers quietly break). Cross-platform (Android/macOS/Windows/Linux from one Rust
core), local-only, no accounts. Android app is the intended paid product; desktop/CLI
free/open-core.

## 3. Current shipped state (all on `main`, pushed)

- Core transfer, folders, search, login, **resumable + crc32-verified uploads**,
  **pause/resume**, **folder-zip download**, **QR/`?k=` pairing key**, transfer
  **history**, "Open location", universal macOS binary, CI (`release.yml`), landing
  site.
- **This session's fixes** (commit `2ceea49`): download now sends `Content-Length`
  so browsers show a **progress bar** (tiny_http was chunking ≥32 KB —
  `.with_chunked_threshold(usize::MAX)`); **`DELETE /upload` + "clear ×"** to discard
  interrupted uploads; **"Open location" fallback** (`reveal_target` / `revealTarget`
  → `<shared dir>/<name>`) so received files resolve even for old history rows.
  13 `zap-core` tests + 1 `zap-desktop` test, all green.
- **Landing site** (`site/`, Cloudflare Pages, git-connected to `main`, live at
  **https://zap.sodatta.workers.dev**): screenshots under each "how it works" step,
  **favicon** (`site/favicon.svg` + `apple-touch-icon.png`). The public **Android
  APK was added then removed** (`c6d7a9a`) — see §5.

## 4. Forward plan — the Z-family

Local-first Rust networking utilities, all "Z", all: no cloud, no accounts, no
discovery-reliance, honest about OS limits, reuse Zap's core + design.

| App | Job | Spec |
| --- | --- | --- |
| **Zap** | file/folder transfer (+ fold-ins) | `docs/apps/zap.md` |
| **Zulu** | continuous clipboard/link/snippet **sync** | `docs/apps/zulu.md` |
| **Zeus** | **wake / power** PCs from phone (WoL + SSH) | `docs/apps/zeus.md` |
| **Zod** | LAN **recon** (scan/ping/ports/speed) | `docs/apps/zod.md` |
| **Zeta** | **phone-as-trackpad/keyboard** for the PC | *spec not written yet* — see §6 |

**Build order:** Zap fold-ins → Zulu → Zeus → Zod. Zeta is in the "control" cluster
with Zeus. Extract a shared core crate (`zero`/`znet`: pairing + LAN transport +
presence/SSE) as the family grows.

**Zap fold-ins to build next** (`docs/apps/zap.md`): 1) send text/link/snippet,
2) one-time/burn-after-read send, 3) presence/trusted-devices (build the SSE/presence
primitive in the core — Zulu needs it too), 4) hotspot "Bridge" fallback.

## 5. Open decisions / immediate next actions

- **Owner is testing** the desktop `.dmg` + the live site. Waiting on confirmation.
- **CI/CD run pending owner's go**: tag `v*` → `release.yml` builds macOS/Windows/
  Linux → pull the fresh **Windows `.zip` + Linux `.deb`** into `site/download/` and
  push (they're currently from an older CI run, before today's fixes). **Keep the
  local universal `.dmg`** — CI's dmg is single-arch.
- **Before any public APK / Play Store**: **reserve the `com.zap.transfer` package
  name** on Play Console first (package-squatting risk was the reason the public APK
  was pulled). Also decide monetization (paid-upfront vs free+Pro IAP) — a free
  public APK undercuts a paid plan.
- **Uncommitted right now**: `docs/apps/*.md` (the specs + this file) — commit when
  ready.

## 6. Ideas evaluated this session (so they aren't re-litigated)

- ✅ **Keep/build**: Zulu (clipboard sync, mobile = share-sheet + tap-to-copy due to
  background-clipboard OS blocks), Zeus (WoL, trivial + reliable), Zod (Android/
  desktop; iOS limited), **Zeta = phone-as-trackpad** (input-only, no video; the
  hard part is per-OS input injection: Windows easy, macOS needs Accessibility perm,
  Linux-Wayland is locked → uinput/libei).
- ❌ **Dropped**: controlling an **iPhone** from Android (Apple blocks screen control
  + input injection entirely); **Offline Device Discovery** (that's the mDNS/BLE wall
  Zap deliberately avoids); **full remote desktop** as an early app (RustDesk is an
  open-source Rust incumbent + huge video-streaming scope); **Wi-Fi Analyzer** (no
  iOS scan API, Android throttled); **Universal Pair** (= KDE Connect/Phone Link,
  too big + iOS-locked, revisit only after the core crate + 2–3 shipped apps).

## 7. Working conventions

- **Commit directly to `main`**, no feature-branch ceremony; **push only when asked.**
- Rebuild `dist/` (`scripts/build-dist.sh`) + reinstall the APK after changes so the
  owner can test on real hardware (a phone is USB-connected; MIUI — see HANDOFF §7:
  no `adb input`, so drive tests via browser/CLI, not the phone UI).
- **Honesty over hype** — the owner's audience includes networking pros. Never claim
  magic speed (it's LAN throughput) or resume-through-a-dead-router.
- Verify end-to-end (drive the real flow), not just tests. Desktop GUI has a
  `ZAP_SHOT` headless screenshot harness.
