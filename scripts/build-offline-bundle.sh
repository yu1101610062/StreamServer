#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
OUTPUT_DIR="${ROOT_DIR}/dist"
SKIP_IMAGES=0
GPU_SUPPORT=""
HOST_BINARY_TARGET_TRIPLE="x86_64-unknown-linux-musl"
BUILD_MUSL_BIN_SCRIPT="${ROOT_DIR}/scripts/build-musl-binaries.sh"
PREBUILD_UI=1
ALLOW_MISSING_DESKTOP_INSTALLERS=1
FRONTEND_SKIP_INSTALL=0
FRONTEND_SOURCE_DIRS=()

DEFAULT_APT_MIRROR="http://mirrors.aliyun.com"
DEFAULT_UBUNTU_APT_MIRROR="${DEFAULT_APT_MIRROR}"
DEFAULT_CARGO_REGISTRY_MIRROR="sparse+https://rsproxy.cn/index/"
DEFAULT_NPM_REGISTRY_MIRROR="https://registry.npmmirror.com"
DOCKERHUB_MIRROR_HOST="m.daocloud.io"

image_ref_has_registry() {
  local image_ref="$1"
  local first_segment="${image_ref%%/*}"
  [[ "${image_ref}" == */* ]] && {
    [[ "${first_segment}" == *.* ]] || [[ "${first_segment}" == *:* ]] || [[ "${first_segment}" == "localhost" ]]
  }
}

dockerhub_mirror_ref() {
  local image_ref="$1"

  if [[ "${image_ref}" == "${DOCKERHUB_MIRROR_HOST}/docker.io/"* ]]; then
    printf '%s\n' "${image_ref}"
  elif [[ "${image_ref}" == docker.io/* ]]; then
    printf '%s/%s\n' "${DOCKERHUB_MIRROR_HOST}" "${image_ref}"
  elif image_ref_has_registry "${image_ref}"; then
    printf '%s\n' "${image_ref}"
  elif [[ "${image_ref}" == */* ]]; then
    printf '%s/docker.io/%s\n' "${DOCKERHUB_MIRROR_HOST}" "${image_ref}"
  else
    printf '%s/docker.io/library/%s\n' "${DOCKERHUB_MIRROR_HOST}" "${image_ref}"
  fi
}

dockerhub_library_mirror_ref() {
  local image_ref="$1"

  if image_ref_has_registry "${image_ref}"; then
    printf '%s\n' "${image_ref}"
  elif [[ "${image_ref}" == */* ]]; then
    printf '%s\n' "${image_ref}"
  else
    dockerhub_mirror_ref "${image_ref}"
  fi
}

dockerhub_upstream_ref() {
  local image_ref="$1"

  if [[ "${image_ref}" == "${DOCKERHUB_MIRROR_HOST}/docker.io/"* ]]; then
    printf '%s\n' "${image_ref#${DOCKERHUB_MIRROR_HOST}/}"
  elif [[ "${image_ref}" == docker.io/* ]]; then
    printf '%s\n' "${image_ref}"
  elif image_ref_has_registry "${image_ref}"; then
    printf '%s\n' "${image_ref}"
  elif [[ "${image_ref}" == */* ]]; then
    printf 'docker.io/%s\n' "${image_ref}"
  else
    printf 'docker.io/library/%s\n' "${image_ref}"
  fi
}

