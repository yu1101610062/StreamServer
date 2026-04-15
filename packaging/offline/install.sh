#!/usr/bin/env bash
set -euo pipefail

PACKAGE_ROOT="$(cd "$(dirname "$0")" && pwd)"
MANIFEST_FILE="${PACKAGE_ROOT}/package-manifest.env"
DEPLOY_DOC_SOURCE="${PACKAGE_ROOT}/docs/17-离线部署打包与安装.md"
CERT_SOURCE_DIR="${PACKAGE_ROOT}/certs"

if [ ! -f "${MANIFEST_FILE}" ]; then
  echo "缺少 ${MANIFEST_FILE}" >&2
  exit 1
fi

# shellcheck disable=SC1090
. "${MANIFEST_FILE}"

BUNDLE_VARIANT="${BUNDLE_VARIANT:-gpu-enabled}"
BUNDLE_GPU_SUPPORT="${BUNDLE_GPU_SUPPORT:-true}"
MEDIA_AGENT_GPU_IMAGE="${MEDIA_AGENT_GPU_IMAGE:-}"
MEDIA_AGENT_GPU_IMAGE_ARCHIVE="${MEDIA_AGENT_GPU_IMAGE_ARCHIVE:-}"

COMPOSE_CMD=()
COMPOSE_CMD_DISPLAY=""
COMPOSE_FILE_NAME="compose.yml"

log() {
  printf '[streamserver-install] %s\n' "$*"
}

fail() {
  printf '[streamserver-install] ERROR: %s\n' "$*" >&2
  exit 1
}

require_cmd() {
  command -v "$1" >/dev/null 2>&1 || fail "缺少命令: $1"
}

ensure_linux_amd64() {
  [ "$(uname -s)" = "Linux" ] || fail "安装脚本只能在 Linux 上运行"
  case "$(uname -m)" in
    x86_64|amd64) ;;
    *) fail "目标主机必须是 Linux AMD64，当前架构为 $(uname -m)" ;;
  esac
}

detect_compose_cmd() {
  if docker compose version >/dev/null 2>&1; then
    COMPOSE_CMD=(docker compose)
    COMPOSE_CMD_DISPLAY="docker compose"
    return 0
  fi
  if command -v docker-compose >/dev/null 2>&1 && docker-compose version >/dev/null 2>&1; then
    COMPOSE_CMD=(docker-compose)
    COMPOSE_CMD_DISPLAY="docker-compose"
    return 0
  fi
  fail "缺少 Compose 命令，请安装 docker compose 插件或 docker-compose"
}

compose_cmd() {
  [ "${#COMPOSE_CMD[@]}" -gt 0 ] || fail "Compose 命令尚未初始化"
  "${COMPOSE_CMD[@]}" "$@"
}

compose_with_file() {
  compose_cmd -f "${COMPOSE_FILE_NAME}" "$@"
}

ensure_docker_ready() {
  require_cmd docker
  docker info >/dev/null 2>&1 || fail "Docker 不可用，请先启动 Docker Engine"
  detect_compose_cmd
}

prompt() {
  local message="$1"
  local default_value="${2:-}"
  local answer
  if [ -n "${default_value}" ]; then
    printf '%s [%s]: ' "${message}" "${default_value}" >&2
  else
    printf '%s: ' "${message}" >&2
  fi
  read -r answer
  if [ -z "${answer}" ]; then
    answer="${default_value}"
  fi
  printf '%s' "${answer}"
}

prompt_non_empty() {
  local message="$1"
  local default_value="${2:-}"
  local answer
  while true; do
    answer="$(prompt "${message}" "${default_value}")"
    if [ -n "${answer}" ]; then
      printf '%s' "${answer}"
      return 0
    fi
    echo "输入不能为空。" >&2
  done
}

prompt_yes_no() {
  local message="$1"
  local default_value="${2:-Y}"
  local answer
  while true; do
    answer="$(prompt "${message}" "${default_value}")"
    case "${answer}" in
      Y|y|yes|YES) return 0 ;;
      N|n|no|NO) return 1 ;;
      *) echo "请输入 Y 或 N。" >&2 ;;
    esac
  done
}

prompt_secret() {
  local message="$1"
  local answer
  printf '%s: ' "${message}" >&2
  read -r -s answer
  printf '\n' >&2
  printf '%s' "${answer}"
}

prompt_password_with_confirmation() {
  local message="$1"
  local password
  local confirm
  while true; do
    password="$(prompt_secret "${message}")"
    [ -n "${password}" ] || {
      echo "输入不能为空。" >&2
      continue
    }
    [ "${#password}" -ge 8 ] || {
      echo "密码至少需要 8 个字符。" >&2
      continue
    }
    confirm="$(prompt_secret "再次输入以确认")"
    if [ "${password}" = "${confirm}" ]; then
      printf '%s' "${password}"
      return 0
    fi
    echo "两次输入的密码不一致，请重试。" >&2
  done
}

normalize_csv_labels() {
  local raw="${1:-}"
  local part
  local trimmed
  local existing
  local joined=""
  local duplicate=0
  local normalized_parts=()

  IFS=',' read -r -a parts <<< "${raw}"
  for part in "${parts[@]}"; do
    trimmed="$(printf '%s' "${part}" | sed 's/^[[:space:]]*//;s/[[:space:]]*$//')"
    [ -n "${trimmed}" ] || continue

    duplicate=0
    for existing in "${normalized_parts[@]}"; do
      if [ "${existing}" = "${trimmed}" ]; then
        duplicate=1
        break
      fi
    done
    [ "${duplicate}" -eq 1 ] && continue
    normalized_parts+=("${trimmed}")
  done

  for part in "${normalized_parts[@]}"; do
    if [ -n "${joined}" ]; then
      joined="${joined},${part}"
    else
      joined="${part}"
    fi
  done

  printf '%s' "${joined}"
}

collect_agent_labels() {
  local default_label="$1"
  local extra_labels

  printf '当前节点默认会写入算力标签: %s\n' "${default_label}" >&2
  extra_labels="$(prompt "额外节点标签（逗号分隔，可留空）" "")"
  printf '%s' "$(normalize_csv_labels "${default_label},${extra_labels}")"
}

generate_uuid() {
  if command -v uuidgen >/dev/null 2>&1; then
    uuidgen | tr '[:upper:]' '[:lower:]'
    return 0
  fi
  if [ -r /proc/sys/kernel/random/uuid ]; then
    cat /proc/sys/kernel/random/uuid
    return 0
  fi
  fail "无法生成 UUID，请安装 uuidgen"
}

generate_secret() {
  od -An -N16 -tx1 /dev/urandom | tr -d ' \n'
}

detect_primary_ip() {
  local ip
  if command -v ip >/dev/null 2>&1; then
    ip="$(ip route get 1.1.1.1 2>/dev/null | awk '/src/ { for (i = 1; i <= NF; i++) if ($i == "src") { print $(i + 1); exit } }')"
    if [ -n "${ip}" ]; then
      printf '%s' "${ip}"
      return 0
    fi
  fi
  if command -v hostname >/dev/null 2>&1; then
    ip="$(hostname -I 2>/dev/null | awk '{ print $1 }')"
    if [ -n "${ip}" ]; then
      printf '%s' "${ip}"
      return 0
    fi
  fi
  printf '%s' "127.0.0.1"
}

