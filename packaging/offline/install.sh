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

COMPOSE_CMD=()
COMPOSE_CMD_DISPLAY=""

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

escape_sed_replacement() {
  printf '%s' "$1" | sed 's/[\\/&|]/\\&/g'
}

archive_for_image_key() {
  case "$1" in
    postgres) printf '%s' "${POSTGRES_IMAGE_ARCHIVE}" ;;
    media-core) printf '%s' "${MEDIA_CORE_IMAGE_ARCHIVE}" ;;
    media-agent) printf '%s' "${MEDIA_AGENT_IMAGE_ARCHIVE}" ;;
    zlmediakit) printf '%s' "${ZLM_IMAGE_ARCHIVE}" ;;
    *) fail "未知镜像标识: $1" ;;
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

prepare_install_dir() {
  local install_dir="$1"
  if [ -e "${install_dir}" ] && [ -n "$(find "${install_dir}" -mindepth 1 -maxdepth 1 2>/dev/null | head -n 1)" ]; then
    prompt_yes_no "目录 ${install_dir} 已存在且非空，是否继续覆盖模板文件？" "N" || fail "用户取消安装"
  fi
  mkdir -p "${install_dir}"
  mkdir -p "${install_dir}/docs"
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
HOOK_SHARED_SECRET=${HOOK_SHARED_SECRET}
HOOK_SOURCE_ALLOWLIST=${HOOK_SOURCE_ALLOWLIST}
STORAGE_ALLOWLIST=${STORAGE_ALLOWLIST}
AUTH_ENABLED=${AUTH_ENABLED}
JWT_PUBLIC_KEY=${JWT_PUBLIC_KEY}

# HTTPS 默认关闭。
# 当前应用不内置 HTTPS listener，如需 HTTPS，请在反向代理中终止 TLS 后转发到 media-core:8080。
#
# gRPC mTLS 默认关闭。如需开启，可在确认或替换证书后取消以下注释：
# CORE_GRPC_TLS_CERT_PATH=/certs/self-signed/media-core.pem
# CORE_GRPC_TLS_KEY_PATH=/certs/self-signed/media-core.key
# CORE_GRPC_TLS_CLIENT_CA_PATH=/certs/self-signed/ca.pem
EOF
}

write_worker_bridge_env() {
  local env_file="$1"
  cat >"${env_file}" <<EOF
COMPOSE_PROJECT_NAME=${PROJECT_NAME}
MEDIA_AGENT_IMAGE=${MEDIA_AGENT_IMAGE}
ZLM_IMAGE=${ZLM_IMAGE}
NODE_ID=${NODE_ID}
AGENT_NODE_NAME=${AGENT_NODE_NAME}
CORE_HTTP_HOST=${CORE_HTTP_HOST}
CORE_HTTP_PORT=${CORE_HTTP_PORT}
CORE_GRPC_HOST=${CORE_GRPC_HOST}
CORE_GRPC_PORT=${CORE_GRPC_PORT}
PUBLIC_HOST=${PUBLIC_HOST}
AGENT_HTTP_PORT=${AGENT_HTTP_PORT}
ZLM_HTTP_PORT=${ZLM_HTTP_PORT}
ZLM_RTMP_PORT=${ZLM_RTMP_PORT}
ZLM_RTSP_PORT=${ZLM_RTSP_PORT}
HOOK_SHARED_SECRET=${HOOK_SHARED_SECRET}
AGENT_NETWORK_MODE=bridge
AGENT_LABELS=offline,worker,bridge
WORK_ROOT=/data/media/work
STACK_SUBNET=${STACK_SUBNET}
AGENT_IP=${AGENT_IP}
ZLM_IP=${ZLM_IP}

# mTLS 默认关闭。如需开启，请将 AGENT_CORE_ENDPOINT 改为 https，并取消以下注释：
# AGENT_CORE_ENDPOINT=https://${CORE_GRPC_HOST}:${CORE_GRPC_PORT}
# AGENT_CERT_PATH=/certs/self-signed/media-agent.pem
# AGENT_KEY_PATH=/certs/self-signed/media-agent.key
# AGENT_CA_PATH=/certs/self-signed/ca.pem
# AGENT_TLS_DOMAIN_NAME=streamserver-core.local
EOF
}

