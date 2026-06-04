#!/usr/bin/env bash
set -euo pipefail

PACKAGE_ROOT="$(cd "$(dirname "$0")" && pwd)"
MANIFEST_FILE="${PACKAGE_ROOT}/package-manifest.env"

CHECK_ONLY=0
START_AFTER_INSTALL=1
INSTALL_ROLE=""
INSTALL_DIR=""
INSTANCE_NAME=""
DATABASE_MODE=""
DATABASE_URL_INPUT=""
SERVICE_USER="${SERVICE_USER:-streamserver}"
SERVICE_GROUP="${SERVICE_GROUP:-streamserver}"
UNIT_BASENAME=""

log() {
  printf '[streamserver-native-install] %s\n' "$*"
}

fail() {
  printf '[streamserver-native-install] ERROR: %s\n' "$*" >&2
  exit 1
}

require_cmd() {
  command -v "$1" >/dev/null 2>&1 || fail "缺少命令: $1"
}

load_manifest() {
  [ -f "${MANIFEST_FILE}" ] || fail "缺少 ${MANIFEST_FILE}"
  # shellcheck disable=SC1090
  . "${MANIFEST_FILE}"
}

usage() {
  cat <<EOF
用法:
  ./install.sh [--check-only] [--role ROLE] [--install-dir DIR] [--instance-name NAME]
               [--database-url URL] [--no-start]

角色:
  control-plane
  worker-host-cpu
  worker-host-gpu
  all-in-one-host-cpu
  all-in-one-host-gpu

说明:
  Native 安装器不检查、不安装、不调用 Docker 或 Compose。
EOF
}

parse_args() {
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --check-only)
        CHECK_ONLY=1
        shift
        ;;
      --role)
        [ "$#" -ge 2 ] || fail "--role 需要参数"
        INSTALL_ROLE="$2"
        shift 2
        ;;
      --install-dir)
        [ "$#" -ge 2 ] || fail "--install-dir 需要参数"
        INSTALL_DIR="$2"
        shift 2
        ;;
      --instance-name)
        [ "$#" -ge 2 ] || fail "--instance-name 需要参数"
        INSTANCE_NAME="$2"
        shift 2
        ;;
      --database-url)
        [ "$#" -ge 2 ] || fail "--database-url 需要参数"
        DATABASE_URL_INPUT="$2"
        DATABASE_MODE="external"
        shift 2
        ;;
      --no-start)
        START_AFTER_INSTALL=0
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

ensure_linux_amd64() {
  [ "$(uname -s)" = "Linux" ] || fail "安装脚本只能在 Linux 上运行"
  case "$(uname -m)" in
    x86_64|amd64) ;;
    *) fail "目标主机必须是 Linux AMD64，当前架构为 $(uname -m)" ;;
  esac
}

ensure_prerequisites() {
  ensure_linux_amd64
  require_cmd tar
  require_cmd curl
  require_cmd openssl
  require_cmd systemctl
  require_cmd sed
  require_cmd awk
  if ! command -v sha256sum >/dev/null 2>&1; then
    fail "缺少 sha256sum"
  fi
  if [ ! -d /run/systemd/system ]; then
    fail "未检测到正在运行的 systemd，native 安装暂不支持该主机"
  fi
  if command -v docker >/dev/null 2>&1; then
    log "检测到 Docker，但 native 安装器不会调用 Docker。"
  else
    log "未检测到 Docker；native 运行时不依赖 Docker。"
  fi
}

verify_package_checksums() {
  [ -f "${PACKAGE_ROOT}/SHA256SUMS" ] || fail "缺少 SHA256SUMS"
  (cd "${PACKAGE_ROOT}" && sha256sum -c SHA256SUMS >/dev/null)
  log "包内 SHA256SUMS 校验通过"
}

assert_no_docker_assets() {
  # native 安装包必须是 Docker-free 运行时；这里防止历史离线包资产混入。
  if find "${PACKAGE_ROOT}" \( -path '*/images/*' -o -name compose.yml -o -name docker-compose.yml -o -name streamserver-compose \) | grep -q .; then
    fail "native 包中发现 Docker/Compose 运行时资产"
  fi
  [ ! -d "${PACKAGE_ROOT}/tools/docker" ] || fail "native 包中不得包含 tools/docker"
}

role_has_core() {
  case "$1" in
    control-plane|all-in-one-host-cpu|all-in-one-host-gpu) return 0 ;;
    *) return 1 ;;
  esac
}

role_has_worker() {
  case "$1" in
    worker-host-cpu|worker-host-gpu|all-in-one-host-cpu|all-in-one-host-gpu) return 0 ;;
    *) return 1 ;;
  esac
}

role_is_gpu() {
  case "$1" in
    worker-host-gpu|all-in-one-host-gpu) return 0 ;;
    *) return 1 ;;
  esac
}

validate_role_supported() {
  local role="$1"
  # 安装角色受包变体约束，CPU 包不能安装 GPU worker，minimal 包不能安装 worker。
  case "${role}" in
    control-plane)
      ;;
    worker-host-cpu|all-in-one-host-cpu)
      [ "${BUNDLE_WORKER_SUPPORT:-false}" = "true" ] || fail "当前包不包含 worker runtime"
      ;;
    worker-host-gpu|all-in-one-host-gpu)
      [ "${BUNDLE_WORKER_SUPPORT:-false}" = "true" ] || fail "当前包不包含 worker runtime"
      [ "${BUNDLE_GPU_SUPPORT:-false}" = "true" ] || fail "当前包不包含 GPU runtime"
      ;;
    *)
      fail "未知安装角色: ${role}"
      ;;
  esac
}

select_role() {
  local answer
  if [ -n "${INSTALL_ROLE}" ]; then
    validate_role_supported "${INSTALL_ROLE}"
    return 0
  fi
  echo "请选择安装角色:" >&2
  echo "  1) control-plane" >&2
  if [ "${BUNDLE_WORKER_SUPPORT:-false}" = "true" ]; then
    echo "  2) worker-host-cpu" >&2
    echo "  3) all-in-one-host-cpu" >&2
  fi
  if [ "${BUNDLE_GPU_SUPPORT:-false}" = "true" ]; then
    echo "  4) worker-host-gpu" >&2
    echo "  5) all-in-one-host-gpu" >&2
  fi
  while true; do
    answer="$(prompt "输入角色编号" "1")"
    case "${answer}" in
      1) INSTALL_ROLE="control-plane" ;;
      2) INSTALL_ROLE="worker-host-cpu" ;;
      3) INSTALL_ROLE="all-in-one-host-cpu" ;;
      4) INSTALL_ROLE="worker-host-gpu" ;;
      5) INSTALL_ROLE="all-in-one-host-gpu" ;;
      *) echo "请输入有效编号。" >&2; continue ;;
    esac
    if validate_role_supported "${INSTALL_ROLE}"; then
      return 0
    fi
  done
}

sanitize_instance_name() {
  printf '%s' "$1" | sed 's/[^A-Za-z0-9_.@-]/-/g; s/-\{2,\}/-/g; s/^-//; s/-$//'
}

default_instance_name() {
  case "$1" in
    control-plane) printf '%s' "ss-core" ;;
    worker-host-cpu) printf '%s' "ss-worker-cpu" ;;
    worker-host-gpu) printf '%s' "ss-worker-gpu" ;;
    all-in-one-host-cpu) printf '%s' "ss-aio-cpu" ;;
    all-in-one-host-gpu) printf '%s' "ss-aio-gpu" ;;
  esac
}

