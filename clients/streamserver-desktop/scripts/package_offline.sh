#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."

if ! command -v flutter >/dev/null 2>&1; then
  echo "flutter was not found on PATH" >&2
  exit 127
fi

TARGET_PLATFORM="${1:-}"
if [[ -z "$TARGET_PLATFORM" ]]; then
  echo "usage: $0 <macos|linux|windows>" >&2
  exit 2
fi

./scripts/build_native.sh
flutter pub get
flutter build "$TARGET_PLATFORM" --release

mkdir -p dist
STAMP="$(date +%Y%m%d-%H%M%S)"
ARTIFACT=""

case "$TARGET_PLATFORM" in
  macos)
    APP_PATH="build/macos/Build/Products/Release/streamserver_desktop.app"
    cp build/native/libstreamserver_desktop_native.dylib \
      "$APP_PATH/Contents/MacOS/"
    if command -v codesign >/dev/null 2>&1; then
      codesign --force --deep --sign - --entitlements macos/Runner/Release.entitlements "$APP_PATH"
      codesign --verify --deep --strict "$APP_PATH"
    fi
    ARTIFACT="streamserver-desktop-macos-$STAMP.tar.gz"
    tar -C build/macos/Build/Products/Release -czf "dist/$ARTIFACT" streamserver_desktop.app
    ;;
  linux)
    cp build/native/libstreamserver_desktop_native.so build/linux/x64/release/bundle/
    ARTIFACT="streamserver-desktop-linux-x64-$STAMP.tar.gz"
    tar -C build/linux/x64/release/bundle -czf "dist/$ARTIFACT" .
    ;;
  windows)
    cp build/native/streamserver_desktop_native.dll build/windows/x64/runner/Release/
    ARTIFACT="streamserver-desktop-windows-x64-$STAMP.zip"
    (cd build/windows/x64/runner/Release && zip -r "../../../../../dist/$ARTIFACT" .)
    ;;
  *)
    echo "unsupported target platform: $TARGET_PLATFORM" >&2
    exit 2
    ;;
esac

(cd dist && shasum -a 256 "$ARTIFACT" > SHA256SUMS)
echo "offline package written to dist/"