write_worker_host_env() {
  local env_file="$1"
  cat >"${env_file}" <<EOF
COMPOSE_PROJECT_NAME=${PROJECT_NAME}
MEDIA_AGENT_IMAGE=${MEDIA_AGENT_IMAGE}
ZLM_IMAGE=${ZLM_IMAGE}
NODE_ID=${NODE_ID}
AGENT_NODE_NAME=${AGENT_NODE_NAME}
CORE_HTTP_HOST=${CORE_HTTP_HOST}
CORE_HTTP_PORT=${CORE_HTTP_PORT}
CORE_GRPC_HOST=${CORE_GRPC_HOST}
CORE_GRPC_PORT=${CORE_GRPC_PORT}
PUBLIC_HOST=${PUBLIC_HOST}
AGENT_HTTP_PORT=${AGENT_HTTP_PORT}
ZLM_HTTP_PORT=${ZLM_HTTP_PORT}
ZLM_RTMP_PORT=${ZLM_RTMP_PORT}
ZLM_RTSP_PORT=${ZLM_RTSP_PORT}
HOOK_SHARED_SECRET=${HOOK_SHARED_SECRET}
AGENT_NETWORK_MODE=host
AGENT_LABELS=offline,worker,host
WORK_ROOT=/data/media/work

# mTLS 默认关闭。如需开启，请将 AGENT_CORE_ENDPOINT 改为 https，并取消以下注释：
# AGENT_CORE_ENDPOINT=https://${CORE_GRPC_HOST}:${CORE_GRPC_PORT}
# AGENT_CERT_PATH=/certs/self-signed/media-agent.pem
# AGENT_KEY_PATH=/certs/self-signed/media-agent.key
# AGENT_CA_PATH=/certs/self-signed/ca.pem
# AGENT_TLS_DOMAIN_NAME=streamserver-core.local
EOF
}

write_all_in_one_env() {
  local env_file="$1"
  cat >"${env_file}" <<EOF
COMPOSE_PROJECT_NAME=${PROJECT_NAME}
POSTGRES_IMAGE=${POSTGRES_IMAGE}
MEDIA_CORE_IMAGE=${MEDIA_CORE_IMAGE}
MEDIA_AGENT_IMAGE=${MEDIA_AGENT_IMAGE}
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
HOOK_SHARED_SECRET=${HOOK_SHARED_SECRET}
PUBLIC_HOST=${PUBLIC_HOST}
NODE_ID=${NODE_ID}
AGENT_NODE_NAME=${AGENT_NODE_NAME}
AGENT_LABELS=offline,all-in-one
STORAGE_ALLOWLIST=/data/media/work,/data/zlm/record,/data/zlm/www
AUTH_ENABLED=false
JWT_PUBLIC_KEY=
STACK_SUBNET=${STACK_SUBNET}
CORE_IP=${CORE_IP}
AGENT_IP=${AGENT_IP}
ZLM_IP=${ZLM_IP}
POSTGRES_IP=${POSTGRES_IP}

# HTTPS 默认关闭。
# 当前应用不内置 HTTPS listener，如需 HTTPS，请在反向代理中终止 TLS 后转发到 media-core:8080。
#
# gRPC mTLS 默认关闭。启用时请同时为 media-core 和 media-agent 设置以下变量：
# CORE_GRPC_TLS_CERT_PATH=/certs/self-signed/media-core.pem
# CORE_GRPC_TLS_KEY_PATH=/certs/self-signed/media-core.key
# CORE_GRPC_TLS_CLIENT_CA_PATH=/certs/self-signed/ca.pem
# AGENT_CORE_ENDPOINT=https://media-core:50051
# AGENT_CERT_PATH=/certs/self-signed/media-agent.pem
# AGENT_KEY_PATH=/certs/self-signed/media-agent.key
# AGENT_CA_PATH=/certs/self-signed/ca.pem
# AGENT_TLS_DOMAIN_NAME=streamserver-core.local
EOF
}

write_all_in_one_host_env() {
  local env_file="$1"
  cat >"${env_file}" <<EOF
COMPOSE_PROJECT_NAME=${PROJECT_NAME}
POSTGRES_IMAGE=${POSTGRES_IMAGE}
MEDIA_CORE_IMAGE=${MEDIA_CORE_IMAGE}
MEDIA_AGENT_IMAGE=${MEDIA_AGENT_IMAGE}
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
HOOK_SHARED_SECRET=${HOOK_SHARED_SECRET}
HOOK_SOURCE_ALLOWLIST=${HOOK_SOURCE_ALLOWLIST}
PUBLIC_HOST=${PUBLIC_HOST}
NODE_ID=${NODE_ID}
AGENT_NODE_NAME=${AGENT_NODE_NAME}
AGENT_LABELS=offline,all-in-one,host
STORAGE_ALLOWLIST=/data/media/work,/data/zlm/record,/data/zlm/www
AUTH_ENABLED=false
JWT_PUBLIC_KEY=
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
  cp "${PACKAGE_ROOT}/templates/${template_name}/compose.yml" "${install_dir}/compose.yml"
}