dockerhub_short_ref() {
  local upstream_ref="$1"

  upstream_ref="$(dockerhub_upstream_ref "${upstream_ref}")"
  if [[ "${upstream_ref}" == docker.io/library/* ]]; then
    printf '%s\n' "${upstream_ref#docker.io/library/}"
    return 0
  fi
  if [[ "${upstream_ref}" == docker.io/* ]]; then
    printf '%s\n' "${upstream_ref#docker.io/}"
    return 0
  fi
  return 1
}

resolve_env_or_default() {
  local var_name="$1"
  local default_value="$2"

  if [ "${!var_name+x}" = x ]; then
    printf '%s\n' "${!var_name}"
  else
    printf '%s\n' "${default_value}"
  fi
}

APT_MIRROR="$(resolve_env_or_default APT_MIRROR "${DEFAULT_APT_MIRROR}")"
UBUNTU_APT_MIRROR="$(resolve_env_or_default UBUNTU_APT_MIRROR "${DEFAULT_UBUNTU_APT_MIRROR}")"
CARGO_REGISTRY_MIRROR="$(resolve_env_or_default CARGO_REGISTRY_MIRROR "${DEFAULT_CARGO_REGISTRY_MIRROR}")"
NPM_REGISTRY_MIRROR="$(resolve_env_or_default NPM_REGISTRY_MIRROR "${DEFAULT_NPM_REGISTRY_MIRROR}")"
FRONTEND_BUILDER_IMAGE="$(resolve_env_or_default FRONTEND_BUILDER_IMAGE "$(dockerhub_library_mirror_ref 'node:22-bookworm')")"
RUST_BUILDER_IMAGE="$(resolve_env_or_default RUST_BUILDER_IMAGE "$(dockerhub_library_mirror_ref 'rust:1.85-bookworm')")"
MEDIA_CORE_RUNTIME_BASE_IMAGE="$(resolve_env_or_default MEDIA_CORE_RUNTIME_BASE_IMAGE "$(dockerhub_library_mirror_ref 'debian:bookworm-slim')")"
MEDIA_AGENT_RUNTIME_BASE_IMAGE="$(resolve_env_or_default MEDIA_AGENT_RUNTIME_BASE_IMAGE 'jrottenberg/ffmpeg:7.1-ubuntu2404')"
MEDIA_AGENT_GPU_RUNTIME_BASE_IMAGE="$(resolve_env_or_default MEDIA_AGENT_GPU_RUNTIME_BASE_IMAGE 'jrottenberg/ffmpeg:7.1-nvidia2204')"
POSTGRES_SOURCE_IMAGE="$(resolve_env_or_default POSTGRES_SOURCE_IMAGE "$(dockerhub_library_mirror_ref 'postgres:18.3')")"
ZLM_SOURCE_IMAGE="$(resolve_env_or_default ZLM_SOURCE_IMAGE 'zlmediakit/zlmediakit:master@sha256:8b24d1d4a30736b2001e5d78fc46057cb3abf4cae527818f238678826537389f')"

log() {
  printf '[offline-bundle] %s\n' "$*"
}

fail() {
  printf '[offline-bundle] ERROR: %s\n' "$*" >&2
  exit 1
}

require_cmd() {
  command -v "$1" >/dev/null 2>&1 || fail "缺少命令: $1"
}

usage() {
  cat <<EOF
用法:
  $(basename "$0") [--output-dir DIR] [--skip-images] [--with-gpu|--without-gpu]
                 [--prebuilt-ui-dir DIR|--build-ui-in-docker]

说明:
  在 macOS arm64 或 Linux 主机上构建 Linux AMD64 离线部署包。
  默认先本地构建 media-core 前端静态资源，并同步已存在的 Windows/macOS 桌面安装包；
  然后生成离线部署包。默认输出到 ./dist。

参数:
  --output-dir DIR                输出目录，默认 ./dist
  --skip-images                   只生成骨架包，跳过镜像和宿主机挂载二进制
  --with-gpu                      生成 GPU-enabled 包
  --without-gpu                   生成 CPU-only 包
  --prebuilt-ui-dir DIR           使用已有前端静态资源目录，跳过本地前端构建
  --build-ui-in-docker            使用 Docker 的 media-ui-export 阶段导出 UI，等同旧底层打包行为
  --require-desktop-installers    本地构建 UI 时要求 Windows/macOS 安装包都存在
  --allow-missing-installers      本地构建 UI 时允许缺少某个平台安装包，默认行为
  --desktop-source-dir DIR        额外桌面安装包扫描目录，可重复
  --skip-frontend-install         跳过前端 npm ci / npm install 检查

环境变量:
  APT_MIRROR             默认 http://mirrors.aliyun.com；如显式置空则回退 Debian 官方源，也会作为 media-agent Ubuntu 运行时的默认镜像源。
  UBUNTU_APT_MIRROR      默认 http://mirrors.aliyun.com；如显式置空则回退 Ubuntu 官方源；如设置则覆盖 media-agent CPU/GPU 运行时的 Ubuntu 源。
  CARGO_REGISTRY_MIRROR  默认 sparse+https://rsproxy.cn/index/；如显式置空则回退 crates.io 官方源。
  NPM_REGISTRY_MIRROR    默认 https://registry.npmmirror.com；如显式置空则回退 npm 官方源。
  FRONTEND_BUILDER_IMAGE 默认 m.daocloud.io/docker.io/library/node:22-bookworm；可覆写前端构建基础镜像。
  RUST_BUILDER_IMAGE     默认 m.daocloud.io/docker.io/library/rust:1.85-bookworm；可覆写 Rust 构建基础镜像。
  MUSL_CARGO_HOME_DIR    默认 ./.build-cache/musl/cargo-home；可覆写 musl 构建的 Cargo 缓存目录。
  MUSL_CARGO_TARGET_DIR  默认 ./target/docker-musl；可覆写 musl 构建的 target 缓存目录。
  MEDIA_CORE_RUNTIME_BASE_IMAGE      默认 m.daocloud.io/docker.io/library/debian:bookworm-slim；可覆写 media-core 运行时基础镜像。
  MEDIA_AGENT_RUNTIME_BASE_IMAGE     默认 jrottenberg/ffmpeg:7.1-ubuntu2404；可覆写 media-agent CPU 运行时基础镜像。
  MEDIA_AGENT_GPU_RUNTIME_BASE_IMAGE 默认 jrottenberg/ffmpeg:7.1-nvidia2204；可覆写 media-agent GPU 运行时基础镜像。
  POSTGRES_SOURCE_IMAGE  可覆盖 PostgreSQL 拉取源；脚本会优先复用本地已有的 linux/amd64 镜像，不存在时才联网拉取。
  ZLM_SOURCE_IMAGE       可覆盖 ZLMediaKit 拉取源；脚本会优先复用本地已有的 linux/amd64 镜像，不存在时才联网拉取。
  PREBUILT_UI_DIR        如设置，直接使用该目录中的前端静态资源，不再本地或通过 Docker 构建前端。
EOF
}

prompt_yes_no() {
  local message="$1"
  local default_value="${2:-Y}"
  local answer
  while true; do
    printf '%s [%s]: ' "${message}" "${default_value}" >&2
    read -r answer
    if [ -z "${answer}" ]; then
      answer="${default_value}"
    fi
    case "${answer}" in
      Y|y|yes|YES) return 0 ;;
      N|n|no|NO) return 1 ;;
      *) echo "请输入 Y 或 N。" >&2 ;;
    esac
  done
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
      --with-gpu)
        GPU_SUPPORT="true"
        shift
        ;;
      --without-gpu)
        GPU_SUPPORT="false"
        shift
        ;;
      --prebuilt-ui-dir)
        [ "$#" -ge 2 ] || fail "--prebuilt-ui-dir 需要参数"
        PREBUILT_UI_DIR="$2"
        PREBUILD_UI=0
        shift 2
        ;;
      --build-ui-in-docker)
        PREBUILD_UI=0
        unset PREBUILT_UI_DIR
        shift
        ;;
      --require-desktop-installers|--require-desktop-clients)
        ALLOW_MISSING_DESKTOP_INSTALLERS=0
        shift
        ;;
      --allow-missing-installers|--allow-missing-clients)
        ALLOW_MISSING_DESKTOP_INSTALLERS=1
        shift
        ;;
      --desktop-source-dir)
        [ "$#" -ge 2 ] || fail "--desktop-source-dir 需要参数"
        FRONTEND_SOURCE_DIRS+=("$2")
        shift 2
        ;;
      --skip-frontend-install)
        FRONTEND_SKIP_INSTALL=1
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

prepare_frontend_ui() {
  local frontend_args=()
  local source_dir=""

  if [ -n "${PREBUILT_UI_DIR:-}" ]; then
    [ -f "${PREBUILT_UI_DIR}/index.html" ] || fail "PREBUILT_UI_DIR 不是有效前端静态资源目录: ${PREBUILT_UI_DIR}"
    log "使用预构建前端静态资源: ${PREBUILT_UI_DIR}"
    return 0
  fi

  if [ "${PREBUILD_UI}" -eq 0 ]; then
    log "前端静态资源将通过 Docker media-ui-export 阶段导出"
    return 0
  fi

  require_cmd node

  if [ "${ALLOW_MISSING_DESKTOP_INSTALLERS}" -eq 1 ]; then
    frontend_args+=(--allow-missing-installers)
  fi
  if [ "${FRONTEND_SKIP_INSTALL}" -eq 1 ]; then
    frontend_args+=(--skip-install)
  fi
  if [ "${#FRONTEND_SOURCE_DIRS[@]}" -gt 0 ]; then
    for source_dir in "${FRONTEND_SOURCE_DIRS[@]}"; do
      frontend_args+=(--source-dir "${source_dir}")
    done
  fi

  log "构建前端静态资源并同步桌面安装包"
  if [ "${#frontend_args[@]}" -gt 0 ]; then
    node "${ROOT_DIR}/scripts/build-frontend-ui.mjs" "${frontend_args[@]}"
  else
    node "${ROOT_DIR}/scripts/build-frontend-ui.mjs"
  fi
  PREBUILT_UI_DIR="${ROOT_DIR}/crates/media-core/ui"
  export PREBUILT_UI_DIR
}

resolve_gpu_support() {
  if [ -n "${GPU_SUPPORT}" ]; then
    return 0
  fi

  if [ -t 0 ]; then
    if prompt_yes_no "是否打包 GPU 版本（包含 GPU 镜像和 GPU 模板）？" "Y"; then
      GPU_SUPPORT="true"
    else
      GPU_SUPPORT="false"
    fi
    return 0
  fi

  GPU_SUPPORT="true"
  log "未指定 GPU 选项且当前为非交互环境，默认生成 GPU-enabled 离线包"
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

export_host_binaries() {
  local output_dir="$1"

  mkdir -p "${output_dir}"
  log "构建 media-core/media-agent linux/amd64 宿主机挂载二进制"
  "${BUILD_MUSL_BIN_SCRIPT}" \
    --target-triple "${HOST_BINARY_TARGET_TRIPLE}" \
    --package media-core \
    --package media-agent \
    --package streamserver-config \
    --output-dir "${output_dir}"
}

export_ui_assets() {
  local output_dir="$1"

  mkdir -p "${output_dir}"
  if [ -n "${PREBUILT_UI_DIR:-}" ]; then
    [ -f "${PREBUILT_UI_DIR}/index.html" ] || fail "PREBUILT_UI_DIR 不是有效前端静态资源目录: ${PREBUILT_UI_DIR}"
    log "使用预构建前端静态资源: ${PREBUILT_UI_DIR}"
    rm -rf "${output_dir}/ui"
    mkdir -p "${output_dir}/ui"
    cp -R "${PREBUILT_UI_DIR}/." "${output_dir}/ui/"
    return 0
  fi

  log "导出 media-core 前端静态资源"
  docker buildx build \
    --platform linux/amd64 \
    --target media-ui-export \
    --build-arg NPM_REGISTRY_MIRROR="${NPM_REGISTRY_MIRROR}" \
    --build-arg FRONTEND_BUILDER_IMAGE="${FRONTEND_BUILDER_IMAGE}" \
    --output "type=local,dest=${output_dir}" \
    "${ROOT_DIR}"
}

resolve_media_agent_ubuntu_mirror() {
  if [ -n "${UBUNTU_APT_MIRROR}" ]; then
    printf '%s\n' "${UBUNTU_APT_MIRROR}"
  else
    printf '%s\n' "${APT_MIRROR}"
  fi
}

smoke_test_media_core_image() {
  local image_ref="$1"
  local binary_path="$2"
  local output=""

  [ -x "${binary_path}" ] || fail "缺少可执行的 media-core 二进制: ${binary_path}"

  log "校验 ${image_ref} 的 media-core 启动包装器"
  if ! output="$(
    docker run --rm \
      --platform linux/amd64 \
      -v "${binary_path}:/opt/streamserver/bin/media-core:ro" \
      "${image_ref}" --help 2>&1
  )"; then
    # 旧版 media-core 尚未实现顶层 --help，但出现这条报错也足以证明包装器已经
    # 成功执行了宿主机挂载的二进制；其他错误则仍视为包装器校验失败。
    if ! printf '%s' "${output}" | grep -Fq "unsupported command \`--help\`"; then
      printf '%s\n' "${output}" >&2
      fail "镜像 ${image_ref} 未通过 media-core 宿主机挂载二进制校验"
    fi
  fi
}

smoke_test_media_agent_image() {
  local image_ref="$1"
  local binary_path="$2"
  local require_nvenc="${3:-false}"
  local container_name="streamserver-media-agent-smoke-$RANDOM-$$"

  [ -x "${binary_path}" ] || fail "缺少可执行的 media-agent 二进制: ${binary_path}"

  log "校验 ${image_ref} 的 FFmpeg 运行时"
  docker run --rm --platform linux/amd64 --entrypoint sh "${image_ref}" -lc '
    set -eu
    command -v curl >/dev/null
    ffmpeg -version | grep -q "^ffmpeg version 7\.1"
    ffprobe -version >/dev/null
    ffmpeg -hide_banner -f lavfi -i testsrc=size=128x72:rate=1 -t 1 -c:v libx265 -an -f flv -y /tmp/hevc-test.flv >/tmp/hevc-flv-smoke.log 2>&1
    test -s /tmp/hevc-test.flv
  ' >/dev/null || fail "镜像 ${image_ref} 未通过 FFmpeg 7.1 / HEVC->FLV 校验"

  if [ "${require_nvenc}" = "true" ]; then
    docker run --rm --platform linux/amd64 --entrypoint sh "${image_ref}" -lc '
      set -eu
      ffmpeg -hide_banner -encoders 2>/dev/null | grep -q " h264_nvenc"
      ffmpeg -hide_banner -encoders 2>/dev/null | grep -q " hevc_nvenc"
    ' >/dev/null || fail "镜像 ${image_ref} 未检测到 NVENC 编码器"
  fi

  docker rm -f "${container_name}" >/dev/null 2>&1 || true
  docker run -d --rm \
    --platform linux/amd64 \
    --name "${container_name}" \
    -v "${binary_path}:/opt/streamserver/bin/media-agent:ro" \
    -e STREAMSERVER_ENV=production \
    -e WORK_ROOT=/data/media/work \
    "${image_ref}" >/dev/null

  local ready=0
  local response=""
  local i
  for i in 1 2 3 4 5; do
    response="$(docker exec "${container_name}" sh -lc 'curl -fsS http://127.0.0.1:8081/health/ready' 2>/dev/null || true)"
    if printf '%s' "${response}" | grep -q '"status":"ready"'; then
      ready=1
      break
    fi
    sleep 1
  done

  if [ "${ready}" -ne 1 ]; then
    docker logs "${container_name}" >&2 || true
    docker rm -f "${container_name}" >/dev/null 2>&1 || true
    fail "镜像 ${image_ref} 未通过 media-agent 健康检查"
  fi

  docker rm -f "${container_name}" >/dev/null 2>&1 || true
}

local_source_candidate() {
  local image_ref="$1"
  local upstream_ref=""
  local short_ref=""
  local candidate=""

  upstream_ref="$(dockerhub_upstream_ref "${image_ref}")"
  if short_ref="$(dockerhub_short_ref "${image_ref}")"; then
    :
  else
    short_ref=""
  fi

  while IFS= read -r candidate; do
    [ -n "${candidate}" ] || continue
    if docker image inspect "${candidate}" >/dev/null 2>&1; then
      printf '%s\n' "${candidate}"
      return 0
    fi
  done <<EOF
${image_ref}
${image_ref%@*}
${upstream_ref}
${upstream_ref%@*}
${short_ref}
${short_ref%@*}
EOF
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
  local media_agent_gpu_image="$3"
  local postgres_image="$4"
  local zlm_image="$5"
  local gpu_support="$6"
  local media_agent_ubuntu_mirror="$7"
  local host_artifacts_output_dir="$8"

  export_host_binaries "${host_artifacts_output_dir}"
  export_ui_assets "${host_artifacts_output_dir}"

  log "构建 media-core linux/amd64 镜像"
  docker buildx build \
    --platform linux/amd64 \
    --target media-core-runtime \
    --build-arg DEBIAN_MIRROR="${APT_MIRROR}" \
    --build-arg CARGO_REGISTRY_MIRROR="${CARGO_REGISTRY_MIRROR}" \
    --build-arg FRONTEND_BUILDER_IMAGE="${FRONTEND_BUILDER_IMAGE}" \
    --build-arg RUST_BUILDER_IMAGE="${RUST_BUILDER_IMAGE}" \
    --build-arg MEDIA_CORE_RUNTIME_BASE_IMAGE="${MEDIA_CORE_RUNTIME_BASE_IMAGE}" \
    --build-arg MEDIA_AGENT_RUNTIME_BASE_IMAGE="${MEDIA_AGENT_RUNTIME_BASE_IMAGE}" \
    --build-arg MEDIA_AGENT_GPU_RUNTIME_BASE_IMAGE="${MEDIA_AGENT_GPU_RUNTIME_BASE_IMAGE}" \
    --load \
    -t "${media_core_image}" \
    "${ROOT_DIR}"
  verify_loaded_image_arch "${media_core_image}"
  smoke_test_media_core_image "${media_core_image}" "${host_artifacts_output_dir}/media-core"

  log "构建 media-agent linux/amd64 镜像"
  docker buildx build \
    --platform linux/amd64 \
    --target media-agent-runtime \
    --build-arg DEBIAN_MIRROR="${APT_MIRROR}" \
    --build-arg UBUNTU_MIRROR="${media_agent_ubuntu_mirror}" \
    --build-arg CARGO_REGISTRY_MIRROR="${CARGO_REGISTRY_MIRROR}" \
    --build-arg FRONTEND_BUILDER_IMAGE="${FRONTEND_BUILDER_IMAGE}" \
    --build-arg RUST_BUILDER_IMAGE="${RUST_BUILDER_IMAGE}" \
    --build-arg MEDIA_CORE_RUNTIME_BASE_IMAGE="${MEDIA_CORE_RUNTIME_BASE_IMAGE}" \
    --build-arg MEDIA_AGENT_RUNTIME_BASE_IMAGE="${MEDIA_AGENT_RUNTIME_BASE_IMAGE}" \
    --build-arg MEDIA_AGENT_GPU_RUNTIME_BASE_IMAGE="${MEDIA_AGENT_GPU_RUNTIME_BASE_IMAGE}" \
    --load \
    -t "${media_agent_image}" \
    "${ROOT_DIR}"
  verify_loaded_image_arch "${media_agent_image}"
  smoke_test_media_agent_image "${media_agent_image}" "${host_artifacts_output_dir}/media-agent" "false"

  if [ "${gpu_support}" = "true" ]; then
    log "构建 media-agent-gpu linux/amd64 镜像"
    docker buildx build \
      --platform linux/amd64 \
      --target media-agent-gpu-runtime \
      --build-arg DEBIAN_MIRROR="${APT_MIRROR}" \
      --build-arg UBUNTU_MIRROR="${media_agent_ubuntu_mirror}" \
      --build-arg CARGO_REGISTRY_MIRROR="${CARGO_REGISTRY_MIRROR}" \
      --build-arg FRONTEND_BUILDER_IMAGE="${FRONTEND_BUILDER_IMAGE}" \
      --build-arg RUST_BUILDER_IMAGE="${RUST_BUILDER_IMAGE}" \
      --build-arg MEDIA_CORE_RUNTIME_BASE_IMAGE="${MEDIA_CORE_RUNTIME_BASE_IMAGE}" \
      --build-arg MEDIA_AGENT_RUNTIME_BASE_IMAGE="${MEDIA_AGENT_RUNTIME_BASE_IMAGE}" \
      --build-arg MEDIA_AGENT_GPU_RUNTIME_BASE_IMAGE="${MEDIA_AGENT_GPU_RUNTIME_BASE_IMAGE}" \
      --load \
      -t "${media_agent_gpu_image}" \
      "${ROOT_DIR}"
    verify_loaded_image_arch "${media_agent_gpu_image}"
    smoke_test_media_agent_image "${media_agent_gpu_image}" "${host_artifacts_output_dir}/media-agent" "true"
  fi

  prepare_source_image "${POSTGRES_SOURCE_IMAGE}" "${postgres_image}" "postgres"

  prepare_source_image "${ZLM_SOURCE_IMAGE}" "${zlm_image}" "ZLMediaKit"
}

write_manifest() {
  local bundle_root="$1"
  local bundle_version="$2"
  local media_core_image="$3"
  local media_agent_image="$4"
  local media_agent_gpu_image="$5"
  local postgres_image="$6"
  local zlm_image="$7"
  local gpu_support="$8"
  local bundle_variant="$9"
  local media_agent_gpu_image_value=""
  local media_agent_gpu_archive_value=""

  if [ "${gpu_support}" = "true" ]; then
    media_agent_gpu_image_value="${media_agent_gpu_image}"
    media_agent_gpu_archive_value="images/media-agent-gpu-linux-amd64.tar"
  fi

  cat >"${bundle_root}/package-manifest.env" <<EOF
BUNDLE_VERSION=${bundle_version}
BUNDLE_VARIANT=${bundle_variant}
BUNDLE_GPU_SUPPORT=${gpu_support}
POSTGRES_IMAGE=${postgres_image}
POSTGRES_IMAGE_ARCHIVE=images/postgres-linux-amd64.tar
MEDIA_CORE_IMAGE=${media_core_image}
MEDIA_CORE_IMAGE_ARCHIVE=images/media-core-linux-amd64.tar
MEDIA_CORE_BINARY_PATH=binaries/media-core-linux-amd64
MEDIA_CORE_UI_PATH=ui/media-core
MEDIA_AGENT_IMAGE=${media_agent_image}
MEDIA_AGENT_IMAGE_ARCHIVE=images/media-agent-linux-amd64.tar
MEDIA_AGENT_BINARY_PATH=binaries/media-agent-linux-amd64
STREAMSERVER_CONFIG_BINARY_PATH=binaries/streamserver-config-linux-amd64
MEDIA_AGENT_GPU_IMAGE=${media_agent_gpu_image_value}
MEDIA_AGENT_GPU_IMAGE_ARCHIVE=${media_agent_gpu_archive_value}
ZLM_IMAGE=${zlm_image}
ZLM_IMAGE_ARCHIVE=images/zlmediakit-linux-amd64.tar
EOF
}

write_build_info() {
  local bundle_root="$1"
  local bundle_name="$2"
  local version="$3"
  local bundle_variant="$4"
  local gpu_support="$5"
  local commit

  commit="$(git -C "${ROOT_DIR}" rev-parse --short HEAD 2>/dev/null || true)"

  cat >"${bundle_root}/build-info.txt" <<EOF
bundle_name=${bundle_name}
version=${version}
built_at=$(date -u '+%Y-%m-%dT%H:%M:%SZ')
builder_os=$(uname -s)
builder_arch=$(uname -m)
git_commit=${commit}
bundle_variant=${bundle_variant}
gpu_support=${gpu_support}
EOF
}

copy_static_assets() {
  local bundle_root="$1"
  local gpu_support="$2"

  mkdir -p "${bundle_root}/templates"
  mkdir -p "${bundle_root}/docs"
  cp "${ROOT_DIR}/packaging/offline/install.sh" "${bundle_root}/install.sh"
  chmod +x "${bundle_root}/install.sh"
  mkdir -p "${bundle_root}/templates/common"
  cp -R "${ROOT_DIR}/packaging/offline/templates/common/." "${bundle_root}/templates/common/"
  cp -R "${ROOT_DIR}/packaging/offline/templates/control-plane" "${bundle_root}/templates/control-plane"
  cp -R "${ROOT_DIR}/packaging/offline/templates/worker-host" "${bundle_root}/templates/worker-host-cpu"
  cp -R "${ROOT_DIR}/packaging/offline/templates/all-in-one-host" "${bundle_root}/templates/all-in-one-host-cpu"
  if [ "${gpu_support}" = "true" ]; then
    cp -R "${ROOT_DIR}/packaging/offline/templates/worker-host-gpu" "${bundle_root}/templates/worker-host-gpu"
    cp -R "${ROOT_DIR}/packaging/offline/templates/all-in-one-host-gpu" "${bundle_root}/templates/all-in-one-host-gpu"
  fi
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
  local media_agent_gpu_image="$4"
  local postgres_image="$5"
  local zlm_image="$6"
  local gpu_support="$7"

  mkdir -p "${bundle_root}/images"
  log "导出离线镜像包"
  docker save -o "${bundle_root}/images/media-core-linux-amd64.tar" "${media_core_image}"
  docker save -o "${bundle_root}/images/media-agent-linux-amd64.tar" "${media_agent_image}"
  if [ "${gpu_support}" = "true" ]; then
    docker save -o "${bundle_root}/images/media-agent-gpu-linux-amd64.tar" "${media_agent_gpu_image}"
  fi
  docker save -o "${bundle_root}/images/postgres-linux-amd64.tar" "${postgres_image}"
  docker save -o "${bundle_root}/images/zlmediakit-linux-amd64.tar" "${zlm_image}"
}

save_host_artifacts() {
  local bundle_root="$1"
  local host_artifacts_output_dir="$2"

  mkdir -p "${bundle_root}/binaries"
  cp "${host_artifacts_output_dir}/media-core" "${bundle_root}/binaries/media-core-linux-amd64"
  cp "${host_artifacts_output_dir}/media-agent" "${bundle_root}/binaries/media-agent-linux-amd64"
  cp "${host_artifacts_output_dir}/streamserver-config" "${bundle_root}/binaries/streamserver-config-linux-amd64"
  chmod 755 \
    "${bundle_root}/binaries/media-core-linux-amd64" \
    "${bundle_root}/binaries/media-agent-linux-amd64" \
    "${bundle_root}/binaries/streamserver-config-linux-amd64"

  mkdir -p "${bundle_root}/ui/media-core"
  cp -R "${host_artifacts_output_dir}/ui/." "${bundle_root}/ui/media-core/"
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

resolve_bundle_name() {
  local output_dir="$1"
  local base_name="$2"
  local candidate="${base_name}"
  local suffix=2

  while [ -e "${output_dir}/${candidate}.tar.gz" ] || [ -e "${output_dir}/${candidate}.tar.gz.sha256" ]; do
    candidate="${base_name}-${suffix}"
    suffix=$((suffix + 1))
  done

  printf '%s' "${candidate}"
}

main() {
  local version
  local build_date
  local bundle_name_base
  local bundle_name
  local bundle_version
  local bundle_variant
  local media_core_image
  local media_agent_image
  local media_agent_gpu_image
  local media_agent_ubuntu_mirror
  local postgres_image
  local zlm_image
  local stage_dir
  local bundle_root
  local archive_path
  local host_artifacts_output_dir

  parse_args "$@"
  ensure_supported_packaging_host
  ensure_tools
  resolve_gpu_support
  prepare_frontend_ui
  media_agent_ubuntu_mirror="$(resolve_media_agent_ubuntu_mirror)"

  if [ -n "${APT_MIRROR}" ]; then
    log "使用 APT 镜像: ${APT_MIRROR}"
  else
    log "APT 使用 Debian 官方源"
  fi
  if [ -n "${media_agent_ubuntu_mirror}" ]; then
    log "media-agent 运行时使用 Ubuntu APT 镜像: ${media_agent_ubuntu_mirror}"
  else
    log "media-agent 运行时使用 Ubuntu 官方源"
  fi

  if [ -n "${CARGO_REGISTRY_MIRROR}" ]; then
    log "使用 Cargo 镜像: ${CARGO_REGISTRY_MIRROR}"
  else
    log "Cargo 使用 crates.io 官方源"
  fi
  if [ -n "${NPM_REGISTRY_MIRROR}" ]; then
    log "使用 npm 镜像: ${NPM_REGISTRY_MIRROR}"
  else
    log "npm 使用官方 registry"
  fi
  log "前端构建基础镜像: ${FRONTEND_BUILDER_IMAGE}"
  log "Rust 构建基础镜像: ${RUST_BUILDER_IMAGE}"
  log "media-core 运行时基础镜像: ${MEDIA_CORE_RUNTIME_BASE_IMAGE}"
  log "media-agent CPU 运行时基础镜像: ${MEDIA_AGENT_RUNTIME_BASE_IMAGE}"
  log "media-agent GPU 运行时基础镜像: ${MEDIA_AGENT_GPU_RUNTIME_BASE_IMAGE}"
  log "PostgreSQL 拉取源: ${POSTGRES_SOURCE_IMAGE}"
  log "ZLMediaKit 拉取源: ${ZLM_SOURCE_IMAGE}"

  version="$(workspace_version)"
  [ -n "${version}" ] || fail "无法从 Cargo.toml 解析版本号"
  build_date="$(date '+%Y%m%d')"
  bundle_version="v${version}"
  bundle_variant="$( [ "${GPU_SUPPORT}" = "true" ] && printf '%s' "gpu-enabled" || printf '%s' "cpu-only" )"
  bundle_name_base="streamserver-offline-${bundle_version}-linux-amd64-${bundle_variant}-${build_date}"
  mkdir -p "${OUTPUT_DIR}"
  bundle_name="$(resolve_bundle_name "${OUTPUT_DIR}" "${bundle_name_base}")"

  media_core_image="streamserver/media-core:${version}-linux-amd64"
  media_agent_image="streamserver/media-agent:${version}-linux-amd64"
  media_agent_gpu_image="streamserver/media-agent-gpu:${version}-linux-amd64"
  postgres_image="streamserver/postgres:18.3-linux-amd64"
  zlm_image="streamserver/zlmediakit:master-linux-amd64"

  stage_dir="$(mktemp -d "${TMPDIR:-/tmp}/streamserver-offline.XXXXXX")"
  bundle_root="${stage_dir}/${bundle_name}"
  archive_path="${OUTPUT_DIR}/${bundle_name}.tar.gz"
  host_artifacts_output_dir="${stage_dir}/exported-host-assets"

  mkdir -p "${bundle_root}"

  if [ "${SKIP_IMAGES}" -eq 0 ]; then
    build_or_pull_images "${media_core_image}" "${media_agent_image}" "${media_agent_gpu_image}" "${postgres_image}" "${zlm_image}" "${GPU_SUPPORT}" "${media_agent_ubuntu_mirror}" "${host_artifacts_output_dir}"
  else
    log "跳过镜像与宿主机挂载物构建导出，仅生成骨架包"
    mkdir -p "${bundle_root}/images"
    mkdir -p "${bundle_root}/binaries"
    echo "此包由 --skip-images 生成，未包含任何镜像。" >"${bundle_root}/images/SKIPPED.txt"
    echo "此包由 --skip-images 生成，未包含任何宿主机挂载二进制。" >"${bundle_root}/binaries/SKIPPED.txt"
    if [ -n "${PREBUILT_UI_DIR:-}" ] && [ -f "${PREBUILT_UI_DIR}/index.html" ]; then
      log "写入预构建前端静态资源: ${PREBUILT_UI_DIR}"
      mkdir -p "${bundle_root}/ui/media-core"
      cp -R "${PREBUILT_UI_DIR}/." "${bundle_root}/ui/media-core/"
    else
      mkdir -p "${bundle_root}/ui"
      echo "此包由 --skip-images 生成，未包含任何宿主机挂载前端静态资源。" >"${bundle_root}/ui/SKIPPED.txt"
    fi
  fi

  copy_static_assets "${bundle_root}" "${GPU_SUPPORT}"
  generate_self_signed_certs "${bundle_root}"
  write_manifest "${bundle_root}" "${bundle_version}" "${media_core_image}" "${media_agent_image}" "${media_agent_gpu_image}" "${postgres_image}" "${zlm_image}" "${GPU_SUPPORT}" "${bundle_variant}"
  write_build_info "${bundle_root}" "${bundle_name}" "${version}" "${bundle_variant}" "${GPU_SUPPORT}"

  if [ "${SKIP_IMAGES}" -eq 0 ]; then
    save_images "${bundle_root}" "${media_core_image}" "${media_agent_image}" "${media_agent_gpu_image}" "${postgres_image}" "${zlm_image}" "${GPU_SUPPORT}"
    save_host_artifacts "${bundle_root}" "${host_artifacts_output_dir}"
  fi

  create_archive "${stage_dir}" "${bundle_name}" "${archive_path}"

  log "离线包已生成: ${archive_path}"
  log "校验文件已生成: ${archive_path}.sha256"
}

main "$@"