discover_ipv4_interfaces() {
  require_cmd ip
  ip -o -4 addr show up scope global 2>/dev/null \
    | awk '!seen[$2]++ { split($4, cidr, "/"); print $2 "|" cidr[1] }'
}

detect_primary_interface_entry() {
  if ! command -v ip >/dev/null 2>&1; then
    return 0
  fi
  ip route get 1.1.1.1 2>/dev/null \
    | awk '{
        for (i = 1; i <= NF; i++) {
          if ($i == "dev") dev = $(i + 1)
          if ($i == "src") src = $(i + 1)
        }
      }
      END {
        if (dev != "" && src != "") {
          print dev "|" src
        }
      }'
}

print_interface_options() {
  local entry
  local index=1
  for entry in "$@"; do
    printf '  %d) %s (%s)\n' \
      "${index}" \
      "${entry%%|*}" \
      "${entry#*|}" >&2
    index=$((index + 1))
  done
}

resolve_interface_choice() {
  local choice="$1"
  shift
  local entries=("$@")
  local entry
  local index=1

  if [[ "${choice}" =~ ^[0-9]+$ ]]; then
    for entry in "${entries[@]}"; do
      if [ "${index}" -eq "${choice}" ]; then
        printf '%s' "${entry}"
        return 0
      fi
      index=$((index + 1))
    done
    return 1
  fi

  for entry in "${entries[@]}"; do
    if [ "${entry%%|*}" = "${choice}" ]; then
      printf '%s' "${entry}"
      return 0
    fi
  done
  return 1
}

prompt_interface_selection() {
  local label="$1"
  local default_name="$2"
  local default_ip="$3"
  shift 3
  local entries=("$@")
  local answer
  local selected

  while true; do
    printf '%s 可用网卡（输入编号或网卡名）:\n' "${label}" >&2
    print_interface_options "${entries[@]}"
    answer="$(prompt "${label}" "${default_name}")"
    if selected="$(resolve_interface_choice "${answer}" "${entries[@]}")"; then
      printf '%s' "${selected}"
      return 0
    fi
    printf '无效选择，请输入上面的编号或网卡名。默认推荐 %s (%s)\n' "${default_name}" "${default_ip}" >&2
  done
}

configure_host_interface_defaults() {
  local primary_entry
  local multicast_entry
  local default_entry
  local default_name
  local default_ip
  local entry
  local found_default=0
  local entries=()

  mapfile -t entries < <(discover_ipv4_interfaces)
  [ "${#entries[@]}" -gt 0 ] || fail "未检测到可用的 IPv4 网卡，无法为 host 工作节点生成绑定配置"

  default_entry="$(detect_primary_interface_entry)"
  if [ -z "${default_entry}" ]; then
    default_entry="${entries[0]}"
  else
    default_name="${default_entry%%|*}"
    for entry in "${entries[@]}"; do
      if [ "${entry%%|*}" = "${default_name}" ]; then
        default_entry="${entry}"
        found_default=1
        break
      fi
    done
    if [ "${found_default}" -ne 1 ]; then
      default_entry="${entries[0]}"
    fi
  fi
  default_name="${default_entry%%|*}"
  default_ip="${default_entry#*|}"

  echo "host 工作节点需要分别选择主网卡和组播网卡。" >&2
  echo "建议：普通流量走主网卡；真实组播收发优先使用独立组播网卡，没有时可与主网卡相同。" >&2

  primary_entry="$(prompt_interface_selection "主网卡" "${default_name}" "${default_ip}" "${entries[@]}")"
  PRIMARY_INTERFACE_NAME="${primary_entry%%|*}"
  PRIMARY_INTERFACE_IP="${primary_entry#*|}"

  multicast_entry="$(prompt_interface_selection "组播网卡" "${PRIMARY_INTERFACE_NAME}" "${PRIMARY_INTERFACE_IP}" "${entries[@]}")"
  MULTICAST_INTERFACE_NAME="${multicast_entry%%|*}"
  MULTICAST_INTERFACE_IP="${multicast_entry#*|}"
}

escape_sed_replacement() {
  printf '%s' "$1" | sed 's/[\\/&|]/\\&/g'
}

archive_for_image_key() {
  case "$1" in
    postgres) printf '%s' "${POSTGRES_IMAGE_ARCHIVE}" ;;
    media-core) printf '%s' "${MEDIA_CORE_IMAGE_ARCHIVE}" ;;
    media-agent) printf '%s' "${MEDIA_AGENT_IMAGE_ARCHIVE}" ;;
    media-agent-gpu) printf '%s' "${MEDIA_AGENT_GPU_IMAGE_ARCHIVE}" ;;
    zlmediakit) printf '%s' "${ZLM_IMAGE_ARCHIVE}" ;;
    *) fail "未知镜像标识: $1" ;;
  esac
}

binary_rel_for_key() {
  case "$1" in
    media-core) printf '%s' "${MEDIA_CORE_BINARY_PATH:-}" ;;
    media-agent) printf '%s' "${MEDIA_AGENT_BINARY_PATH:-}" ;;
    *) fail "未知二进制标识: $1" ;;
  esac
}

ui_rel_for_key() {
  case "$1" in
    media-core) printf '%s' "${MEDIA_CORE_UI_PATH:-}" ;;
    *) fail "未知前端静态资源标识: $1" ;;
  esac
}

load_image_archive() {
  local archive_rel="$1"
  local archive_path="${PACKAGE_ROOT}/${archive_rel}"
  [ -f "${archive_path}" ] || fail "缺少镜像归档 ${archive_rel}"
  log "加载镜像 ${archive_rel}"
  docker load -i "${archive_path}" >/dev/null
}

ensure_images_loaded() {
  local key
  for key in "$@"; do
    load_image_archive "$(archive_for_image_key "${key}")"
  done
}

install_host_binary() {
  local install_dir="$1"
  local binary_key="$2"
  local binary_rel
  local source_path
  local target_path

  binary_rel="$(binary_rel_for_key "${binary_key}")"
  [ -n "${binary_rel}" ] || fail "离线包未声明 ${binary_key} 二进制路径"

  source_path="${PACKAGE_ROOT}/${binary_rel}"
  [ -f "${source_path}" ] || fail "缺少宿主机挂载二进制 ${binary_rel}"

  mkdir -p "${install_dir}/bin"
  target_path="${install_dir}/bin/${binary_key}"
  cp "${source_path}" "${target_path}"
  chmod 755 "${target_path}"
  log "已写入宿主机挂载二进制: ${target_path}"
}

install_host_binaries() {
  local install_dir="$1"
  shift
  local binary_key

  for binary_key in "$@"; do
    install_host_binary "${install_dir}" "${binary_key}"
  done
}

install_host_ui() {
  local install_dir="$1"
  local ui_key="$2"
  local ui_rel
  local source_path
  local target_path

  ui_rel="$(ui_rel_for_key "${ui_key}")"
  [ -n "${ui_rel}" ] || fail "离线包未声明 ${ui_key} 前端静态资源路径"

  source_path="${PACKAGE_ROOT}/${ui_rel}"
  [ -d "${source_path}" ] || fail "缺少宿主机挂载前端静态资源 ${ui_rel}"

  target_path="${install_dir}/ui"
  rm -rf "${target_path}"
  mkdir -p "${target_path}"
  cp -R "${source_path}/." "${target_path}/"
  log "已写入宿主机挂载前端静态资源: ${target_path}"
}