prepare_control_plane_layout() {
  local install_dir="$1"
  mkdir -p "${install_dir}/data/postgres"
}

prepare_worker_layout() {
  local install_dir="$1"
  mkdir -p \
    "${install_dir}/data/media/work" \
    "${install_dir}/data/media/logs" \
    "${install_dir}/data/zlm/www" \
    "${install_dir}/data/zlm/record"
}

emit_manual_start_hint() {
  local install_dir="$1"
  log "已写入部署文件，稍后可执行:"
  log "  cd ${install_dir} && ${COMPOSE_CMD_DISPLAY} up -d"
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
      compose_cmd up -d
    )
    log "已启动，常用命令:"
    log "  cd ${install_dir} && ${COMPOSE_CMD_DISPLAY} ps"
    log "  cd ${install_dir} && ${COMPOSE_CMD_DISPLAY} logs -f"
  else
    emit_manual_start_hint "${install_dir}"
  fi
}

select_role() {
  local answer
  echo "请选择安装角色:"
  echo "  1) control-plane"
  echo "     用途: 只安装中心控制面，包含 media-core 和 PostgreSQL。"
  echo "     适合: 多工作节点部署中的中心节点，或你已经有独立媒体工作节点的情况。"
  echo "     网络特性: 不处理媒体组播，只暴露控制面 HTTP/gRPC 端口。"
  echo
  echo "  2) worker-bridge"
  echo "     用途: 只安装媒体工作节点，包含 media-agent 和 ZLMediaKit。"
  echo "     适合: 普通拉流、转发、录像、RTSP/RTMP/HLS 联调。"
  echo "     网络特性: 使用 Docker bridge 网络，和宿主网络隔离较好。"
  echo "     限制: 不推荐用于真实 UDP 组播场景。"
  echo
  echo "  3) worker-host"
  echo "     用途: 只安装媒体工作节点，包含 media-agent 和 ZLMediaKit。"
  echo "     适合: 真实 UDP 组播、需要直接绑定宿主机网卡的工作节点。"
  echo "     网络特性: media-agent 和 ZLMediaKit 直接使用 host 网络。"
  echo "     注意: 会直接占用宿主机媒体端口，更适合专用媒体节点。"
  echo
  echo "  4) all-in-one"
  echo "     用途: 单机安装完整系统，包含 media-core、PostgreSQL、media-agent、ZLMediaKit。"
  echo "     适合: 单机演示、离线验收、非组播场景的一体化体验。"
  echo "     网络特性: 全部服务使用 Docker bridge 网络，隔离性最好，对宿主机干扰最小。"
  echo "     优先推荐: 只要不需要真实组播，单机模式应先选它。"
  echo "     限制: 不推荐用于真实 UDP 组播。"
  echo
  echo "  5) all-in-one-host"
  echo "     用途: 单机安装完整系统，但媒体面直连宿主机网络。"
  echo "     适合: 同一台机器上既跑控制面又跑真实组播验证。"
  echo "     网络特性: media-core/PostgreSQL 保持 bridge；media-agent/ZLMediaKit 使用 host。"
  echo "     优点: 比全量 host 更克制，不会把数据库和控制面也切到 host 网络。"
  echo "     适用前提: 只有在确实需要 host 网络或直连网卡时才值得选择。"
  echo "     注意: 仍会直接占用宿主机的 80/554/1935/8081 等媒体相关端口。"
  while true; do
    answer="$(prompt "输入角色编号" "1")"
    case "${answer}" in
      1) printf '%s' "control-plane"; return 0 ;;
      2) printf '%s' "worker-bridge"; return 0 ;;
      3) printf '%s' "worker-host"; return 0 ;;
      4) printf '%s' "all-in-one"; return 0 ;;
      5) printf '%s' "all-in-one-host"; return 0 ;;
      *) echo "请输入 1 到 5。" >&2 ;;
    esac
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
  HOOK_SOURCE_ALLOWLIST="$(prompt "Hook 源 IP 白名单，逗号分隔（可留空）" "")"
  AUTH_ENABLED="false"
  JWT_PUBLIC_KEY=""
  STORAGE_ALLOWLIST="/data/media/work,/data/zlm/record,/data/zlm/www"

  [ -n "${POSTGRES_PASSWORD}" ] || POSTGRES_PASSWORD="${default_password}"
  [ -n "${HOOK_SHARED_SECRET}" ] || HOOK_SHARED_SECRET="${default_secret}"

  prepare_install_dir "${INSTALL_DIR}"
  copy_common_assets "${INSTALL_DIR}"
  copy_compose_template "control-plane" "${INSTALL_DIR}"
  prepare_control_plane_layout "${INSTALL_DIR}"
  write_control_plane_env "${INSTALL_DIR}/.env"
  ensure_images_loaded postgres media-core
  show_tls_notice "${INSTALL_DIR}" "streamserver-core.local" "${CORE_GRPC_PORT}" || return 0
  start_stack_if_requested "${INSTALL_DIR}"
}

