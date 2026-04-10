#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
OUTPUT_DIR="${ROOT_DIR}/dist"
SKIP_IMAGES=0

APT_MIRROR="${APT_MIRROR:-http://mirrors.tuna.tsinghua.edu.cn}"
CARGO_REGISTRY_MIRROR="${CARGO_REGISTRY_MIRROR:-sparse+https://rsproxy.cn/index/}"
POSTGRES_SOURCE_IMAGE="${POSTGRES_SOURCE_IMAGE:-postgres:16.4-bookworm@sha256:e62fbf9d3e2b49816a32c400ed2dba83e3b361e6833e624024309c35d334b412}"
ZLM_SOURCE_IMAGE="${ZLM_SOURCE_IMAGE:-zlmediakit/zlmediakit:master@sha256:8b24d1d4a30736b2001e5d78fc46057cb3abf4cae527818f238678826537389f}"

log() {
  printf '[offline-package] %s\n' "$*"
}

fail() {
  printf '[offline-package] ERROR: %s\n' "$*" >&2
  exit 1
}

require_cmd() {
  command -v "$1" >/dev/null 2>&1 || fail "缺少命令: $1"
}

usage() {
  cat <<EOF
用法:
  $(basename "$0") [--output-dir DIR] [--skip-images]

说明:
  在 macOS arm64 或 Linux 主机上构建 Linux AMD64 离线部署包。
  默认输出到 ./dist。

环境变量:
  APT_MIRROR             默认 http://mirrors.tuna.tsinghua.edu.cn；设为空则保留 Debian 官方源。
  CARGO_REGISTRY_MIRROR  默认 sparse+https://rsproxy.cn/index/；设为空则使用 crates.io 官方源。
  POSTGRES_SOURCE_IMAGE  可覆盖 PostgreSQL 拉取源；脚本会优先复用本地已有的 linux/amd64 镜像，不存在时才联网拉取。
  ZLM_SOURCE_IMAGE       可覆盖 ZLMediaKit 拉取源；脚本会优先复用本地已有的 linux/amd64 镜像，不存在时才联网拉取。
EOF
}

parse_args() {
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --output-dir)
        [ "$#" -ge 2 ] || fail "--output-dir 需要参数"
        OUTPUT_DIR="$2"
        shift 2
        ;;
      --skip-images)
        SKIP_IMAGES=1
        shift
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

ensure_supported_packaging_host() {
  local host_os
  local host_arch

  host_os="$(uname -s)"
  host_arch="$(uname -m)"

  case "${host_os}" in
    Darwin)
      [ "${host_arch}" = "arm64" ] || fail "macOS 打包仍要求在 macOS arm64 上运行"
      ;;
    Linux)
      ;;
    *)
      fail "打包脚本只支持在 macOS arm64 或 Linux 主机上运行"
      ;;
  esac
}

docker_buildx_available() {
  docker buildx version >/dev/null 2>&1
}

