# networking - Zap⚡

The **networking** category of [zOrigin](../README.md). Shared engine
`znet-core` (LAN web transport) + its live product **Zap**: fast file & folder
transfer over local Wi-Fi, cross-platform (macOS / Windows / Linux / Android),
with a browser-only receiver. Future networking products (Zulu / Zeus / Zod /
Zeta) will live here too, reusing `znet-core`.

> All commands below are run from this `networking/` directory (the Cargo
> workspace root).

## Status

`zap` is built around a pluggable `Transport` trait so the same CLI can move
files over several links. Transports are being added simplest-first:

| Transport         | Android side needed        | Status      |
| ----------------- | -------------------------- | ----------- |
| **USB (ADB)**     | Developer mode only        | ✅ working  |
| **Wi-Fi (web)**   | A browser                  | ✅ working  |
| Wi-Fi (native)    | Companion agent (TBD)      | planned     |
| USB (MTP)         | None                       | planned     |
| Bluetooth         | Companion agent (TBD)      | planned     |

Two control models coexist:

- **Host-driven** transports (ADB, and later native Wi-Fi / MTP / Bluetooth)
  implement the `Transport` trait - the Mac enumerates devices and drives
  `ls`/`pull`/`push`.
- **Server-mode** transports invert control: the Mac serves, the phone drives.
  The web transport (`zap serve`) is the first of these and needs no app on
  the phone - just a browser.

## Install

Requires the `adb` binary for the USB transport:

```sh
brew install android-platform-tools
```

`zap` finds `adb` via `$ZAP_ADB`, then `$ANDROID_HOME/platform-tools/adb`, then `$PATH`.

Build:

```sh
cargo build --release
```

## Usage

```sh
zap devices                          # list connected devices
zap ls /sdcard/DCIM                  # list a directory on the phone
zap pull /sdcard/DCIM/photo.jpg .    # phone -> Mac
zap push ~/song.mp3 /sdcard/Music/   # Mac -> phone
```

When exactly one device is connected it's selected automatically; otherwise
pass `--device <serial>`. Choose a transport with `--transport <name>`
(default: `adb`).

### Wi-Fi via the browser (no app)

```sh
zap serve --dir ~/Downloads          # share ~/Downloads over Wi-Fi
```

`zap serve` prints a URL and a QR code. On a phone connected to the same
Wi-Fi, scan the QR (or open the URL) to get a page that can:

- **upload** files to the Mac (they land in `--dir`), and
- **download** any file the Mac is sharing from `--dir`.

Options: `--dir` (default `.`), `--port` (default `8080`), `--bind`
(default `0.0.0.0`).

## Architecture

This category is a Cargo **workspace** (`networking/`) so the same core logic can
back multiple front ends - the desktop CLI and GUI today, the Android app, and
future networking products next.

```
networking/              # this category = the Cargo workspace root
├── crates/
│   ├── znet-core/       # platform-neutral logic; no terminal/UI concerns (the shared engine)
│   │   ├── src/transport/   #   Transport trait + AdbTransport (host-driven)
│   │   │   ├── mod.rs       #     trait, Device, RemoteEntry
│   │   │   └── adb.rs       #     shells out to `adb`
│   │   └── src/web/         #   web transport (server-mode)
│   │       ├── mod.rs       #     tiny_http server: serve(config, on_ready)
│   │       └── index.html   #     phone-facing page, embedded via include_str!
│   ├── zap-cli/         # desktop CLI binary (`zap`)
│   │   ├── src/main.rs      #   dispatch, device resolution, banner + QR
│   │   └── src/cli.rs       #   clap command definitions
│   ├── zap-desktop/     # desktop GUI app (egui) - the "control panel"
│   │   └── src/main.rs      #   start/stop, URL + QR, folder picker, secure
│   └── zap-android/     # cdylib: JNI bridge to znet_core::web::spawn
└── android/zap/         # the Zap Android app (Gradle project)
```

Design rules that keep it multi-platform:

- **`znet-core` does no presentation.** `web::serve` takes an `on_ready`
  callback and hands back a `ServerInfo` (share dir, port, LAN IP, `url()`);
  the *caller* decides how to show it. The CLI prints a banner + terminal QR;
  an Android app will render its own UI. The terminal-only `qrcode` dependency
  lives in `zap-cli`, not the core.
- Adding a host-driven transport = one new `impl Transport` plus a variant in
  `TransportKind`; the CLI commands don't change.

### Cross-platform reach

The web transport already covers every device pairing without per-OS code: any
device can run the server, and the client is always just a browser.

- **macOS / Windows / Linux** - a `zap` CLI and a `zap-desktop` GUI app, both
  built from one `cargo build` (`znet-core` uses only portable `std` +
  `tiny_http`). Distributed via GitHub.
- **Android** - the phone hosts the server itself, via a Kotlin app over JNI.

### Desktop GUI

`crates/zap-desktop` is an [egui](https://github.com/emilk/egui) app: a small
control panel (start/stop, share-folder picker, "require password", live URL +
scannable QR) that calls `znet_core::web::spawn`. Other devices connect through
the browser at the shown URL.

```sh
cargo run --release --package zap-desktop     # run locally
cargo bundle --release --package zap-desktop  # macOS .app + .dmg (needs cargo-bundle)
```

Installers for macOS (`.dmg`) and Windows (`.zip`) are built automatically by
`.github/workflows/release.yml` on every `v*` git tag and attached to the
GitHub Release. (Builds are unsigned - macOS users right-click → Open the first
time; Windows users click "More info → Run anyway" past SmartScreen.)

### Android

`crates/zap-android` is a `cdylib` that exposes `znet_core::web::spawn` to Kotlin
through three JNI calls (`nativeStart` / `nativeUrl` / `nativeStop`). A
foreground service keeps the server alive on the home Wi-Fi. Because `znet-core`
is presentation-free, the phone runs the exact same server code as the desktop.
The Gradle project lives at `android/zap/`.

Building the `.so` needs the Android toolchain (kept out of the default desktop
build):

```sh
# one-time
rustup target add aarch64-linux-android armv7-linux-androideabi \
                  x86_64-linux-android i686-linux-android
cargo install cargo-ndk
# with Android Studio's NDK installed and $ANDROID_NDK_HOME set:
cargo ndk -t arm64-v8a -t armeabi-v7a -o android/zap/app/src/main/jniLibs \
    build -p zap-android --release
```
