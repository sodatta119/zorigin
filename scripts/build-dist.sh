#!/usr/bin/env bash
# Build the installable for THIS machine's OS into ./dist.
#
# One machine can only build its own platform's GUI installer - Mac makes a
# .dmg, Linux makes a .deb + tarball. To get every platform's installer in one
# place, push a `v*` git tag and let CI (.github/workflows/release.yml) build
# macOS + Windows + Linux and attach them to the GitHub Release.
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$(pwd)"                 # repo root - dist/ lives here
mkdir -p "$ROOT/dist"

if ! command -v cargo-bundle >/dev/null 2>&1; then
  echo "cargo-bundle not found. Install it with:  cargo install cargo-bundle"
  exit 1
fi

# The Cargo workspace lives under networking/ (zOrigin category layout). Run all
# cargo commands from there; target/ is networking/target, dist/ stays at repo root.
cd "$ROOT/networking"

case "$(uname -s)" in
  Darwin)
    # Universal (Intel + Apple Silicon): build both arches, then `lipo` them into
    # one fat binary so a single .dmg runs natively on any Mac. cargo-bundle only
    # builds one arch, so we bundle for the .app skeleton (icon + Info.plist) and
    # then swap in the universal binary and repackage the .dmg ourselves.
    ARM=aarch64-apple-darwin
    X86=x86_64-apple-darwin
    for t in "$ARM" "$X86"; do rustup target add "$t" >/dev/null 2>&1 || true; done

    cargo build --release --package zap-cli --package zap-desktop \
      --target "$ARM" --target "$X86"

    # Universal CLI.
    lipo -create -output "$ROOT/dist/zap-macos-cli" \
      "target/$X86/release/zap" "target/$ARM/release/zap"

    # Bundle the .app, then replace its binary with the universal one.
    ( cd crates/zap-desktop && cargo bundle --release )
    APP=target/release/bundle/osx/zap.app
    lipo -create -output "$APP/Contents/MacOS/zap-desktop" \
      "target/$X86/release/zap-desktop" "target/$ARM/release/zap-desktop"

    # Package a universal .dmg (the .app + an /Applications drop target).
    STAGE=target/zap-dmg
    rm -rf "$STAGE"; mkdir -p "$STAGE"
    cp -R "$APP" "$STAGE/"
    ln -s /Applications "$STAGE/Applications"
    rm -f "$ROOT/dist/zap-macos.dmg"
    hdiutil create -volname zap -srcfolder "$STAGE" -ov -format UDZO "$ROOT/dist/zap-macos.dmg" >/dev/null
    echo "✅ dist/zap-macos.dmg (universal)  +  dist/zap-macos-cli (universal)"
    lipo -info "$ROOT/dist/zap-macos-cli"

    # --- Zulu (clipboard sync; desktop app only, no CLI) ---
    cargo build --release --package zulu-desktop --target "$ARM" --target "$X86"
    ( cd crates/zulu-desktop && cargo bundle --release )
    ZAPP=target/release/bundle/osx/Zulu.app
    lipo -create -output "$ZAPP/Contents/MacOS/zulu-desktop" \
      "target/$X86/release/zulu-desktop" "target/$ARM/release/zulu-desktop"
    ZSTAGE=target/zulu-dmg
    rm -rf "$ZSTAGE"; mkdir -p "$ZSTAGE"
    cp -R "$ZAPP" "$ZSTAGE/"
    ln -s /Applications "$ZSTAGE/Applications"
    rm -f "$ROOT/dist/zulu-macos.dmg"
    hdiutil create -volname Zulu -srcfolder "$ZSTAGE" -ov -format UDZO "$ROOT/dist/zulu-macos.dmg" >/dev/null
    echo "✅ dist/zulu-macos.dmg (universal)"
    ;;
  Linux)
    cargo build --release --package zap-cli
    ( cd crates/zap-desktop && cargo bundle --release --format deb )
    cp target/release/bundle/deb/*.deb "$ROOT/dist/"
    cp target/release/zap-desktop "$ROOT/dist/zap-linux"
    cp target/release/zap "$ROOT/dist/zap-linux-cli"
    echo "✅ dist/*.deb  +  dist/zap-linux  +  dist/zap-linux-cli"

    # --- Zulu (desktop app only) ---
    ( cd crates/zulu-desktop && cargo bundle --release --format deb )
    cp target/release/bundle/deb/zulu*.deb "$ROOT/dist/zulu-linux.deb" 2>/dev/null || true
    cp target/release/zulu-desktop "$ROOT/dist/zulu-linux"
    echo "✅ dist/zulu-linux.deb  +  dist/zulu-linux"
    ;;
  *)
    echo "This script builds macOS/Linux. For Windows, run on Windows:"
    echo "  cargo build --release --package zap-desktop --package zap-cli"
    exit 1
    ;;
esac

echo
echo "Note: this only built $(uname -s). For ALL platforms at once, tag a release:"
echo "  git tag v0.1.0 && git push --tags   (CI builds macOS + Windows + Linux)"
