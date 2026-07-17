# Zulu — clipboard / link / snippet sync

> **One-liner:** copy on one device, it's instantly available on your other
> devices. "Zulu time" = everything stays in sync.
>
> Reuse Zap's core wherever possible — see `docs/apps/README.md` → "Reuse Zap's
> design & code". Zulu is the app that shares the **most** with Zap (pairing +
> LAN transport + browser client + the new presence/SSE primitive).

## What it is

Continuous, bidirectional sync of small clipboard content across your paired
devices — **text, links, code snippets, and small images** — plus a short
**clipboard history** and **pinned snippets**. Not "send a file" (that's Zap);
this is "my clipboard follows me".

## The hard part: mobile OS clipboard restrictions (read this first)

This is the make-or-break constraint. Design *around* it — don't pretend it
doesn't exist.

- **Reading the clipboard in the background is blocked** on modern mobile:
  Android 10+ only lets the **foreground** app read the clipboard; iOS is stricter
  still. So "auto-capture whatever you copy" is **not possible in the background**
  on phones.
- **Writing the clipboard from a web page needs a user gesture** (the Clipboard
  API requires user activation) → on the web receiver it's **tap-to-copy**, not
  silent.

Resulting design (honest, reliable):

| Direction | Desktop (native app) | Mobile (Android/iOS) |
| --- | --- | --- |
| **Send** (copy → push) | Auto: native app watches clipboard changes and pushes | Assisted: **Share-sheet → "Send to Zulu"**, or a quick-tile / in-app "push clipboard" button (foreground) |
| **Receive** (push → paste) | Auto: native app writes to the OS clipboard | **Tap-to-copy** in the open Zulu page/app (Android may allow auto-write from a foreground app) |

Net: **desktop ↔ desktop is truly seamless**; **mobile is assisted**. That's fine —
say so. It still beats every "just works" tool that silently fails.

## Architecture

- **Transport & pairing**: reuse Zap's core (`crates/zap-core/src/web`) — one device
  hosts, others connect (native app or an open browser tab). Same QR/URL/`?k=` token
  pairing.
- **Live delivery**: **Server-Sent Events (SSE)** (or long-poll) from the host to
  every connected device — this is the **presence/push primitive that Zap doesn't
  have yet**; build it in the shared core (Zap's presence feature reuses it too).
- **State**: an in-memory ring of recent clips on the host + optional small on-disk
  history; cap size; images downscaled/size-limited.
- **Receiver page**: keep it a foreground tab (mobile) or the native app (desktop),
  listening on SSE, rendering the clip list with one-tap copy.

## Features (prioritized)

1. **Text + link sync** (the core loop).
2. **Clipboard history** (last N items, searchable).
3. **One-tap copy / Share-sheet send** (the mobile-safe path).
4. **Small image sync** (size-capped, downscaled).
5. **Pinned snippets** (frequently-pasted text).
6. **End-to-end encryption** (on-brand; native↔native first — see Zap H1.5/rustls).

## Platforms

- **Desktop (macOS/Windows/Linux)** — egui shell like `zap-desktop`; full auto sync.
- **Android** — Kotlin shell like the Zap app; assisted (share-target + tap-to-copy).
- **iOS** — later; very limited (document the gap honestly).

## Design

Family look (reuse Zap's): dark/light, cards, bolt-adjacent mark. Give Zulu its own
accent tint but keep it recognizably part of the set. Reuse `zap-desktop`'s
`tune_theme` and the `ZAP_SHOT`-style screenshot harness for verification.

## Non-goals

No cloud, no accounts, no background-clipboard hacks that fight the OS, no file
transfer (that's Zap — link to it instead).

## First milestone

Desktop↔desktop text/link sync over the LAN via SSE + pairing, proven end-to-end
(copy on A → appears on B, auto-pasted). Then add the Android share-target sender
and tap-to-copy receiver.
