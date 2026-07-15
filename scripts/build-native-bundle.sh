#!/usr/bin/env bash
set -euo pipefail
export COPYFILE_DISABLE=1

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
OUTPUT_DIR="${ROOT_DIR}/dist"
BUILD_MUSL_BIN_SCRIPT="${ROOT_DIR}/scripts/build-musl-binaries.sh"
HOST_BINARY_TARGET_TRIPLE="${HOST_BINARY_TARGET_TRIPLE:-x86_64-unknown-linux-musl}"
NATIVE_VARIANT=""
PREBUILT_UI_DIR="${PREBUILT_UI_DIR:-}"
FRONTEND_SKIP_INSTALL=0
REFRESH_RUNTIME_CACHE=0
OFFLINE_RUNTIME_CACHE=0
NO_RUNTIME_CACHE=0
PRUNE_IMAGES_AFTER_EXTRACT=0
TEMP_STAGE_DIR=""

DEFAULT_APT_MIRROR="http://mirrors.aliyun.com"
DEFAULT_CARGO_REGISTRY_MIRROR="sparse+https://rsproxy.cn/index/"
DOCKERHUB_MIRROR_HOST="m.daocloud.io"
NATIVE_RUNTIME_CACHE_DIR="${NATIVE_RUNTIME_CACHE_DIR:-${ROOT_DIR}/.build-cache/native-runtime}"
NATIVE_RUNTIME_CACHE_EXTRACTOR_VERSION="20260602-1"

log() {
  printf '[native-bundle] %s\n' "$*"
}

fail() {
  printf '[native-bundle] ERROR: %s\n' "$*" >&2
  exit 1
}

