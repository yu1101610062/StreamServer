#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
CLIENT_DIR="$ROOT/clients/streamserver-desktop"
OUT_DIR="$CLIENT_DIR/build/native"

mkdir -p "$OUT_DIR"
rm -f \
  "$OUT_DIR"/SHA256SUMS \
  "$OUT_DIR"/libstreamserver_desktop*.dylib \
  "$OUT_DIR"/libstreamserver_desktop*.so \
  "$OUT_DIR"/streamserver_desktop*.dll

cargo build -p streamserver-desktop --release

case "$(uname -s)" in
  Darwin)
    cp "$ROOT/target/release/libstreamserver_desktop.dylib" "$OUT_DIR/"
    ;;
  Linux)
    cp "$ROOT/target/release/libstreamserver_desktop.so" "$OUT_DIR/"
    ;;
  MINGW*|MSYS*|CYGWIN*)
    cp "$ROOT/target/release/streamserver_desktop.dll" "$OUT_DIR/"
    ;;
  *)
    echo "unsupported host platform: $(uname -s)" >&2
    exit 1
    ;;
esac

(cd "$OUT_DIR" && shasum -a 256 * > SHA256SUMS)
echo "native library written to $OUT_DIR"