ensure_nvidia_runtime_ready() {
  require_cmd nvidia-smi
  nvidia-smi >/dev/null 2>&1 || fail "NVIDIA 驱动不可用，请先确认宿主机可以正常执行 nvidia-smi"
  docker info --format '{{json .Runtimes}}' 2>/dev/null | grep -q '"nvidia"' \
    || fail "Docker 未检测到 nvidia runtime，请先安装并配置 nvidia-container-toolkit"
}

prepare_install_dir() {
  local install_dir="$1"
  if [ -e "${install_dir}" ] && [ -n "$(find "${install_dir}" -mindepth 1 -maxdepth 1 2>/dev/null | head -n 1)" ]; then
    prompt_yes_no "目录 ${install_dir} 已存在且非空，是否继续覆盖模板文件？" "N" || fail "用户取消安装"
  fi
  mkdir -p "${install_dir}"
  mkdir -p "${install_dir}/docs"
}

configure_auth_defaults() {
  AUTH_MODE="disabled"
  AUTH_ENABLED="false"
  JWT_PUBLIC_KEY=""
  AUTH_JWT_PRIVATE_KEY_PATH=""
  AUTH_JWT_PUBLIC_KEY_PATH=""
  AUTH_ACCESS_TOKEN_TTL="15m"
  AUTH_REFRESH_TOKEN_TTL="7d"
  AUTH_BOOTSTRAP_ADMIN_USERNAME=""
  AUTH_BOOTSTRAP_ADMIN_PASSWORD=""
}

prompt_local_auth_configuration() {
  configure_auth_defaults
  if ! prompt_yes_no "是否启用 media-core 内建用户名密码鉴权？" "N"; then
    return 0
  fi

  AUTH_MODE="local_password"
  AUTH_ENABLED="true"
  AUTH_BOOTSTRAP_ADMIN_USERNAME="$(prompt_non_empty "管理员用户名" "admin")"
  AUTH_BOOTSTRAP_ADMIN_PASSWORD="$(prompt_password_with_confirmation "管理员密码")"
}

prepare_local_auth_assets() {
  local install_dir="$1"
  local auth_dir
  local private_key_host_path
  local public_key_host_path

  [ "${AUTH_MODE}" = "local_password" ] || return 0

  require_cmd openssl
  auth_dir="${install_dir}/certs/auth"
  private_key_host_path="${auth_dir}/jwt-ed25519-private.pem"
  public_key_host_path="${auth_dir}/jwt-ed25519-public.pem"

  mkdir -p "${auth_dir}"
  openssl genpkey -algorithm Ed25519 -out "${private_key_host_path}" >/dev/null 2>&1
  openssl pkey -in "${private_key_host_path}" -pubout -out "${public_key_host_path}" >/dev/null 2>&1
  chmod 600 "${private_key_host_path}"
  chmod 644 "${public_key_host_path}"

  AUTH_JWT_PRIVATE_KEY_PATH="/certs/auth/$(basename "${private_key_host_path}")"
  AUTH_JWT_PUBLIC_KEY_PATH="/certs/auth/$(basename "${public_key_host_path}")"
  JWT_PUBLIC_KEY=""
}

wait_for_compose_service_ready() {
  local install_dir="$1"
  local service_name="$2"
  local timeout_seconds="${3:-90}"
  local container_id=""
  local state=""
  local waited=0

  while [ "${waited}" -lt "${timeout_seconds}" ]; do
    container_id="$(
      cd "${install_dir}" &&
      compose_with_file ps -q "${service_name}" 2>/dev/null | head -n 1
    )"
    if [ -n "${container_id}" ]; then
      state="$(
        docker inspect --format '{{if .State.Health}}{{.State.Health.Status}}{{else}}{{.State.Status}}{{end}}' "${container_id}" 2>/dev/null || true
      )"
      case "${state}" in
        healthy|running)
          return 0
          ;;
        unhealthy|exited|dead)
          fail "服务 ${service_name} 启动失败，当前状态为 ${state}"
          ;;
      esac
    fi
    sleep 2
    waited=$((waited + 2))
  done

  fail "等待服务 ${service_name} 就绪超时"
}

bootstrap_local_admin_if_needed() {
  local install_dir="$1"
  local output
  local postgres_container_id=""
  local postgres_state=""
  local postgres_was_running=0

  [ "${AUTH_MODE}" = "local_password" ] || return 0

  postgres_container_id="$(
    cd "${install_dir}" &&
    compose_with_file ps -q postgres 2>/dev/null | head -n 1
  )"
  if [ -n "${postgres_container_id}" ]; then
    postgres_state="$(
      docker inspect --format '{{.State.Status}}' "${postgres_container_id}" 2>/dev/null || true
    )"
    if [ "${postgres_state}" = "running" ]; then
      postgres_was_running=1
    fi
  fi

  log "已启用本地用户名密码鉴权，准备初始化管理员账号 ${AUTH_BOOTSTRAP_ADMIN_USERNAME}"
  (
    cd "${install_dir}"
    compose_with_file up -d postgres >/dev/null
  )
  wait_for_compose_service_ready "${install_dir}" "postgres" 90

  if ! output="$(
    cd "${install_dir}" &&
    printf '%s' "${AUTH_BOOTSTRAP_ADMIN_PASSWORD}" | \
      compose_with_file run --rm --no-deps -T media-core \
        media-core auth bootstrap-admin --username "${AUTH_BOOTSTRAP_ADMIN_USERNAME}" --password-stdin 2>&1
  )"; then
    (
      cd "${install_dir}"
      if [ "${postgres_was_running}" -ne 1 ]; then
        compose_with_file stop postgres >/dev/null 2>&1 || true
      fi
    )
    if printf '%s' "${output}" | grep -q "an enabled admin user already exists"; then
      log "检测到数据库中已存在启用中的管理员账号，跳过 bootstrap-admin。"
      return 0
    fi
    printf '%s\n' "${output}" >&2
    fail "初始化管理员账号失败"
  fi

  log "已完成管理员账号初始化。"
  (
    cd "${install_dir}"
    if [ "${postgres_was_running}" -ne 1 ]; then
      compose_with_file stop postgres >/dev/null 2>&1 || true
    fi
  )
}

write_control_plane_env() {
  local env_file="$1"
  cat >"${env_file}" <<EOF
COMPOSE_PROJECT_NAME=${PROJECT_NAME}
POSTGRES_IMAGE=${POSTGRES_IMAGE}
MEDIA_CORE_IMAGE=${MEDIA_CORE_IMAGE}
POSTGRES_DB=${POSTGRES_DB}
POSTGRES_USER=${POSTGRES_USER}
POSTGRES_PASSWORD=${POSTGRES_PASSWORD}
CORE_HTTP_PORT=${CORE_HTTP_PORT}
CORE_GRPC_PORT=${CORE_GRPC_PORT}
STACK_SUBNET=${STACK_SUBNET}
POSTGRES_IP=${POSTGRES_IP}
HOOK_SHARED_SECRET=${HOOK_SHARED_SECRET}
HOOK_SOURCE_ALLOWLIST=${HOOK_SOURCE_ALLOWLIST}
STORAGE_ALLOWLIST=${STORAGE_ALLOWLIST}
AUTH_MODE=${AUTH_MODE}
AUTH_ENABLED=${AUTH_ENABLED}
JWT_PUBLIC_KEY=${JWT_PUBLIC_KEY}
AUTH_JWT_PRIVATE_KEY_PATH=${AUTH_JWT_PRIVATE_KEY_PATH}
AUTH_JWT_PUBLIC_KEY_PATH=${AUTH_JWT_PUBLIC_KEY_PATH}
AUTH_ACCESS_TOKEN_TTL=${AUTH_ACCESS_TOKEN_TTL}
AUTH_REFRESH_TOKEN_TTL=${AUTH_REFRESH_TOKEN_TTL}

# HTTPS 默认关闭。
# 当前应用不内置 HTTPS listener，如需 HTTPS，请在反向代理中终止 TLS 后转发到 media-core:8080。
#
# gRPC mTLS 默认关闭。如需开启，可在确认或替换证书后取消以下注释：
# CORE_GRPC_TLS_CERT_PATH=/certs/self-signed/media-core.pem
# CORE_GRPC_TLS_KEY_PATH=/certs/self-signed/media-core.key
# CORE_GRPC_TLS_CLIENT_CA_PATH=/certs/self-signed/ca.pem
EOF
}

