# Task handoff - restructure the repo into the zOrigin category layout

> Resume brief for the **repo-restructure task**. Written mid-conversation so it can
> be picked up in a fresh chat (revert-and-resume). Everything needed to continue is
> here; the general project context is in `docs/HANDOFF.md`.
>
> **Status: EXECUTED (2026-07-18).** Local restructure done + verified: workspace
> moved to `networking/`, `android/` -> `networking/android/zap/`, core crate
> renamed `zap-core` -> **`znet-core`** (owner's pick). `site/` and `docs/` stayed
> at repo top. `scripts/build-dist.sh` + `.github/workflows/release.yml` +
> `.gitignore` updated for the new paths. **GitHub repo rename `zap` -> `zorigin`
> deliberately deferred** (keeps the Cloudflare git connection + deploy intact).
>
> Verified: `cargo test -p znet-core --lib` = 13/13 green; `cargo build` green;
> `./scripts/build-dist.sh` produced the universal `.dmg` + CLI at repo-root
> `dist/`; `.so` rebuilt via cargo-ndk; APK rebuilt + reinstalled + launches clean
> on device. Cloudflare untouched (no config file in repo; `site/` unmoved).

---

## 1. What the owner asked for

Reorganize the repo so **zOrigin is the umbrella**, with a **category layer**, and
products living under their category:

> zOrigin → **networking** (category) → products (Zap, Zulu, Zeus, Zod, Zeta)

Critical constraint the owner added: **zOrigin is NOT only networking.** Networking
is category #1; **other, non-networking categories will come later** and will have
their *own* separate core (they won't reuse the networking core). So the structure
must have a real category level, not just "all products flat under zOrigin".

## 2. Current state (before any restructure)

- Repo: `git@github.com:sodatta119/zap.git` (private), branch `main`.
- **Cargo workspace at repo root** (`Cargo.toml`): members
  `crates/zap-core`, `crates/zap-cli`, `crates/zap-desktop`, `crates/zap-android`.
  `default-members` = core + cli + desktop (android built via cargo-ndk).
- `android/` = Gradle project (the Zap Android app; `.so`s under
  `android/app/src/main/jniLibs/<abi>/libzap_android.so`).
- `site/` = **zorigin.net** static site: `/` = zOrigin landing, `/zap`, `/zulu`,
  `/zeus`, `/zod`, `/zeta` product pages; shared `product.css`; `anime.min.js`.
  **Cloudflare Workers/Pages, git-connected, serves output dir `site/`,
  auto-deploys on push to `main`.** Live at **https://zorigin.net**.
- `docs/` = `HANDOFF.md`, `roadmap.md`, `backlog.md`, `apps/` (README, START-HERE,
  zap.md, zulu.md, zulu-brief.md, zeus.md, zod.md).
- `scripts/build-dist.sh`, `.github/workflows/release.yml`. `dist/` + `target/`
  gitignored.

## 3. Agreed target structure (single repo, category folders)

Chosen model: **one monorepo now** (shared core + solo dev + YAGNI); split into a
GitHub **org of per-category repos later**, only when a 2nd category actually exists.

```
zorigin/                          # repo (GitHub rename zap->zorigin = LATER, see §5)
├── site/                         # STAYS AT REPO TOP - umbrella, category-agnostic.
│                                 #   Cloudflare serves site/ -> keeping it here means
│                                 #   the deploy is UNTOUCHED by the reorg. Important.
├── networking/                   # category #1
│   ├── Cargo.toml                # the Cargo workspace (moves here from repo root)
│   ├── Cargo.lock
│   ├── crates/
│   │   ├── znet-core/            # shared NETWORKING engine (renamed from zap-core)
│   │   ├── zap-cli/  zap-desktop/  zap-android/
│   │   └── (later) zulu-*, zeus-*, zod-*, zeta-*
│   ├── android/
│   │   └── zap/                  # the current android/ project moves here
│   └── docs/                     # networking handoff + apps/ specs (or keep docs at top)
├── docs/                         # org-level docs (optional; could stay for this file)
├── scripts/                      # build-dist.sh (paths updated for networking/)
├── .github/                      # workflows (paths updated)
└── README.md
└── (future) <other-category>/    # its own stack + core, independent of networking
```

### Why these choices
- **`site/` at top**: it's the whole-company site (all categories). Keeping it at the
  repo root means Cloudflare's `site` output dir is unchanged -> **no deploy rewire**.
- **Shared core = networking-scoped -> rename `zap-core` -> `znet-core`** (z-networking).
  A future non-networking category gets a different core; naming it `zorigin-core`
  would wrongly imply org-wide. (README already floated `zero`/`znet`.)
- **Monorepo, not multi-repo**: Zap and Zulu reuse ~80% of the same engine
  (pairing, LAN transport, browser client, resumable, the SSE/presence primitive to
  build). Path-deps in one workspace = frictionless. Org/multi-repo has publish/
  version overhead - defer until a 2nd category or more contributors.

## 4. Migration steps (the actual work, when green-lit)

Do it in ONE careful pass, then verify the build before committing.

1. `git mv crates networking/crates`; `git mv android networking/android/zap`;
   `git mv Cargo.toml Cargo.lock networking/`. Decide docs: either
   `git mv docs/HANDOFF.md docs/roadmap.md docs/backlog.md docs/apps networking/docs/`
   (keep this restructure doc at top) or leave `docs/` at top. (Owner pref: TBD.)
2. In `networking/Cargo.toml`: update member paths (still `crates/...` relative to
   the new workspace root, so likely unchanged), and rename the core.
3. **Rename crate `zap-core` -> `znet-core`**: its `Cargo.toml` `name` + `[lib] name`
   (`zap_core` -> `znet_core`); every `use zap_core::` -> `use znet_core::` in
   zap-cli / zap-desktop / zap-android; and `zap-core = { path = ... }` dep entries
   -> `znet-core`. **Do NOT rename `zap-android`'s lib** - the `.so` is loaded by
   Kotlin as `System.loadLibrary("zap_android")` and the JNI symbols are
   `Java_com_zap_transfer_*` (package-based, unaffected). Keep `libzap_android.so`.
4. `scripts/build-dist.sh`: run cargo from `networking/`; `cargo bundle` from
   `networking/crates/zap-desktop`; cargo-ndk `-o networking/android/zap/app/src/main/jniLibs`;
   copy dmg/cli into repo-root `dist/`.
5. `.github/workflows/release.yml`: update working-directory / paths to `networking/`.
6. Android gradle: internal paths are relative to `android/zap/`, so intra-project
   paths are fine; only the cargo-ndk output path in build-dist.sh changes.
7. **Leave `site/` at repo root** -> Cloudflare build output dir stays `site` ->
   no change needed there.
8. Verify: from `networking/` run `cargo test -p znet-core --lib` (13 tests),
   `cargo build`, `./scripts/build-dist.sh` (or the updated path), rebuild APK; all
   green. Then commit + push (per convention: commit AND push to `main`).
9. **GitHub repo rename `zap` -> `zorigin`**: SEPARATE later step. After renaming,
   re-verify the Cloudflare Worker/Pages git connection still deploys (it keys off
   the repo), and update the local remote: `git remote set-url origin ...zorigin.git`.

## 5. OPEN - needs owner's answer before executing

1. **Go / no-go**: execute the restructure now, or keep planning? (Owner leaning:
   do the local restructure now; defer GitHub rename.)
2. **Core crate name**: `znet-core` (recommended) vs `net-core` vs `zero` / `znet`.
3. (minor) Move `docs/` under `networking/`, or keep at repo top?

## 6. Working conventions (carry over)

- **Commit directly to `main` AND push in the same step** - no branch, don't wait to
  be asked to push (owner treats "commit" as "commit + push"; see memory).
- **Single hyphen only** - never em/en dashes anywhere.
- **Verify end-to-end** before committing a non-trivial change; rebuild dist + APK so
  the owner can test on the USB phone (MIUI: no `adb input`).
- This is a big mechanical change touching many files - do it in ONE pass and
  verify the build, don't iterate symptom-by-symptom.
