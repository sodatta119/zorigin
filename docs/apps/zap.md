# Zap — remaining features

> Zap is the shipped file/folder transfer app. This file lists what's **left to
> add**. The authoritative plan lives in `docs/roadmap.md` (Horizon 0/1) and
> `docs/backlog.md`; **read those first** — this file only adds the newly-decided
> "fold-in" features and the current priority call.

Zap's identity: **move files & folders across devices over Wi-Fi, reliably, with
no app on the receiver.** Rule of thumb — *anything that is "send a thing on
demand" belongs in Zap; anything that is "continuous sync", "diagnostics", or
"device control" is a separate Z-app.*

## Already shipped (for context)

Browse / upload / download, folder nav + search, session-cookie login, path-safety,
Android end-to-end, desktop GUI (light/dark), per-file live speed, **resumable +
crc32-verified uploads**, upload **pause/resume**, **folder upload + folder-zip
download**, **QR/link pairing key** (`?k=`), transfer **history**, "Open location",
universal macOS binary, landing site. See `docs/HANDOFF.md` §8.

## Fold-in features to add (new — decided this phase)

These deepen Zap as the transfer app; each reuses the existing core.

1. **Send text / link / snippet** — alongside files, let the sender push a bit of
   text or a URL. It lands on the host (and shows in the browser client) as a
   copyable item. Small addition to the send flow + a tiny text endpoint. **Most
   requested; do first.**
2. **One-time / burn-after-read** — a per-send "ephemeral" toggle: the file/text is
   served once (or for N minutes) then deleted. Builds on the existing download +
   `.zap-part-`/temp handling.
3. **Presence / trusted devices** — remember paired devices and show "which of my
   devices are online → tap to send", instead of re-sharing a URL/QR each time.
   This is roadmap **H1.4 (pairing)** made concrete; needs a lightweight presence
   signal (SSE/long-poll — the same primitive Zulu needs, so build it in the core).
4. **Bridge (hotspot fallback)** — when there's no shared Wi-Fi, a guided flow:
   one device makes a hotspot, the other joins, then Zap works. Directly fixes
   Zap's biggest limitation (needs same subnet). *Honesty:* iOS programmatic
   hotspot is limited — document the gap; Android is workable.

## Still-open from the existing roadmap (don't duplicate — see `docs/roadmap.md`)

- **H0-D2** macOS code signing / notarization (needs Apple Developer account).
- **Play Store** launch (H0-P1..P7): reserve `com.zap.transfer` **first** (package
  squatting risk — see the APK discussion), signing/AAB, data-safety, closed testing.
- **Monetization** decision (paid-upfront vs free + Pro IAP). Note: this gates
  whether a public APK on the landing page is acceptable.
- **H1.5 Encrypted LAN** (TLS via rustls, self-signed, native↔native) — also the
  prerequisite that **unblocks H1.2 PWA + Android share-target** (PWA/service-worker/
  share_target need a secure context; plain `http://<lan-ip>` is not one).
- **H1.X Cross-network** (iroh) — gated on real demand.

## Design / build

Everything already follows the family design. Keep using `scripts/build-dist.sh`,
`release.yml`, the gradle APK flow, and the `site/` landing pattern. Amber accent,
dark/light, bolt mark, `favicon.svg`.

## Priority call

Do the **fold-ins in order 1 → 4** (text/link send is the quick, high-value start),
then pick up the roadmap items as the owner's launch/monetization decisions land.
Build the **presence (SSE) primitive in the shared core** while doing #3, because
Zulu depends on it.