collect_basic_inputs() {
  local default_dir="/opt/streamserver/${INSTALL_ROLE}"
  [ -n "${INSTALL_DIR}" ] || INSTALL_DIR="$(prompt_non_empty "安装目录" "${default_dir}")"
  [ -n "${INSTANCE_NAME}" ] || INSTANCE_NAME="$(prompt_non_empty "实例名" "$(default_instance_name "${INSTALL_ROLE}")")"
  INSTANCE_NAME="$(sanitize_instance_name "${INSTANCE_NAME}")"
  [ -n "${INSTANCE_NAME}" ] || fail "实例名不能为空"
  # systemd unit 统一带 ss- 前缀，避免和系统已有服务名称直接冲突。
  case "${INSTANCE_NAME}" in
    ss-*) UNIT_BASENAME="${INSTANCE_NAME}" ;;
    *) UNIT_BASENAME="ss-${INSTANCE_NAME}" ;;
  esac
}

ensure_root_for_install() {
  [ "$(id -u)" -eq 0 ] || fail "安装 systemd 服务需要 root，请使用 root 执行 install.sh"
}

ensure_service_user() {
  if ! getent group "${SERVICE_GROUP}" >/dev/null 2>&1; then
    groupadd --system "${SERVICE_GROUP}"
  fi
  if ! id -u "${SERVICE_USER}" >/dev/null 2>&1; then
    useradd --system --gid "${SERVICE_GROUP}" --home-dir /nonexistent --shell /usr/sbin/nologin "${SERVICE_USER}"
  fi
}

generate_secret() {
  openssl rand -hex 24
}

generate_uuid() {
  if command -v uuidgen >/dev/null 2>&1; then
    uuidgen | tr '[:upper:]' '[:lower:]'
  else
    cat /proc/sys/kernel/random/uuid
  fi
}

detect_default_ip() {
  local ip_value=""
  if command -v hostname >/dev/null 2>&1; then
    ip_value="$(hostname -I 2>/dev/null | awk '{ print $1 }' || true)"
  fi
  if [ -z "${ip_value}" ] && command -v ip >/dev/null 2>&1; then
    ip_value="$(ip -4 route get 1.1.1.1 2>/dev/null | awk '{ for (i = 1; i <= NF; i++) if ($i == "src") { print $(i + 1); exit } }' || true)"
  fi
  printf '%s' "${ip_value}"
}

copy_file_atomically() {
  local source="$1"
  local target="$2"
  local temp
  # 二进制和脚本先写临时文件再 mv，避免安装中断时留下半写入目标。
  mkdir -p "$(dirname "${target}")"
  temp="$(mktemp "$(dirname "${target}")/.tmp.XXXXXX")"
  cp "${source}" "${temp}"
  chmod 755 "${temp}"
  mv -f "${temp}" "${target}"
}

install_binary() {
  local rel_var="$1"
  local target_name="$2"
  local rel="${!rel_var:-}"
  [ -n "${rel}" ] || fail "package-manifest.env 缺少 ${rel_var}"
  [ -f "${PACKAGE_ROOT}/${rel}" ] || fail "缺少二进制: ${rel}"
  copy_file_atomically "${PACKAGE_ROOT}/${rel}" "${INSTALL_DIR}/bin/${target_name}"
  log "已安装二进制: ${INSTALL_DIR}/bin/${target_name}"
}

install_tree() {
  local source="$1"
  local target="$2"
  [ -d "${source}" ] || fail "缺少目录: ${source}"
  rm -rf "${target}"
  mkdir -p "${target}"
  cp -R "${source}/." "${target}/"
}

write_runtime_wrapper() {
  local target="$1"
  local binary="$2"
  local lib_dir="$3"
  local python_home="${4:-}"
  # 三方 runtime 通过 wrapper 设置库路径，自研二进制仍直接放在 bin 下。
  local extra_library_path="${5:-}"
  local argv0_mode="${6:-wrapper}"
  local loader="${lib_dir}/ld-linux-x86-64.so.2"
  [ -x "${binary}" ] || fail "缺少 runtime 二进制: ${binary}"
  mkdir -p "$(dirname "${target}")"
  cat >"${target}" <<EOF
#!/usr/bin/env sh
set -eu
loader='${loader}'
lib_dir='${lib_dir}'
binary='${binary}'
python_home='${python_home}'
extra_library_path='${extra_library_path}'
argv0_mode='${argv0_mode}'
library_path="\${lib_dir}\${extra_library_path:+:\${extra_library_path}}"
if [ -n "\${python_home}" ] && [ -d "\${python_home}" ]; then
  PYTHONHOME="\${python_home}"
  export PYTHONHOME
fi
case "\${argv0_mode}" in
  binary) runtime_argv0="\${binary}" ;;
  wrapper) runtime_argv0="\$0" ;;
  *) runtime_argv0="\${argv0_mode}" ;;
esac
if [ -x "\${loader}" ]; then
  exec "\${loader}" --library-path "\${library_path}" --argv0 "\${runtime_argv0}" "\${binary}" "\$@"
fi
LD_LIBRARY_PATH="\${library_path}\${LD_LIBRARY_PATH:+:\${LD_LIBRARY_PATH}}"
export LD_LIBRARY_PATH
exec "\${binary}" "\$@"
EOF
  chmod 755 "${target}"
}

postgres_runtime_command_path() {
  local runtime_root="$1"
  local command_name="$2"
  local candidate
  for candidate in "${runtime_root}"/lib/postgresql/*/bin/"${command_name}"; do
    [ -x "${candidate}" ] || continue
    printf '%s\n' "${candidate}"
    return 0
  done
  printf '%s\n' "${runtime_root}/bin/${command_name}"
}

postgres_runtime_pkglib_dir() {
  local runtime_root="$1"
  local candidate
  for candidate in "${runtime_root}"/lib/postgresql/*/lib; do
    [ -d "${candidate}" ] || continue
    printf '%s\n' "${candidate}"
    return 0
  done
  printf '%s\n' "${runtime_root}/lib/postgresql"
}

postgres_runtime_share_dir() {
  local runtime_root="$1"
  local candidate
  for candidate in "${runtime_root}"/share/postgresql/* "${runtime_root}"/share/*; do
    [ -d "${candidate}/extension" ] || continue
    [ -f "${candidate}/postgres.bki" ] || continue
    printf '%s\n' "${candidate}"
    return 0
  done
  fail "缺少 PostgreSQL share 目录: ${runtime_root}/share"
}

postgres_runtime_commands() {
  local runtime_root="$1"
  {
    find "${runtime_root}/bin" -maxdepth 1 -type f -perm -111 -print 2>/dev/null || true
    find "${runtime_root}"/lib/postgresql/*/bin -maxdepth 1 -type f -perm -111 -print 2>/dev/null || true
  } | sed 's#.*/##' | LC_ALL=C sort -u
}

backup_existing_install() {
  [ -e "${INSTALL_DIR}/.env" ] || return 0
  local backup_dir="${INSTALL_DIR}.backup-$(date '+%Y%m%d-%H%M%S')"
  mkdir -p "${backup_dir}"
  for item in .env bin ui runtime zlm docs systemd uninstall.sh; do
    [ -e "${INSTALL_DIR}/${item}" ] || continue
    cp -R "${INSTALL_DIR}/${item}" "${backup_dir}/${item}"
  done
  log "已备份现有部署: ${backup_dir}"
}

