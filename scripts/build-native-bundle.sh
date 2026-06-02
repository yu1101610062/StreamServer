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
FRONTEND_SOURCE_DIRS=()

DEFAULT_APT_MIRROR="http://mirrors.aliyun.com"
DEFAULT_CARGO_REGISTRY_MIRROR="sparse+https://rsproxy.cn/index/"
DEFAULT_NPM_REGISTRY_MIRROR="https://registry.npmmirror.com"
DOCKERHUB_MIRROR_HOST="m.daocloud.io"

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
NPM_REGISTRY_MIRROR="$(resolve_env_or_default NPM_REGISTRY_MIRROR "${DEFAULT_NPM_REGISTRY_MIRROR}")"
RUST_BUILDER_IMAGE="$(resolve_env_or_default RUST_BUILDER_IMAGE "$(dockerhub_library_mirror_ref 'rust:1.85-bookworm')")"
MEDIA_AGENT_RUNTIME_BASE_IMAGE="$(resolve_env_or_default MEDIA_AGENT_RUNTIME_BASE_IMAGE 'jrottenberg/ffmpeg:7.1-ubuntu2404')"
MEDIA_AGENT_GPU_RUNTIME_BASE_IMAGE="$(resolve_env_or_default MEDIA_AGENT_GPU_RUNTIME_BASE_IMAGE 'jrottenberg/ffmpeg:7.1-nvidia2204')"
POSTGRES_SOURCE_IMAGE="$(resolve_env_or_default POSTGRES_SOURCE_IMAGE "$(dockerhub_library_mirror_ref 'postgres:18.3')")"
ZLM_SOURCE_IMAGE="$(resolve_env_or_default ZLM_SOURCE_IMAGE 'zlmediakit/zlmediakit:master@sha256:8b24d1d4a30736b2001e5d78fc46057cb3abf4cae527818f238678826537389f')"
ZLM_PYTHON_STDLIB_IMAGE="$(resolve_env_or_default ZLM_PYTHON_STDLIB_IMAGE "$(dockerhub_library_mirror_ref 'python:3.12-slim-bookworm')")"

usage() {
  cat <<EOF
用法:
  $(basename "$0") [--output-dir DIR] [--with-gpu|--without-gpu|--control-plane-minimal]
                 [--prebuilt-ui-dir DIR] [--skip-frontend-install] [--desktop-source-dir DIR]

说明:
  生成无 Docker 运行时依赖的 StreamServer native 离线包。构建机可以使用 Docker
  builder 和 Docker 镜像提取运行时资产；目标机安装运行不需要 Docker。

包变体:
  --without-gpu             生成 cpu-only 包，包含 CPU FFmpeg、ZLMediaKit、随包 PostgreSQL runtime
  --with-gpu                生成 gpu-enabled 包，在 cpu-only 基础上增加 GPU FFmpeg runtime
  --control-plane-minimal   只包含 media-core、streamserver-config 和 UI，数据库使用外部 PostgreSQL

环境变量:
  RUST_BUILDER_IMAGE                  默认 ${RUST_BUILDER_IMAGE}
  MEDIA_AGENT_RUNTIME_BASE_IMAGE      默认 ${MEDIA_AGENT_RUNTIME_BASE_IMAGE}
  MEDIA_AGENT_GPU_RUNTIME_BASE_IMAGE  默认 ${MEDIA_AGENT_GPU_RUNTIME_BASE_IMAGE}
  POSTGRES_SOURCE_IMAGE               默认 ${POSTGRES_SOURCE_IMAGE}
  ZLM_SOURCE_IMAGE                    默认 ${ZLM_SOURCE_IMAGE}
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
      --with-gpu)
        NATIVE_VARIANT="gpu-enabled"
        shift
        ;;
      --without-gpu)
        NATIVE_VARIANT="cpu-only"
        shift
        ;;
      --control-plane-minimal)
        NATIVE_VARIANT="control-plane-minimal"
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
      --desktop-source-dir)
        [ "$#" -ge 2 ] || fail "--desktop-source-dir 需要参数"
        FRONTEND_SOURCE_DIRS+=("$2")
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

  if [ -z "${NATIVE_VARIANT}" ]; then
    NATIVE_VARIANT="cpu-only"
  fi
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
  local frontend_args=()
  local source_dir

  if [ -n "${PREBUILT_UI_DIR}" ]; then
    [ -f "${PREBUILT_UI_DIR}/index.html" ] || fail "PREBUILT_UI_DIR 不是有效前端静态资源目录: ${PREBUILT_UI_DIR}"
    return 0
  fi

  require_cmd node
  frontend_args+=(--allow-missing-installers)
  if [ "${FRONTEND_SKIP_INSTALL}" -eq 1 ]; then
    frontend_args+=(--skip-install)
  fi
  if [ "${#FRONTEND_SOURCE_DIRS[@]}" -gt 0 ]; then
    for source_dir in "${FRONTEND_SOURCE_DIRS[@]}"; do
      frontend_args+=(--source-dir "${source_dir}")
    done
  fi

  log "构建前端静态资源"
  node "${ROOT_DIR}/scripts/build-frontend-ui.mjs" "${frontend_args[@]}"
  PREBUILT_UI_DIR="${ROOT_DIR}/crates/media-core/ui"
}

