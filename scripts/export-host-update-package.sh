#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
OUTPUT_DIR="${ROOT_DIR}/dist"
PACKAGE_NAME=""
COMPONENTS=()
STAGING_DIR=""

APT_MIRROR="${APT_MIRROR:-}"
UBUNTU_APT_MIRROR="${UBUNTU_APT_MIRROR:-}"
CARGO_REGISTRY_MIRROR="${CARGO_REGISTRY_MIRROR:-}"
NPM_REGISTRY_MIRROR="${NPM_REGISTRY_MIRROR:-}"
FRONTEND_BUILDER_IMAGE="${FRONTEND_BUILDER_IMAGE:-node:22-bookworm}"
RUST_BUILDER_IMAGE="${RUST_BUILDER_IMAGE:-rust:1.85-bookworm}"
MEDIA_CORE_RUNTIME_BASE_IMAGE="${MEDIA_CORE_RUNTIME_BASE_IMAGE:-debian:bookworm-slim}"
MEDIA_AGENT_RUNTIME_BASE_IMAGE="${MEDIA_AGENT_RUNTIME_BASE_IMAGE:-jrottenberg/ffmpeg:7.1-ubuntu2404}"
MEDIA_AGENT_GPU_RUNTIME_BASE_IMAGE="${MEDIA_AGENT_GPU_RUNTIME_BASE_IMAGE:-jrottenberg/ffmpeg:7.1-nvidia2204}"

usage() {
  cat <<'EOF'
用法:
  ./scripts/export-host-update-package.sh --component media-core [--component ui] [--output-dir DIR]
  ./scripts/export-host-update-package.sh --all

说明:
  导出宿主机挂载场景下的独立更新包。只会构建所选组件，不再强制同时编译
  media-core、media-agent 和前端。

参数:
  --component NAME   需要导出的组件，可重复。支持: media-core, media-agent, ui
  --all              导出全部组件
  --output-dir DIR   输出目录，默认 ./dist
  --package-name X   自定义解压后的目录名
EOF
}

log() {
  printf '[host-update-package] %s\n' "$*"
}

fail() {
  printf '[host-update-package] ERROR: %s\n' "$*" >&2
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
  require_cmd tar
  require_cmd cp
  require_cmd chmod
  require_cmd mktemp
  if ! command -v shasum >/dev/null 2>&1 && ! command -v sha256sum >/dev/null 2>&1; then
    fail "缺少校验和命令: shasum 或 sha256sum"
  fi
}

workspace_version() {
  awk '
    /^\[workspace.package\]/ { in_section = 1; next }
    /^\[/ && in_section { in_section = 0 }
    in_section && /^version = / {
      gsub(/"/, "", $3)
      print $3
      exit
    }
  ' "${ROOT_DIR}/Cargo.toml"
}

sha256_file() {
  local file="$1"
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "${file}"
  else
    sha256sum "${file}"
  fi
}

append_component() {
  local component="$1"
  local existing
  if [ "${#COMPONENTS[@]}" -gt 0 ]; then
    for existing in "${COMPONENTS[@]}"; do
      if [ "${existing}" = "${component}" ]; then
        return 0
      fi
    done
  fi
  COMPONENTS+=("${component}")
}

parse_args() {
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --component)
        [ "$#" -ge 2 ] || fail "--component 需要参数"
        case "$2" in
          media-core|media-agent|ui) append_component "$2" ;;
          *) fail "不支持的组件: $2" ;;
        esac
        shift 2
        ;;
      --all)
        append_component "media-core"
        append_component "media-agent"
        append_component "ui"
        shift
        ;;
      --output-dir)
        [ "$#" -ge 2 ] || fail "--output-dir 需要参数"
        OUTPUT_DIR="$2"
        shift 2
        ;;
      --package-name)
        [ "$#" -ge 2 ] || fail "--package-name 需要参数"
        PACKAGE_NAME="$2"
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