prepare_layout() {
  backup_existing_install
  mkdir -p \
    "${INSTALL_DIR}/bin" \
    "${INSTALL_DIR}/runtime" \
    "${INSTALL_DIR}/ui" \
    "${INSTALL_DIR}/zlm" \
    "${INSTALL_DIR}/docs" \
    "${INSTALL_DIR}/certs/auth" \
    "${INSTALL_DIR}/data/media/work" \
    "${INSTALL_DIR}/data/media/logs" \
    "${INSTALL_DIR}/data/postgres-run" \
    "${INSTALL_DIR}/data/zlm/www/output/mp4" \
    "${INSTALL_DIR}/data/zlm/www/output/hls" \
    "${INSTALL_DIR}/data/zlm/www/record" \
    "${INSTALL_DIR}/data/zlm/www/snap"
}

copy_package_assets() {
  install_binary MEDIA_CORE_BINARY_PATH media-core
  install_binary STREAMSERVER_CONFIG_BINARY_PATH streamserver-config
  if role_has_worker "${INSTALL_ROLE}"; then
    install_binary MEDIA_AGENT_BINARY_PATH media-agent
    local ffmpeg_variant="cpu"
    role_is_gpu "${INSTALL_ROLE}" && ffmpeg_variant="gpu"
    install_tree "${PACKAGE_ROOT}/runtime/ffmpeg/${ffmpeg_variant}" "${INSTALL_DIR}/runtime/ffmpeg/${ffmpeg_variant}"
    install_tree "${PACKAGE_ROOT}/runtime/zlm" "${INSTALL_DIR}/runtime/zlm"
    install_tree "${PACKAGE_ROOT}/templates/common" "${INSTALL_DIR}/zlm"
    [ -f "${INSTALL_DIR}/zlm/zlm.render-config.sh" ] && mv "${INSTALL_DIR}/zlm/zlm.render-config.sh" "${INSTALL_DIR}/zlm/render-config.sh"
    [ -f "${INSTALL_DIR}/zlm/zlm.config.ini.template" ] && mv "${INSTALL_DIR}/zlm/zlm.config.ini.template" "${INSTALL_DIR}/zlm/config.ini.template"
    chmod +x "${INSTALL_DIR}/zlm/render-config.sh"
    write_runtime_wrapper "${INSTALL_DIR}/bin/ffmpeg" \
      "${INSTALL_DIR}/runtime/ffmpeg/${ffmpeg_variant}/bin/ffmpeg" \
      "${INSTALL_DIR}/runtime/ffmpeg/${ffmpeg_variant}/lib"
    write_runtime_wrapper "${INSTALL_DIR}/bin/ffprobe" \
      "${INSTALL_DIR}/runtime/ffmpeg/${ffmpeg_variant}/bin/ffprobe" \
      "${INSTALL_DIR}/runtime/ffmpeg/${ffmpeg_variant}/lib"
    write_runtime_wrapper "${INSTALL_DIR}/bin/zlm-mediaserver" \
      "${INSTALL_DIR}/runtime/zlm/MediaServer" \
      "${INSTALL_DIR}/runtime/zlm/lib" \
      "${INSTALL_DIR}/runtime/zlm/python"
  fi
  if role_has_core "${INSTALL_ROLE}" && [ "${DATABASE_MODE}" = "bundled" ]; then
    install_tree "${PACKAGE_ROOT}/${POSTGRES_RUNTIME_PATH}" "${INSTALL_DIR}/runtime/postgres"
    local postgres_command postgres_binary postgres_pkglib_dir postgres_argv0_mode
    postgres_pkglib_dir="$(postgres_runtime_pkglib_dir "${INSTALL_DIR}/runtime/postgres")"
    while IFS= read -r postgres_command; do
      [ -n "${postgres_command}" ] || continue
      postgres_binary="$(postgres_runtime_command_path "${INSTALL_DIR}/runtime/postgres" "${postgres_command}")"
      postgres_argv0_mode="wrapper"
      if [ "${postgres_command}" = "postgres" ]; then
        postgres_argv0_mode="binary"
      fi
      write_runtime_wrapper "${INSTALL_DIR}/bin/${postgres_command}" \
        "${postgres_binary}" \
        "${INSTALL_DIR}/runtime/postgres/lib" \
        "" \
        "${postgres_pkglib_dir}" \
        "${postgres_argv0_mode}"
    done < <(postgres_runtime_commands "${INSTALL_DIR}/runtime/postgres")
  fi
  if role_has_core "${INSTALL_ROLE}"; then
    install_tree "${PACKAGE_ROOT}/${MEDIA_CORE_UI_PATH}" "${INSTALL_DIR}/ui"
  fi
  if [ -d "${PACKAGE_ROOT}/docs" ]; then
    install_tree "${PACKAGE_ROOT}/docs" "${INSTALL_DIR}/docs"
  fi
}

install_uninstaller() {
  [ -f "${PACKAGE_ROOT}/uninstall.sh" ] || fail "缺少卸载脚本: ${PACKAGE_ROOT}/uninstall.sh"
  copy_file_atomically "${PACKAGE_ROOT}/uninstall.sh" "${INSTALL_DIR}/uninstall.sh"
  log "已安装卸载脚本: ${INSTALL_DIR}/uninstall.sh"
}

write_env_entry() {
  local file="$1"
  local key="$2"
  local value="$3"
  printf '%s=%s\n' "${key}" "${value}" >>"${file}"
}

write_env_common() {
  local env_file="$1"
  : >"${env_file}"
  write_env_entry "${env_file}" DEPLOY_MODE native
  write_env_entry "${env_file}" INSTALL_ROLE "${INSTALL_ROLE}"
  write_env_entry "${env_file}" INSTANCE_NAME "${INSTANCE_NAME}"
  write_env_entry "${env_file}" SYSTEMD_TARGET "${UNIT_BASENAME}.target"
  write_env_entry "${env_file}" SYSTEMD_CORE_UNIT "${UNIT_BASENAME}-core.service"
  write_env_entry "${env_file}" SYSTEMD_AGENT_UNIT "${UNIT_BASENAME}-agent.service"
  write_env_entry "${env_file}" SYSTEMD_ZLM_UNIT "${UNIT_BASENAME}-zlm.service"
  write_env_entry "${env_file}" SYSTEMD_POSTGRES_UNIT "${UNIT_BASENAME}-postgres.service"
}

configure_database() {
  POSTGRES_DB="${POSTGRES_DB:-streamserver}"
  POSTGRES_USER="${POSTGRES_USER:-postgres}"
  POSTGRES_PASSWORD="${POSTGRES_PASSWORD:-$(generate_secret)}"
  POSTGRES_PORT="${POSTGRES_PORT:-5432}"
  if ! role_has_core "${INSTALL_ROLE}"; then
    return 0
  fi
  if [ "${DATABASE_MODE}" = "external" ]; then
    DATABASE_URL="${DATABASE_URL_INPUT}"
    return 0
  fi
  if [ "${BUNDLE_POSTGRES_RUNTIME:-false}" = "true" ] && prompt_yes_no "是否使用包内 PostgreSQL runtime？选择 N 则输入外部 DATABASE_URL" "Y"; then
    DATABASE_MODE="bundled"
    DATABASE_URL="postgresql://${POSTGRES_USER}:${POSTGRES_PASSWORD}@127.0.0.1:${POSTGRES_PORT}/${POSTGRES_DB}"
  else
    DATABASE_MODE="external"
    DATABASE_URL="$(prompt_non_empty "外部 DATABASE_URL" "${DATABASE_URL_INPUT}")"
  fi
}