ensure_tools() {
  require_cmd docker
  docker info >/dev/null 2>&1 || fail "Docker 不可用，请先启动 Docker Desktop 或 Docker Engine"
  require_cmd openssl
  require_cmd tar
  if ! command -v shasum >/dev/null 2>&1 && ! command -v sha256sum >/dev/null 2>&1; then
    fail "缺少校验和命令: shasum 或 sha256sum"
  fi
  if [ "${SKIP_IMAGES}" -eq 0 ] && ! docker_buildx_available; then
    fail "缺少 docker buildx；完整打包需要 docker buildx，--skip-images 可忽略此依赖"
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

verify_loaded_image_arch() {
  local image_ref="$1"
  local platform
  platform="$(docker image inspect "${image_ref}" --format '{{.Os}}/{{.Architecture}}' 2>/dev/null || true)"
  [ "${platform}" = "linux/amd64" ] || fail "镜像 ${image_ref} 平台不是 linux/amd64，而是 ${platform:-unknown}"
}

local_source_candidate() {
  local image_ref="$1"
  if docker image inspect "${image_ref}" >/dev/null 2>&1; then
    printf '%s\n' "${image_ref}"
    return 0
  fi
  if [[ "${image_ref}" == *"@"* ]]; then
    local tag_ref="${image_ref%@*}"
    if docker image inspect "${tag_ref}" >/dev/null 2>&1; then
      printf '%s\n' "${tag_ref}"
      return 0
    fi
  fi
  return 1
}

prepare_source_image() {
  local source_image="$1"
  local target_image="$2"
  local label="$3"
  local local_candidate=""
  local platform=""

  if local_candidate="$(local_source_candidate "${source_image}")"; then
    platform="$(docker image inspect "${local_candidate}" --format '{{.Os}}/{{.Architecture}}' 2>/dev/null || true)"
    if [ "${platform}" = "linux/amd64" ]; then
      log "复用本地 ${label} 镜像: ${local_candidate}"
      docker tag "${local_candidate}" "${target_image}"
      verify_loaded_image_arch "${target_image}"
      return 0
    fi
    log "本地 ${label} 镜像存在但平台是 ${platform:-unknown}，改为联网拉取 linux/amd64"
  else
    log "本地未发现 ${label} 镜像，联网拉取 linux/amd64"
  fi

  docker pull --platform linux/amd64 "${source_image}" >/dev/null
  docker tag "${source_image}" "${target_image}"
  verify_loaded_image_arch "${target_image}"
}

build_or_pull_images() {
  local media_core_image="$1"
  local media_agent_image="$2"
  local postgres_image="$3"
  local zlm_image="$4"

  log "构建 media-core linux/amd64 镜像"
  docker buildx build \
    --platform linux/amd64 \
    --target media-core-runtime \
    --build-arg DEBIAN_MIRROR="${APT_MIRROR}" \
    --build-arg CARGO_REGISTRY_MIRROR="${CARGO_REGISTRY_MIRROR}" \
    --load \
    -t "${media_core_image}" \
    "${ROOT_DIR}"
  verify_loaded_image_arch "${media_core_image}"

  log "构建 media-agent linux/amd64 镜像"
  docker buildx build \
    --platform linux/amd64 \
    --target media-agent-runtime \
    --build-arg DEBIAN_MIRROR="${APT_MIRROR}" \
    --build-arg CARGO_REGISTRY_MIRROR="${CARGO_REGISTRY_MIRROR}" \
    --load \
    -t "${media_agent_image}" \
    "${ROOT_DIR}"
  verify_loaded_image_arch "${media_agent_image}"

  prepare_source_image "${POSTGRES_SOURCE_IMAGE}" "${postgres_image}" "postgres"

  prepare_source_image "${ZLM_SOURCE_IMAGE}" "${zlm_image}" "ZLMediaKit"
}

write_manifest() {
  local bundle_root="$1"
  local bundle_version="$2"
  local media_core_image="$3"
  local media_agent_image="$4"
  local postgres_image="$5"
  local zlm_image="$6"

  cat >"${bundle_root}/package-manifest.env" <<EOF
BUNDLE_VERSION=${bundle_version}
POSTGRES_IMAGE=${postgres_image}
POSTGRES_IMAGE_ARCHIVE=images/postgres-linux-amd64.tar
MEDIA_CORE_IMAGE=${media_core_image}
MEDIA_CORE_IMAGE_ARCHIVE=images/media-core-linux-amd64.tar
MEDIA_AGENT_IMAGE=${media_agent_image}
MEDIA_AGENT_IMAGE_ARCHIVE=images/media-agent-linux-amd64.tar
ZLM_IMAGE=${zlm_image}
ZLM_IMAGE_ARCHIVE=images/zlmediakit-linux-amd64.tar
EOF
}

write_build_info() {
  local bundle_root="$1"
  local bundle_name="$2"
  local version="$3"
  local commit

  commit="$(git -C "${ROOT_DIR}" rev-parse --short HEAD 2>/dev/null || true)"

  cat >"${bundle_root}/build-info.txt" <<EOF
bundle_name=${bundle_name}
version=${version}
built_at=$(date -u '+%Y-%m-%dT%H:%M:%SZ')
builder_os=$(uname -s)
builder_arch=$(uname -m)
git_commit=${commit}
EOF
}

copy_static_assets() {
  local bundle_root="$1"

  mkdir -p "${bundle_root}/templates"
  mkdir -p "${bundle_root}/docs"
  cp "${ROOT_DIR}/packaging/offline/install.sh" "${bundle_root}/install.sh"
  chmod +x "${bundle_root}/install.sh"
  cp -R "${ROOT_DIR}/packaging/offline/templates/." "${bundle_root}/templates/"
  cp "${ROOT_DIR}/docs/17-离线部署打包与安装.md" "${bundle_root}/docs/"
}

generate_self_signed_certs() {
  local bundle_root="$1"
  local cert_dir="${bundle_root}/certs/self-signed"
  local ca_key="${cert_dir}/ca.key"
  local ca_pem="${cert_dir}/ca.pem"
  local core_key="${cert_dir}/media-core.key"
  local core_csr="${cert_dir}/media-core.csr"
  local core_pem="${cert_dir}/media-core.pem"
  local agent_key="${cert_dir}/media-agent.key"
  local agent_csr="${cert_dir}/media-agent.csr"
  local agent_pem="${cert_dir}/media-agent.pem"
  local https_key="${cert_dir}/https.key"
  local https_csr="${cert_dir}/https.csr"
  local https_pem="${cert_dir}/https.pem"
  local core_ext="${cert_dir}/media-core.ext"
  local agent_ext="${cert_dir}/media-agent.ext"
  local https_ext="${cert_dir}/https.ext"

  mkdir -p "${cert_dir}"

  openssl genrsa -out "${ca_key}" 2048 >/dev/null 2>&1
  openssl req -x509 -new -nodes \
    -key "${ca_key}" \
    -sha256 \
    -days 3650 \
    -out "${ca_pem}" \
    -subj "/CN=StreamServer Offline Dev CA" >/dev/null 2>&1

  cat >"${core_ext}" <<EOF
subjectAltName=DNS:streamserver-core.local,DNS:media-core,DNS:localhost,IP:127.0.0.1
extendedKeyUsage=serverAuth
keyUsage=digitalSignature,keyEncipherment
EOF
  openssl genrsa -out "${core_key}" 2048 >/dev/null 2>&1
  openssl req -new -key "${core_key}" -out "${core_csr}" -subj "/CN=streamserver-core.local" >/dev/null 2>&1
  openssl x509 -req \
    -in "${core_csr}" \
    -CA "${ca_pem}" \
    -CAkey "${ca_key}" \
    -CAcreateserial \
    -out "${core_pem}" \
    -days 825 \
    -sha256 \
    -extfile "${core_ext}" >/dev/null 2>&1

  cat >"${agent_ext}" <<EOF
subjectAltName=DNS:streamserver-agent.local,DNS:media-agent,DNS:localhost,IP:127.0.0.1
extendedKeyUsage=clientAuth
keyUsage=digitalSignature,keyEncipherment
EOF
  openssl genrsa -out "${agent_key}" 2048 >/dev/null 2>&1
  openssl req -new -key "${agent_key}" -out "${agent_csr}" -subj "/CN=streamserver-agent.local" >/dev/null 2>&1
  openssl x509 -req \
    -in "${agent_csr}" \
    -CA "${ca_pem}" \
    -CAkey "${ca_key}" \
    -CAcreateserial \
    -out "${agent_pem}" \
    -days 825 \
    -sha256 \
    -extfile "${agent_ext}" >/dev/null 2>&1

  cat >"${https_ext}" <<EOF
subjectAltName=DNS:streamserver-web.local,DNS:localhost,IP:127.0.0.1
extendedKeyUsage=serverAuth
keyUsage=digitalSignature,keyEncipherment
EOF
  openssl genrsa -out "${https_key}" 2048 >/dev/null 2>&1
  openssl req -new -key "${https_key}" -out "${https_csr}" -subj "/CN=streamserver-web.local" >/dev/null 2>&1
  openssl x509 -req \
    -in "${https_csr}" \
    -CA "${ca_pem}" \
    -CAkey "${ca_key}" \
    -CAcreateserial \
    -out "${https_pem}" \
    -days 825 \
    -sha256 \
    -extfile "${https_ext}" >/dev/null 2>&1

  rm -f \
    "${core_csr}" \
    "${agent_csr}" \
    "${https_csr}" \
    "${core_ext}" \
    "${agent_ext}" \
    "${https_ext}" \
    "${cert_dir}/ca.srl"

  cat >"${bundle_root}/certs/README.md" <<EOF
# Self-Signed Certificates

该目录内预置了一套离线测试用自签名证书：

- \`self-signed/ca.pem\` / \`ca.key\`: 测试 CA
- \`self-signed/media-core.pem\` / \`media-core.key\`: gRPC mTLS 服务端证书
- \`self-signed/media-agent.pem\` / \`media-agent.key\`: gRPC mTLS 客户端证书
- \`self-signed/https.pem\` / \`https.key\`: 给前置反向代理测试 HTTPS 用的服务端证书

注意：

- HTTPS 和 mTLS 在安装模板中默认关闭。
- 这些自签名证书仅用于离线测试或内网临时验证。
- 如果现场已有正式证书，安装时应优先替换为正式证书。
EOF
}

save_images() {
  local bundle_root="$1"
  local media_core_image="$2"
  local media_agent_image="$3"
  local postgres_image="$4"
  local zlm_image="$5"

  mkdir -p "${bundle_root}/images"
  log "导出离线镜像包"
  docker save -o "${bundle_root}/images/media-core-linux-amd64.tar" "${media_core_image}"
  docker save -o "${bundle_root}/images/media-agent-linux-amd64.tar" "${media_agent_image}"
  docker save -o "${bundle_root}/images/postgres-linux-amd64.tar" "${postgres_image}"
  docker save -o "${bundle_root}/images/zlmediakit-linux-amd64.tar" "${zlm_image}"
}

cleanup_macos_metadata() {
  local bundle_root="$1"
  find "${bundle_root}" \( -name '.DS_Store' -o -name '._*' \) -delete
  if command -v xattr >/dev/null 2>&1; then
    xattr -rc "${bundle_root}" 2>/dev/null || true
  fi
}

create_archive() {
  local stage_dir="$1"
  local bundle_name="$2"
  local archive_path="$3"

  cleanup_macos_metadata "${stage_dir}/${bundle_name}"
  mkdir -p "$(dirname "${archive_path}")"
  COPYFILE_DISABLE=1 tar \
    --exclude '.DS_Store' \
    --exclude '._*' \
    -czf "${archive_path}" \
    -C "${stage_dir}" \
    "${bundle_name}"
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "${archive_path}" >"${archive_path}.sha256"
  else
    sha256sum "${archive_path}" >"${archive_path}.sha256"
  fi
}

main() {
  local version
  local timestamp
  local bundle_name
  local bundle_version
  local media_core_image
  local media_agent_image
  local postgres_image
  local zlm_image
  local stage_dir
  local bundle_root
  local archive_path

  parse_args "$@"
  ensure_supported_packaging_host
  ensure_tools

  if [ -n "${APT_MIRROR}" ]; then
    log "使用 APT 镜像: ${APT_MIRROR}"
  else
    log "APT 使用 Debian 官方源"
  fi

  if [ -n "${CARGO_REGISTRY_MIRROR}" ]; then
    log "使用 Cargo 镜像: ${CARGO_REGISTRY_MIRROR}"
  else
    log "Cargo 使用 crates.io 官方源"
  fi

  version="$(workspace_version)"
  [ -n "${version}" ] || fail "无法从 Cargo.toml 解析版本号"
  timestamp="$(date '+%Y%m%d%H%M%S')"
  bundle_version="v${version}"
  bundle_name="streamserver-offline-${bundle_version}-linux-amd64-${timestamp}"

  media_core_image="streamserver/media-core:${version}-linux-amd64"
  media_agent_image="streamserver/media-agent:${version}-linux-amd64"
  postgres_image="streamserver/postgres:16.4-bookworm-linux-amd64"
  zlm_image="streamserver/zlmediakit:master-linux-amd64"

  stage_dir="$(mktemp -d "${TMPDIR:-/tmp}/streamserver-offline.XXXXXX")"
  bundle_root="${stage_dir}/${bundle_name}"
  archive_path="${OUTPUT_DIR}/${bundle_name}.tar.gz"

  mkdir -p "${bundle_root}"

  if [ "${SKIP_IMAGES}" -eq 0 ]; then
    build_or_pull_images "${media_core_image}" "${media_agent_image}" "${postgres_image}" "${zlm_image}"
  else
    log "跳过镜像构建与导出，仅生成骨架包"
    mkdir -p "${bundle_root}/images"
    echo "此包由 --skip-images 生成，未包含任何镜像。" >"${bundle_root}/images/SKIPPED.txt"
  fi

  copy_static_assets "${bundle_root}"
  generate_self_signed_certs "${bundle_root}"
  write_manifest "${bundle_root}" "${bundle_version}" "${media_core_image}" "${media_agent_image}" "${postgres_image}" "${zlm_image}"
  write_build_info "${bundle_root}" "${bundle_name}" "${version}"

  if [ "${SKIP_IMAGES}" -eq 0 ]; then
    save_images "${bundle_root}" "${media_core_image}" "${media_agent_image}" "${postgres_image}" "${zlm_image}"
  fi

  create_archive "${stage_dir}" "${bundle_name}" "${archive_path}"

  log "离线包已生成: ${archive_path}"
  log "校验文件已生成: ${archive_path}.sha256"
}

main "$@"
