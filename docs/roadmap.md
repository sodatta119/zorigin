# zap — Detailed Roadmap (Horizon 0 & 1)

> Task-by-task execution plan. Companion to `docs/backlog.md` (the menu) and
> `docs/HANDOFF.md` (how the code works). This file is the **sequenced plan**.

### Principles (keep the discipline)
1. **Ship the wedge, then deepen it.** Don't start Horizon 1 until Horizon 0
   shows a real demand signal (Gate G1/G2 below).
2. **Stay in the LAN wedge.** Nothing that needs cloud/accounts/discovery unless
   evidence forces it (protects the differentiator).
3. **Every task independently shippable + verifiable.**
4. **Traction = wedge + distribution + trust**, not feature depth. Validation is
   a first-class part of the roadmap, not an afterthought.

### Legend
- Status: `[x]` done · `[~]` partial · `[ ]` todo
- Effort: **S** ≤1 day · **M** 2–4 days · **L** 1–2 weeks
- Lever: `WEDGE` · `DIST` (distribution) · `TRUST` · `$` (monetization) ·
  `REL` (reliability) · `VALID` (validation)

---

## HORIZON 0 — Ship the wedge & validate demand

**Goal:** launch the Android product, harden LAN reliability, and *prove people
switch and (some) pay* — before investing in Horizon 1.

> Reality check: the **core product is already built and verified** (web
> browse/upload/download, directory nav + search, session-cookie login,
> path-traversal protection, Android end-to-end, desktop GUI, per-file transfer
> speed). H0 is therefore mostly **reliability + launch + validation**, not
> feature-building.

### Phase 0.1 — Reliability hardening (round 2) · `REL`  ✅ complete (2026-07-15)
- [x] **H0-R0** Round 1: host warns on no-LAN-IP + "same Wi-Fi / disable AP
      isolation" hint. *(done)*
- [x] **H0-R1** `SO_REUSEADDR` on the listener socket. **S**
      *Done:* listener built via `socket2` (`SO_REUSEADDR` before bind) →
      `Server::from_listener`. Restart regression test
      (`restart_same_port_does_not_hit_eaddrinuse`) passes.
- [x] **H0-R2** AP / client-isolation detection. **M**
      *Done:* host-side watchdog. Core tracks `requests_seen()`; when reachable
      but zero client requests after a 20s grace period, CLI/desktop/Android show
      a specific AP-isolation / wrong-network / guest-network message instead of
      a silent hang. Env override `ZAP_NO_CLIENT_SECS` (desktop) for testing.
      *(Client-side detection is impossible — under isolation the page never loads.)*