configure_core_values() {
  CORE_HTTP_PORT="${CORE_HTTP_PORT:-8080}"
  CORE_GRPC_PORT="${CORE_GRPC_PORT:-50051}"
  HOOK_SHARED_SECRET="${HOOK_SHARED_SECRET:-$(generate_secret)}"
  HOOK_SOURCE_ALLOWLIST="${HOOK_SOURCE_ALLOWLIST:-}"
  STORAGE_ALLOWLIST="${STORAGE_ALLOWLIST:-${INSTALL_DIR}/data/media/work,${INSTALL_DIR}/data/zlm/www}"
  AUTH_MODE="${AUTH_MODE:-disabled}"
  AUTH_ENABLED="false"
  JWT_PUBLIC_KEY=""
  AUTH_JWT_PRIVATE_KEY_PATH=""
  AUTH_JWT_PUBLIC_KEY_PATH=""
  AUTH_ACCESS_TOKEN_TTL="${AUTH_ACCESS_TOKEN_TTL:-15m}"
  AUTH_REFRESH_TOKEN_TTL="${AUTH_REFRESH_TOKEN_TTL:-7d}"
  ADMIN_USERNAME=""
  ADMIN_PASSWORD=""
  if role_has_core "${INSTALL_ROLE}" && prompt_yes_no "是否启用本地用户名密码鉴权？" "N"; then
    AUTH_MODE="local_password"
    AUTH_ENABLED="true"
    ADMIN_USERNAME="$(prompt_non_empty "管理员用户名" "admin")"
    ADMIN_PASSWORD="$(prompt_secret "管理员密码")"
    [ -n "${ADMIN_PASSWORD}" ] || fail "管理员密码不能为空"
    openssl genpkey -algorithm Ed25519 -out "${INSTALL_DIR}/certs/auth/jwt-ed25519-private.pem" >/dev/null 2>&1
    openssl pkey -in "${INSTALL_DIR}/certs/auth/jwt-ed25519-private.pem" -pubout -out "${INSTALL_DIR}/certs/auth/jwt-ed25519-public.pem" >/dev/null 2>&1
    chmod 600 "${INSTALL_DIR}/certs/auth/jwt-ed25519-private.pem"
    chmod 644 "${INSTALL_DIR}/certs/auth/jwt-ed25519-public.pem"
    AUTH_JWT_PRIVATE_KEY_PATH="${INSTALL_DIR}/certs/auth/jwt-ed25519-private.pem"
    AUTH_JWT_PUBLIC_KEY_PATH="${INSTALL_DIR}/certs/auth/jwt-ed25519-public.pem"
  fi
}

configure_worker_values() {
  local default_ip
  default_ip="$(detect_default_ip)"
  [ -n "${default_ip}" ] || default_ip="127.0.0.1"
  NODE_ID="${NODE_ID:-$(generate_uuid)}"
  AGENT_NODE_NAME="${AGENT_NODE_NAME:-$(hostname -s 2>/dev/null || echo streamserver-node)}"
  PUBLIC_HOST="${PUBLIC_HOST:-${default_ip}}"
  CORE_HTTP_HOST="${CORE_HTTP_HOST:-${default_ip}}"
  CORE_GRPC_HOST="${CORE_GRPC_HOST:-${CORE_HTTP_HOST}}"
  if role_has_core "${INSTALL_ROLE}"; then
    CORE_HTTP_HOST="127.0.0.1"
    CORE_GRPC_HOST="127.0.0.1"
  fi
  if [ -z "${HOOK_SHARED_SECRET:-}" ]; then
    HOOK_SHARED_SECRET="$(prompt_non_empty "ZLM Hook/API 密钥（需与 control-plane 一致）" "")"
  fi
  AGENT_HTTP_PORT="${AGENT_HTTP_PORT:-8081}"
  ZLM_HTTP_PORT="${ZLM_HTTP_PORT:-80}"
  ZLM_HTTPS_PORT="${ZLM_HTTPS_PORT:-0}"
  ZLM_RTMP_PORT="${ZLM_RTMP_PORT:-1935}"
  ZLM_RTMPS_PORT="${ZLM_RTMPS_PORT:-0}"
  ZLM_RTSP_PORT="${ZLM_RTSP_PORT:-554}"
  ZLM_RTSPS_PORT="${ZLM_RTSPS_PORT:-0}"
  ZLM_RTP_PROXY_PORT="${ZLM_RTP_PROXY_PORT:-10000}"
  ZLM_RTP_PROXY_PORT_RANGE="${ZLM_RTP_PROXY_PORT_RANGE:-30000-30500}"
  ZLM_RTC_SIGNALING_PORT="${ZLM_RTC_SIGNALING_PORT:-8000}"
  ZLM_RTC_SIGNALING_SSL_PORT="${ZLM_RTC_SIGNALING_SSL_PORT:-0}"
  ZLM_RTC_ICE_PORT="${ZLM_RTC_ICE_PORT:-0}"
  ZLM_RTC_ICE_TCP_PORT="${ZLM_RTC_ICE_TCP_PORT:-0}"
  ZLM_RTC_PORT="${ZLM_RTC_PORT:-0}"
  ZLM_RTC_TCP_PORT="${ZLM_RTC_TCP_PORT:-0}"
  ZLM_RTC_PORT_RANGE="${ZLM_RTC_PORT_RANGE:-0-0}"
  ZLM_SRT_PORT="${ZLM_SRT_PORT:-0}"
  ZLM_SHELL_PORT="${ZLM_SHELL_PORT:-0}"
  ZLM_ONVIF_PORT="${ZLM_ONVIF_PORT:-0}"
  AGENT_PRIMARY_INTERFACE_NAME="${AGENT_PRIMARY_INTERFACE_NAME:-}"
  AGENT_PRIMARY_INTERFACE_IP="${AGENT_PRIMARY_INTERFACE_IP:-${default_ip}}"
  AGENT_MULTICAST_INTERFACE_NAME="${AGENT_MULTICAST_INTERFACE_NAME:-${AGENT_PRIMARY_INTERFACE_NAME}}"
  AGENT_MULTICAST_INTERFACE_IP="${AGENT_MULTICAST_INTERFACE_IP:-${AGENT_PRIMARY_INTERFACE_IP}}"
  AGENT_NETWORK_MODE="host"
  AGENT_ACCELERATION_MODE="cpu"
  AGENT_LABELS="cpu"
  if role_is_gpu "${INSTALL_ROLE}"; then
    AGENT_ACCELERATION_MODE="gpu"
    AGENT_LABELS="gpu"
  fi
  AGENT_MAX_RUNTIME_SLOTS="${AGENT_MAX_RUNTIME_SLOTS:-0}"
  AGENT_RUNTIME_MANAGER_START_LIMIT="${AGENT_RUNTIME_MANAGER_START_LIMIT:-8}"
  AGENT_RUNTIME_MANAGER_STOP_LIMIT="${AGENT_RUNTIME_MANAGER_STOP_LIMIT:-16}"
  AGENT_RUNTIME_MANAGER_RECORDING_LIMIT="${AGENT_RUNTIME_MANAGER_RECORDING_LIMIT:-12}"
  AGENT_RUNTIME_MANAGER_ADOPT_LIMIT="${AGENT_RUNTIME_MANAGER_ADOPT_LIMIT:-1}"
  AGENT_HLS_RECORD_SEGMENT_SEC="${AGENT_HLS_RECORD_SEGMENT_SEC:-60}"
  AGENT_ARTIFACT_CLEANUP_ENABLED="${AGENT_ARTIFACT_CLEANUP_ENABLED:-true}"
  AGENT_ARTIFACT_CLEANUP_THRESHOLD_PERCENT="${AGENT_ARTIFACT_CLEANUP_THRESHOLD_PERCENT:-85}"
  AGENT_ARTIFACT_CLEANUP_STRATEGY="${AGENT_ARTIFACT_CLEANUP_STRATEGY:-delete_oldest_then_reject}"
  AGENT_ARTIFACT_CLEANUP_CHECK_INTERVAL_SEC="${AGENT_ARTIFACT_CLEANUP_CHECK_INTERVAL_SEC:-30}"
  WORK_ROOT="${INSTALL_DIR}/data/media/work"
  UPLOAD_MAX_BYTES="${UPLOAD_MAX_BYTES:-10737418240}"
  UPLOAD_ALLOWED_EXTENSIONS="${UPLOAD_ALLOWED_EXTENSIONS:-mp4,mov,m4v,mkv,webm,ts,m2ts,mts,flv}"
  UPLOAD_PROBE_TIMEOUT_SEC="${UPLOAD_PROBE_TIMEOUT_SEC:-30}"
  PUBLIC_MEDIA_BASE_URL="${PUBLIC_MEDIA_BASE_URL:-}"
}