write_worker_host_env() {
  local env_file="$1"
  local media_agent_image="$2"
  local acceleration_mode="$3"
  local agent_labels="$4"
  cat >"${env_file}" <<EOF
COMPOSE_PROJECT_NAME=${PROJECT_NAME}
MEDIA_AGENT_IMAGE=${media_agent_image}
ZLM_IMAGE=${ZLM_IMAGE}
NODE_ID=${NODE_ID}
AGENT_NODE_NAME=${AGENT_NODE_NAME}
CORE_HTTP_HOST=${CORE_HTTP_HOST}
CORE_HTTP_PORT=${CORE_HTTP_PORT}
CORE_GRPC_HOST=${CORE_GRPC_HOST}
CORE_GRPC_PORT=${CORE_GRPC_PORT}
PUBLIC_HOST=${PUBLIC_HOST}
ZLM_API_HOST=${ZLM_API_HOST}
AGENT_HTTP_PORT=${AGENT_HTTP_PORT}
ZLM_HTTP_PORT=${ZLM_HTTP_PORT}
ZLM_RTMP_PORT=${ZLM_RTMP_PORT}
ZLM_RTSP_PORT=${ZLM_RTSP_PORT}
AGENT_PRIMARY_INTERFACE_NAME=${PRIMARY_INTERFACE_NAME}
AGENT_PRIMARY_INTERFACE_IP=${PRIMARY_INTERFACE_IP}
AGENT_MULTICAST_INTERFACE_NAME=${MULTICAST_INTERFACE_NAME}
AGENT_MULTICAST_INTERFACE_IP=${MULTICAST_INTERFACE_IP}
HOOK_SHARED_SECRET=${HOOK_SHARED_SECRET}
AGENT_NETWORK_MODE=host
AGENT_ACCELERATION_MODE=${acceleration_mode}
AGENT_LABELS=${agent_labels}
AGENT_MAX_RUNTIME_SLOTS=0
WORK_ROOT=/data/media/work

# mTLS 默认关闭。如需开启，请将 AGENT_CORE_ENDPOINT 改为 https，并取消以下注释：
# AGENT_CORE_ENDPOINT=https://${CORE_GRPC_HOST}:${CORE_GRPC_PORT}
# AGENT_CERT_PATH=/certs/self-signed/media-agent.pem
# AGENT_KEY_PATH=/certs/self-signed/media-agent.key
# AGENT_CA_PATH=/certs/self-signed/ca.pem
# AGENT_TLS_DOMAIN_NAME=streamserver-core.local
EOF
}

write_all_in_one_host_env() {
  local env_file="$1"
  local media_agent_image="$2"
  local acceleration_mode="$3"
  local agent_labels="$4"
  cat >"${env_file}" <<EOF
COMPOSE_PROJECT_NAME=${PROJECT_NAME}
POSTGRES_IMAGE=${POSTGRES_IMAGE}
MEDIA_CORE_IMAGE=${MEDIA_CORE_IMAGE}
MEDIA_AGENT_IMAGE=${media_agent_image}
ZLM_IMAGE=${ZLM_IMAGE}
POSTGRES_DB=${POSTGRES_DB}
POSTGRES_USER=${POSTGRES_USER}
POSTGRES_PASSWORD=${POSTGRES_PASSWORD}
CORE_HTTP_PORT=${CORE_HTTP_PORT}
CORE_GRPC_PORT=${CORE_GRPC_PORT}
AGENT_HTTP_PORT=${AGENT_HTTP_PORT}
ZLM_HTTP_PORT=${ZLM_HTTP_PORT}
ZLM_RTMP_PORT=${ZLM_RTMP_PORT}
ZLM_RTSP_PORT=${ZLM_RTSP_PORT}
ZLM_API_HOST=${ZLM_API_HOST}
HOOK_SHARED_SECRET=${HOOK_SHARED_SECRET}
HOOK_SOURCE_ALLOWLIST=${HOOK_SOURCE_ALLOWLIST}
PUBLIC_HOST=${PUBLIC_HOST}
NODE_ID=${NODE_ID}
AGENT_NODE_NAME=${AGENT_NODE_NAME}
AGENT_PRIMARY_INTERFACE_NAME=${PRIMARY_INTERFACE_NAME}
AGENT_PRIMARY_INTERFACE_IP=${PRIMARY_INTERFACE_IP}
AGENT_MULTICAST_INTERFACE_NAME=${MULTICAST_INTERFACE_NAME}
AGENT_MULTICAST_INTERFACE_IP=${MULTICAST_INTERFACE_IP}
AGENT_ACCELERATION_MODE=${acceleration_mode}
AGENT_LABELS=${agent_labels}
AGENT_MAX_RUNTIME_SLOTS=0
STORAGE_ALLOWLIST=/data/media/work,/data/zlm/www
AUTH_MODE=${AUTH_MODE}
AUTH_ENABLED=${AUTH_ENABLED}
JWT_PUBLIC_KEY=${JWT_PUBLIC_KEY}
AUTH_JWT_PRIVATE_KEY_PATH=${AUTH_JWT_PRIVATE_KEY_PATH}
AUTH_JWT_PUBLIC_KEY_PATH=${AUTH_JWT_PUBLIC_KEY_PATH}
AUTH_ACCESS_TOKEN_TTL=${AUTH_ACCESS_TOKEN_TTL}
AUTH_REFRESH_TOKEN_TTL=${AUTH_REFRESH_TOKEN_TTL}
STACK_SUBNET=${STACK_SUBNET}
CORE_IP=${CORE_IP}
POSTGRES_IP=${POSTGRES_IP}

# HTTPS 默认关闭。
# 当前应用不内置 HTTPS listener，如需 HTTPS，请在反向代理中终止 TLS 后转发到 media-core:8080。
#
# gRPC mTLS 默认关闭。启用时请同时为 media-core 和 media-agent 设置以下变量：
# CORE_GRPC_TLS_CERT_PATH=/certs/self-signed/media-core.pem
# CORE_GRPC_TLS_KEY_PATH=/certs/self-signed/media-core.key
# CORE_GRPC_TLS_CLIENT_CA_PATH=/certs/self-signed/ca.pem
# AGENT_CORE_ENDPOINT=https://127.0.0.1:${CORE_GRPC_PORT}
# AGENT_CERT_PATH=/certs/self-signed/media-agent.pem
# AGENT_KEY_PATH=/certs/self-signed/media-agent.key
# AGENT_CA_PATH=/certs/self-signed/ca.pem
# AGENT_TLS_DOMAIN_NAME=streamserver-core.local
EOF
}

