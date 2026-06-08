#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
CLIENT_DIR="$ROOT/clients/streamserver-desktop"
OUT_DIR="$CLIENT_DIR/build/native"

mkdir -p "$OUT_DIR"

cargo build -p streamserver-desktop-native --release

case "$(uname -s)" in
  Darwin)
    cp "$ROOT/target/release/libstreamserver_desktop_native.dylib" "$OUT_DIR/"
    ;;
  Linux)
    cp "$ROOT/target/release/libstreamserver_desktop_native.so" "$OUT_DIR/"
    ;;
  MINGW*|MSYS*|CYGWIN*)
    cp "$ROOT/target/release/streamserver_desktop_native.dll" "$OUT_DIR/"
    ;;
  *)
    echo "unsupported host platform: $(uname -s)" >&2
    exit 1
    ;;
esac

(cd "$OUT_DIR" && shasum -a 256 * > SHA256SUMS)
echo "native library written to $OUT_DIR"