write_env_file() {
  local env_file="${INSTALL_DIR}/.env"
  local ffmpeg_variant="cpu"
  role_is_gpu "${INSTALL_ROLE}" && ffmpeg_variant="gpu"
  write_env_common "${env_file}"
  if role_has_core "${INSTALL_ROLE}"; then
    write_env_entry "${env_file}" POSTGRES_DB "${POSTGRES_DB}"
    write_env_entry "${env_file}" POSTGRES_USER "${POSTGRES_USER}"
    write_env_entry "${env_file}" POSTGRES_PASSWORD "${POSTGRES_PASSWORD}"
    write_env_entry "${env_file}" POSTGRES_PORT "${POSTGRES_PORT}"
    write_env_entry "${env_file}" DATABASE_URL "${DATABASE_URL}"
    write_env_entry "${env_file}" CORE_HTTP_ADDR "0.0.0.0:${CORE_HTTP_PORT}"
    write_env_entry "${env_file}" CORE_HTTP_PORT "${CORE_HTTP_PORT}"
    write_env_entry "${env_file}" CORE_GRPC_ADDR "0.0.0.0:${CORE_GRPC_PORT}"
    write_env_entry "${env_file}" CORE_GRPC_PORT "${CORE_GRPC_PORT}"
    write_env_entry "${env_file}" STREAMSERVER_UI_DIR "${INSTALL_DIR}/ui"
    write_env_entry "${env_file}" HOOK_SHARED_SECRET "${HOOK_SHARED_SECRET}"
    write_env_entry "${env_file}" HOOK_SOURCE_ALLOWLIST "${HOOK_SOURCE_ALLOWLIST}"
    write_env_entry "${env_file}" STORAGE_ALLOWLIST "${STORAGE_ALLOWLIST}"
    write_env_entry "${env_file}" AUTH_MODE "${AUTH_MODE}"
    write_env_entry "${env_file}" AUTH_ENABLED "${AUTH_ENABLED}"
    write_env_entry "${env_file}" JWT_PUBLIC_KEY "${JWT_PUBLIC_KEY}"
    write_env_entry "${env_file}" AUTH_JWT_PRIVATE_KEY_PATH "${AUTH_JWT_PRIVATE_KEY_PATH}"
    write_env_entry "${env_file}" AUTH_JWT_PUBLIC_KEY_PATH "${AUTH_JWT_PUBLIC_KEY_PATH}"
    write_env_entry "${env_file}" AUTH_ACCESS_TOKEN_TTL "${AUTH_ACCESS_TOKEN_TTL}"
    write_env_entry "${env_file}" AUTH_REFRESH_TOKEN_TTL "${AUTH_REFRESH_TOKEN_TTL}"
  fi
  if role_has_worker "${INSTALL_ROLE}"; then
    write_env_entry "${env_file}" NODE_ID "${NODE_ID}"
    write_env_entry "${env_file}" AGENT_NODE_ID "${NODE_ID}"
    write_env_entry "${env_file}" AGENT_NODE_NAME "${AGENT_NODE_NAME}"
    write_env_entry "${env_file}" CORE_HTTP_HOST "${CORE_HTTP_HOST}"
    write_env_entry "${env_file}" CORE_HTTP_PORT "${CORE_HTTP_PORT:-8080}"
    write_env_entry "${env_file}" CORE_GRPC_HOST "${CORE_GRPC_HOST}"
    write_env_entry "${env_file}" CORE_GRPC_PORT "${CORE_GRPC_PORT:-50051}"
    write_env_entry "${env_file}" AGENT_CORE_ENDPOINT "http://${CORE_GRPC_HOST}:${CORE_GRPC_PORT:-50051}"
    write_env_entry "${env_file}" PUBLIC_HOST "${PUBLIC_HOST}"
    write_env_entry "${env_file}" AGENT_STREAM_ADDR "http://${PUBLIC_HOST}:${ZLM_HTTP_PORT}"
    write_env_entry "${env_file}" AGENT_HTTP_ADDR "0.0.0.0:${AGENT_HTTP_PORT}"
    write_env_entry "${env_file}" AGENT_HTTP_PORT "${AGENT_HTTP_PORT}"
    write_env_entry "${env_file}" HOOK_SHARED_SECRET "${HOOK_SHARED_SECRET}"
    write_env_entry "${env_file}" ZLM_API_HOST "127.0.0.1"
    write_env_entry "${env_file}" ZLM_API_BASE "http://127.0.0.1:${ZLM_HTTP_PORT}"
    write_env_entry "${env_file}" ZLM_API_SECRET "${HOOK_SHARED_SECRET:-${ZLM_API_SECRET:-}}"
    write_env_entry "${env_file}" ZLM_API_ALLOW_IP_RANGE "::1,127.0.0.1,10.0.0.0-10.255.255.255,172.16.0.0-172.31.255.255,192.168.0.0-192.168.255.255"
    write_env_entry "${env_file}" ZLM_HOOK_SHARED_SECRET "${HOOK_SHARED_SECRET:-${ZLM_API_SECRET:-}}"
    write_env_entry "${env_file}" ZLM_SERVER_ID "${NODE_ID}"
    write_env_entry "${env_file}" ZLM_HOOK_BASE "http://${CORE_HTTP_HOST}:${CORE_HTTP_PORT:-8080}/internal/hooks/zlm/${NODE_ID}"
    write_env_entry "${env_file}" ZLM_HTTP_PORT "${ZLM_HTTP_PORT}"
    write_env_entry "${env_file}" ZLM_HTTPS_PORT "${ZLM_HTTPS_PORT}"
    write_env_entry "${env_file}" ZLM_RTMP_PORT "${ZLM_RTMP_PORT}"
    write_env_entry "${env_file}" ZLM_RTMPS_PORT "${ZLM_RTMPS_PORT}"
    write_env_entry "${env_file}" ZLM_RTSP_PORT "${ZLM_RTSP_PORT}"
    write_env_entry "${env_file}" ZLM_RTSPS_PORT "${ZLM_RTSPS_PORT}"
    write_env_entry "${env_file}" ZLM_RTP_PROXY_PORT "${ZLM_RTP_PROXY_PORT}"
    write_env_entry "${env_file}" ZLM_RTP_PROXY_PORT_RANGE "${ZLM_RTP_PROXY_PORT_RANGE}"
    write_env_entry "${env_file}" ZLM_RTC_SIGNALING_PORT "${ZLM_RTC_SIGNALING_PORT}"
    write_env_entry "${env_file}" ZLM_RTC_SIGNALING_SSL_PORT "${ZLM_RTC_SIGNALING_SSL_PORT}"
    write_env_entry "${env_file}" ZLM_RTC_ICE_PORT "${ZLM_RTC_ICE_PORT}"
    write_env_entry "${env_file}" ZLM_RTC_ICE_TCP_PORT "${ZLM_RTC_ICE_TCP_PORT}"
    write_env_entry "${env_file}" ZLM_RTC_PORT "${ZLM_RTC_PORT}"
    write_env_entry "${env_file}" ZLM_RTC_TCP_PORT "${ZLM_RTC_TCP_PORT}"
    write_env_entry "${env_file}" ZLM_RTC_PORT_RANGE "${ZLM_RTC_PORT_RANGE}"
    write_env_entry "${env_file}" ZLM_SRT_PORT "${ZLM_SRT_PORT}"
    write_env_entry "${env_file}" ZLM_SHELL_PORT "${ZLM_SHELL_PORT}"
    write_env_entry "${env_file}" ZLM_ONVIF_PORT "${ZLM_ONVIF_PORT}"
    write_env_entry "${env_file}" ZLM_WWW_ROOT "${INSTALL_DIR}/data/zlm/www"
    write_env_entry "${env_file}" ZLM_RECORD_ROOT "${INSTALL_DIR}/data/zlm/www/record"
    write_env_entry "${env_file}" ZLM_SNAP_ROOT "${INSTALL_DIR}/data/zlm/www/snap"
    write_env_entry "${env_file}" ZLM_DEFAULT_PEM "${INSTALL_DIR}/runtime/zlm/default.pem"
    write_env_entry "${env_file}" FFMPEG_BIN "${INSTALL_DIR}/bin/ffmpeg"
    write_env_entry "${env_file}" FFPROBE_BIN "${INSTALL_DIR}/bin/ffprobe"
    write_env_entry "${env_file}" ZLM_OUTPUT_MP4_ROOT "${INSTALL_DIR}/data/zlm/www/output/mp4"
    write_env_entry "${env_file}" ZLM_OUTPUT_HLS_ROOT "${INSTALL_DIR}/data/zlm/www/output/hls"
    write_env_entry "${env_file}" AGENT_PRIMARY_INTERFACE_NAME "${AGENT_PRIMARY_INTERFACE_NAME}"
    write_env_entry "${env_file}" AGENT_PRIMARY_INTERFACE_IP "${AGENT_PRIMARY_INTERFACE_IP}"
    write_env_entry "${env_file}" AGENT_MULTICAST_INTERFACE_NAME "${AGENT_MULTICAST_INTERFACE_NAME}"
    write_env_entry "${env_file}" AGENT_MULTICAST_INTERFACE_IP "${AGENT_MULTICAST_INTERFACE_IP}"
    write_env_entry "${env_file}" AGENT_NETWORK_MODE "${AGENT_NETWORK_MODE}"
    write_env_entry "${env_file}" AGENT_ACCELERATION_MODE "${AGENT_ACCELERATION_MODE}"
    write_env_entry "${env_file}" AGENT_LABELS "${AGENT_LABELS}"
    write_env_entry "${env_file}" AGENT_MAX_RUNTIME_SLOTS "${AGENT_MAX_RUNTIME_SLOTS}"
    write_env_entry "${env_file}" AGENT_RUNTIME_MANAGER_START_LIMIT "${AGENT_RUNTIME_MANAGER_START_LIMIT}"
    write_env_entry "${env_file}" AGENT_RUNTIME_MANAGER_STOP_LIMIT "${AGENT_RUNTIME_MANAGER_STOP_LIMIT}"
    write_env_entry "${env_file}" AGENT_RUNTIME_MANAGER_RECORDING_LIMIT "${AGENT_RUNTIME_MANAGER_RECORDING_LIMIT}"
    write_env_entry "${env_file}" AGENT_RUNTIME_MANAGER_ADOPT_LIMIT "${AGENT_RUNTIME_MANAGER_ADOPT_LIMIT}"
    write_env_entry "${env_file}" AGENT_HLS_RECORD_SEGMENT_SEC "${AGENT_HLS_RECORD_SEGMENT_SEC}"
    write_env_entry "${env_file}" AGENT_ARTIFACT_CLEANUP_ENABLED "${AGENT_ARTIFACT_CLEANUP_ENABLED}"
    write_env_entry "${env_file}" AGENT_ARTIFACT_CLEANUP_THRESHOLD_PERCENT "${AGENT_ARTIFACT_CLEANUP_THRESHOLD_PERCENT}"
    write_env_entry "${env_file}" AGENT_ARTIFACT_CLEANUP_STRATEGY "${AGENT_ARTIFACT_CLEANUP_STRATEGY}"
    write_env_entry "${env_file}" AGENT_ARTIFACT_CLEANUP_CHECK_INTERVAL_SEC "${AGENT_ARTIFACT_CLEANUP_CHECK_INTERVAL_SEC}"
    write_env_entry "${env_file}" WORK_ROOT "${WORK_ROOT}"
    write_env_entry "${env_file}" UPLOAD_MAX_BYTES "${UPLOAD_MAX_BYTES}"
    write_env_entry "${env_file}" UPLOAD_ALLOWED_EXTENSIONS "${UPLOAD_ALLOWED_EXTENSIONS}"
    write_env_entry "${env_file}" UPLOAD_PROBE_TIMEOUT_SEC "${UPLOAD_PROBE_TIMEOUT_SEC}"
    write_env_entry "${env_file}" PUBLIC_MEDIA_BASE_URL "${PUBLIC_MEDIA_BASE_URL}"
    write_env_entry "${env_file}" OUTPUT_MOUNT_RELATIVE_PREFIX_MP4 output
    write_env_entry "${env_file}" OUTPUT_MOUNT_RELATIVE_PREFIX_HLS output
    write_env_entry "${env_file}" ZLM_AUTO_CLOSE_ON_NO_READER_ENABLED true
    write_env_entry "${env_file}" AGENT_ALLOW_ENHANCED_RTMP_EXPOSE true
  fi
  chmod 600 "${env_file}"
}