export_business_binaries() {
  local output_dir="$1"
  mkdir -p "${output_dir}"
  log "构建 Linux AMD64 musl 业务二进制"
  APT_MIRROR="${APT_MIRROR}" \
  CARGO_REGISTRY_MIRROR="${CARGO_REGISTRY_MIRROR}" \
  RUST_BUILDER_IMAGE="${RUST_BUILDER_IMAGE}" \
    bash "${BUILD_MUSL_BIN_SCRIPT}" \
      --target-triple "${HOST_BINARY_TARGET_TRIPLE}" \
      --package media-core \
      --package media-agent \
      --package streamserver-config \
      --output-dir "${output_dir}"
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

  pull_linux_amd64_image "${image}"
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
  EXPORT_COMMANDS="postgres initdb pg_ctl pg_isready psql" docker run --rm --platform linux/amd64 --entrypoint sh -e EXPORT_COMMANDS "${image}" -eu -c '
    export_dir=/tmp/streamserver-export
    rm -rf "${export_dir}"
    mkdir -p "${export_dir}/bin" "${export_dir}/lib"
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
      binary_path="$(find_postgres_binary "${command_name}")"
      cp -L "${binary_path}" "${export_dir}/bin/${command_name}"
      chmod 755 "${export_dir}/bin/${command_name}"
      copy_deps "${binary_path}"
    done
    if [ -d /usr/local/share/postgresql ]; then
      cp -a /usr/local/share/postgresql "${export_dir}/share"
    elif [ -d /usr/share/postgresql ]; then
      cp -a /usr/share/postgresql "${export_dir}/share"
    fi
    if [ -d /usr/local/lib/postgresql ]; then
      mkdir -p "${export_dir}/lib/postgresql"
      cp -a /usr/local/lib/postgresql/. "${export_dir}/lib/postgresql/"
    elif [ -d /usr/lib/postgresql ]; then
      mkdir -p "${export_dir}/lib/postgresql"
      cp -a /usr/lib/postgresql/. "${export_dir}/lib/postgresql/"
    fi
    tar -C "${export_dir}" -cf - .
  ' | tar -C "${output_dir}" -xf -
}

copy_static_assets() {
  local bundle_root="$1"
  mkdir -p "${bundle_root}/templates/systemd" "${bundle_root}/templates/common" "${bundle_root}/docs"
  cp "${ROOT_DIR}/packaging/native/install.sh" "${bundle_root}/install.sh"
  chmod +x "${bundle_root}/install.sh"
  cp -R "${ROOT_DIR}/packaging/native/templates/systemd/." "${bundle_root}/templates/systemd/"
  cp -R "${ROOT_DIR}/packaging/native/templates/common/." "${bundle_root}/templates/common/"
  if [ -f "${ROOT_DIR}/docs/18-native-static-deployment.md" ]; then
    cp "${ROOT_DIR}/docs/18-native-static-deployment.md" "${bundle_root}/docs/"
  fi
}

