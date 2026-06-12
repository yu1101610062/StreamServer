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
if [[ "$TARGET_PLATFORM" == "macos" ]]; then
  flutter config --no-enable-swift-package-manager >/dev/null
fi
flutter build "$TARGET_PLATFORM" --release

ensure_appimagetool() {
  if command -v appimagetool >/dev/null 2>&1; then
    command -v appimagetool
    return
  fi

  local cache_dir="${XDG_CACHE_HOME:-$HOME/.cache}/streamserver-desktop"
  local tool="${cache_dir}/appimagetool-x86_64.AppImage"
  if [[ ! -x "$tool" ]]; then
    local tmp_tool="${tool}.tmp"
    mkdir -p "$cache_dir"
    if ! curl -LfsS --retry 3 --retry-delay 2 \
      https://github.com/AppImage/AppImageKit/releases/download/continuous/appimagetool-x86_64.AppImage \
      -o "$tmp_tool"; then
      rm -f "$tmp_tool"
      echo "failed to download appimagetool" >&2
      return 1
    fi
    mv "$tmp_tool" "$tool"
    chmod 755 "$tool"
  fi
  printf '%s\n' "$tool"
}

build_linux_appimage() {
  local release_dir="$1"
  local stamp="$2"
  local version safe_version stage appdir appimage appimagetool

  version="$(awk '/^version:/ { print $2; exit }' pubspec.yaml)"
  safe_version="${version//+/-}"
  stage="$(mktemp -d)"
  trap 'rm -rf "$stage"' RETURN

  appimagetool="$(ensure_appimagetool)"
  appdir="${stage}/StreamServerDesktop.AppDir"
  appimage="dist/streamserver-desktop-linux-x86_64-${safe_version}-${stamp}.AppImage"

  mkdir -p "${appdir}/opt/streamserver-desktop"
  cp -a "${release_dir}/." "${appdir}/opt/streamserver-desktop/"
  cp linux/runner/resources/streamserver-console.png \
    "${appdir}/streamserver-console.png"
  cp linux/runner/resources/streamserver-console.png "${appdir}/.DirIcon"

  cat >"${appdir}/AppRun" <<'EOF'
#!/usr/bin/env sh
set -eu

here="$(dirname "$(readlink -f "$0")")"
cd "${here}/opt/streamserver-desktop"
exec "${here}/opt/streamserver-desktop/streamserver_desktop" "$@"
EOF
  chmod 755 "${appdir}/AppRun"

  cat >"${appdir}/streamserver-desktop.desktop" <<'EOF'
[Desktop Entry]
Type=Application
Name=StreamServer控制台
Exec=streamserver-desktop
Icon=streamserver-console
Terminal=false
Categories=AudioVideo;Video;
EOF

  APPIMAGE_EXTRACT_AND_RUN=1 ARCH=x86_64 "$appimagetool" \
    "$appdir" \
    "$appimage"
  (cd dist && sha256sum "$(basename "$appimage")" >> SHA256SUMS)
  echo "AppImage written to $appimage"
}

mkdir -p dist
STAMP="$(date +%Y%m%d-%H%M%S)"
ARTIFACT=""

case "$TARGET_PLATFORM" in
  macos)
    APP_PATH="build/macos/Build/Products/Release/streamserver_desktop.app"
    mkdir -p "$APP_PATH/Contents/Frameworks"
    rm -f "$APP_PATH/Contents/Frameworks"/libstreamserver_desktop*.dylib
    cp build/native/libstreamserver_desktop.dylib \
      "$APP_PATH/Contents/Frameworks/"
    if command -v codesign >/dev/null 2>&1; then
      codesign --force --sign - "$APP_PATH/Contents/Frameworks/libstreamserver_desktop.dylib"
      codesign --force --deep --sign - --entitlements macos/Runner/Release.entitlements "$APP_PATH"
      codesign --verify --deep --strict "$APP_PATH"
    fi
    ARTIFACT="streamserver-desktop-macos-$STAMP.tar.gz"
    tar -C build/macos/Build/Products/Release -czf "dist/$ARTIFACT" streamserver_desktop.app
    ;;
  linux)
    rm -f build/linux/x64/release/bundle/libstreamserver_desktop*.so
    cp build/native/libstreamserver_desktop.so build/linux/x64/release/bundle/
    ARTIFACT="streamserver-desktop-linux-x64-$STAMP.tar.gz"
    tar -C build/linux/x64/release/bundle -czf "dist/$ARTIFACT" .
    ;;
  windows)
    rm -f build/windows/x64/runner/Release/streamserver_desktop*.dll
    cp build/native/streamserver_desktop.dll build/windows/x64/runner/Release/
    ARTIFACT="streamserver-desktop-windows-x64-$STAMP.zip"
    (cd build/windows/x64/runner/Release && zip -r "../../../../../dist/$ARTIFACT" .)
    ;;
  *)
    echo "unsupported target platform: $TARGET_PLATFORM" >&2
    exit 2
    ;;
esac

(cd dist && shasum -a 256 "$ARTIFACT" > SHA256SUMS)
if [[ "$TARGET_PLATFORM" == "linux" ]]; then
  build_linux_appimage "build/linux/x64/release/bundle" "$STAMP"
fi
echo "offline package written to dist/"