render_zlm_config() {
  local install_dir="$1"
  local hook_base="$2"
  local allow_ip_range="$3"
  local template_file="${PACKAGE_ROOT}/templates/common/zlm.config.ini.template"
  local output_file="${install_dir}/zlm/config.ini"
  local escaped_hook_base
  local escaped_allow_ip_range
  local escaped_secret
  local escaped_node_id
  local escaped_http_port
  local escaped_rtmp_port
  local escaped_rtsp_port

  [ -f "${template_file}" ] || fail "缺少 ZLM 模板 ${template_file}"
  mkdir -p "${install_dir}/zlm"

  escaped_hook_base="$(escape_sed_replacement "${hook_base}")"
  escaped_allow_ip_range="$(escape_sed_replacement "${allow_ip_range}")"
  escaped_secret="$(escape_sed_replacement "${HOOK_SHARED_SECRET}")"
  escaped_node_id="$(escape_sed_replacement "${NODE_ID}")"
  escaped_http_port="$(escape_sed_replacement "${ZLM_HTTP_PORT}")"
  escaped_rtmp_port="$(escape_sed_replacement "${ZLM_RTMP_PORT}")"
  escaped_rtsp_port="$(escape_sed_replacement "${ZLM_RTSP_PORT}")"

  sed \
    -e "s|__ZLM_API_SECRET__|${escaped_secret}|g" \
    -e "s|__HOOK_SHARED_SECRET__|${escaped_secret}|g" \
    -e "s|__ZLM_SERVER_ID__|${escaped_node_id}|g" \
    -e "s|__HOOK_BASE__|${escaped_hook_base}|g" \
    -e "s|__ZLM_API_ALLOW_IP_RANGE__|${escaped_allow_ip_range}|g" \
    -e "s|__ZLM_HTTP_PORT__|${escaped_http_port}|g" \
    -e "s|__ZLM_RTMP_PORT__|${escaped_rtmp_port}|g" \
    -e "s|__ZLM_RTSP_PORT__|${escaped_rtsp_port}|g" \
    "${template_file}" >"${output_file}"
}

copy_common_assets() {
  local install_dir="$1"
  cp "${DEPLOY_DOC_SOURCE}" "${install_dir}/docs/"
  [ -d "${CERT_SOURCE_DIR}" ] || fail "缺少证书目录 ${CERT_SOURCE_DIR}"
  mkdir -p "${install_dir}/certs"
  cp -R "${CERT_SOURCE_DIR}/." "${install_dir}/certs/"
}

copy_compose_template() {
  local template_name="$1"
  local install_dir="$2"
  cp "${PACKAGE_ROOT}/templates/${template_name}/compose.yml" "${install_dir}/${COMPOSE_FILE_NAME}"
  ln -sfn "${COMPOSE_FILE_NAME}" "${install_dir}/docker-compose.yml"
}

prepare_control_plane_layout() {
  local install_dir="$1"
  # PostgreSQL 18 re-executes the entrypoint as the postgres user after
  # creating PGDATA=/var/lib/postgresql/18/docker. If the version directory is
  # auto-created with a restrictive umask (for example 750), the second pass
  # cannot traverse it and startup fails with "mkdir ... /var/lib/postgresql/18:
  # Permission denied". Pre-create the versioned layout on the host so the
  # parent directory remains traversable.
  mkdir -p "${install_dir}/data/postgres/18/docker"
  chmod 755 \
    "${install_dir}/data/postgres" \
    "${install_dir}/data/postgres/18" \
    "${install_dir}/data/postgres/18/docker" 2>/dev/null || true
}

prepare_worker_layout() {
  local install_dir="$1"
  mkdir -p \
    "${install_dir}/data/media/work" \
    "${install_dir}/data/media/logs" \
    "${install_dir}/data/zlm/www"
}

emit_manual_start_hint() {
  local install_dir="$1"
  log "已写入部署文件，稍后可执行:"
  log "  cd ${install_dir} && ${COMPOSE_CMD_DISPLAY} -f ${COMPOSE_FILE_NAME} up -d"
  log "后续如仅更新 media-core/media-agent，可直接替换 ${install_dir}/bin 下对应二进制后再重新拉起相关服务。"
}

show_tls_notice() {
  local install_dir="$1"
  local grpc_host_hint="${2:-<control-plane-host>}"
  local grpc_port_hint="${3:-50051}"

  mkdir -p "${install_dir}/certs/custom"
  log "已放置自签名证书到 ${install_dir}/certs/self-signed"
  log "HTTPS 和 mTLS 默认保持关闭。"
  log "如果你已有正式证书，建议在启动前替换 ${install_dir}/certs/self-signed 下的测试证书，或改用 ${install_dir}/certs/custom"

  if prompt_yes_no "是否已有正式证书，需要在启动前先替换自签名证书？" "N"; then
    log "请先把正式证书放到 ${install_dir}/certs/custom，然后再修改 ${install_dir}/.env。"
    log "mTLS 开启示例:"
    log "  CORE_GRPC_TLS_CERT_PATH=/certs/custom/media-core.pem"
    log "  CORE_GRPC_TLS_KEY_PATH=/certs/custom/media-core.key"
    log "  CORE_GRPC_TLS_CLIENT_CA_PATH=/certs/custom/ca.pem"
    log "  AGENT_CORE_ENDPOINT=https://${grpc_host_hint}:${grpc_port_hint}"
    log "  AGENT_CERT_PATH=/certs/custom/media-agent.pem"
    log "  AGENT_KEY_PATH=/certs/custom/media-agent.key"
    log "  AGENT_CA_PATH=/certs/custom/ca.pem"
    log "  AGENT_TLS_DOMAIN_NAME=streamserver-core.local"
    log "HTTPS 开启说明:"
    log "  当前应用无内置 HTTPS 监听，请在反向代理中加载 ${install_dir}/certs/custom/https.pem 和 https.key 后转发到 media-core:8080。"
    emit_manual_start_hint "${install_dir}"
    return 1
  fi

  log "如需后续测试 TLS，可直接使用 ${install_dir}/certs/self-signed 下的测试证书。"
  log "mTLS 启用时，建议将 agent 侧地址改为 https://${grpc_host_hint}:${grpc_port_hint} 并设置 AGENT_TLS_DOMAIN_NAME=streamserver-core.local"
  log "HTTPS 如需启用，请在前置 Nginx/Caddy/Traefik 中加载 ${install_dir}/certs/self-signed/https.pem 和 https.key，再转发到 media-core:8080。"
  return 0
}

start_stack_if_requested() {
  local install_dir="$1"
  if prompt_yes_no "是否立即启动该部署？" "Y"; then
    (
      cd "${install_dir}"
      compose_with_file up -d
    )
    log "已启动，常用命令:"
    log "  cd ${install_dir} && ${COMPOSE_CMD_DISPLAY} -f ${COMPOSE_FILE_NAME} ps"
    log "  cd ${install_dir} && ${COMPOSE_CMD_DISPLAY} -f ${COMPOSE_FILE_NAME} logs -f"
  else
    emit_manual_start_hint "${install_dir}"
  fi
}

