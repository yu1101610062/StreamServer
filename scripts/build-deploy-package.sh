#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
ALLOW_MISSING_CLIENTS=0
PACKAGE_ARGS=()

usage() {
  cat <<'EOF'
用法:
  ./scripts/build-deploy-package.sh [--allow-missing-clients] [package-offline-bundle 参数...]

说明:
  构建离线部署包。脚本会先构建前端静态资源，并内置 Windows/macOS 桌面客户端安装包；
  然后调用 scripts/package-offline-bundle.sh 生成部署包。

常用示例:
  ./scripts/build-deploy-package.sh --with-gpu
  ./scripts/build-deploy-package.sh --without-gpu --output-dir ./dist
  ./scripts/build-deploy-package.sh --allow-missing-clients --skip-images
EOF
}

log() {
  printf '[deploy-package-build] %s\n' "$*"
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --allow-missing-clients)
      ALLOW_MISSING_CLIENTS=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      PACKAGE_ARGS+=("$1")
      shift
      ;;
  esac
done

frontend_args=()
if [ "${ALLOW_MISSING_CLIENTS}" -eq 1 ]; then
  frontend_args+=(--allow-missing-clients)
fi

log "构建前端静态资源"
node "${ROOT_DIR}/scripts/build-frontend-with-desktop-clients.mjs" "${frontend_args[@]}"

log "构建离线部署包"
PREBUILT_UI_DIR="${ROOT_DIR}/crates/media-core/ui" \
  "${ROOT_DIR}/scripts/package-offline-bundle.sh" "${PACKAGE_ARGS[@]}"