build_component() {
  local component="$1"
  local raw_output_dir="$2"
  local package_root="$3"
  local target=""
  local exported_name=""

  case "${component}" in
    media-core)
      target="media-core-bin-export"
      exported_name="media-core"
      ;;
    media-agent)
      target="media-agent-bin-export"
      exported_name="media-agent"
      ;;
    ui)
      target="media-ui-export"
      exported_name="ui"
      ;;
    *)
      fail "未知组件: ${component}"
      ;;
  esac

  mkdir -p "${raw_output_dir}"
  log "导出 ${component} (${target})"
  docker buildx build \
    --platform linux/amd64 \
    --target "${target}" \
    --build-arg DEBIAN_MIRROR="${APT_MIRROR}" \
    --build-arg UBUNTU_MIRROR="${UBUNTU_APT_MIRROR}" \
    --build-arg CARGO_REGISTRY_MIRROR="${CARGO_REGISTRY_MIRROR}" \
    --build-arg NPM_REGISTRY_MIRROR="${NPM_REGISTRY_MIRROR}" \
    --build-arg FRONTEND_BUILDER_IMAGE="${FRONTEND_BUILDER_IMAGE}" \
    --build-arg RUST_BUILDER_IMAGE="${RUST_BUILDER_IMAGE}" \
    --build-arg MEDIA_CORE_RUNTIME_BASE_IMAGE="${MEDIA_CORE_RUNTIME_BASE_IMAGE}" \
    --build-arg MEDIA_AGENT_RUNTIME_BASE_IMAGE="${MEDIA_AGENT_RUNTIME_BASE_IMAGE}" \
    --build-arg MEDIA_AGENT_GPU_RUNTIME_BASE_IMAGE="${MEDIA_AGENT_GPU_RUNTIME_BASE_IMAGE}" \
    --output "type=local,dest=${raw_output_dir}" \
    "${ROOT_DIR}"

  case "${component}" in
    media-core)
      mkdir -p "${package_root}/binaries"
      cp "${raw_output_dir}/${exported_name}" "${package_root}/binaries/media-core-linux-amd64"
      chmod +x "${package_root}/binaries/media-core-linux-amd64"
      ;;
    media-agent)
      mkdir -p "${package_root}/binaries"
      cp "${raw_output_dir}/${exported_name}" "${package_root}/binaries/media-agent-linux-amd64"
      chmod +x "${package_root}/binaries/media-agent-linux-amd64"
      ;;
    ui)
      mkdir -p "${package_root}/ui"
      cp -a "${raw_output_dir}/${exported_name}" "${package_root}/ui/media-core"
      ;;
  esac
}

write_manifest() {
  local package_root="$1"
  local version="$2"
  local built_at="$3"
  local components_csv="$4"

  cat >"${package_root}/update-manifest.env" <<EOF
PACKAGE_NAME=${PACKAGE_NAME}
PACKAGE_VERSION=${version}
PACKAGE_COMPONENTS=${components_csv}
BUILT_AT=${built_at}
MEDIA_CORE_BINARY_PATH=binaries/media-core-linux-amd64
MEDIA_AGENT_BINARY_PATH=binaries/media-agent-linux-amd64
MEDIA_CORE_UI_PATH=ui/media-core
EOF
}

write_build_info() {
  local package_root="$1"
  local version="$2"
  local built_at="$3"
  local components_csv="$4"

  cat >"${package_root}/build-info.txt" <<EOF
package_name=${PACKAGE_NAME}
package_version=${version}
built_at=${built_at}
builder_os=$(uname -s)
builder_arch=$(uname -m)
components=${components_csv}
git_commit=$(git -C "${ROOT_DIR}" rev-parse HEAD 2>/dev/null || printf 'unknown')
EOF
}

main() {
  local version=""
  local timestamp=""
  local package_root=""
  local tarball_path=""
  local components_csv=""
  local component=""
  local raw_output_dir=""

  parse_args "$@"
  ensure_tools

  [ "${#COMPONENTS[@]}" -gt 0 ] || fail "请至少指定一个 --component，或使用 --all"

  version="$(workspace_version)"
  [ -n "${version}" ] || fail "无法读取 workspace 版本号"

  timestamp="$(date '+%Y%m%d-%H%M%S')"
  if [ -z "${PACKAGE_NAME}" ]; then
    components_csv="$(printf '%s\n' "${COMPONENTS[@]}" | paste -sd ',' -)"
    PACKAGE_NAME="streamserver-host-update-v${version}-linux-amd64-${components_csv//,/+}-${timestamp}"
  else
    components_csv="$(printf '%s\n' "${COMPONENTS[@]}" | paste -sd ',' -)"
  fi

  mkdir -p "${OUTPUT_DIR}"
  STAGING_DIR="$(mktemp -d "${TMPDIR:-/tmp}/streamserver-host-update.XXXXXX")"
  trap 'rm -rf "${STAGING_DIR}"' EXIT

  package_root="${STAGING_DIR}/${PACKAGE_NAME}"
  mkdir -p "${package_root}/docs"

  for component in "${COMPONENTS[@]}"; do
    raw_output_dir="${STAGING_DIR}/raw-${component}"
    build_component "${component}" "${raw_output_dir}" "${package_root}"
  done

  cp "${ROOT_DIR}/packaging/offline/apply-host-update.sh" "${package_root}/apply-host-update.sh"
  chmod +x "${package_root}/apply-host-update.sh"
  cp "${ROOT_DIR}/docs/17-离线部署打包与安装.md" "${package_root}/docs/"

  write_manifest "${package_root}" "${version}" "$(date -u '+%Y-%m-%dT%H:%M:%SZ')" "${components_csv}"
  write_build_info "${package_root}" "${version}" "$(date -u '+%Y-%m-%dT%H:%M:%SZ')" "${components_csv}"

  tarball_path="${OUTPUT_DIR}/${PACKAGE_NAME}.tar.gz"
  tar -C "${STAGING_DIR}" -czf "${tarball_path}" "${PACKAGE_NAME}"
  sha256_file "${tarball_path}" > "${tarball_path}.sha256"

  log "更新包已生成:"
  log "  ${tarball_path}"
  log "  ${tarball_path}.sha256"
}

main "$@"