select_role() {
  local answer
  {
    echo "请选择安装角色:"
    echo "  1) control-plane"
    echo "     用途: 只安装中心控制面，包含 media-core 和 PostgreSQL。"
    echo "     适合: 多工作节点部署中的中心节点，或你已经有独立媒体工作节点的情况。"
    echo "     网络特性: media-core 使用 host；PostgreSQL 保持 bridge。"
    echo
    echo "  2) worker-host-cpu"
    echo "     用途: 安装 CPU-only 媒体工作节点，包含 media-agent 和 ZLMediaKit。"
    echo "     适合: 所有工作节点场景，尤其是组播和需要直接绑定宿主机网卡的情况。"
    echo "     网络特性: media-agent 和 ZLMediaKit 直接使用 host 网络。"
    echo "     注意: 会直接占用宿主机媒体端口，更适合专用媒体节点。"
    echo
    if [ "${BUNDLE_GPU_SUPPORT}" = "true" ]; then
      echo "  3) worker-host-gpu"
      echo "     用途: 安装 GPU-enabled 媒体工作节点。"
      echo "     适合: 以转码、重编码推流为主，希望优先走 CUDA/NVENC 的场景。"
      echo "     前提: 宿主机可执行 nvidia-smi，Docker 已配置 nvidia runtime。"
      echo
      echo "  4) all-in-one-host-cpu"
      echo "     用途: 单机安装完整系统（CPU-only 工作节点），但媒体面直连宿主机网络。"
      echo "     适合: 同一台机器上既跑控制面又跑真实组播验证。"
      echo "     网络特性: media-core/media-agent/ZLMediaKit 使用 host；PostgreSQL 保持 bridge。"
      echo "     优点: 比全量 host 更克制，不会把数据库和控制面也切到 host 网络。"
      echo "     适用前提: 只有在确实需要 host 网络或直连网卡时才值得选择。"
      echo "     注意: 仍会直接占用宿主机的 8080/50051/8081/80/554/1935 等端口。"
      echo
      echo "  5) all-in-one-host-gpu"
      echo "     用途: 单机安装完整系统，并让工作节点部分支持 NVIDIA GPU。"
      echo "     适合: 单机联调、单机媒体验证，以及本机 GPU 转码链路验证。"
      echo "     前提: 宿主机可执行 nvidia-smi，Docker 已配置 nvidia runtime。"
      echo
      echo "输入方式: 直接输入上面的角色编号 1-5 后回车。"
    else
      echo "  3) all-in-one-host-cpu"
      echo "     用途: 单机安装完整系统（CPU-only 工作节点），但媒体面直连宿主机网络。"
      echo "     适合: 同一台机器上既跑控制面又跑真实组播验证。"
      echo "     网络特性: media-core/media-agent/ZLMediaKit 使用 host；PostgreSQL 保持 bridge。"
      echo "     优点: 比全量 host 更克制，不会把数据库和控制面也切到 host 网络。"
      echo "     适用前提: 只有在确实需要 host 网络或直连网卡时才值得选择。"
      echo "     注意: 仍会直接占用宿主机的 8080/50051/8081/80/554/1935 等端口。"
      echo
      echo "当前离线包为 CPU-only，不包含 GPU 镜像和 GPU 模板。"
      echo "输入方式: 直接输入上面的角色编号 1-3 后回车。"
    fi
    echo "默认值: 直接回车等同于输入 1（control-plane）。"
    if [ "${BUNDLE_GPU_SUPPORT}" = "true" ]; then
      echo "快速建议: 普通工作节点选 2，需要 GPU 工作节点选 3，单机联调选 4，需要单机 GPU 联调选 5。"
    else
      echo "快速建议: 普通工作节点选 2，需要单机联调选 3。"
    fi
  } >&2
  while true; do
    if [ "${BUNDLE_GPU_SUPPORT}" = "true" ]; then
      answer="$(prompt "输入角色编号（1-5）" "1")"
      case "${answer}" in
        1) printf '%s' "control-plane"; return 0 ;;
        2) printf '%s' "worker-host-cpu"; return 0 ;;
        3) printf '%s' "worker-host-gpu"; return 0 ;;
        4) printf '%s' "all-in-one-host-cpu"; return 0 ;;
        5) printf '%s' "all-in-one-host-gpu"; return 0 ;;
        *) echo "请输入 1 到 5。" >&2 ;;
      esac
    else
      answer="$(prompt "输入角色编号（1-3）" "1")"
      case "${answer}" in
        1) printf '%s' "control-plane"; return 0 ;;
        2) printf '%s' "worker-host-cpu"; return 0 ;;
        3) printf '%s' "all-in-one-host-cpu"; return 0 ;;
        *) echo "请输入 1 到 3。" >&2 ;;
      esac
    fi
  done
}

configure_control_plane() {
  local default_dir="/opt/streamserver/control-plane"
  local default_secret
  local default_password

  default_secret="$(generate_secret)"
  default_password="$(generate_secret)"

  PROJECT_NAME="$(prompt_non_empty "Compose 项目名" "streamserver-control")"
  INSTALL_DIR="$(prompt_non_empty "安装目录" "${default_dir}")"
  POSTGRES_DB="$(prompt_non_empty "PostgreSQL 数据库名" "streamserver")"
  POSTGRES_USER="$(prompt_non_empty "PostgreSQL 用户名" "postgres")"
  POSTGRES_PASSWORD="$(prompt "PostgreSQL 密码（留空自动生成）" "")"
  HOOK_SHARED_SECRET="$(prompt "ZLM Hook/API 密钥（留空自动生成）" "")"
  CORE_HTTP_PORT="$(prompt_non_empty "media-core HTTP 暴露端口" "8080")"
  CORE_GRPC_PORT="$(prompt_non_empty "media-core gRPC 暴露端口" "50051")"
  STACK_SUBNET="172.29.0.0/24"
  POSTGRES_IP="172.29.0.40"
  HOOK_SOURCE_ALLOWLIST="$(prompt "Hook 源 IP 白名单，逗号分隔（可留空）" "")"
  STORAGE_ALLOWLIST="/data/media/work,/data/zlm/www"
  prompt_local_auth_configuration

  [ -n "${POSTGRES_PASSWORD}" ] || POSTGRES_PASSWORD="${default_password}"
  [ -n "${HOOK_SHARED_SECRET}" ] || HOOK_SHARED_SECRET="${default_secret}"

  prepare_install_dir "${INSTALL_DIR}"
  copy_common_assets "${INSTALL_DIR}"
  prepare_local_auth_assets "${INSTALL_DIR}"
  copy_compose_template "control-plane" "${INSTALL_DIR}"
  prepare_control_plane_layout "${INSTALL_DIR}"
  install_host_binaries "${INSTALL_DIR}" media-core
  install_host_ui "${INSTALL_DIR}" media-core
  write_control_plane_env "${INSTALL_DIR}/.env"
  ensure_images_loaded postgres media-core
  bootstrap_local_admin_if_needed "${INSTALL_DIR}"
  show_tls_notice "${INSTALL_DIR}" "streamserver-core.local" "${CORE_GRPC_PORT}" || return 0
  start_stack_if_requested "${INSTALL_DIR}"
}