configure_worker_bridge() {
  local default_dir="/opt/streamserver/worker-bridge"
  local default_ip

  default_ip="$(detect_primary_ip)"
  PROJECT_NAME="$(prompt_non_empty "Compose 项目名" "streamserver-worker")"
  INSTALL_DIR="$(prompt_non_empty "安装目录" "${default_dir}")"
  NODE_ID="$(prompt_non_empty "节点 UUID（留空自动生成）" "$(generate_uuid)")"
  AGENT_NODE_NAME="$(prompt_non_empty "节点名称" "$(hostname -s 2>/dev/null || echo worker-1)")"
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
  STACK_SUBNET="172.29.0.0/24"
  AGENT_IP="172.29.0.30"
  ZLM_IP="172.29.0.20"

  [ -n "${HOOK_SHARED_SECRET}" ] || fail "worker 角色必须提供与 control-plane 一致的 Hook/API 密钥"

  prepare_install_dir "${INSTALL_DIR}"
  copy_common_assets "${INSTALL_DIR}"
  copy_compose_template "worker-bridge" "${INSTALL_DIR}"
  prepare_worker_layout "${INSTALL_DIR}"
  render_zlm_config \
    "${INSTALL_DIR}" \
    "http://${CORE_HTTP_HOST}:${CORE_HTTP_PORT}/internal/hooks/zlm/${NODE_ID}" \
    "::1,127.0.0.1,${AGENT_IP}"
  write_worker_bridge_env "${INSTALL_DIR}/.env"
  ensure_images_loaded media-agent zlmediakit
  show_tls_notice "${INSTALL_DIR}" "${CORE_GRPC_HOST}" "${CORE_GRPC_PORT}" || return 0
  start_stack_if_requested "${INSTALL_DIR}"
}

configure_worker_host() {
  local default_dir="/opt/streamserver/worker-host"
  local default_ip

  default_ip="$(detect_primary_ip)"
  PROJECT_NAME="$(prompt_non_empty "Compose 项目名" "streamserver-worker")"
  INSTALL_DIR="$(prompt_non_empty "安装目录" "${default_dir}")"
  NODE_ID="$(prompt_non_empty "节点 UUID（留空自动生成）" "$(generate_uuid)")"
  AGENT_NODE_NAME="$(prompt_non_empty "节点名称" "$(hostname -s 2>/dev/null || echo worker-1)")"
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

  [ -n "${HOOK_SHARED_SECRET}" ] || fail "worker 角色必须提供与 control-plane 一致的 Hook/API 密钥"

  prepare_install_dir "${INSTALL_DIR}"
  copy_common_assets "${INSTALL_DIR}"
  copy_compose_template "worker-host" "${INSTALL_DIR}"
  prepare_worker_layout "${INSTALL_DIR}"
  render_zlm_config \
    "${INSTALL_DIR}" \
    "http://${CORE_HTTP_HOST}:${CORE_HTTP_PORT}/internal/hooks/zlm/${NODE_ID}" \
    "::1,127.0.0.1"
  write_worker_host_env "${INSTALL_DIR}/.env"
  ensure_images_loaded media-agent zlmediakit
  show_tls_notice "${INSTALL_DIR}" "${CORE_GRPC_HOST}" "${CORE_GRPC_PORT}" || return 0
  start_stack_if_requested "${INSTALL_DIR}"
}