- [x] **H0-R3** Actionable connection-failure message in the web UI. **S**
      *Done:* `loadDir` distinguishes a never-reached host (→ "Can't reach this
      device" card w/ Wi-Fi/AP-isolation bullets + Try again) from a server-side
      folder error. Verified end-to-end in a browser (killed the server mid-session).
- [x] **H0-R4** Surface host reachability (bound IP + "reachable at <url>")
      prominently on CLI / desktop / Android. **S**
      *Done:* green "Reachable at <url>" with prominent URL on all three surfaces.

### Phase 0.2 — Validation experiments · `VALID`  ⛔ do BEFORE Play Store spend & all of H1
- [ ] **H0-V1** Landing page + 60-sec **Mac↔Android** demo video. **M**
- [ ] **H0-V2** Post to communities (r/androidapps, r/apple, r/opensource, HN,
      LocalSend issues) with the "works when AirDrop/Quick Share can't" angle;
      measure signups / stars / comments. **S**
- [ ] **H0-V3** Willingness-to-pay probe (waitlist + "would you pay ₹X?" / fake
      "Buy Pro" button → waitlist). **S**
- [ ] **H0-V4** 5–10 user interviews: current tool, the pain, would-they-switch,
      would-they-pay. **M**

> **🚦 GATE G1 (Go / No-Go):** proceed to paid launch + Horizon 1 only if there's
> a **repeated, specific pain** + a credible switch trigger + *some* willingness
> to pay. If weak → fix positioning / wedge first; **do not** build more.

### Phase 0.3 — Play Store launch (Android is the paid product) · `$`
- [ ] **H0-P1** Release signing + build **AAB** (upload keystore, Play App
      Signing). **M**
- [ ] **H0-P2** `MANAGE_EXTERNAL_STORAGE` declaration form + demo video.
      Plan-B ready: SAF-only scoped storage if rejected. **M**
- [ ] **H0-P3** Privacy policy URL (GitHub Pages) — "files stay on your LAN, no
      data collected." **S**
- [ ] **H0-P4** Data-safety form (no data collected/shared) + content rating. **S**
- [ ] **H0-P5** Store listing: icon 512, feature graphic 1024×500, screenshots,
      short+full description. **M**
- [ ] **H0-P6** Closed testing: 20 testers × 14 days (new personal-account rule). **L**
- [ ] **H0-P7** Production release. **S**

### Phase 0.4 — Monetization wiring (decide after G1) · `$`
- [ ] **H0-M1** Decide **paid-upfront (~₹50) vs free + Pro (IAP)**. **S**
- [ ] **H0-M2** If IAP: define the Pro line (e.g. no size cap / encrypted /
      turbo) and wire Play Billing. **M**

### Phase 0.5 — Desktop distribution polish · `DIST`
- [x] **H0-D1** Universal macOS binary (x86_64 + aarch64 via `lipo`). **M**
      *Done (2026-07-15):* `build-dist.sh` compiles both Darwin targets, lipos
      the CLI, swaps a fat binary into the cargo-bundle `.app`, repackages the
      `.dmg` via `hdiutil`. `lipo -info` confirms `x86_64 arm64` on both; CLI runs.
- [ ] **H0-D2** Code signing / notarization (macOS; Windows cert later if cost
      justifies). **M**
- [ ] **H0-D3** Validate Windows/Linux installers end-to-end on real machines/VMs
      (CI produces them; nobody has run them). **M**
- [ ] **H0-D4** Before any public repo: **extract the paid Android app** out of
      the monorepo (git history is permanent). **M**

> **🚦 GATE G2 (exit H0 → enter H1):** app launched + demand signal from G1 + a
> *specific* validated pain worth deepening. Pick H1 phases by what users pulled.

---

## HORIZON 1 — Deepen the wedge (practical bets)

**Rule:** start a phase only when H0 evidence pulls it. Each stays inside the LAN
wedge (no cloud) except the explicitly-gated cross-network spike.

### Cheap speed wins (no P2P) · `WEDGE`  ✅ done (2026-07-16)
- [x] Larger listener socket buffers (`SO_SNDBUF`/`SO_RCVBUF` 1 MiB, best-effort,
      before listen so accepted conns inherit).
- [x] Parallel folder uploads: files within a folder upload with bounded
      concurrency (4 lanes) — hides per-request latency for many-file folders;
      aggregate progress + pause/resume preserved. Verified: 12-file nested
      folder byte-exact.
- [x] Keep-alive: HTTP/1.1 keep-alive already on (tiny_http default); XHR reuses
      connections. No change needed.
- Loose multi-file selection already uploads in parallel (uploadOne not awaited).

### Phase 1.1 — Big-file superpower ⭐ (flagship) · `WEDGE` `$`  ✅ complete (2026-07-15)
*Why:* resumable, no-limit, verified large-file transfer over flaky Wi-Fi is a
real un-owned pain; natural Pro line. All LAN.
- [x] **H1-B1** Resume protocol: `HEAD /upload?path&name` → `X-Zap-Offset`; client
      resumes its `PUT ...&offset=<n>` from there. The temp file's size *is* the
      checkpoint (no cross-request state).
- [x] **H1-B2** Upload resume: append to `.zap-part-<name>`, verify offset
      (409 + real offset on mismatch), atomic `rename` on complete.
- [x] **H1-B3** Download resume: HTTP `Range` on `/download` (206 +
      `Content-Range` + `Accept-Ranges: bytes`). Unit-tested.
- [x] **H1-B4** Integrity verify: streaming **crc32** (dependency-free, table-
      driven; browser + Rust interop confirmed against `zlib.crc32`). Client
      sends `X-Zap-Crc32`; host recomputes on finalize → `X-Zap-Verified` +
      "✓ verified" in the web UI and "verified" in native transfer views.
- [x] **H1-B5** End-to-end streaming (chunked read/write, `io::copy`); no whole-
      file buffering — client reads 8 MB chunks, host streams to disk.
- [x] **H1-B6** Web client: chunked upload, crc-as-we-read, retry-from-offset on
      failure; desktop + Android transfers reflect resumed progress + verified.
      *Verified:* multi-chunk 20 MB upload byte-exact + verified; seeded 5 MB
      partial → HEAD offset → resumed → 12 MB byte-exact + verified; core unit
      tests for resume/offset-mismatch/range/crc. *(Live 5 GB Wi-Fi-kill demo:
      owner to run on device.)*
- [x] **H1-B7** Per-upload **Pause / Resume** button in the web client (sender
      side — the only side that drives the chunk loop). Pause aborts the in-flight
      chunk + holds the loop; Resume re-HEADs the host offset and continues, no
      bytes lost. *Verified:* paused a 200 MB upload at 48% (host temp file froze),
      resumed → byte-exact + verified. Host side could add Cancel later; downloads
      are the browser's own (our `Range` enables its resume).

### Phase 1.2 — PWA receiver + Android share-target ⭐ · `DIST`
*Why:* the "no app on receiver" superpower + share-sheet presence = virality and
the friction AirDrop wins on. Mostly web tech, no new infra.
- [ ] **H1-P1** Make the served page an installable **PWA** (manifest + service
      worker + offline shell). **M**
- [ ] **H1-P2** Register **Android Web Share Target** (`share_target` POST) so
      "zap" appears in the system share sheet → sends to the paired host. **M**
- [ ] **H1-P3** Pairing memory in the PWA (remember host URL + token). **S**
- [ ] **H1-P4** iOS "Add to Home Screen" PWA basics (note: iOS share-target is
      limited — document the gap). **S**
      *Accept:* install PWA, Share a photo from Gallery → it lands on the host.

### Phase 1.3 — Whole-folder / bulk transfer · `WEDGE`  ✅ complete (2026-07-15)
- [x] **H1-F1** Folder **upload** (`webkitdirectory` pick + drag-drop entry
      traversal) preserving structure. One aggregate row (combined bar + speed +
      ETA + file count) + folder-level pause/resume; each file still resumable +
      crc-verified; host `create_dir_all`s subfolders. *Verified:* nested
      MyAlbum/sub/deep tree uploaded byte-exact.
- [x] **H1-F2** Folder **download** as a streamed **ZIP** (store method, ZIP64
      when >4 GB, dependency-free, background producer thread → chunked response;
      folder rows get a download-zip icon). *Verified:* python `zipfile.testzip`
      (all CRCs), macOS `unzip -t`, and byte-exact extraction of a nested tree;
      Rust unit test for structure. *(Note: `webkitdirectory` folder-pick is
      desktop-browser mainly; Android browser folder-pick is limited — download
      works everywhere.)*
      *Accept:* send a nested folder both ways; structure intact. ✅

### Phase 1.4 — Frictionless verified pairing · `WEDGE` (switch friction)
- [x] **H1-K1** Pairing key in the QR / share link → scanning (or opening a
      shared keyed link) grants access even in Secure mode, no password typing.
      *Done (2026-07-15):* `ServerInfo::url_with_key()` appends `?k=<token>`;
      the server auto-authenticates a matching key (303 + session cookie, then
      redirects to the clean path). CLI + desktop QR encode the keyed URL;
      Android share uses it (`nativeShareUrl`). Verified: core unit test + curl +
      browser (keyed URL → app, key stripped from the address bar).
- [ ] **H1-K2** Remembered / trusted devices + one-tap reconnect on the same
      LAN. **M** *(needs native persistence per platform — deferred)*
      *Accept:* Secure on; scan QR → straight into the app, no typing. ✅ (K1)

### Phase 1.5 — Encrypted LAN (trust / Pro) · `TRUST` `$`
*Why:* traffic is plaintext HTTP today — a real objection for sensitive files /
hostile Wi-Fi / enterprise.
- [ ] **H1-E1** TLS via `rustls` (self-signed) for **native↔native** with cert
      pinning in Android/desktop. **M**
- [ ] **H1-E2** SAS / fingerprint compare (emoji/number) verification UX. **M**
- [ ] **H1-E3** Decide the browser story (self-signed → scary warning); likely
      **native-only / Pro**. Document the caveat. **S**
      *Accept:* native↔native encrypted + verified; browser caveat documented.

### Phase 1.X — Cross-network 🔒 GATED (only if demand is strong)
*Do not start without repeated cross-network pull + willingness to pay. Uses a
library (don't hand-roll NAT traversal); LAN stays the default.*
- [ ] **H1-X1** Spike: [iroh](https://iroh.computer) P2P send between two zap
      instances across different networks. **L**
- [ ] **H1-X2** Opt-in **Pro** "send to my devices anywhere"; LAN remains the
      default path. **L**

---

## Sequencing at a glance
```
H0.1 reliability ─┐
                  ├─► 🚦G1 validate ─► H0.3 Play Store ─► H0.4 monetize
H0.2 validation ──┘                    H0.5 desktop polish
                                             │
                                        🚦G2 (launched + demand)
                                             │
                 H1.1 big-file ⭐ ──┬── H1.2 PWA/share-target ⭐
                 H1.3 folders ──────┤
                 H1.4 pairing ──────┤
                 H1.5 encrypted ────┘
                 H1.X cross-network 🔒 (gated on demand)
```

**If you build only two H1 things:** `H1.1 (big-file)` + `H1.2 (PWA/share-target)`
— one gives a reason to switch/pay, the other gives distribution, both stay in
the wedge.