configure_worker_host() {
  local default_dir="/opt/streamserver/worker-host-cpu"
  local default_ip
  local agent_labels

  PROJECT_NAME="$(prompt_non_empty "Compose 项目名" "streamserver-worker-cpu")"
  INSTALL_DIR="$(prompt_non_empty "安装目录" "${default_dir}")"
  NODE_ID="$(prompt_non_empty "节点 UUID（留空自动生成）" "$(generate_uuid)")"
  AGENT_NODE_NAME="$(prompt_non_empty "节点名称" "$(hostname -s 2>/dev/null || echo worker-1)")"
  configure_host_interface_defaults
  default_ip="${PRIMARY_INTERFACE_IP}"
  ZLM_API_HOST="${PRIMARY_INTERFACE_IP}"
  CORE_HTTP_HOST="$(prompt_non_empty "control-plane HTTP 地址或域名" "${default_ip}")"
  CORE_HTTP_PORT="$(prompt_non_empty "control-plane HTTP 端口" "8080")"
  CORE_GRPC_HOST="$(prompt_non_empty "control-plane gRPC 地址或域名" "${CORE_HTTP_HOST}")"
  CORE_GRPC_PORT="$(prompt_non_empty "control-plane gRPC 端口" "50051")"
  PUBLIC_HOST="$(prompt_non_empty "当前工作节点对外可访问的主机名或 IP" "${default_ip}")"
  HOOK_SHARED_SECRET="$(prompt "ZLM Hook/API 密钥（需与 control-plane 一致）" "")"
  AGENT_HTTP_PORT="8081"
  ZLM_HTTP_PORT="80"
  ZLM_RTMP_PORT="1935"
  ZLM_RTSP_PORT="554"
  agent_labels="$(collect_agent_labels "cpu")"

  [ -n "${HOOK_SHARED_SECRET}" ] || fail "worker 角色必须提供与 control-plane 一致的 Hook/API 密钥"

  prepare_install_dir "${INSTALL_DIR}"
  copy_common_assets "${INSTALL_DIR}"
  copy_compose_template "worker-host-cpu" "${INSTALL_DIR}"
  prepare_worker_layout "${INSTALL_DIR}"
  install_host_binaries "${INSTALL_DIR}" media-agent
  render_zlm_config \
    "${INSTALL_DIR}" \
    "http://${CORE_HTTP_HOST}:${CORE_HTTP_PORT}/internal/hooks/zlm/${NODE_ID}" \
    "::1,127.0.0.1,10.0.0.0-10.255.255.255,172.16.0.0-172.31.255.255,192.168.0.0-192.168.255.255"
  write_worker_host_env "${INSTALL_DIR}/.env" "${MEDIA_AGENT_IMAGE}" "cpu" "${agent_labels}"
  ensure_images_loaded media-agent zlmediakit
  show_tls_notice "${INSTALL_DIR}" "${CORE_GRPC_HOST}" "${CORE_GRPC_PORT}" || return 0
  start_stack_if_requested "${INSTALL_DIR}"
}

configure_worker_host_gpu() {
  local default_dir="/opt/streamserver/worker-host-gpu"
  local default_ip
  local agent_labels

  [ "${BUNDLE_GPU_SUPPORT}" = "true" ] || fail "当前离线包为 CPU-only，不支持 GPU 工作节点模板"
  ensure_nvidia_runtime_ready

  PROJECT_NAME="$(prompt_non_empty "Compose 项目名" "streamserver-worker-gpu")"
  INSTALL_DIR="$(prompt_non_empty "安装目录" "${default_dir}")"
  NODE_ID="$(prompt_non_empty "节点 UUID（留空自动生成）" "$(generate_uuid)")"
  AGENT_NODE_NAME="$(prompt_non_empty "节点名称" "$(hostname -s 2>/dev/null || echo worker-gpu-1)")"
  configure_host_interface_defaults
  default_ip="${PRIMARY_INTERFACE_IP}"
  ZLM_API_HOST="${PRIMARY_INTERFACE_IP}"
  CORE_HTTP_HOST="$(prompt_non_empty "control-plane HTTP 地址或域名" "${default_ip}")"
  CORE_HTTP_PORT="$(prompt_non_empty "control-plane HTTP 端口" "8080")"
  CORE_GRPC_HOST="$(prompt_non_empty "control-plane gRPC 地址或域名" "${CORE_HTTP_HOST}")"
  CORE_GRPC_PORT="$(prompt_non_empty "control-plane gRPC 端口" "50051")"
  PUBLIC_HOST="$(prompt_non_empty "当前工作节点对外可访问的主机名或 IP" "${default_ip}")"
  HOOK_SHARED_SECRET="$(prompt "ZLM Hook/API 密钥（需与 control-plane 一致）" "")"
  AGENT_HTTP_PORT="8081"
  ZLM_HTTP_PORT="80"
  ZLM_RTMP_PORT="1935"
  ZLM_RTSP_PORT="554"
  agent_labels="$(collect_agent_labels "gpu")"

  [ -n "${HOOK_SHARED_SECRET}" ] || fail "worker 角色必须提供与 control-plane 一致的 Hook/API 密钥"

  prepare_install_dir "${INSTALL_DIR}"
  copy_common_assets "${INSTALL_DIR}"
  copy_compose_template "worker-host-gpu" "${INSTALL_DIR}"
  prepare_worker_layout "${INSTALL_DIR}"
  install_host_binaries "${INSTALL_DIR}" media-agent
  render_zlm_config \
    "${INSTALL_DIR}" \
    "http://${CORE_HTTP_HOST}:${CORE_HTTP_PORT}/internal/hooks/zlm/${NODE_ID}" \
    "::1,127.0.0.1,10.0.0.0-10.255.255.255,172.16.0.0-172.31.255.255,192.168.0.0-192.168.255.255"
  write_worker_host_env "${INSTALL_DIR}/.env" "${MEDIA_AGENT_GPU_IMAGE}" "gpu" "${agent_labels}"
  ensure_images_loaded media-agent-gpu zlmediakit
  show_tls_notice "${INSTALL_DIR}" "${CORE_GRPC_HOST}" "${CORE_GRPC_PORT}" || return 0
  start_stack_if_requested "${INSTALL_DIR}"
}