configure_all_in_one() {
  local default_dir="/opt/streamserver/all-in-one"
  local default_secret
  local default_password
  local default_ip

  default_secret="$(generate_secret)"
  default_password="$(generate_secret)"
  default_ip="$(detect_primary_ip)"

  PROJECT_NAME="$(prompt_non_empty "Compose 项目名" "streamserver-all-in-one")"
  INSTALL_DIR="$(prompt_non_empty "安装目录" "${default_dir}")"
  NODE_ID="$(prompt_non_empty "节点 UUID（留空自动生成）" "$(generate_uuid)")"
  AGENT_NODE_NAME="$(prompt_non_empty "节点名称" "$(hostname -s 2>/dev/null || echo node-1)")"
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
  ZLM_IP="172.29.0.20"
  AGENT_IP="172.29.0.30"
  POSTGRES_IP="172.29.0.40"

  [ -n "${POSTGRES_PASSWORD}" ] || POSTGRES_PASSWORD="${default_password}"
  [ -n "${HOOK_SHARED_SECRET}" ] || HOOK_SHARED_SECRET="${default_secret}"

  prepare_install_dir "${INSTALL_DIR}"
  copy_common_assets "${INSTALL_DIR}"
  copy_compose_template "all-in-one" "${INSTALL_DIR}"
  prepare_control_plane_layout "${INSTALL_DIR}"
  prepare_worker_layout "${INSTALL_DIR}"
  render_zlm_config \
    "${INSTALL_DIR}" \
    "http://media-core:8080/internal/hooks/zlm/${NODE_ID}" \
    "::1,127.0.0.1,${AGENT_IP}"
  write_all_in_one_env "${INSTALL_DIR}/.env"
  ensure_images_loaded postgres media-core media-agent zlmediakit
  show_tls_notice "${INSTALL_DIR}" "media-core" "${CORE_GRPC_PORT}" || return 0
  start_stack_if_requested "${INSTALL_DIR}"
}

configure_all_in_one_host() {
  local default_dir="/opt/streamserver/all-in-one-host"
  local default_secret
  local default_password
  local default_ip

  default_secret="$(generate_secret)"
  default_password="$(generate_secret)"
  default_ip="$(detect_primary_ip)"

  PROJECT_NAME="$(prompt_non_empty "Compose 项目名" "streamserver-all-in-one-host")"
  INSTALL_DIR="$(prompt_non_empty "安装目录" "${default_dir}")"
  NODE_ID="$(prompt_non_empty "节点 UUID（留空自动生成）" "$(generate_uuid)")"
  AGENT_NODE_NAME="$(prompt_non_empty "节点名称" "$(hostname -s 2>/dev/null || echo node-1)")"
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

  [ -n "${POSTGRES_PASSWORD}" ] || POSTGRES_PASSWORD="${default_password}"
  [ -n "${HOOK_SHARED_SECRET}" ] || HOOK_SHARED_SECRET="${default_secret}"

  prepare_install_dir "${INSTALL_DIR}"
  copy_common_assets "${INSTALL_DIR}"
  copy_compose_template "all-in-one-host" "${INSTALL_DIR}"
  prepare_control_plane_layout "${INSTALL_DIR}"
  prepare_worker_layout "${INSTALL_DIR}"
  render_zlm_config \
    "${INSTALL_DIR}" \
    "http://127.0.0.1:${CORE_HTTP_PORT}/internal/hooks/zlm/${NODE_ID}" \
    "::1,127.0.0.1"
  write_all_in_one_host_env "${INSTALL_DIR}/.env"
  ensure_images_loaded postgres media-core media-agent zlmediakit
  log "all-in-one-host 说明: media-agent 和 ZLMediaKit 会直接占用宿主机端口 ${AGENT_HTTP_PORT}/${ZLM_HTTP_PORT}/${ZLM_RTMP_PORT}/${ZLM_RTSP_PORT}。"
  log "如果这些端口已被宿主机其他服务占用，请先释放端口，或改用非 host 模式。"
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
    worker-bridge) configure_worker_bridge ;;
    worker-host) configure_worker_host ;;
    all-in-one) configure_all_in_one ;;
    all-in-one-host) configure_all_in_one_host ;;
    *) fail "未知角色 ${role}" ;;
  esac
}

main "$@"