render_template() {
  local source="$1"
  local target="$2"
  local ffmpeg_variant="cpu"
  local postgres_unit="" postgres_requires="" core_unit="" core_requires="" zlm_unit=""
  local postgres_pkglib_dir=""
  local gpu_nvidia_pre="" gpu_h264_pre="" gpu_hevc_pre=""
  role_is_gpu "${INSTALL_ROLE}" && ffmpeg_variant="gpu"
  role_has_core "${INSTALL_ROLE}" && core_unit="${UNIT_BASENAME}-core.service"
  role_has_worker "${INSTALL_ROLE}" && zlm_unit="${UNIT_BASENAME}-zlm.service"
  if role_has_core "${INSTALL_ROLE}" && [ "${DATABASE_MODE}" = "bundled" ]; then
    postgres_unit="${UNIT_BASENAME}-postgres.service"
    postgres_requires="Requires=${postgres_unit}"
    postgres_pkglib_dir="$(postgres_runtime_pkglib_dir "${INSTALL_DIR}/runtime/postgres")"
  fi
  if role_has_worker "${INSTALL_ROLE}" && role_has_core "${INSTALL_ROLE}"; then
    core_requires="Requires=${UNIT_BASENAME}-core.service"
  fi
  if role_is_gpu "${INSTALL_ROLE}"; then
    gpu_nvidia_pre="ExecStartPre=/usr/bin/nvidia-smi"
    gpu_h264_pre="ExecStartPre=/bin/sh -c '${INSTALL_DIR}/bin/ffmpeg -hide_banner -encoders 2>/dev/null | grep -q h264_nvenc && ${INSTALL_DIR}/bin/ffmpeg -v error -hide_banner -nostdin -f lavfi -i testsrc2=size=640x360:rate=15 -t 1 -c:v h264_nvenc -an -f null -'"
    gpu_hevc_pre="ExecStartPre=/bin/sh -c '${INSTALL_DIR}/bin/ffmpeg -hide_banner -encoders 2>/dev/null | grep -q hevc_nvenc && ${INSTALL_DIR}/bin/ffmpeg -v error -hide_banner -nostdin -f lavfi -i testsrc2=size=640x360:rate=15 -t 1 -c:v hevc_nvenc -an -f null -'"
  fi
  sed_escape() {
    printf '%s' "$1" | sed 's/[&|]/\\&/g'
  }
  sed \
    -e "s|__INSTANCE_NAME__|$(sed_escape "${INSTANCE_NAME}")|g" \
    -e "s|__INSTALL_DIR__|$(sed_escape "${INSTALL_DIR}")|g" \
    -e "s|__SERVICE_USER__|$(sed_escape "${SERVICE_USER}")|g" \
    -e "s|__SERVICE_GROUP__|$(sed_escape "${SERVICE_GROUP}")|g" \
    -e "s|__TARGET_UNIT__|$(sed_escape "${UNIT_BASENAME}.target")|g" \
    -e "s|__POSTGRES_UNIT__|$(sed_escape "${postgres_unit}")|g" \
    -e "s|__POSTGRES_REQUIRES__|$(sed_escape "${postgres_requires}")|g" \
    -e "s|__POSTGRES_PKGLIB_DIR__|$(sed_escape "${postgres_pkglib_dir}")|g" \
    -e "s|__CORE_UNIT__|$(sed_escape "${core_unit}")|g" \
    -e "s|__CORE_REQUIRES__|$(sed_escape "${core_requires}")|g" \
    -e "s|__ZLM_UNIT__|$(sed_escape "${zlm_unit}")|g" \
    -e "s|__FFMPEG_VARIANT__|$(sed_escape "${ffmpeg_variant}")|g" \
    -e "s|__GPU_NVIDIA_SMI_PRE__|$(sed_escape "${gpu_nvidia_pre}")|g" \
    -e "s|__GPU_H264_PRE__|$(sed_escape "${gpu_h264_pre}")|g" \
    -e "s|__GPU_HEVC_PRE__|$(sed_escape "${gpu_hevc_pre}")|g" \
    "${source}" >"${target}"
}

