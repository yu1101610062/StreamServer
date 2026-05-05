#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
OUTPUT_DIR="${ROOT_DIR}/dist/musl-bin"
TARGET_TRIPLE="x86_64-unknown-linux-musl"
PACKAGES=()

APT_MIRROR="${APT_MIRROR:-}"
CARGO_REGISTRY_MIRROR="${CARGO_REGISTRY_MIRROR:-}"
RUST_BUILDER_IMAGE="${RUST_BUILDER_IMAGE:-rust:1.85-bookworm}"
MUSL_CARGO_HOME_DIR="${MUSL_CARGO_HOME_DIR:-${ROOT_DIR}/.build-cache/musl/cargo-home}"
MUSL_CARGO_TARGET_DIR="${MUSL_CARGO_TARGET_DIR:-${ROOT_DIR}/target/docker-musl}"
MUSL_REBUILD_BUILDER="${MUSL_REBUILD_BUILDER:-0}"

log() {
  printf '[musl-binaries] %s\n' "$*"
}

fail() {
  printf '[musl-binaries] ERROR: %s\n' "$*" >&2
  exit 1
}

usage() {
  cat <<EOF
用法:
  $(basename "$0") [--target-triple TARGET] [--package NAME ...] [--output-dir DIR]

说明:
  使用官方 Rust Docker 镜像构建 musl 目标二进制，并保留 Cargo/target 缓存。
  默认构建 media-core 和 media-agent。

参数:
  --target-triple TARGET  目标三元组，默认 x86_64-unknown-linux-musl；支持: x86_64-unknown-linux-musl, aarch64-unknown-linux-musl
  --package NAME          需要构建的 Cargo package，可重复；默认: media-core, media-agent
  --output-dir DIR        输出目录，默认 ./dist/musl-bin；构建完成后会导出同名二进制到该目录

环境变量:
  APT_MIRROR             默认留空，使用 Debian 官方源
  CARGO_REGISTRY_MIRROR  默认留空，使用 crates.io 官方源
  RUST_BUILDER_IMAGE     默认 rust:1.85-bookworm
  MUSL_CARGO_HOME_DIR    默认 ${ROOT_DIR}/.build-cache/musl/cargo-home
  MUSL_CARGO_TARGET_DIR  默认 ${ROOT_DIR}/target/docker-musl
EOF
}

require_cmd() {
  command -v "$1" >/dev/null 2>&1 || fail "缺少命令: $1"
}

docker_buildx_available() {
  docker buildx version >/dev/null 2>&1
}

append_package() {
  local package="$1"
  local existing
  if [ "${#PACKAGES[@]}" -gt 0 ]; then
    for existing in "${PACKAGES[@]}"; do
      if [ "${existing}" = "${package}" ]; then
        return 0
      fi
    done
  fi
  PACKAGES+=("${package}")
}

parse_args() {
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --target-triple)
        [ "$#" -ge 2 ] || fail "--target-triple 需要参数"
        TARGET_TRIPLE="$2"
        shift 2
        ;;
      --package)
        [ "$#" -ge 2 ] || fail "--package 需要参数"
        append_package "$2"
        shift 2
        ;;
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

  if [ "${#PACKAGES[@]}" -eq 0 ]; then
    append_package "media-core"
    append_package "media-agent"
  fi
}

resolve_target_platform() {
  case "${TARGET_TRIPLE}" in
    x86_64-unknown-linux-musl)
      printf '%s\n' "linux/amd64"
      ;;
    aarch64-unknown-linux-musl)
      printf '%s\n' "linux/arm64"
      ;;
    *)
      fail "不支持的 --target-triple: ${TARGET_TRIPLE}"
      ;;
  esac
}

ensure_tools() {
  require_cmd docker
  docker info >/dev/null 2>&1 || fail "Docker 不可用，请先启动 Docker Desktop 或 Docker Engine"
  docker_buildx_available || fail "缺少 docker buildx"
}

write_cargo_config() {
  mkdir -p "${MUSL_CARGO_HOME_DIR}"
  if [ -n "${CARGO_REGISTRY_MIRROR}" ]; then
    cat >"${MUSL_CARGO_HOME_DIR}/config.toml" <<EOF
[source.crates-io]
replace-with = "mirror"

[source.mirror]
registry = "${CARGO_REGISTRY_MIRROR}"

[net]
git-fetch-with-cli = true
EOF
  else
    cat >"${MUSL_CARGO_HOME_DIR}/config.toml" <<'EOF'
[net]
git-fetch-with-cli = true
EOF
  fi
}

build_builder_image() {
  local platform="$1"
  local image_tag="streamserver-rust-musl-builder:${TARGET_TRIPLE}"

  if [ "${MUSL_REBUILD_BUILDER}" != "1" ] && docker image inspect "${image_tag}" >/dev/null 2>&1; then
    log "复用 builder 镜像: ${image_tag}"
    return 0
  fi

  log "构建 builder 镜像: ${image_tag} (${platform})"
  docker buildx build \
    --platform "${platform}" \
    --load \
    -f "${ROOT_DIR}/packaging/docker/Dockerfile.rust-musl-builder" \
    --build-arg RUST_BUILDER_IMAGE="${RUST_BUILDER_IMAGE}" \
    --build-arg DEBIAN_MIRROR="${APT_MIRROR}" \
    --build-arg TARGET_TRIPLE="${TARGET_TRIPLE}" \
    -t "${image_tag}" \
    "${ROOT_DIR}/packaging/docker" >/dev/null
}

run_build() {
  local platform="$1"
  local image_tag="streamserver-rust-musl-builder:${TARGET_TRIPLE}"
  local cargo_args=()
  local package=""

  mkdir -p "${OUTPUT_DIR}" "${MUSL_CARGO_HOME_DIR}" "${MUSL_CARGO_TARGET_DIR}"
  write_cargo_config

  for package in "${PACKAGES[@]}"; do
    cargo_args+=("-p" "${package}")
  done

  log "开始构建: ${PACKAGES[*]} -> ${TARGET_TRIPLE}"
  docker run --rm \
    --platform "${platform}" \
    --user "$(id -u):$(id -g)" \
    -e HOME=/tmp/streamserver-builder-home \
    -e CARGO_HOME=/cargo-home \
    -e CARGO_TARGET_DIR=/workspace-target \
    -v "${ROOT_DIR}:/workspace" \
    -v "${MUSL_CARGO_HOME_DIR}:/cargo-home" \
    -v "${MUSL_CARGO_TARGET_DIR}:/workspace-target" \
    -w /workspace \
    "${image_tag}" \
    cargo build --locked --release --target "${TARGET_TRIPLE}" "${cargo_args[@]}"
}

export_binaries() {
  local package=""
  local source_binary=""

  for package in "${PACKAGES[@]}"; do
    source_binary="${MUSL_CARGO_TARGET_DIR}/${TARGET_TRIPLE}/release/${package}"
    [ -f "${source_binary}" ] || fail "未找到构建产物: ${source_binary}"
    cp "${source_binary}" "${OUTPUT_DIR}/${package}"
    chmod 755 "${OUTPUT_DIR}/${package}"
  done
}

main() {
  local platform=""

  parse_args "$@"
  ensure_tools
  platform="$(resolve_target_platform)"
  build_builder_image "${platform}"
  run_build "${platform}"
  export_binaries
  log "构建完成，产物已导出到: ${OUTPUT_DIR}"
}

main "$@"