configure_all_in_one_host() {
  local default_dir="/opt/streamserver/all-in-one-host-cpu"
  local default_secret
  local default_password
  local default_ip
  local agent_labels

  default_secret="$(generate_secret)"
  default_password="$(generate_secret)"

  PROJECT_NAME="$(prompt_non_empty "Compose 项目名" "streamserver-all-in-one-host-cpu")"
  INSTALL_DIR="$(prompt_non_empty "安装目录" "${default_dir}")"
  NODE_ID="$(prompt_non_empty "节点 UUID（留空自动生成）" "$(generate_uuid)")"
  AGENT_NODE_NAME="$(prompt_non_empty "节点名称" "$(hostname -s 2>/dev/null || echo node-1)")"
  configure_host_interface_defaults
  default_ip="${PRIMARY_INTERFACE_IP}"
  ZLM_API_HOST="${PRIMARY_INTERFACE_IP}"
  POSTGRES_DB="$(prompt_non_empty "PostgreSQL 数据库名" "streamserver")"
  POSTGRES_USER="$(prompt_non_empty "PostgreSQL 用户名" "postgres")"
  POSTGRES_PASSWORD="$(prompt "PostgreSQL 密码（留空自动生成）" "")"
  HOOK_SHARED_SECRET="$(prompt "ZLM Hook/API 密钥（留空自动生成）" "")"
  PUBLIC_HOST="$(prompt_non_empty "当前主机对外可访问的主机名或 IP" "${default_ip}")"
  CORE_HTTP_PORT="8080"
  CORE_GRPC_PORT="50051"
  AGENT_HTTP_PORT="8081"
  ZLM_HTTP_PORT="80"
  ZLM_RTMP_PORT="1935"
  ZLM_RTSP_PORT="554"
  STACK_SUBNET="172.29.0.0/24"
  CORE_IP="172.29.0.10"
  POSTGRES_IP="172.29.0.40"
  HOOK_SOURCE_ALLOWLIST=""
  prompt_local_auth_configuration
  agent_labels="$(collect_agent_labels "cpu")"

  [ -n "${POSTGRES_PASSWORD}" ] || POSTGRES_PASSWORD="${default_password}"
  [ -n "${HOOK_SHARED_SECRET}" ] || HOOK_SHARED_SECRET="${default_secret}"

  prepare_install_dir "${INSTALL_DIR}"
  copy_common_assets "${INSTALL_DIR}"
  prepare_local_auth_assets "${INSTALL_DIR}"
  copy_compose_template "all-in-one-host-cpu" "${INSTALL_DIR}"
  prepare_control_plane_layout "${INSTALL_DIR}"
  prepare_worker_layout "${INSTALL_DIR}"
  install_host_binaries "${INSTALL_DIR}" media-core media-agent
  install_host_ui "${INSTALL_DIR}" media-core
  render_zlm_config \
    "${INSTALL_DIR}" \
    "http://127.0.0.1:${CORE_HTTP_PORT}/internal/hooks/zlm/${NODE_ID}" \
    "::1,127.0.0.1,10.0.0.0-10.255.255.255,172.16.0.0-172.31.255.255,192.168.0.0-192.168.255.255"
  write_all_in_one_host_env "${INSTALL_DIR}/.env" "${MEDIA_AGENT_IMAGE}" "cpu" "${agent_labels}"
  ensure_images_loaded postgres media-core media-agent zlmediakit
  bootstrap_local_admin_if_needed "${INSTALL_DIR}"
  log "all-in-one-host-cpu 说明: media-core、media-agent 和 ZLMediaKit 会直接占用宿主机端口 ${CORE_HTTP_PORT}/${CORE_GRPC_PORT}/${AGENT_HTTP_PORT}/${ZLM_HTTP_PORT}/${ZLM_RTMP_PORT}/${ZLM_RTSP_PORT}。"
  log "如果这些端口已被宿主机其他服务占用，请先释放端口，或改用非 host 模式。"
  show_tls_notice "${INSTALL_DIR}" "127.0.0.1" "${CORE_GRPC_PORT}" || return 0
  start_stack_if_requested "${INSTALL_DIR}"
}

configure_all_in_one_host_gpu() {
  local default_dir="/opt/streamserver/all-in-one-host-gpu"
  local default_secret
  local default_password
  local default_ip
  local agent_labels

  [ "${BUNDLE_GPU_SUPPORT}" = "true" ] || fail "当前离线包为 CPU-only，不支持 GPU 一体机模板"
  ensure_nvidia_runtime_ready

  default_secret="$(generate_secret)"
  default_password="$(generate_secret)"

  PROJECT_NAME="$(prompt_non_empty "Compose 项目名" "streamserver-all-in-one-host-gpu")"
  INSTALL_DIR="$(prompt_non_empty "安装目录" "${default_dir}")"
  NODE_ID="$(prompt_non_empty "节点 UUID（留空自动生成）" "$(generate_uuid)")"
  AGENT_NODE_NAME="$(prompt_non_empty "节点名称" "$(hostname -s 2>/dev/null || echo node-gpu-1)")"
  configure_host_interface_defaults
  default_ip="${PRIMARY_INTERFACE_IP}"
  ZLM_API_HOST="${PRIMARY_INTERFACE_IP}"
  POSTGRES_DB="$(prompt_non_empty "PostgreSQL 数据库名" "streamserver")"
  POSTGRES_USER="$(prompt_non_empty "PostgreSQL 用户名" "postgres")"
  POSTGRES_PASSWORD="$(prompt "PostgreSQL 密码（留空自动生成）" "")"
  HOOK_SHARED_SECRET="$(prompt "ZLM Hook/API 密钥（留空自动生成）" "")"
  PUBLIC_HOST="$(prompt_non_empty "当前主机对外可访问的主机名或 IP" "${default_ip}")"
  CORE_HTTP_PORT="8080"
  CORE_GRPC_PORT="50051"
  AGENT_HTTP_PORT="8081"
  ZLM_HTTP_PORT="80"
  ZLM_RTMP_PORT="1935"
  ZLM_RTSP_PORT="554"
  STACK_SUBNET="172.29.0.0/24"
  CORE_IP="172.29.0.10"
  POSTGRES_IP="172.29.0.40"
  HOOK_SOURCE_ALLOWLIST=""
  prompt_local_auth_configuration
  agent_labels="$(collect_agent_labels "gpu")"

  [ -n "${POSTGRES_PASSWORD}" ] || POSTGRES_PASSWORD="${default_password}"
  [ -n "${HOOK_SHARED_SECRET}" ] || HOOK_SHARED_SECRET="${default_secret}"

  prepare_install_dir "${INSTALL_DIR}"
  copy_common_assets "${INSTALL_DIR}"
  prepare_local_auth_assets "${INSTALL_DIR}"
  copy_compose_template "all-in-one-host-gpu" "${INSTALL_DIR}"
  prepare_control_plane_layout "${INSTALL_DIR}"
  prepare_worker_layout "${INSTALL_DIR}"
  install_host_binaries "${INSTALL_DIR}" media-core media-agent
  install_host_ui "${INSTALL_DIR}" media-core
  render_zlm_config \
    "${INSTALL_DIR}" \
    "http://127.0.0.1:${CORE_HTTP_PORT}/internal/hooks/zlm/${NODE_ID}" \
    "::1,127.0.0.1,10.0.0.0-10.255.255.255,172.16.0.0-172.31.255.255,192.168.0.0-192.168.255.255"
  write_all_in_one_host_env "${INSTALL_DIR}/.env" "${MEDIA_AGENT_GPU_IMAGE}" "gpu" "${agent_labels}"
  ensure_images_loaded postgres media-core media-agent-gpu zlmediakit
  bootstrap_local_admin_if_needed "${INSTALL_DIR}"
  log "all-in-one-host-gpu 说明: media-core、media-agent 和 ZLMediaKit 会直接占用宿主机端口 ${CORE_HTTP_PORT}/${CORE_GRPC_PORT}/${AGENT_HTTP_PORT}/${ZLM_HTTP_PORT}/${ZLM_RTMP_PORT}/${ZLM_RTSP_PORT}。"
  log "该模式要求宿主机 NVIDIA 驱动和 Docker nvidia runtime 均已就绪。"
  show_tls_notice "${INSTALL_DIR}" "127.0.0.1" "${CORE_GRPC_PORT}" || return 0
  start_stack_if_requested "${INSTALL_DIR}"
}

main() {
  local role
  ensure_linux_amd64
  ensure_docker_ready

  log "离线包版本: ${BUNDLE_VERSION}"
  role="$(select_role)"
  case "${role}" in
    control-plane) configure_control_plane ;;
    worker-host-cpu) configure_worker_host ;;
    worker-host-gpu) configure_worker_host_gpu ;;
    all-in-one-host-cpu) configure_all_in_one_host ;;
    all-in-one-host-gpu) configure_all_in_one_host_gpu ;;
    *) fail "未知角色 ${role}" ;;
  esac
}

main "$@"