require_cmd() {
  command -v "$1" >/dev/null 2>&1 || fail "缺少命令: $1"
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
  if image_ref_has_registry "${image_ref}" || [[ "${image_ref}" == */* ]]; then
    printf '%s\n' "${image_ref}"
  else
    dockerhub_mirror_ref "${image_ref}"
  fi
}

APT_MIRROR="$(resolve_env_or_default APT_MIRROR "${DEFAULT_APT_MIRROR}")"
CARGO_REGISTRY_MIRROR="$(resolve_env_or_default CARGO_REGISTRY_MIRROR "${DEFAULT_CARGO_REGISTRY_MIRROR}")"
RUST_BUILDER_IMAGE="$(resolve_env_or_default RUST_BUILDER_IMAGE "$(dockerhub_library_mirror_ref 'rust:1.85-bookworm')")"
MEDIA_AGENT_RUNTIME_BASE_IMAGE="$(resolve_env_or_default MEDIA_AGENT_RUNTIME_BASE_IMAGE 'jrottenberg/ffmpeg:8.1-ubuntu2404')"
MEDIA_AGENT_GPU_RUNTIME_BASE_IMAGE="$(resolve_env_or_default MEDIA_AGENT_GPU_RUNTIME_BASE_IMAGE 'jrottenberg/ffmpeg:8.1-nvidia2404')"
POSTGRES_SOURCE_IMAGE="$(resolve_env_or_default POSTGRES_SOURCE_IMAGE "$(dockerhub_library_mirror_ref 'postgres:18.3')")"
ZLM_SOURCE_IMAGE="$(resolve_env_or_default ZLM_SOURCE_IMAGE 'zlmediakit/zlmediakit:master@sha256:8b24d1d4a30736b2001e5d78fc46057cb3abf4cae527818f238678826537389f')"
ZLM_PYTHON_STDLIB_IMAGE="$(resolve_env_or_default ZLM_PYTHON_STDLIB_IMAGE "$(dockerhub_library_mirror_ref 'python:3.12-slim-bookworm')")"
case "${NATIVE_RUNTIME_CACHE_DIR}" in
  /*) ;;
  *) NATIVE_RUNTIME_CACHE_DIR="${ROOT_DIR}/${NATIVE_RUNTIME_CACHE_DIR}" ;;
esac

usage() {
  cat <<EOF
用法:
  $(basename "$0") [--output-dir DIR] [--with-gpu|--without-gpu|--control-plane-minimal]
                 [--prebuilt-ui-dir DIR] [--skip-frontend-install]
                 [--refresh-runtime-cache|--offline-runtime-cache|--no-runtime-cache]
                 [--prune-images-after-extract]

说明:
  生成无 Docker 运行时依赖的 StreamServer native 离线包。构建机可以使用 Docker
  builder 和 Docker 镜像提取运行时资产；目标机安装运行不需要 Docker。
  未指定包变体时，脚本会在交互终端中询问要生成 CPU、GPU 还是 control-plane-minimal 包。

包变体:
  --without-gpu             生成 cpu-only 包，包含 CPU FFmpeg、ZLMediaKit、随包 PostgreSQL runtime
  --with-gpu                生成 gpu-enabled 包，在 cpu-only 基础上增加 GPU FFmpeg runtime
  --control-plane-minimal   只包含 media-core、streamserver-config 和 UI，数据库使用外部 PostgreSQL

runtime 缓存:
  --refresh-runtime-cache   忽略已有 runtime 缓存，重新从镜像提取并覆盖缓存
  --offline-runtime-cache   只允许使用已有 runtime 缓存；缺失或无效时失败，不拉取 runtime 镜像
  --no-runtime-cache        禁用 runtime 缓存，保持每次从镜像提取的旧行为
  --prune-images-after-extract
                            低磁盘模式：runtime 提取后立即删除相关镜像，退出时清理 Docker build cache

环境变量:
  RUST_BUILDER_IMAGE                  默认 ${RUST_BUILDER_IMAGE}
  MEDIA_AGENT_RUNTIME_BASE_IMAGE      默认 ${MEDIA_AGENT_RUNTIME_BASE_IMAGE}
  MEDIA_AGENT_GPU_RUNTIME_BASE_IMAGE  默认 ${MEDIA_AGENT_GPU_RUNTIME_BASE_IMAGE}
  POSTGRES_SOURCE_IMAGE               默认 ${POSTGRES_SOURCE_IMAGE}
  ZLM_SOURCE_IMAGE                    默认 ${ZLM_SOURCE_IMAGE}
  NATIVE_RUNTIME_CACHE_DIR            默认 ${NATIVE_RUNTIME_CACHE_DIR}
EOF
}

set_native_variant() {
  local variant="$1"
  if [ -n "${NATIVE_VARIANT}" ] && [ "${NATIVE_VARIANT}" != "${variant}" ]; then
    fail "只能指定一个 native 包变体，当前已选择 ${NATIVE_VARIANT}，不能再选择 ${variant}"
  fi
  NATIVE_VARIANT="${variant}"
}

parse_args() {
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --output-dir)
        [ "$#" -ge 2 ] || fail "--output-dir 需要参数"
        OUTPUT_DIR="$2"
        shift 2
        ;;
      --with-gpu)
        set_native_variant "gpu-enabled"
        shift
        ;;
      --without-gpu)
        set_native_variant "cpu-only"
        shift
        ;;
      --control-plane-minimal)
        set_native_variant "control-plane-minimal"
        shift
        ;;
      --prebuilt-ui-dir)
        [ "$#" -ge 2 ] || fail "--prebuilt-ui-dir 需要参数"
        PREBUILT_UI_DIR="$2"
        shift 2
        ;;
      --skip-frontend-install)
        FRONTEND_SKIP_INSTALL=1
        shift
        ;;
      --refresh-runtime-cache)
        REFRESH_RUNTIME_CACHE=1
        shift
        ;;
      --offline-runtime-cache)
        OFFLINE_RUNTIME_CACHE=1
        shift
        ;;
      --no-runtime-cache)
        NO_RUNTIME_CACHE=1
        shift
        ;;
      --prune-images-after-extract)
        PRUNE_IMAGES_AFTER_EXTRACT=1
        shift
        ;;
      -h|--help)
        PRUNE_IMAGES_AFTER_EXTRACT=0
        usage
        exit 0
        ;;
      *)
        fail "未知参数: $1"
        ;;
    esac
  done

  if [ "${NO_RUNTIME_CACHE}" -eq 1 ] && { [ "${REFRESH_RUNTIME_CACHE}" -eq 1 ] || [ "${OFFLINE_RUNTIME_CACHE}" -eq 1 ]; }; then
    fail "--no-runtime-cache 不能与 --refresh-runtime-cache 或 --offline-runtime-cache 同时使用"
  fi
  if [ "${REFRESH_RUNTIME_CACHE}" -eq 1 ] && [ "${OFFLINE_RUNTIME_CACHE}" -eq 1 ]; then
    fail "--refresh-runtime-cache 不能与 --offline-runtime-cache 同时使用"
  fi
}

prompt_native_variant() {
  local choice
  [ -n "${NATIVE_VARIANT}" ] && return 0
  if [ ! -t 0 ]; then
    fail "未指定 native 包变体；非交互环境请显式传 --without-gpu、--with-gpu 或 --control-plane-minimal"
  fi

  printf '%s\n' "请选择要构建的 native 包变体:" >&2
  printf '%s\n' "  1) cpu-only              CPU 版本，包含 CPU FFmpeg、ZLMediaKit、PostgreSQL runtime" >&2
  printf '%s\n' "  2) gpu-enabled           GPU 版本，在 CPU runtime 基础上增加 GPU FFmpeg runtime" >&2
  printf '%s\n' "  3) control-plane-minimal 只包含控制面、配置工具和 UI，数据库使用外部 PostgreSQL" >&2
  while true; do
    read -r -p "请输入 1/2/3 或 cpu/gpu/minimal: " choice || fail "读取 native 包变体失败"
    case "${choice}" in
      1|cpu|CPU|cpu-only|without-gpu|--without-gpu)
        NATIVE_VARIANT="cpu-only"
        return 0
        ;;
      2|gpu|GPU|gpu-enabled|with-gpu|--with-gpu)
        NATIVE_VARIANT="gpu-enabled"
        return 0
        ;;
      3|minimal|control-plane-minimal|--control-plane-minimal)
        NATIVE_VARIANT="control-plane-minimal"
        return 0
        ;;
      *)
        printf '%s\n' "无效选择: ${choice}" >&2
        ;;
    esac
  done
}

ensure_tools() {
  require_cmd docker
  docker info >/dev/null 2>&1 || fail "Docker 不可用；native 包允许构建阶段使用 Docker，但目标机不依赖 Docker"
  require_cmd openssl
  require_cmd tar
  if ! command -v shasum >/dev/null 2>&1 && ! command -v sha256sum >/dev/null 2>&1; then
    fail "缺少校验和命令: shasum 或 sha256sum"
  fi
}

prune_docker_image() {
  local image="$1"

  [ "${PRUNE_IMAGES_AFTER_EXTRACT}" -eq 1 ] || return 0
  [ -n "${image}" ] || return 0
  command -v docker >/dev/null 2>&1 || return 0

  if docker image inspect "${image}" >/dev/null 2>&1; then
    log "删除构建期镜像: ${image}"
    docker image rm "${image}" >/dev/null 2>&1 || log "镜像仍被占用，跳过删除: ${image}"
  fi
}

prune_musl_builder_images() {
  [ "${PRUNE_IMAGES_AFTER_EXTRACT}" -eq 1 ] || return 0
  prune_docker_image "streamserver-rust-musl-builder:${HOST_BINARY_TARGET_TRIPLE}"
  prune_docker_image "${RUST_BUILDER_IMAGE}"
  docker builder prune -af >/dev/null 2>&1 || true
}

prune_docker_build_artifacts() {
  [ "${PRUNE_IMAGES_AFTER_EXTRACT}" -eq 1 ] || return 0
  command -v docker >/dev/null 2>&1 || return 0
  docker info >/dev/null 2>&1 || return 0

  log "清理 Docker 构建缓存和未使用镜像"
  docker system prune -af >/dev/null 2>&1 || true
  docker builder prune -af >/dev/null 2>&1 || true
}

cleanup_temp_artifacts() {
  local status=$?

  if [ -n "${TEMP_STAGE_DIR}" ] && [ -d "${TEMP_STAGE_DIR}" ]; then
    rm -rf "${TEMP_STAGE_DIR}"
  fi
  prune_docker_build_artifacts
  exit "${status}"
}

trap cleanup_temp_artifacts EXIT

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

prepare_frontend_ui() {
  if [ -n "${PREBUILT_UI_DIR}" ]; then
    [ -f "${PREBUILT_UI_DIR}/index.html" ] || fail "PREBUILT_UI_DIR 不是有效前端静态资源目录: ${PREBUILT_UI_DIR}"
    return 0
  fi

  require_cmd node

  log "构建前端静态资源"
  if [ "${FRONTEND_SKIP_INSTALL}" -eq 1 ]; then
    node "${ROOT_DIR}/scripts/build-frontend-ui.mjs" --skip-install
  else
    node "${ROOT_DIR}/scripts/build-frontend-ui.mjs"
  fi
  PREBUILT_UI_DIR="${ROOT_DIR}/crates/media-core/ui"
}

export_business_binaries() {
  local output_dir="$1"
  local env_args=()
  local ephemeral_musl_cache=0
  local musl_cargo_home_dir="${MUSL_CARGO_HOME_DIR:-}"
  local musl_cargo_target_dir="${MUSL_CARGO_TARGET_DIR:-}"

  if [ "${PRUNE_IMAGES_AFTER_EXTRACT}" -eq 1 ] \
    && [ -n "${TEMP_STAGE_DIR}" ] \
    && [ -z "${musl_cargo_home_dir}" ] \
    && [ -z "${musl_cargo_target_dir}" ]; then
    ephemeral_musl_cache=1
    musl_cargo_home_dir="${TEMP_STAGE_DIR}/musl-cargo-home"
    musl_cargo_target_dir="${TEMP_STAGE_DIR}/musl-target"
  fi

  env_args=(
    "APT_MIRROR=${APT_MIRROR}"
    "CARGO_REGISTRY_MIRROR=${CARGO_REGISTRY_MIRROR}"
    "RUST_BUILDER_IMAGE=${RUST_BUILDER_IMAGE}"
  )
  if [ -n "${musl_cargo_home_dir}" ]; then
    env_args+=("MUSL_CARGO_HOME_DIR=${musl_cargo_home_dir}")
  fi
  if [ -n "${musl_cargo_target_dir}" ]; then
    env_args+=("MUSL_CARGO_TARGET_DIR=${musl_cargo_target_dir}")
  fi

  mkdir -p "${output_dir}"
  log "构建 Linux AMD64 musl 业务二进制"
  env "${env_args[@]}" bash "${BUILD_MUSL_BIN_SCRIPT}" \
    --target-triple "${HOST_BINARY_TARGET_TRIPLE}" \
    --package media-core \
    --package media-agent \
    --package media-gateway \
    --package streamserver-config \
    --output-dir "${output_dir}"
  if [ "${ephemeral_musl_cache}" -eq 1 ]; then
    rm -rf "${musl_cargo_home_dir}" "${musl_cargo_target_dir}"
  fi
  prune_musl_builder_images
}

pull_linux_amd64_image() {
  local image="$1"
  log "准备 linux/amd64 镜像: ${image}"
  if docker pull --platform linux/amd64 "${image}" >/dev/null; then
    return 0
  fi
  if docker image inspect "${image}" >/dev/null 2>&1; then
    log "拉取镜像失败，复用本地已有镜像: ${image}"
    return 0
  fi
  fail "拉取镜像失败且本地不存在: ${image}"
}

checksum_stdin() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum | awk '{ print $1 }'
  else
    shasum -a 256 | awk '{ print $1 }'
  fi
}

runtime_cache_key() {
  local kind="$1"
  local image="$2"
  shift 2
  {
    printf 'kind=%s\n' "${kind}"
    printf 'image=%s\n' "${image}"
    printf 'platform=linux/amd64\n'
    printf 'extractor_version=%s\n' "${NATIVE_RUNTIME_CACHE_EXTRACTOR_VERSION}"
    while [ "$#" -gt 0 ]; do
      printf 'arg=%s\n' "$1"
      shift
    done
  } | checksum_stdin
}

runtime_cache_path() {
  local kind="$1"
  local key="$2"
  printf '%s/%s/%s' "${NATIVE_RUNTIME_CACHE_DIR}" "${kind}" "${key}"
}

write_runtime_cache_checksums() {
  local cache_dir="$1"
  (
    cd "${cache_dir}/payload"
    if command -v sha256sum >/dev/null 2>&1; then
      find . -type f -print | LC_ALL=C sort | while read -r file; do
        sha256sum "${file#./}"
      done >"${cache_dir}/SHA256SUMS"
    else
      find . -type f -print | LC_ALL=C sort | while read -r file; do
        shasum -a 256 "${file#./}"
      done >"${cache_dir}/SHA256SUMS"
    fi
  )
  [ -s "${cache_dir}/SHA256SUMS" ]
}

runtime_cache_valid() {
  local cache_dir="$1"
  [ -d "${cache_dir}/payload" ] || return 1
  [ -s "${cache_dir}/SHA256SUMS" ] || return 1
  (
    cd "${cache_dir}/payload"
    if command -v sha256sum >/dev/null 2>&1; then
      sha256sum -c "${cache_dir}/SHA256SUMS" >/dev/null 2>&1
    else
      shasum -a 256 -c "${cache_dir}/SHA256SUMS" >/dev/null 2>&1
    fi
  )
}

copy_runtime_from_cache() {
  local cache_dir="$1"
  local output_dir="$2"
  rm -rf "${output_dir}"
  mkdir -p "${output_dir}"
  cp -R "${cache_dir}/payload/." "${output_dir}/"
}

write_runtime_cache_metadata() {
  local cache_dir="$1"
  local kind="$2"
  local image="$3"
  local key="$4"
  shift 4
  {
    printf 'CACHE_KEY=%s\n' "${key}"
    printf 'KIND=%s\n' "${kind}"
    printf 'SOURCE_IMAGE=%s\n' "${image}"
    printf 'PLATFORM=linux/amd64\n'
    printf 'EXTRACTOR_VERSION=%s\n' "${NATIVE_RUNTIME_CACHE_EXTRACTOR_VERSION}"
    printf 'CREATED_AT=%s\n' "$(date -u '+%Y-%m-%dT%H:%M:%SZ')"
    while [ "$#" -gt 0 ]; do
      printf 'EXTRACT_ARG=%s\n' "$1"
      shift
    done
  } >"${cache_dir}/metadata.env"
}

runtime_cache_lock_stale() {
  local lock_dir="$1"
  local pid=""

  [ -f "${lock_dir}/pid" ] || return 1
  pid="$(cat "${lock_dir}/pid" 2>/dev/null || true)"
  case "${pid}" in
    ""|*[!0-9]*)
      return 0
      ;;
  esac
  kill -0 "${pid}" 2>/dev/null && return 1
  return 0
}

acquire_runtime_cache_lock() {
  local lock_dir="$1"
  local waited=0

  # runtime 资产缓存可能被 CPU/GPU 两个包并行复用，用目录锁避免半写入缓存被读取。
  while ! mkdir "${lock_dir}" 2>/dev/null; do
    if runtime_cache_lock_stale "${lock_dir}"; then
      log "清理 stale runtime 缓存锁: ${lock_dir}"
      rm -rf "${lock_dir}"
      continue
    fi
    waited=$((waited + 1))
    if [ $((waited % 30)) -eq 0 ]; then
      log "等待 runtime 缓存锁: ${lock_dir}"
    fi
    sleep 1
  done
  printf '%s\n' "$$" >"${lock_dir}/pid"
}

release_runtime_cache_lock() {
  local lock_dir="$1"
  rm -rf "${lock_dir}"
}

extract_runtime_with_cache() {
  local kind="$1"
  local image="$2"
  local output_dir="$3"
  local extractor="$4"
  shift 4
  local key cache_dir tmp_dir parent_dir lock_dir

  if [ "${NO_RUNTIME_CACHE}" -eq 1 ]; then
    # 调试时可禁用缓存，强制保持每次从镜像重新提取的旧行为。
    pull_linux_amd64_image "${image}"
    if ! "${extractor}" "${image}" "${output_dir}" "$@"; then
      prune_docker_image "${image}"
      fail "runtime 提取失败: ${kind} (${image})"
    fi
    prune_docker_image "${image}"
    return 0
  fi

  key="$(runtime_cache_key "${kind}" "${image}" "$@")"
  cache_dir="$(runtime_cache_path "${kind}" "${key}")"
  parent_dir="${NATIVE_RUNTIME_CACHE_DIR}/${kind}"
  lock_dir="${parent_dir}/${key}.lock"
  mkdir -p "${parent_dir}"
  acquire_runtime_cache_lock "${lock_dir}"

  # 命中有效缓存时不再拉镜像，保证离线/重复打包只依赖本地缓存目录。
  if [ "${REFRESH_RUNTIME_CACHE}" -eq 0 ] && runtime_cache_valid "${cache_dir}"; then
    log "复用 runtime 缓存: ${kind} (${image})"
    if ! copy_runtime_from_cache "${cache_dir}" "${output_dir}"; then
      release_runtime_cache_lock "${lock_dir}"
      fail "runtime 缓存复制失败: ${kind} (${cache_dir})"
    fi
    release_runtime_cache_lock "${lock_dir}"
    return 0
  fi

  if [ -e "${cache_dir}" ] && [ "${REFRESH_RUNTIME_CACHE}" -eq 0 ]; then
    log "runtime 缓存无效，将重新提取: ${kind} (${cache_dir})"
  fi

  if [ "${OFFLINE_RUNTIME_CACHE}" -eq 1 ]; then
    # 离线缓存模式用于验证构建机是否已经具备全部 runtime 资产。
    release_runtime_cache_lock "${lock_dir}"
    fail "runtime 缓存缺失或无效: ${kind} (${image}); cache=${cache_dir}"
  fi

  pull_linux_amd64_image "${image}"
  tmp_dir="$(mktemp -d "${parent_dir}/.tmp.${key}.XXXXXX")"
  mkdir -p "${tmp_dir}/payload"

  # 先写临时目录并校验 checksum，最后原子替换正式缓存，避免留下部分提取结果。
  if ! "${extractor}" "${image}" "${tmp_dir}/payload" "$@"; then
    prune_docker_image "${image}"
    rm -rf "${tmp_dir}"
    release_runtime_cache_lock "${lock_dir}"
    fail "runtime 提取失败: ${kind} (${image})"
  fi
  prune_docker_image "${image}"

  if ! write_runtime_cache_metadata "${tmp_dir}" "${kind}" "${image}" "${key}" "$@"; then
    rm -rf "${tmp_dir}"
    release_runtime_cache_lock "${lock_dir}"
    fail "runtime 缓存 metadata 写入失败: ${kind} (${tmp_dir})"
  fi
  if ! write_runtime_cache_checksums "${tmp_dir}"; then
    rm -rf "${tmp_dir}"
    release_runtime_cache_lock "${lock_dir}"
    fail "runtime 缓存为空: ${kind} (${image})"
  fi
  if ! { rm -rf "${cache_dir}" && mv "${tmp_dir}" "${cache_dir}"; }; then
    rm -rf "${tmp_dir}"
    release_runtime_cache_lock "${lock_dir}"
    fail "runtime 缓存写入失败: ${kind} (${cache_dir})"
  fi
  log "已写入 runtime 缓存: ${kind} (${cache_dir})"

  if ! copy_runtime_from_cache "${cache_dir}" "${output_dir}"; then
    release_runtime_cache_lock "${lock_dir}"
    fail "runtime 缓存复制失败: ${kind} (${cache_dir})"
  fi
  release_runtime_cache_lock "${lock_dir}"
}

extract_commands_from_image() {
  local image="$1"
  local output_dir="$2"
  shift 2
  local commands="$*"

  rm -rf "${output_dir}"
  mkdir -p "${output_dir}"
  log "从 ${image} 提取命令: ${commands}"
  EXPORT_COMMANDS="${commands}" docker run --rm --platform linux/amd64 --entrypoint sh -e EXPORT_COMMANDS "${image}" -eu -c '
    export_dir=/tmp/streamserver-export
    rm -rf "${export_dir}"
    mkdir -p "${export_dir}/bin" "${export_dir}/lib"
    copy_deps() {
      binary="$1"
      if ! ldd "${binary}" >/tmp/ldd.out 2>/tmp/ldd.err; then
        cat /tmp/ldd.out /tmp/ldd.err >/tmp/ldd.all || true
      else
        cat /tmp/ldd.out >/tmp/ldd.all
      fi
      awk "
        /=> \\/.*\\(/ { print \$3 }
        /^[[:space:]]*\\/.*\\(/ { print \$1 }
        /ld-linux/ {
          for (i = 1; i <= NF; i++) if (\$i ~ /^\\//) print \$i
        }
      " /tmp/ldd.all | sort -u | while read -r lib; do
        [ -n "${lib}" ] || continue
        [ -f "${lib}" ] || continue
        cp -L "${lib}" "${export_dir}/lib/$(basename "${lib}")"
      done
    }
    for command_name in ${EXPORT_COMMANDS}; do
      binary_path="$(command -v "${command_name}")"
      cp -L "${binary_path}" "${export_dir}/bin/${command_name}"
      chmod 755 "${export_dir}/bin/${command_name}"
      copy_deps "${binary_path}"
    done
    tar -C "${export_dir}" -cf - .
  ' | tar -C "${output_dir}" -xf -
}

extract_zlm_runtime() {
  local image="$1"
  local output_dir="$2"

  rm -rf "${output_dir}"
  mkdir -p "${output_dir}"
  log "从 ${image} 提取 ZLMediaKit runtime"
  docker run --rm --platform linux/amd64 --entrypoint sh "${image}" -eu -c '
    export_dir=/tmp/streamserver-export
    rm -rf "${export_dir}"
    mkdir -p "${export_dir}/lib"
    media_server="$(command -v MediaServer 2>/dev/null || true)"
    if [ -z "${media_server}" ]; then
      media_server="$(find / -type f -name MediaServer -perm -111 2>/dev/null | head -n 1)"
    fi
    [ -n "${media_server}" ] || { echo "MediaServer not found" >&2; exit 1; }
    default_pem="$(find / -type f -name default.pem 2>/dev/null | head -n 1)"
    [ -n "${default_pem}" ] || { echo "default.pem not found" >&2; exit 1; }
    cp -L "${media_server}" "${export_dir}/MediaServer"
    cp -L "${default_pem}" "${export_dir}/default.pem"
    chmod 755 "${export_dir}/MediaServer"
    python_version="$(python3 -c "import sys; print(f\"{sys.version_info.major}.{sys.version_info.minor}\")" 2>/dev/null || true)"
    if [ -n "${python_version}" ]; then
      for python_lib_root in /usr/local/lib /usr/lib; do
        if [ -d "${python_lib_root}/python${python_version}" ]; then
          mkdir -p "${export_dir}/python/lib"
          cp -a "${python_lib_root}/python${python_version}" "${export_dir}/python/lib/"
          break
        fi
      done
    fi
    if ! ldd "${media_server}" >/tmp/ldd.out 2>/tmp/ldd.err; then
      cat /tmp/ldd.out /tmp/ldd.err >/tmp/ldd.all || true
    else
      cat /tmp/ldd.out >/tmp/ldd.all
    fi
    awk "
      /=> \\/.*\\(/ { print \$3 }
      /^[[:space:]]*\\/.*\\(/ { print \$1 }
      /ld-linux/ {
        for (i = 1; i <= NF; i++) if (\$i ~ /^\\//) print \$i
      }
    " /tmp/ldd.all | sort -u | while read -r lib; do
      [ -n "${lib}" ] || continue
      [ -f "${lib}" ] || continue
      cp -L "${lib}" "${export_dir}/lib/$(basename "${lib}")"
    done
    tar -C "${export_dir}" -cf - .
  ' | tar -C "${output_dir}" -xf -
}

zlm_python_version() {
  local output_dir="$1"
  find "${output_dir}/lib" -maxdepth 1 -type f -name 'libpython*.so*' 2>/dev/null \
    | sed -n 's#.*libpython\([0-9][0-9]*\.[0-9][0-9]*\).*#\1#p' \
    | head -n 1
}

extract_python_stdlib_runtime() {
  local image="$1"
  local output_dir="$2"
  local python_version="$3"

  [ -n "${python_version}" ] || return 0
  if [ -f "${output_dir}/lib/python${python_version}/encodings/__init__.py" ]; then
    return 0
  fi

  rm -rf "${output_dir}"
  mkdir -p "${output_dir}"
  log "从 ${image} 提取 Python ${python_version} stdlib"
  PYTHON_VERSION="${python_version}" docker run --rm --platform linux/amd64 --entrypoint sh -e PYTHON_VERSION "${image}" -eu -c '
    export_dir=/tmp/streamserver-python-export
    rm -rf "${export_dir}"
    mkdir -p "${export_dir}/lib"
    for python_lib_root in /usr/local/lib /usr/lib; do
      if [ -d "${python_lib_root}/python${PYTHON_VERSION}" ]; then
        cp -a "${python_lib_root}/python${PYTHON_VERSION}" "${export_dir}/lib/"
        break
      fi
    done
    [ -f "${export_dir}/lib/python${PYTHON_VERSION}/encodings/__init__.py" ] || {
      echo "Python stdlib encodings not found for ${PYTHON_VERSION}" >&2
      exit 1
    }
    tar -C "${export_dir}" -cf - .
  ' | tar -C "${output_dir}" -xf -
}

extract_postgres_runtime() {
  local image="$1"
  local output_dir="$2"

  rm -rf "${output_dir}"
  mkdir -p "${output_dir}"
  log "从 ${image} 提取 PostgreSQL runtime"
  docker run --rm --platform linux/amd64 --entrypoint sh "${image}" -eu -c '
    export_dir=/tmp/streamserver-export
    rm -rf "${export_dir}"
    mkdir -p "${export_dir}/bin" "${export_dir}/lib" "${export_dir}/share"
    find_postgres_binary() {
      command_name="$1"
      for candidate in \
        "/usr/local/pgsql/bin/${command_name}" \
        "/usr/local/postgresql/bin/${command_name}" \
        /usr/local/lib/postgresql/*/bin/"${command_name}" \
        /usr/lib/postgresql/*/bin/"${command_name}"
      do
        [ -x "${candidate}" ] || continue
        printf "%s\n" "${candidate}"
        return 0
      done
      command -v "${command_name}"
    }
    postgres_share_dir() {
      for candidate in \
        /usr/local/share/postgresql \
        /usr/share/postgresql
      do
        [ -d "${candidate}" ] || continue
        printf "%s\n" "${candidate}"
        return 0
      done
      return 1
    }
    postgres_lib_root() {
      for candidate in \
        /usr/local/lib/postgresql \
        /usr/lib/postgresql
      do
        [ -d "${candidate}" ] || continue
        printf "%s\n" "${candidate}"
        return 0
      done
      return 1
    }
    copy_deps() {
      binary="$1"
      if ! ldd "${binary}" >/tmp/ldd.out 2>/tmp/ldd.err; then
        cat /tmp/ldd.out /tmp/ldd.err >/tmp/ldd.all || true
      else
        cat /tmp/ldd.out >/tmp/ldd.all
      fi
      awk "
        /=> \\/.*\\(/ { print \$3 }
        /^[[:space:]]*\\/.*\\(/ { print \$1 }
        /ld-linux/ {
          for (i = 1; i <= NF; i++) if (\$i ~ /^\\//) print \$i
        }
      " /tmp/ldd.all | sort -u | while read -r lib; do
        [ -n "${lib}" ] || continue
        [ -f "${lib}" ] || continue
        cp -L "${lib}" "${export_dir}/lib/$(basename "${lib}")"
      done
    }
    share_dir="$(postgres_share_dir)"
    cp -a "${share_dir}" "${export_dir}/share/postgresql"

    lib_root="$(postgres_lib_root)"
    if [ -n "${lib_root}" ]; then
      mkdir -p "${export_dir}/lib/postgresql"
      cp -a "${lib_root}/." "${export_dir}/lib/postgresql/"
      find "${lib_root}" -path "*/bin/*" -type f -perm /111 -print | sort | while read -r postgres_tool; do
        command_name="$(basename "${postgres_tool}")"
        cp -L "${postgres_tool}" "${export_dir}/bin/${command_name}"
        chmod 755 "${export_dir}/bin/${command_name}"
      done
      for required_command in postgres initdb pg_ctl pg_isready psql pg_dump pg_restore pg_dumpall pg_basebackup; do
        [ -x "${export_dir}/bin/${required_command}" ] || {
          binary_path="$(find_postgres_binary "${required_command}")"
          cp -L "${binary_path}" "${export_dir}/bin/${required_command}"
          chmod 755 "${export_dir}/bin/${required_command}"
        }
      done
      find "${lib_root}" -path "*/bin/*" -type f -perm /111 -print | while read -r postgres_tool; do
        copy_deps "${postgres_tool}"
      done
      find "${lib_root}" -type f -name "*.so" -print | while read -r extension_lib; do
        copy_deps "${extension_lib}"
      done
    fi

    manifest="${export_dir}/postgres-extension-manifest.tsv"
    : >"${manifest}"
    find "${share_dir}" -path "*/extension/*.control" -type f -print | sort | while read -r control_file; do
      extension_name="$(basename "${control_file}" .control)"
      default_version="$(
        awk -F= "
          /^[[:space:]]*default_version[[:space:]]*=/ {
            value = \$2
            gsub(/^[[:space:]]+|[[:space:]]+$/, \"\", value)
            gsub(/^'\''|'\''$/, \"\", value)
            print value
            exit
          }
        " "${control_file}"
      )"
      relative_control="${control_file#${share_dir}/}"
      printf "%s\t%s\t%s\n" "${extension_name}" "${default_version}" "${relative_control}" >>"${manifest}"
    done
    [ -s "${manifest}" ] || {
      echo "PostgreSQL extension manifest is empty" >&2
      exit 1
    }
    tar -C "${export_dir}" -cf - .
  ' | tar -C "${output_dir}" -xf -
}

copy_static_assets() {
  local bundle_root="$1"
  mkdir -p "${bundle_root}/templates/systemd" "${bundle_root}/templates/common" "${bundle_root}/docs"
  # 安装器和卸载器随包携带，目标机不需要访问源码仓库。
  cp "${ROOT_DIR}/packaging/native/install.sh" "${bundle_root}/install.sh"
  cp "${ROOT_DIR}/packaging/native/uninstall.sh" "${bundle_root}/uninstall.sh"
  chmod +x "${bundle_root}/install.sh"
  chmod +x "${bundle_root}/uninstall.sh"
  cp -R "${ROOT_DIR}/packaging/native/templates/systemd/." "${bundle_root}/templates/systemd/"
  cp -R "${ROOT_DIR}/packaging/native/templates/common/." "${bundle_root}/templates/common/"
  if [ -f "${ROOT_DIR}/docs/zh/08-native-deployment.md" ]; then
    cp "${ROOT_DIR}/docs/zh/08-native-deployment.md" "${bundle_root}/docs/native-deployment.md"
  fi
}

copy_business_artifacts() {
  local bundle_root="$1"
  local binaries_dir="$2"

  mkdir -p "${bundle_root}/binaries" "${bundle_root}/ui/media-core"
  cp "${binaries_dir}/media-core" "${bundle_root}/binaries/media-core-linux-amd64"
  cp "${binaries_dir}/media-agent" "${bundle_root}/binaries/media-agent-linux-amd64"
  cp "${binaries_dir}/media-gateway" "${bundle_root}/binaries/media-gateway-linux-amd64"
  cp "${binaries_dir}/streamserver-config" "${bundle_root}/binaries/streamserver-config-linux-amd64"
  chmod 755 "${bundle_root}"/binaries/*-linux-amd64
  cp -R "${PREBUILT_UI_DIR}/." "${bundle_root}/ui/media-core/"
}

write_manifest() {
  local bundle_root="$1"
  local bundle_version="$2"
  local postgres_runtime="$3"
  local worker_runtime="$4"
  local gpu_runtime="$5"

  # manifest 只记录包内相对路径，安装到相同相对目录结构即可兼容网络挂载。
  cat >"${bundle_root}/package-manifest.env" <<EOF
BUNDLE_VERSION=${bundle_version}
BUNDLE_VARIANT=${NATIVE_VARIANT}
BUNDLE_GPU_SUPPORT=${gpu_runtime}
BUNDLE_WORKER_SUPPORT=${worker_runtime}
BUNDLE_POSTGRES_RUNTIME=${postgres_runtime}
DEPLOY_MODE=native
MEDIA_CORE_BINARY_PATH=binaries/media-core-linux-amd64
MEDIA_AGENT_BINARY_PATH=binaries/media-agent-linux-amd64
MEDIA_GATEWAY_BINARY_PATH=binaries/media-gateway-linux-amd64
STREAMSERVER_CONFIG_BINARY_PATH=binaries/streamserver-config-linux-amd64
MEDIA_CORE_UI_PATH=ui/media-core
FFMPEG_CPU_BINARY_PATH=runtime/ffmpeg/cpu/bin/ffmpeg
FFPROBE_CPU_BINARY_PATH=runtime/ffmpeg/cpu/bin/ffprobe
FFMPEG_CPU_LIB_PATH=runtime/ffmpeg/cpu/lib
FFMPEG_GPU_BINARY_PATH=runtime/ffmpeg/gpu/bin/ffmpeg
FFPROBE_GPU_BINARY_PATH=runtime/ffmpeg/gpu/bin/ffprobe
FFMPEG_GPU_LIB_PATH=runtime/ffmpeg/gpu/lib
ZLM_BINARY_PATH=runtime/zlm/MediaServer
ZLM_DEFAULT_PEM_PATH=runtime/zlm/default.pem
ZLM_LIB_PATH=runtime/zlm/lib
POSTGRES_RUNTIME_PATH=runtime/postgres
POSTGRES_BIN_PATH=runtime/postgres/bin
POSTGRES_LIB_PATH=runtime/postgres/lib
POSTGRES_EXTENSION_MANIFEST_PATH=runtime/postgres/postgres-extension-manifest.tsv
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
builder_os=Linux
builder_arch=x86_64
git_commit=${commit}
bundle_variant=${NATIVE_VARIANT}
target_runtime=docker-free
verification_recommended_location=target-server
EOF
}

normalize_bundle_permissions() {
  local bundle_root="$1"
  python3 - "${bundle_root}" <<'PY'
import os
import pathlib
import stat
import sys

root = pathlib.Path(sys.argv[1])
root_mode = root.lstat().st_mode
if stat.S_ISDIR(root_mode):
    os.chmod(root, stat.S_IMODE(root_mode) & ~0o7022, follow_symlinks=False)
for directory, directories, files in os.walk(root, followlinks=False):
    for name in directories + files:
        path = pathlib.Path(directory) / name
        mode = path.lstat().st_mode
        if not (stat.S_ISDIR(mode) or stat.S_ISREG(mode)):
            continue
        current = stat.S_IMODE(mode)
        normalized = current & ~0o7022
        if normalized != current:
            os.chmod(path, normalized, follow_symlinks=False)
PY
}

write_checksums() {
  local bundle_root="$1"
  (
    cd "${bundle_root}"
    if command -v shasum >/dev/null 2>&1; then
      find . -type f ! -path ./SHA256SUMS -print | LC_ALL=C sort | while read -r file; do
        shasum -a 256 "${file#./}"
      done >SHA256SUMS
    else
      find . -type f ! -path ./SHA256SUMS -print | LC_ALL=C sort | while read -r file; do
        sha256sum "${file#./}"
      done >SHA256SUMS
    fi
  )
  chmod 0644 "${bundle_root}/SHA256SUMS"
}

assert_no_docker_runtime_assets() {
  local bundle_root="$1"
  # Docker 只允许作为构建期提取工具，安装包内不得带 Compose 或镜像运行时资产。
  if [ -n "$(find "${bundle_root}" \
    \( -path '*/images/*' -o -name compose.yml -o -name docker-compose.yml \
      -o -name streamserver-compose \) -print -quit)" ]; then
    fail "native 包中发现 Docker/Compose 运行时资产"
  fi
  if [ -d "${bundle_root}/tools/docker" ]; then
    fail "native 包中不得包含 tools/docker"
  fi
}

create_archive() {
  local stage_dir="$1"
  local bundle_name="$2"
  local archive_path="$3"
  mkdir -p "$(dirname "${archive_path}")"
  COPYFILE_DISABLE=1 tar --no-xattrs --exclude '.DS_Store' --exclude '._*' -czf "${archive_path}" -C "${stage_dir}" "${bundle_name}"
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
  local version bundle_version build_date bundle_name_base bundle_name
  local stage_dir bundle_root archive_path binaries_dir
  local zlm_python_runtime_version=""
  local include_worker="true"
  local include_gpu="false"
  local include_postgres="true"

  parse_args "$@"
  prompt_native_variant
  ensure_tools
  prepare_frontend_ui

  case "${NATIVE_VARIANT}" in
    cpu-only)
      ;;
    gpu-enabled)
      include_gpu="true"
      ;;
    control-plane-minimal)
      include_worker="false"
      include_postgres="false"
      ;;
    *)
      fail "不支持的 native 包变体: ${NATIVE_VARIANT}"
      ;;
  esac

  version="$(workspace_version)"
  [ -n "${version}" ] || fail "无法从 Cargo.toml 解析版本号"
  bundle_version="v${version}"
  build_date="$(date '+%Y%m%d')"
  bundle_name_base="streamserver-native-${bundle_version}-linux-amd64-${NATIVE_VARIANT}-${build_date}"
  mkdir -p "${OUTPUT_DIR}"
  bundle_name="$(resolve_bundle_name "${OUTPUT_DIR}" "${bundle_name_base}")"
  archive_path="${OUTPUT_DIR}/${bundle_name}.tar.gz"

  stage_dir="$(mktemp -d "${TMPDIR:-/tmp}/streamserver-native.XXXXXX")"
  TEMP_STAGE_DIR="${stage_dir}"
  bundle_root="${stage_dir}/${bundle_name}"
  binaries_dir="${stage_dir}/binaries"
  mkdir -p "${bundle_root}"

  export_business_binaries "${binaries_dir}"
  copy_business_artifacts "${bundle_root}" "${binaries_dir}"
  copy_static_assets "${bundle_root}"

  if [ "${include_worker}" = "true" ]; then
    extract_runtime_with_cache \
      "ffmpeg-cpu" \
      "${MEDIA_AGENT_RUNTIME_BASE_IMAGE}" \
      "${bundle_root}/runtime/ffmpeg/cpu" \
      extract_commands_from_image \
      ffmpeg ffprobe
    extract_runtime_with_cache \
      "zlm" \
      "${ZLM_SOURCE_IMAGE}" \
      "${bundle_root}/runtime/zlm" \
      extract_zlm_runtime
    zlm_python_runtime_version="$(zlm_python_version "${bundle_root}/runtime/zlm")"
    if [ -n "${zlm_python_runtime_version}" ]; then
      extract_runtime_with_cache \
        "zlm-python-stdlib" \
        "${ZLM_PYTHON_STDLIB_IMAGE}" \
        "${bundle_root}/runtime/zlm/python" \
        extract_python_stdlib_runtime \
        "${zlm_python_runtime_version}"
    fi
  fi

  if [ "${include_gpu}" = "true" ]; then
    extract_runtime_with_cache \
      "ffmpeg-gpu" \
      "${MEDIA_AGENT_GPU_RUNTIME_BASE_IMAGE}" \
      "${bundle_root}/runtime/ffmpeg/gpu" \
      extract_commands_from_image \
      ffmpeg ffprobe
  fi

  if [ "${include_postgres}" = "true" ]; then
    extract_runtime_with_cache \
      "postgres" \
      "${POSTGRES_SOURCE_IMAGE}" \
      "${bundle_root}/runtime/postgres" \
      extract_postgres_runtime
  fi

  write_manifest "${bundle_root}" "${bundle_version}" "${include_postgres}" "${include_worker}" "${include_gpu}"
  write_build_info "${bundle_root}" "${bundle_name}" "${version}"
  normalize_bundle_permissions "${bundle_root}"
  write_checksums "${bundle_root}"
  assert_no_docker_runtime_assets "${bundle_root}"
  create_archive "${stage_dir}" "${bundle_name}" "${archive_path}"

  log "native 离线包已生成: ${archive_path}"
  log "校验文件已生成: ${archive_path}.sha256"
  log "推荐下一步在目标服务器上验收: ./scripts/verify-native-bundle-on-target.sh --bundle ${archive_path} --host <target-host>"
}

main "$@"