install_systemd_units() {
  local units=()
  local rendered_dir="${INSTALL_DIR}/systemd"
  mkdir -p "${rendered_dir}"
  render_template "${PACKAGE_ROOT}/templates/systemd/streamserver.target" "${rendered_dir}/${UNIT_BASENAME}.target"
  cp "${rendered_dir}/${UNIT_BASENAME}.target" "/etc/systemd/system/${UNIT_BASENAME}.target"
  if role_has_core "${INSTALL_ROLE}" && [ "${DATABASE_MODE}" = "bundled" ]; then
    render_template "${PACKAGE_ROOT}/templates/systemd/streamserver-postgres.service" "${rendered_dir}/${UNIT_BASENAME}-postgres.service"
    cp "${rendered_dir}/${UNIT_BASENAME}-postgres.service" "/etc/systemd/system/${UNIT_BASENAME}-postgres.service"
    units+=("${UNIT_BASENAME}-postgres.service")
  fi
  if role_has_core "${INSTALL_ROLE}"; then
    render_template "${PACKAGE_ROOT}/templates/systemd/streamserver-core.service" "${rendered_dir}/${UNIT_BASENAME}-core.service"
    cp "${rendered_dir}/${UNIT_BASENAME}-core.service" "/etc/systemd/system/${UNIT_BASENAME}-core.service"
    units+=("${UNIT_BASENAME}-core.service")
  fi
  if role_has_worker "${INSTALL_ROLE}"; then
    render_template "${PACKAGE_ROOT}/templates/systemd/streamserver-zlm.service" "${rendered_dir}/${UNIT_BASENAME}-zlm.service"
    render_template "${PACKAGE_ROOT}/templates/systemd/streamserver-agent.service" "${rendered_dir}/${UNIT_BASENAME}-agent.service"
    cp "${rendered_dir}/${UNIT_BASENAME}-zlm.service" "/etc/systemd/system/${UNIT_BASENAME}-zlm.service"
    cp "${rendered_dir}/${UNIT_BASENAME}-agent.service" "/etc/systemd/system/${UNIT_BASENAME}-agent.service"
    units+=("${UNIT_BASENAME}-zlm.service" "${UNIT_BASENAME}-agent.service")
  fi
  systemctl daemon-reload
  systemctl enable "${UNIT_BASENAME}.target" "${units[@]}" >/dev/null
}

run_as_service_user() {
  if command -v runuser >/dev/null 2>&1; then
    runuser -u "${SERVICE_USER}" -- "$@"
  else
    su -s /bin/sh "${SERVICE_USER}" -c "$(printf '%q ' "$@")"
  fi
}

initialize_postgres_if_needed() {
  [ "${DATABASE_MODE}" = "bundled" ] || return 0
  local data_dir="${INSTALL_DIR}/data/postgres"
  local pwfile="${INSTALL_DIR}/.postgres-pw"
  mkdir -p "${data_dir}"
  chown -R "${SERVICE_USER}:${SERVICE_GROUP}" "${INSTALL_DIR}/data" "${INSTALL_DIR}/runtime/postgres"
  if [ ! -f "${data_dir}/PG_VERSION" ]; then
    printf '%s\n' "${POSTGRES_PASSWORD}" >"${pwfile}"
    chown "${SERVICE_USER}:${SERVICE_GROUP}" "${pwfile}"
    chmod 600 "${pwfile}"
    run_as_service_user "${INSTALL_DIR}/bin/initdb" \
      -D "${data_dir}" \
      -U "${POSTGRES_USER}" \
      -L "$(postgres_runtime_share_dir "${INSTALL_DIR}/runtime/postgres")" \
      --pwfile="${pwfile}" \
      --encoding=UTF8 \
      --locale=C
    rm -f "${pwfile}"
  fi
}

