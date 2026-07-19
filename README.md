# zOrigin

Private-first, local-first software that just works on your own network - no
cloud, no accounts, no discovery flakiness. One Rust engine per category, many
focused apps on top.

**https://zorigin.net**

## The model

zOrigin is an umbrella over **categories**; each category has its own engine and
its own products. Networking is category #1; other, non-networking categories
will come later with their own separate core.

```
zOrigin
└── networking   (category #1)  - engine: znet-core
    ├── Zap      (live)
    └── Zulu / Zeus / Zod / Zeta  (planned)
```

## Products

| Product | Category | What it does | Status |
| --- | --- | --- | --- |
| **Zap** | networking | Cross-platform file & folder transfer over local Wi-Fi - the receiver just opens a link in a browser, no app needed | ✅ Live |
| **Zulu** | networking | Clipboard & link sync - copy on one device, it's instantly on your others | 🚧 In progress (desktop app, Android app, no-app web receiver, small images, pinned snippets, opt-in TLS) |
| **Zeus** | networking | Wake & power control - turn your PCs on and off from your phone | Planned |
| **Zod** | networking | LAN recon - see and inspect everything on your Wi-Fi | Planned |
| **Zeta** | networking | Phone as a trackpad for your computer | Planned |

## Repo layout

```
zorigin/
├── networking/   # category #1: Cargo workspace (znet-core engine + Zap cli/desktop/android + Zulu desktop)
│                 #   -> see networking/README.md for build, run, and app details
├── site/         # the zOrigin website (zorigin.net), served by Cloudflare.  / = zOrigin, /zap = Zap
└── docs/         # roadmap, backlog, and per-product briefs (docs/apps/)
```

Categories are folders so a future non-networking category simply adds its own
top-level directory (with its own core), without disturbing `networking/`.

## Build

The code lives under `networking/` (the networking category's Cargo workspace):

```sh
cd networking && cargo build --release
```

See [`networking/README.md`](networking/README.md) for per-app build/run, the
Android (`cargo-ndk` + Gradle) flow, and the installer/distribution setup.
