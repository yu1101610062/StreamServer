#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
OUTPUT_DIR="${ROOT_DIR}/dist"

NPM_REGISTRY_MIRROR="${NPM_REGISTRY_MIRROR:-}"
FRONTEND_BUILDER_IMAGE="${FRONTEND_BUILDER_IMAGE:-node:22-bookworm}"

usage() {
  cat <<'EOF'
用法:
  ./scripts/export-ui-zip.sh [--output-dir DIR]

说明:
  只导出 media-core 前端静态资源，并打包为 ui.zip。

参数:
  --output-dir DIR   输出目录，默认 ./dist
EOF
}

log() {
  printf '[ui-zip-export] %s\n' "$*"
}

fail() {
  printf '[ui-zip-export] ERROR: %s\n' "$*" >&2
  exit 1
}

require_cmd() {
  command -v "$1" >/dev/null 2>&1 || fail "缺少命令: $1"
}

docker_buildx_available() {
  docker buildx version >/dev/null 2>&1
}

ensure_tools() {
  require_cmd docker
  docker info >/dev/null 2>&1 || fail "Docker 不可用，请先启动 Docker Desktop 或 Docker Engine"
  docker_buildx_available || fail "缺少 docker buildx"
  require_cmd zip
  require_cmd mktemp
}

parse_args() {
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --output-dir)
        [ "$#" -ge 2 ] || fail "--output-dir 需要参数"
        OUTPUT_DIR="$2"
        shift 2
        ;;
      -h|--help)
        usage
        exit 0
        ;;
      *)
        fail "未知参数: $1"
        ;;
    esac
  done
}

main() {
  local staging_dir=""
  local raw_output_dir=""
  local zip_path=""

  parse_args "$@"
  ensure_tools

  mkdir -p "${OUTPUT_DIR}"
  staging_dir="$(mktemp -d "${TMPDIR:-/tmp}/streamserver-ui.XXXXXX")"
  trap 'rm -rf "${staging_dir}"' EXIT

  raw_output_dir="${staging_dir}/raw-ui"
  mkdir -p "${raw_output_dir}"

  log "导出 ui (media-ui-export)"
  docker buildx build \
    --platform linux/amd64 \
    --target media-ui-export \
    --build-arg NPM_REGISTRY_MIRROR="${NPM_REGISTRY_MIRROR}" \
    --build-arg FRONTEND_BUILDER_IMAGE="${FRONTEND_BUILDER_IMAGE}" \
    --output "type=local,dest=${raw_output_dir}" \
    "${ROOT_DIR}"

  [ -d "${raw_output_dir}/ui" ] || fail "未找到导出的 ui 目录: ${raw_output_dir}/ui"

  zip_path="${OUTPUT_DIR}/ui.zip"
  rm -f "${zip_path}"
  log "压缩 ui 目录 -> ${zip_path}"
  (
    cd "${raw_output_dir}"
    zip -qry "${zip_path}" ui
  )

  log "UI 压缩包已生成:"
  log "  ${zip_path}"
}

main "$@"