wait_for_postgres() {
  [ "${DATABASE_MODE}" = "bundled" ] || return 0
  local i
  for i in $(seq 1 60); do
    if PGPASSWORD="${POSTGRES_PASSWORD}" "${INSTALL_DIR}/bin/pg_isready" -h 127.0.0.1 -p "${POSTGRES_PORT}" -U "${POSTGRES_USER}" >/dev/null 2>&1; then
      return 0
    fi
    sleep 1
  done
  fail "PostgreSQL 未在预期时间内就绪"
}

ensure_database_exists() {
  [ "${DATABASE_MODE}" = "bundled" ] || return 0
  local escaped_db_name
  escaped_db_name="$(printf '%s' "${POSTGRES_DB}" | sed "s/'/''/g")"
  if PGPASSWORD="${POSTGRES_PASSWORD}" "${INSTALL_DIR}/bin/psql" \
    -h 127.0.0.1 -p "${POSTGRES_PORT}" -U "${POSTGRES_USER}" -d postgres \
    -tAc "SELECT 1 FROM pg_database WHERE datname = '${escaped_db_name}'" \
    | grep -qx '1'; then
    return 0
  fi
  PGPASSWORD="${POSTGRES_PASSWORD}" "${INSTALL_DIR}/bin/createdb" \
    -h 127.0.0.1 -p "${POSTGRES_PORT}" -U "${POSTGRES_USER}" \
    "${POSTGRES_DB}"
}

bootstrap_local_admin_if_needed() {
  [ "${AUTH_MODE}" = "local_password" ] || return 0
  log "初始化本地管理员账号: ${ADMIN_USERNAME}"
  (
    set -a
    # shellcheck disable=SC1091
    . "${INSTALL_DIR}/.env"
    set +a
    printf '%s' "${ADMIN_PASSWORD}" | "${INSTALL_DIR}/bin/media-core" auth bootstrap-admin --username "${ADMIN_USERNAME}" --password-stdin
  )
}

write_streamserverctl() {
  local ctl="${INSTALL_DIR}/bin/streamserverctl"
  cat >"${ctl}" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
INSTALL_DIR="$(cd "$(dirname "$0")/.." && pwd)"
. "${INSTALL_DIR}/.env"

units_for_role() {
  case "${INSTALL_ROLE}" in
    control-plane)
      systemctl list-unit-files "${SYSTEMD_POSTGRES_UNIT}" >/dev/null 2>&1 && printf '%s\n' "${SYSTEMD_POSTGRES_UNIT}"
      printf '%s\n' "${SYSTEMD_CORE_UNIT}"
      ;;
    worker-host-cpu|worker-host-gpu)
      printf '%s\n%s\n' "${SYSTEMD_ZLM_UNIT}" "${SYSTEMD_AGENT_UNIT}"
      ;;
    all-in-one-host-cpu|all-in-one-host-gpu)
      systemctl list-unit-files "${SYSTEMD_POSTGRES_UNIT}" >/dev/null 2>&1 && printf '%s\n' "${SYSTEMD_POSTGRES_UNIT}"
      printf '%s\n%s\n%s\n' "${SYSTEMD_CORE_UNIT}" "${SYSTEMD_ZLM_UNIT}" "${SYSTEMD_AGENT_UNIT}"
      ;;
  esac
}

read_units() {
  mapfile -t STREAMSERVER_UNITS < <(units_for_role)
}

health() {
  if [ -n "${CORE_HTTP_PORT:-}" ] && systemctl list-unit-files "${SYSTEMD_CORE_UNIT:-missing}" >/dev/null 2>&1; then
    curl -fsS "http://127.0.0.1:${CORE_HTTP_PORT}/health/ready" >/dev/null && echo "[OK] media-core"
  fi
  if [ -n "${AGENT_HTTP_PORT:-}" ] && systemctl list-unit-files "${SYSTEMD_AGENT_UNIT:-missing}" >/dev/null 2>&1; then
    curl -fsS "http://127.0.0.1:${AGENT_HTTP_PORT}/health/ready" >/dev/null && echo "[OK] media-agent"
  fi
  if [ -n "${ZLM_HTTP_PORT:-}" ] && systemctl list-unit-files "${SYSTEMD_ZLM_UNIT:-missing}" >/dev/null 2>&1; then
    curl -fsS "http://127.0.0.1:${ZLM_HTTP_PORT}/index/api/getStatistic?secret=${HOOK_SHARED_SECRET}" >/dev/null && echo "[OK] zlmediakit"
  fi
}

case "${1:-status}" in
  start) systemctl start "${SYSTEMD_TARGET}" ;;
  stop) read_units; systemctl stop "${STREAMSERVER_UNITS[@]}" "${SYSTEMD_TARGET}" ;;
  restart) read_units; systemctl restart "${STREAMSERVER_UNITS[@]}" ;;
  status) read_units; systemctl status "${SYSTEMD_TARGET}" "${STREAMSERVER_UNITS[@]}" --no-pager ;;
  logs)
    read_units
    journal_args=()
    for unit in "${STREAMSERVER_UNITS[@]}"; do
      journal_args+=(-u "${unit}")
    done
    journalctl "${journal_args[@]}" -f
    ;;
  health|doctor) health ;;
  *) echo "usage: streamserverctl {start|stop|restart|status|logs|health|doctor}" >&2; exit 2 ;;
esac
EOF
  chmod 755 "${ctl}"
}

fix_permissions() {
  chown -R "${SERVICE_USER}:${SERVICE_GROUP}" "${INSTALL_DIR}"
  chmod 755 "${INSTALL_DIR}" "${INSTALL_DIR}/bin"
}

start_services_if_requested() {
  [ "${START_AFTER_INSTALL}" -eq 1 ] || return 0
  if [ "${DATABASE_MODE}" = "bundled" ]; then
    systemctl start "${UNIT_BASENAME}-postgres.service"
    wait_for_postgres
    ensure_database_exists
    bootstrap_local_admin_if_needed
  elif [ "${AUTH_MODE:-disabled}" = "local_password" ]; then
    bootstrap_local_admin_if_needed
  fi
  systemctl start "${UNIT_BASENAME}.target"
  log "已启动 native 服务。"
  log "状态: ${INSTALL_DIR}/bin/streamserverctl status"
  log "健康检查: ${INSTALL_DIR}/bin/streamserverctl health"
}

main() {
  parse_args "$@"
  load_manifest
  ensure_prerequisites
  verify_package_checksums
  assert_no_docker_assets
  if [ "${CHECK_ONLY}" -eq 1 ]; then
    log "check-only 通过。"
    exit 0
  fi
  ensure_root_for_install
  select_role
  collect_basic_inputs
  configure_database
  prepare_layout
  copy_package_assets
  configure_core_values
  if role_has_worker "${INSTALL_ROLE}"; then
    configure_worker_values
  fi
  write_env_file
  write_streamserverctl
  install_uninstaller
  ensure_service_user
  initialize_postgres_if_needed
  fix_permissions
  install_systemd_units
  start_services_if_requested
  log "安装完成: ${INSTALL_DIR}"
  log "卸载: ${INSTALL_DIR}/uninstall.sh"
}

main "$@"