copy_business_artifacts() {
  local bundle_root="$1"
  local binaries_dir="$2"

  mkdir -p "${bundle_root}/binaries" "${bundle_root}/ui/media-core"
  cp "${binaries_dir}/media-core" "${bundle_root}/binaries/media-core-linux-amd64"
  cp "${binaries_dir}/media-agent" "${bundle_root}/binaries/media-agent-linux-amd64"
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

  cat >"${bundle_root}/package-manifest.env" <<EOF
BUNDLE_VERSION=${bundle_version}
BUNDLE_VARIANT=${NATIVE_VARIANT}
BUNDLE_GPU_SUPPORT=${gpu_runtime}
BUNDLE_WORKER_SUPPORT=${worker_runtime}
BUNDLE_POSTGRES_RUNTIME=${postgres_runtime}
DEPLOY_MODE=native
MEDIA_CORE_BINARY_PATH=binaries/media-core-linux-amd64
MEDIA_AGENT_BINARY_PATH=binaries/media-agent-linux-amd64
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
bundle_variant=${NATIVE_VARIANT}
target_runtime=docker-free
verification_required_host=172.17.13.196
EOF
}

write_checksums() {
  local bundle_root="$1"
  (
    cd "${bundle_root}"
    if command -v shasum >/dev/null 2>&1; then
      find . -type f ! -name SHA256SUMS -print | LC_ALL=C sort | while read -r file; do
        shasum -a 256 "${file#./}"
      done >SHA256SUMS
    else
      find . -type f ! -name SHA256SUMS -print | LC_ALL=C sort | while read -r file; do
        sha256sum "${file#./}"
      done >SHA256SUMS
    fi
  )
}

assert_no_docker_runtime_assets() {
  local bundle_root="$1"
  if find "${bundle_root}" \( -path '*/images/*' -o -name compose.yml -o -name docker-compose.yml -o -name streamserver-compose \) | grep -q .; then
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
  local include_worker="true"
  local include_gpu="false"
  local include_postgres="true"

  parse_args "$@"
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
  bundle_root="${stage_dir}/${bundle_name}"
  binaries_dir="${stage_dir}/binaries"
  mkdir -p "${bundle_root}"

  export_business_binaries "${binaries_dir}"
  copy_business_artifacts "${bundle_root}" "${binaries_dir}"
  copy_static_assets "${bundle_root}"

  if [ "${include_worker}" = "true" ]; then
    pull_linux_amd64_image "${MEDIA_AGENT_RUNTIME_BASE_IMAGE}"
    extract_commands_from_image "${MEDIA_AGENT_RUNTIME_BASE_IMAGE}" "${bundle_root}/runtime/ffmpeg/cpu" ffmpeg ffprobe
    pull_linux_amd64_image "${ZLM_SOURCE_IMAGE}"
    extract_zlm_runtime "${ZLM_SOURCE_IMAGE}" "${bundle_root}/runtime/zlm"
    extract_python_stdlib_runtime "${ZLM_PYTHON_STDLIB_IMAGE}" "${bundle_root}/runtime/zlm/python" "$(zlm_python_version "${bundle_root}/runtime/zlm")"
  fi

  if [ "${include_gpu}" = "true" ]; then
    pull_linux_amd64_image "${MEDIA_AGENT_GPU_RUNTIME_BASE_IMAGE}"
    extract_commands_from_image "${MEDIA_AGENT_GPU_RUNTIME_BASE_IMAGE}" "${bundle_root}/runtime/ffmpeg/gpu" ffmpeg ffprobe
  fi

  if [ "${include_postgres}" = "true" ]; then
    pull_linux_amd64_image "${POSTGRES_SOURCE_IMAGE}"
    extract_postgres_runtime "${POSTGRES_SOURCE_IMAGE}" "${bundle_root}/runtime/postgres"
  fi

  write_manifest "${bundle_root}" "${bundle_version}" "${include_postgres}" "${include_worker}" "${include_gpu}"
  write_build_info "${bundle_root}" "${bundle_name}" "${version}"
  write_checksums "${bundle_root}"
  assert_no_docker_runtime_assets "${bundle_root}"
  create_archive "${stage_dir}" "${bundle_name}" "${archive_path}"

  log "native 离线包已生成: ${archive_path}"
  log "校验文件已生成: ${archive_path}.sha256"
  log "下一步必须在 196 上执行: ./scripts/verify-native-bundle-on-196.sh --bundle ${archive_path}"
}

main "$@"
