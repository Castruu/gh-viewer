#!/usr/bin/env bash
# Builds build/GitHubViewer.app: Rust core -> Swift bindings -> SwiftPM app -> .app bundle.
# Usage: scripts/build.sh [--open]
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

APP_NAME="GitHubViewer"
APP="build/$APP_NAME.app"
BINDINGS="target/bindings"
# Must match LSMinimumSystemVersion in app/Info.plist and platforms in app/Package.swift;
# otherwise C code in Rust dependencies is compiled for the host's macOS version.
export MACOSX_DEPLOYMENT_TARGET=14.0

echo "==> Building Rust core"
cargo build --release -p ghv_core

echo "==> Generating Swift bindings"
rm -rf "$BINDINGS"
cargo run --quiet --release -p uniffi-bindgen -- generate \
    --library target/release/libghv_core.dylib \
    --language swift \
    --out-dir "$BINDINGS"

mkdir -p app/Sources/ghv_coreFFI app/Sources/GitHubViewer/Generated app/RustLib
cp "$BINDINGS/ghv_coreFFI.h" app/Sources/ghv_coreFFI/
# SwiftPM system library targets require the module map to be named module.modulemap.
cp "$BINDINGS/ghv_coreFFI.modulemap" app/Sources/ghv_coreFFI/module.modulemap
cp "$BINDINGS/ghv_core.swift" app/Sources/GitHubViewer/Generated/
# Only the static library goes in RustLib, so the linker can't pick the dylib instead.
cp target/release/libghv_core.a app/RustLib/

echo "==> Building Swift app"
swift build --package-path app -c release
BIN_DIR="$(swift build --package-path app -c release --show-bin-path)"

echo "==> Assembling $APP"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS"
cp "$BIN_DIR/$APP_NAME" "$APP/Contents/MacOS/"
cp app/Info.plist "$APP/Contents/"
# Ad-hoc signature: enough to run locally, not for distribution.
codesign --force --sign - "$APP"

echo "==> Built $ROOT/$APP"

if [[ "${1:-}" == "--open" ]]; then
    # Quit a running copy first so the new build actually launches.
    pkill -x "$APP_NAME" 2>/dev/null || true
    open "$APP"
fi
