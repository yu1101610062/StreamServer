#!/usr/bin/env bash
set -euo pipefail

DOCKER_ROOT="/home/streamserver"
INSTALL_DIR="/home/streamserver"
BACKUP_BASE="/home/bh/桌面"
INSTANCE_NAME="streamserver-native"
BUNDLE_PATH=""
EXECUTE=0
SKIP_DB_RESTORE=0
ALLOW_WORKER_BEFORE_CORE=0

usage() {
  cat <<'EOF'
Usage:
  migrate-docker-to-native-field.sh --bundle /path/to/streamserver-native-*.tar.gz [--execute]

Options:
  --bundle PATH              Native CPU deployment tar.gz.
  --docker-root PATH         Existing Docker deployment root. Default: /home/streamserver
  --install-dir PATH         Native install root. Default: /home/streamserver
  --backup-base PATH         Backup/report parent. Default: /home/bh/桌面
  --instance-name NAME       Native systemd instance name. Default: streamserver-native
  --skip-db-restore          Install all-in-one without restoring Docker DB dump.
  --allow-worker-before-core Allow worker migration when upgraded core API is not detected.
  --execute                  Actually stop Docker, install native, restore DB, and start services.
  -h, --help                 Show this help.

Default mode is dry-run. Add --execute only after reviewing the printed actions.

Recommended order:
  1. Run on 172.16.1.9 all-in-one host first.
  2. Verify core is healthy.
  3. Run on 172.16.1.10 worker host.
EOF
}

log() {
  printf '[docker-to-native] %s\n' "$*"
}

warn() {
  printf '[docker-to-native] WARN: %s\n' "$*" >&2
}

fail() {
  printf '[docker-to-native] ERROR: %s\n' "$*" >&2
  exit 1
}

print_cmd() {
  printf '+'
  local arg
  for arg in "$@"; do
    printf ' %q' "${arg}"
  done
  printf '\n'
}

run() {
  print_cmd "$@"
  if [ "${EXECUTE}" -eq 1 ]; then
    "$@"
  fi
}

run_shell() {
  printf '+ %s\n' "$*"
  if [ "${EXECUTE}" -eq 1 ]; then
    bash -c "$*"
  fi
}

run_sensitive_shell() {
  local label="$1"
  local command="$2"
  printf '+ %s\n' "${label}"
  if [ "${EXECUTE}" -eq 1 ]; then
    bash -c "${command}"
  fi
}

patch_extracted_installer_for_data_preservation() {
  if [ "${EXECUTE}" -ne 1 ]; then
    log "would patch extracted installer to avoid recursive chown of preserved data"
    return 0
  fi

  local installer="${PACKAGE_DIR}/install.sh"
  [ -f "${installer}" ] || fail "missing installer: ${installer}"

  if grep -Fq 'chown -R "${SERVICE_USER}:${SERVICE_GROUP}" "${INSTALL_DIR}/data" "${INSTALL_DIR}/runtime/postgres"' "${installer}"; then
    sed -i \
      's|chown -R "${SERVICE_USER}:${SERVICE_GROUP}" "${INSTALL_DIR}/data" "${INSTALL_DIR}/runtime/postgres"|chown -R "${SERVICE_USER}:${SERVICE_GROUP}" "${data_dir}" "${INSTALL_DIR}/data/postgres-run" "${INSTALL_DIR}/runtime/postgres"|' \
      "${installer}"
  elif grep -Fq 'chown -R "${SERVICE_USER}:${SERVICE_GROUP}" "${data_dir}" "${INSTALL_DIR}/data/postgres-run" "${INSTALL_DIR}/runtime/postgres"' "${installer}"; then
    :
  else
    fail "installer initialize_postgres_if_needed chown pattern is unknown"
  fi

  if ! grep -Fq 'mkdir -p "${INSTALL_DIR}/runtime/zlm/lib/log"' "${installer}"; then
    sed -i \
      '/install_tree "${PACKAGE_ROOT}\/runtime\/zlm" "${INSTALL_DIR}\/runtime\/zlm"/a\    mkdir -p "${INSTALL_DIR}/runtime/zlm/lib/log"' \
      "${installer}"
  fi

  sed -i \
    -e '/"${INSTALL_DIR}\/data\/zlm\/www\/output\/mp4" \\/d' \
    -e '/"${INSTALL_DIR}\/data\/zlm\/www\/output\/hls" \\/d' \
    "${installer}"

  if ! grep -Fq '检测到 output 目录是挂载点' "${installer}"; then
    local patched_output_layout
    patched_output_layout="$(mktemp)"
    awk '
      /^prepare_layout\(\) \{/ { in_prepare = 1 }
      {
        print
        if (in_prepare && $0 ~ /\$\{INSTALL_DIR\}\/data\/zlm\/www\/snap"/) {
          print "  local output_root=\"${INSTALL_DIR}/data/zlm/www/output\""
          print "  if grep -F \" ${output_root} \" /proc/self/mountinfo >/dev/null 2>&1; then"
          print "    log \"检测到 output 目录是挂载点，跳过创建 output/mp4 和 output/hls: ${output_root}\""
          print "  else"
          print "    mkdir -p \"${output_root}/mp4\" \"${output_root}/hls\""
          print "  fi"
        }
        if (in_prepare && $0 == "}") { in_prepare = 0 }
      }
    ' "${installer}" >"${patched_output_layout}"
    cat "${patched_output_layout}" >"${installer}"
    rm -f "${patched_output_layout}"
    chmod +x "${installer}"
  fi

  if grep -Fq 'chown -R "${SERVICE_USER}:${SERVICE_GROUP}" "${INSTALL_DIR}"' "${installer}"; then
    local patched
    patched="$(mktemp)"
    awk '
      {
        if ($0 == "  chown -R \"${SERVICE_USER}:${SERVICE_GROUP}\" \"${INSTALL_DIR}\"") {
          print "  chown \"${SERVICE_USER}:${SERVICE_GROUP}\" \"${INSTALL_DIR}\""
          print "  for item in bin runtime ui zlm docs certs systemd uninstall.sh .env; do"
          print "    [ -e \"${INSTALL_DIR}/${item}\" ] && chown -R \"${SERVICE_USER}:${SERVICE_GROUP}\" \"${INSTALL_DIR}/${item}\""
          print "  done"
          print "  for item in data data/media data/media/work data/media/logs data/postgres data/postgres-run data/zlm data/zlm/www data/zlm/www/record data/zlm/www/snap; do"
          print "    [ -e \"${INSTALL_DIR}/${item}\" ] && chown \"${SERVICE_USER}:${SERVICE_GROUP}\" \"${INSTALL_DIR}/${item}\""
          print "  done"
          next
        }
        print
      }
    ' "${installer}" >"${patched}"
    cat "${patched}" >"${installer}"
    rm -f "${patched}"
    chmod +x "${installer}"
  elif grep -Fq 'for item in bin runtime ui zlm docs certs systemd uninstall.sh .env; do' "${installer}"; then
    :
  else
    fail "installer fix_permissions chown pattern is unknown"
  fi

  sed -i \
    's|data data/media data/media/work data/media/logs data/postgres data/postgres-run data/zlm data/zlm/www data/zlm/www/output data/zlm/www/output/mp4 data/zlm/www/output/hls data/zlm/www/record data/zlm/www/snap|data data/media data/media/work data/media/logs data/postgres data/postgres-run data/zlm data/zlm/www data/zlm/www/record data/zlm/www/snap|g' \
    "${installer}"

  (
    cd "${PACKAGE_DIR}"
    find . -type f ! -name SHA256SUMS -print | LC_ALL=C sort | while read -r file; do
      sha256sum "${file#./}"
    done >SHA256SUMS
  )
}

parse_args() {
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --bundle)
        [ "$#" -ge 2 ] || fail "--bundle requires PATH"
        BUNDLE_PATH="$2"
        shift 2
        ;;
      --docker-root)
        [ "$#" -ge 2 ] || fail "--docker-root requires PATH"
        DOCKER_ROOT="$2"
        shift 2
        ;;
      --install-dir)
        [ "$#" -ge 2 ] || fail "--install-dir requires PATH"
        INSTALL_DIR="$2"
        shift 2
        ;;
      --backup-base)
        [ "$#" -ge 2 ] || fail "--backup-base requires PATH"
        BACKUP_BASE="$2"
        shift 2
        ;;
      --instance-name)
        [ "$#" -ge 2 ] || fail "--instance-name requires NAME"
        INSTANCE_NAME="$2"
        shift 2
        ;;
      --skip-db-restore)
        SKIP_DB_RESTORE=1
        shift
        ;;
      --allow-worker-before-core)
        ALLOW_WORKER_BEFORE_CORE=1
        shift
        ;;
      --execute)
        EXECUTE=1
        shift
        ;;
      -h|--help)
        usage
        exit 0
        ;;
      *)
        fail "unknown argument: $1"
        ;;
    esac
  done
}

require_root_when_execute() {
  if [ "${EXECUTE}" -eq 1 ] && [ "$(id -u)" -ne 0 ]; then
    fail "execute mode must run as root"
  fi
}

require_cmd() {
  command -v "$1" >/dev/null 2>&1 || fail "missing command: $1"
}

load_docker_env() {
  local env_file="${DOCKER_ROOT}/.env"
  [ -f "${env_file}" ] || fail "missing Docker env file: ${env_file}"
  set -a
  # shellcheck disable=SC1090
  . "${env_file}"
  set +a

  [ -n "${INSTALL_ROLE:-}" ] || fail "INSTALL_ROLE is missing in ${env_file}"
  case "${INSTALL_ROLE}" in
    all-in-one-host-cpu|worker-host-cpu) ;;
    *)
      fail "this field script only supports CPU roles, got INSTALL_ROLE=${INSTALL_ROLE}"
      ;;
  esac

  if role_has_core && [ -z "${POSTGRES_PASSWORD:-}" ]; then
    fail "POSTGRES_PASSWORD is missing in ${env_file}; cannot restore DB into native safely"
  fi
  if role_has_worker && [ -z "${NODE_ID:-}" ]; then
    fail "NODE_ID is missing in ${env_file}; refusing to create a new node identity"
  fi
  if role_has_worker && [ -z "${PUBLIC_HOST:-}" ]; then
    fail "PUBLIC_HOST is missing in ${env_file}; stream URLs would be wrong"
  fi
}

role_has_core() {
  case "${INSTALL_ROLE}" in
    all-in-one-host-cpu) return 0 ;;
    *) return 1 ;;
  esac
}

role_has_worker() {
  case "${INSTALL_ROLE}" in
    all-in-one-host-cpu|worker-host-cpu) return 0 ;;
    *) return 1 ;;
  esac
}

unit_prefix() {
  case "${INSTANCE_NAME}" in
    ss-*) printf '%s' "${INSTANCE_NAME}" ;;
    *) printf 'ss-%s' "${INSTANCE_NAME}" ;;
  esac
}

container_name() {
  local service="$1"
  if [ -n "${COMPOSE_PROJECT_NAME:-}" ]; then
    printf '%s-%s-1' "${COMPOSE_PROJECT_NAME}" "${service}"
  else
    printf '%s' "${service}"
  fi
}

ensure_bundle() {
  [ -n "${BUNDLE_PATH}" ] || fail "--bundle is required"
  [ -f "${BUNDLE_PATH}" ] || fail "bundle not found: ${BUNDLE_PATH}"
}

ensure_backup_base() {
  if [ ! -d "${BACKUP_BASE}" ]; then
    warn "backup base does not exist: ${BACKUP_BASE}; using /root"
    BACKUP_BASE="/root"
  fi
}

copy_field_config_to_backup() {
  run mkdir -p "${BACKUP_DIR}"
  if [ -f "${DOCKER_ROOT}/.env" ]; then
    run cp -a "${DOCKER_ROOT}/.env" "${BACKUP_DIR}/docker.env"
  fi
  if [ -f "${DOCKER_ROOT}/compose.yml" ]; then
    run cp -a "${DOCKER_ROOT}/compose.yml" "${BACKUP_DIR}/compose.yml"
  fi
  if [ -f "${DOCKER_ROOT}/docker-compose.yml" ]; then
    run cp -a "${DOCKER_ROOT}/docker-compose.yml" "${BACKUP_DIR}/docker-compose.yml"
  fi
}

dump_docker_database_if_needed() {
  DUMP_FILE=""
  if ! role_has_core || [ "${SKIP_DB_RESTORE}" -eq 1 ]; then
    return 0
  fi

  local pg_container
  pg_container="$(container_name postgres)"
  local dump_name="streamserver_pre_native_${HOSTNAME_SHORT}_${TS}.dump"
  DUMP_FILE="${BACKUP_DIR}/${dump_name}"

  run docker exec "${pg_container}" pg_dump -U "${POSTGRES_USER:-postgres}" -d "${POSTGRES_DB:-streamserver}" -Fc -f "/tmp/${dump_name}"
  run docker cp "${pg_container}:/tmp/${dump_name}" "${DUMP_FILE}"
  run docker exec "${pg_container}" rm -f "/tmp/${dump_name}"
}

stop_existing_native_if_any() {
  local prefix
  prefix="$(unit_prefix)"
  run_shell "systemctl stop '${prefix}.target' '${prefix}-agent.service' '${prefix}-zlm.service' '${prefix}-core.service' '${prefix}-postgres.service' 2>/dev/null || true"
}

stop_docker_app_containers_for_db_dump() {
  role_has_core || return 0

  local services=(media-agent zlmediakit media-core)
  local service name
  for service in "${services[@]}"; do
    name="$(container_name "${service}")"
    run_shell "docker ps -a --format '{{.Names}}' | grep -qx '${name}' && docker stop '${name}' || true"
  done
}

stop_docker_containers() {
  local services=()
  if role_has_core; then
    services=(postgres)
  else
    services=(media-agent zlmediakit)
  fi

  local service name
  for service in "${services[@]}"; do
    name="$(container_name "${service}")"
    run_shell "docker ps -a --format '{{.Names}}' | grep -qx '${name}' && docker stop '${name}' || true"
  done
}

extract_bundle() {
  EXTRACT_DIR="${BACKUP_DIR}/bundle"
  run mkdir -p "${EXTRACT_DIR}"
  run tar -xzf "${BUNDLE_PATH}" -C "${EXTRACT_DIR}"
  if [ "${EXECUTE}" -eq 1 ]; then
    PACKAGE_DIR="$(find "${EXTRACT_DIR}" -mindepth 1 -maxdepth 1 -type d -name 'streamserver-native-*' | sort | head -n 1)"
    [ -n "${PACKAGE_DIR}" ] || fail "could not find extracted streamserver-native-* directory"
    [ -x "${PACKAGE_DIR}/install.sh" ] || fail "install.sh is missing or not executable in ${PACKAGE_DIR}"
  else
    PACKAGE_DIR="${EXTRACT_DIR}/streamserver-native-..."
  fi
  patch_extracted_installer_for_data_preservation
}

prepare_in_place_data_tree() {
  [ "${INSTALL_DIR}" = "${DOCKER_ROOT}" ] || return 0
  role_has_core || return 0

  local postgres_dir="${DOCKER_ROOT}/data/postgres"
  DOCKER_POSTGRES_DATA_BACKUP="${DOCKER_ROOT}/data/postgres.docker-pre-native-${TS}"

  if [ -e "${DOCKER_POSTGRES_DATA_BACKUP}" ]; then
    fail "target Docker postgres backup path already exists: ${DOCKER_POSTGRES_DATA_BACKUP}"
  fi

  if [ -e "${postgres_dir}" ]; then
    run mv "${postgres_dir}" "${DOCKER_POSTGRES_DATA_BACKUP}"
  fi
  run mkdir -p "${postgres_dir}"
}

run_native_installer() {
  local install_env=(
    "SERVICE_USER=streamserver"
    "SERVICE_GROUP=streamserver"
    "POSTGRES_DB=${POSTGRES_DB:-streamserver}"
    "POSTGRES_USER=${POSTGRES_USER:-postgres}"
    "POSTGRES_PASSWORD=${POSTGRES_PASSWORD:-}"
    "POSTGRES_PORT=${POSTGRES_PORT:-5432}"
    "CORE_HTTP_PORT=${CORE_HTTP_PORT:-8080}"
    "CORE_GRPC_PORT=${CORE_GRPC_PORT:-50051}"
    "CORE_HTTP_HOST=${CORE_HTTP_HOST:-127.0.0.1}"
    "CORE_GRPC_HOST=${CORE_GRPC_HOST:-${CORE_HTTP_HOST:-127.0.0.1}}"
    "NODE_ID=${NODE_ID:-}"
    "AGENT_NODE_NAME=${AGENT_NODE_NAME:-}"
    "PUBLIC_HOST=${PUBLIC_HOST:-}"
    "HOOK_SHARED_SECRET=${HOOK_SHARED_SECRET:-${ZLM_API_SECRET:-}}"
    "AUTH_MODE=${AUTH_MODE:-disabled}"
  )

  if role_has_worker && [ -z "${HOOK_SHARED_SECRET:-${ZLM_API_SECRET:-}}" ]; then
    fail "HOOK_SHARED_SECRET/ZLM_API_SECRET is empty in Docker env; native worker cannot be configured safely"
  fi

  printf '+ run native installer with inherited field env (secrets hidden)\n'
  if [ "${EXECUTE}" -eq 1 ]; then
    printf '\n\n\n\n' | env "${install_env[@]}" "${PACKAGE_DIR}/install.sh" \
      --role "${INSTALL_ROLE}" \
      --install-dir "${INSTALL_DIR}" \
      --instance-name "${INSTANCE_NAME}" \
      --no-start
  fi
}

patch_runtime_unit_users() {
  if [ "${EXECUTE}" -ne 1 ]; then
    log "would run core/agent/zlm as root and keep postgres as streamserver"
    return 0
  fi

  local prefix
  prefix="$(unit_prefix)"

  local units=()
  role_has_core && units+=("/etc/systemd/system/${prefix}-core.service")
  if role_has_worker; then
    units+=("/etc/systemd/system/${prefix}-agent.service" "/etc/systemd/system/${prefix}-zlm.service")
  fi

  if [ "${#units[@]}" -gt 0 ]; then
    sed -i 's/^User=streamserver$/User=root/; s/^Group=streamserver$/Group=root/' "${units[@]}"
  fi

  if role_has_worker; then
    rm -f "${INSTALL_DIR}/zlm/config.ini"
    chmod 755 "${INSTALL_DIR}/zlm" "${INSTALL_DIR}/zlm/render-config.sh"
    mkdir -p "${INSTALL_DIR}/runtime/zlm/lib/log"
    chmod 777 "${INSTALL_DIR}/runtime/zlm/lib/log"
  fi

  systemctl daemon-reload
}

set_env_value() {
  local file="$1"
  local key="$2"
  local value="$3"
  [ -f "${file}" ] || fail "missing native env file: ${file}"
  if printf '%s' "${value}" | grep -q '[[:cntrl:]]'; then
    fail "env value for ${key} contains control characters"
  fi
  sed -i "/^${key}=/d" "${file}"
  printf '%s=%s\n' "${key}" "${value}" >>"${file}"
}

delete_env_value() {
  local file="$1"
  local key="$2"
  [ -f "${file}" ] || fail "missing native env file: ${file}"
  sed -i "/^${key}=/d" "${file}"
}

patch_native_env() {
  if [ "${EXECUTE}" -ne 1 ]; then
    log "would patch ${INSTALL_DIR}/.env to use old host data paths and new live/vod slot keys"
    return 0
  fi

  local env_file="${INSTALL_DIR}/.env"
  local live_slots="${AGENT_MAX_LIVE_RUNTIME_SLOTS:-${AGENT_MAX_RUNTIME_SLOTS:-0}}"
  local vod_slots="${AGENT_MAX_VOD_RUNTIME_SLOTS:-${AGENT_MAX_RUNTIME_SLOTS:-0}}"
  local core_http_host="${CORE_HTTP_HOST:-127.0.0.1}"
  local core_grpc_host="${CORE_GRPC_HOST:-${core_http_host}}"
  if role_has_core; then
    core_http_host="127.0.0.1"
    core_grpc_host="127.0.0.1"
  fi

  delete_env_value "${env_file}" AGENT_MAX_RUNTIME_SLOTS
  set_env_value "${env_file}" SERVICE_USER root
  set_env_value "${env_file}" SERVICE_GROUP root

  if role_has_core; then
    set_env_value "${env_file}" POSTGRES_DB "${POSTGRES_DB:-streamserver}"
    set_env_value "${env_file}" POSTGRES_USER "${POSTGRES_USER:-postgres}"
    set_env_value "${env_file}" POSTGRES_PASSWORD "${POSTGRES_PASSWORD:-}"
    set_env_value "${env_file}" POSTGRES_PORT "${POSTGRES_PORT:-5432}"
    set_env_value "${env_file}" DATABASE_URL "postgresql://${POSTGRES_USER:-postgres}:${POSTGRES_PASSWORD:-}@127.0.0.1:${POSTGRES_PORT:-5432}/${POSTGRES_DB:-streamserver}"
    set_env_value "${env_file}" CORE_HTTP_ADDR "0.0.0.0:${CORE_HTTP_PORT:-8080}"
    set_env_value "${env_file}" CORE_GRPC_ADDR "0.0.0.0:${CORE_GRPC_PORT:-50051}"
    set_env_value "${env_file}" STREAMSERVER_UI_DIR "${INSTALL_DIR}/ui"
    set_env_value "${env_file}" HOOK_SHARED_SECRET "${HOOK_SHARED_SECRET:-${ZLM_API_SECRET:-}}"
    set_env_value "${env_file}" HOOK_SOURCE_ALLOWLIST "${HOOK_SOURCE_ALLOWLIST:-}"
    set_env_value "${env_file}" STORAGE_ALLOWLIST "${DOCKER_ROOT}/data/media/work,${DOCKER_ROOT}/data/zlm/www"
    set_env_value "${env_file}" AUTH_MODE "${AUTH_MODE:-disabled}"
    set_env_value "${env_file}" AUTH_ENABLED "${AUTH_ENABLED:-false}"
    set_env_value "${env_file}" AUTH_ACCESS_TOKEN_TTL "${AUTH_ACCESS_TOKEN_TTL:-15m}"
    set_env_value "${env_file}" AUTH_REFRESH_TOKEN_TTL "${AUTH_REFRESH_TOKEN_TTL:-7d}"
  fi

  if role_has_worker; then
    set_env_value "${env_file}" NODE_ID "${NODE_ID:-}"
    set_env_value "${env_file}" AGENT_NODE_ID "${NODE_ID:-}"
    set_env_value "${env_file}" AGENT_NODE_NAME "${AGENT_NODE_NAME:-$(hostname -s 2>/dev/null || echo streamserver-node)}"
    set_env_value "${env_file}" CORE_HTTP_HOST "${core_http_host}"
    set_env_value "${env_file}" CORE_HTTP_PORT "${CORE_HTTP_PORT:-8080}"
    set_env_value "${env_file}" CORE_GRPC_HOST "${core_grpc_host}"
    set_env_value "${env_file}" CORE_GRPC_PORT "${CORE_GRPC_PORT:-50051}"
    set_env_value "${env_file}" AGENT_CORE_ENDPOINT "http://${core_grpc_host}:${CORE_GRPC_PORT:-50051}"
    set_env_value "${env_file}" PUBLIC_HOST "${PUBLIC_HOST:-}"
    set_env_value "${env_file}" AGENT_STREAM_ADDR "http://${PUBLIC_HOST:-127.0.0.1}:${ZLM_HTTP_PORT:-80}"
    set_env_value "${env_file}" AGENT_HTTP_ADDR "0.0.0.0:${AGENT_HTTP_PORT:-8081}"
    set_env_value "${env_file}" AGENT_HTTP_PORT "${AGENT_HTTP_PORT:-8081}"
    set_env_value "${env_file}" HOOK_SHARED_SECRET "${HOOK_SHARED_SECRET:-${ZLM_API_SECRET:-}}"
    set_env_value "${env_file}" ZLM_API_HOST "127.0.0.1"
    set_env_value "${env_file}" ZLM_API_BASE "http://127.0.0.1:${ZLM_HTTP_PORT:-80}"
    set_env_value "${env_file}" ZLM_API_SECRET "${HOOK_SHARED_SECRET:-${ZLM_API_SECRET:-}}"
    set_env_value "${env_file}" ZLM_API_ALLOW_IP_RANGE "${ZLM_API_ALLOW_IP_RANGE:-::1,127.0.0.1,10.0.0.0-10.255.255.255,172.16.0.0-172.31.255.255,192.168.0.0-192.168.255.255}"
    set_env_value "${env_file}" ZLM_HOOK_SHARED_SECRET "${HOOK_SHARED_SECRET:-${ZLM_API_SECRET:-}}"
    set_env_value "${env_file}" ZLM_SERVER_ID "${NODE_ID:-}"
    set_env_value "${env_file}" ZLM_HOOK_BASE "http://${core_http_host}:${CORE_HTTP_PORT:-8080}/internal/hooks/zlm/${NODE_ID:-}"
    set_env_value "${env_file}" ZLM_HTTP_PORT "${ZLM_HTTP_PORT:-80}"
    set_env_value "${env_file}" ZLM_HTTPS_PORT "${ZLM_HTTPS_PORT:-0}"
    set_env_value "${env_file}" ZLM_RTMP_PORT "${ZLM_RTMP_PORT:-1935}"
    set_env_value "${env_file}" ZLM_RTMPS_PORT "${ZLM_RTMPS_PORT:-0}"
    set_env_value "${env_file}" ZLM_RTSP_PORT "${ZLM_RTSP_PORT:-554}"
    set_env_value "${env_file}" ZLM_RTSPS_PORT "${ZLM_RTSPS_PORT:-0}"
    set_env_value "${env_file}" ZLM_RTP_PROXY_PORT "${ZLM_RTP_PROXY_PORT:-0}"
    set_env_value "${env_file}" ZLM_RTP_PROXY_PORT_RANGE "${ZLM_RTP_PROXY_PORT_RANGE:-0-0}"
    set_env_value "${env_file}" ZLM_RTC_SIGNALING_PORT "${ZLM_RTC_SIGNALING_PORT:-0}"
    set_env_value "${env_file}" ZLM_RTC_SIGNALING_SSL_PORT "${ZLM_RTC_SIGNALING_SSL_PORT:-0}"
    set_env_value "${env_file}" ZLM_RTC_ICE_PORT "${ZLM_RTC_ICE_PORT:-0}"
    set_env_value "${env_file}" ZLM_RTC_ICE_TCP_PORT "${ZLM_RTC_ICE_TCP_PORT:-0}"
    set_env_value "${env_file}" ZLM_RTC_PORT "${ZLM_RTC_PORT:-0}"
    set_env_value "${env_file}" ZLM_RTC_TCP_PORT "${ZLM_RTC_TCP_PORT:-0}"
    set_env_value "${env_file}" ZLM_RTC_PORT_RANGE "${ZLM_RTC_PORT_RANGE:-0-0}"
    set_env_value "${env_file}" ZLM_SRT_PORT "${ZLM_SRT_PORT:-0}"
    set_env_value "${env_file}" ZLM_SHELL_PORT "${ZLM_SHELL_PORT:-0}"
    set_env_value "${env_file}" ZLM_ONVIF_PORT "${ZLM_ONVIF_PORT:-0}"
    set_env_value "${env_file}" ZLM_WWW_ROOT "${DOCKER_ROOT}/data/zlm/www"
    set_env_value "${env_file}" ZLM_RECORD_ROOT "${DOCKER_ROOT}/data/zlm/www/record"
    set_env_value "${env_file}" ZLM_SNAP_ROOT "${DOCKER_ROOT}/data/zlm/www/snap"
    set_env_value "${env_file}" ZLM_DEFAULT_PEM "${INSTALL_DIR}/runtime/zlm/default.pem"
    set_env_value "${env_file}" FFMPEG_BIN "${INSTALL_DIR}/bin/ffmpeg"
    set_env_value "${env_file}" FFPROBE_BIN "${INSTALL_DIR}/bin/ffprobe"
    set_env_value "${env_file}" ZLM_OUTPUT_MP4_ROOT "${DOCKER_ROOT}/data/zlm/www/output/mp4"
    set_env_value "${env_file}" ZLM_OUTPUT_HLS_ROOT "${DOCKER_ROOT}/data/zlm/www/output/hls"
    set_env_value "${env_file}" AGENT_PRIMARY_INTERFACE_NAME "${AGENT_PRIMARY_INTERFACE_NAME:-}"
    set_env_value "${env_file}" AGENT_PRIMARY_INTERFACE_IP "${AGENT_PRIMARY_INTERFACE_IP:-${PUBLIC_HOST:-}}"
    set_env_value "${env_file}" AGENT_MULTICAST_INTERFACE_NAME "${AGENT_MULTICAST_INTERFACE_NAME:-${AGENT_PRIMARY_INTERFACE_NAME:-}}"
    set_env_value "${env_file}" AGENT_MULTICAST_INTERFACE_IP "${AGENT_MULTICAST_INTERFACE_IP:-${AGENT_PRIMARY_INTERFACE_IP:-${PUBLIC_HOST:-}}}"
    set_env_value "${env_file}" AGENT_NETWORK_MODE "${AGENT_NETWORK_MODE:-host}"
    set_env_value "${env_file}" AGENT_ACCELERATION_MODE "${AGENT_ACCELERATION_MODE:-cpu}"
    set_env_value "${env_file}" AGENT_LABELS "${AGENT_LABELS:-cpu}"
    set_env_value "${env_file}" AGENT_MAX_LIVE_RUNTIME_SLOTS "${live_slots}"
    set_env_value "${env_file}" AGENT_MAX_VOD_RUNTIME_SLOTS "${vod_slots}"
    set_env_value "${env_file}" AGENT_RUNTIME_MANAGER_START_LIMIT "${AGENT_RUNTIME_MANAGER_START_LIMIT:-8}"
    set_env_value "${env_file}" AGENT_RUNTIME_MANAGER_STOP_LIMIT "${AGENT_RUNTIME_MANAGER_STOP_LIMIT:-16}"
    set_env_value "${env_file}" AGENT_RUNTIME_MANAGER_RECORDING_LIMIT "${AGENT_RUNTIME_MANAGER_RECORDING_LIMIT:-12}"
    set_env_value "${env_file}" AGENT_RUNTIME_MANAGER_ADOPT_LIMIT "${AGENT_RUNTIME_MANAGER_ADOPT_LIMIT:-1}"
    set_env_value "${env_file}" AGENT_RUNTIME_LOG_TAIL_BYTES "${AGENT_RUNTIME_LOG_TAIL_BYTES:-8192}"
    set_env_value "${env_file}" AGENT_RUNTIME_LOG_MAX_FILE_BYTES "${AGENT_RUNTIME_LOG_MAX_FILE_BYTES:-134217728}"
    set_env_value "${env_file}" AGENT_RUNTIME_LOG_RETENTION_DAYS "${AGENT_RUNTIME_LOG_RETENTION_DAYS:-7}"
    set_env_value "${env_file}" AGENT_MP4_RECORD_SEGMENT_SEC "${AGENT_MP4_RECORD_SEGMENT_SEC:-7200}"
    set_env_value "${env_file}" AGENT_HLS_RECORD_SEGMENT_SEC "${AGENT_HLS_RECORD_SEGMENT_SEC:-60}"
    set_env_value "${env_file}" AGENT_ARTIFACT_CLEANUP_ENABLED "${AGENT_ARTIFACT_CLEANUP_ENABLED:-true}"
    set_env_value "${env_file}" AGENT_ARTIFACT_CLEANUP_THRESHOLD_PERCENT "${AGENT_ARTIFACT_CLEANUP_THRESHOLD_PERCENT:-85}"
    set_env_value "${env_file}" AGENT_ARTIFACT_CLEANUP_STRATEGY "${AGENT_ARTIFACT_CLEANUP_STRATEGY:-delete_oldest_then_reject}"
    set_env_value "${env_file}" AGENT_ARTIFACT_CLEANUP_CHECK_INTERVAL_SEC "${AGENT_ARTIFACT_CLEANUP_CHECK_INTERVAL_SEC:-30}"
    set_env_value "${env_file}" WORK_ROOT "${DOCKER_ROOT}/data/media/work"
    set_env_value "${env_file}" UPLOAD_MAX_BYTES "${UPLOAD_MAX_BYTES:-10737418240}"
    set_env_value "${env_file}" UPLOAD_ALLOWED_EXTENSIONS "${UPLOAD_ALLOWED_EXTENSIONS:-mp4,mov,m4v,mkv,webm,ts,m2ts,mts,flv}"
    set_env_value "${env_file}" UPLOAD_PROBE_TIMEOUT_SEC "${UPLOAD_PROBE_TIMEOUT_SEC:-30}"
    set_env_value "${env_file}" PUBLIC_MEDIA_BASE_URL "${PUBLIC_MEDIA_BASE_URL:-}"
  fi

  chmod 600 "${env_file}"
}

wait_for_http() {
  local url="$1"
  local label="$2"
  local display_url="${3:-$url}"
  local i
  if [ "${EXECUTE}" -ne 1 ]; then
    log "would wait for ${label}: ${display_url}"
    return 0
  fi
  for i in $(seq 1 60); do
    if curl -fsS "${url}" >/dev/null 2>&1; then
      log "${label} is ready"
      return 0
    fi
    sleep 1
  done
  fail "${label} did not become ready: ${display_url}"
}

wait_for_postgres() {
  if [ "${EXECUTE}" -ne 1 ]; then
    log "would wait for native postgres"
    return 0
  fi
  local i
  for i in $(seq 1 60); do
    if PGPASSWORD="${POSTGRES_PASSWORD:-}" "${INSTALL_DIR}/bin/pg_isready" \
      -h 127.0.0.1 -p "${POSTGRES_PORT:-5432}" -U "${POSTGRES_USER:-postgres}" >/dev/null 2>&1; then
      log "native postgres is ready"
      return 0
    fi
    sleep 1
  done
  fail "native postgres did not become ready"
}

restore_database_if_needed() {
  if ! role_has_core || [ "${SKIP_DB_RESTORE}" -eq 1 ]; then
    return 0
  fi
  [ -n "${DUMP_FILE:-}" ] || fail "internal error: DUMP_FILE is empty"

  printf '+ create target native database if missing\n'
  if [ "${EXECUTE}" -eq 1 ]; then
    if ! PGPASSWORD="${POSTGRES_PASSWORD:-}" "${INSTALL_DIR}/bin/psql" \
      -h 127.0.0.1 -p "${POSTGRES_PORT:-5432}" -U "${POSTGRES_USER:-postgres}" \
      -d postgres -tAc "SELECT 1 FROM pg_database WHERE datname='${POSTGRES_DB:-streamserver}'" | grep -qx 1; then
      PGPASSWORD="${POSTGRES_PASSWORD:-}" "${INSTALL_DIR}/bin/createdb" \
        -h 127.0.0.1 -p "${POSTGRES_PORT:-5432}" -U "${POSTGRES_USER:-postgres}" \
        "${POSTGRES_DB:-streamserver}"
    fi
  fi

  printf '+ restore Docker pg_dump into native PostgreSQL\n'
  if [ "${EXECUTE}" -eq 1 ]; then
    PGPASSWORD="${POSTGRES_PASSWORD:-}" "${INSTALL_DIR}/bin/pg_restore" \
      -h 127.0.0.1 -p "${POSTGRES_PORT:-5432}" -U "${POSTGRES_USER:-postgres}" \
      -d "${POSTGRES_DB:-streamserver}" --clean --if-exists "${DUMP_FILE}"
  fi
}

verify_worker_core_api() {
  role_has_worker || return 0
  role_has_core && return 0

  local url="http://${CORE_HTTP_HOST:-127.0.0.1}:${CORE_HTTP_PORT:-8080}/api/v1/nodes"
  if [ "${EXECUTE}" -ne 1 ]; then
    log "would verify upgraded core API before worker cutover: ${url}"
    return 0
  fi

  local body
  body="$(curl -fsS "${url}" 2>/dev/null || true)"
  if printf '%s' "${body}" | grep -q 'runtime_slot_loads'; then
    log "upgraded core API detected"
    return 0
  fi

  if [ "${ALLOW_WORKER_BEFORE_CORE}" -eq 1 ]; then
    warn "upgraded core API was not detected, continuing because --allow-worker-before-core was set"
  else
    fail "upgraded core API was not detected at ${url}; run all-in-one migration first, or pass --allow-worker-before-core"
  fi
}

start_native_services() {
  local prefix
  prefix="$(unit_prefix)"

  if role_has_core; then
    run systemctl start "${prefix}-postgres.service"
    wait_for_postgres
    restore_database_if_needed
    run systemctl start "${prefix}-core.service"
    wait_for_http "http://127.0.0.1:${CORE_HTTP_PORT:-8080}/health/ready" "media-core"
  fi

  if role_has_worker; then
    run systemctl start "${prefix}-zlm.service"
    wait_for_http \
      "http://127.0.0.1:${ZLM_HTTP_PORT:-80}/index/api/getStatistic?secret=${HOOK_SHARED_SECRET:-${ZLM_API_SECRET:-}}" \
      "zlmediakit" \
      "http://127.0.0.1:${ZLM_HTTP_PORT:-80}/index/api/getStatistic?secret=***"
    run systemctl start "${prefix}-agent.service"
    wait_for_http "http://127.0.0.1:${AGENT_HTTP_PORT:-8081}/health/ready" "media-agent"
  fi
}

write_summary() {
  local summary="${BACKUP_DIR}/migration-summary.txt"
  if [ "${EXECUTE}" -ne 1 ]; then
    log "dry-run completed; no summary file was written"
    return 0
  fi

  {
    echo "host=${HOSTNAME_SHORT}"
    echo "role=${INSTALL_ROLE}"
    echo "docker_root=${DOCKER_ROOT}"
    echo "install_dir=${INSTALL_DIR}"
    echo "instance_name=${INSTANCE_NAME}"
    echo "backup_dir=${BACKUP_DIR}"
    echo "db_dump=${DUMP_FILE:-}"
    echo "old_media_work=${DOCKER_ROOT}/data/media/work"
    echo "old_zlm_www=${DOCKER_ROOT}/data/zlm/www"
    echo "old_zlm_output=${DOCKER_ROOT}/data/zlm/www/output"
    echo "native_env=${INSTALL_DIR}/.env"
    echo "native_ctl=${INSTALL_DIR}/bin/streamserverctl"
  } >"${summary}"
  log "summary written: ${summary}"
}

main() {
  parse_args "$@"
  require_root_when_execute
  require_cmd docker
  require_cmd curl
  require_cmd tar
  require_cmd find
  ensure_bundle
  ensure_backup_base
  load_docker_env

  TS="$(date '+%Y%m%d_%H%M%S')"
  HOSTNAME_SHORT="$(hostname -s 2>/dev/null || hostname || echo localhost)"
  BACKUP_DIR="${BACKUP_BASE}/ss_docker_to_native_${HOSTNAME_SHORT}_${TS}"

  log "mode: $([ "${EXECUTE}" -eq 1 ] && echo execute || echo dry-run)"
  log "role: ${INSTALL_ROLE}"
  log "docker root: ${DOCKER_ROOT}"
  log "native install dir: ${INSTALL_DIR}"
  log "backup dir: ${BACKUP_DIR}"

  verify_worker_core_api
  copy_field_config_to_backup
  extract_bundle
  stop_existing_native_if_any
  stop_docker_app_containers_for_db_dump
  dump_docker_database_if_needed
  stop_docker_containers
  prepare_in_place_data_tree
  run_native_installer
  patch_runtime_unit_users
  patch_native_env
  start_native_services
  write_summary

  if [ "${EXECUTE}" -eq 0 ]; then
    log "dry-run only. Re-run with --execute to perform migration."
  else
    log "migration completed. Check: ${INSTALL_DIR}/bin/streamserverctl health"
  fi
}

main "$@"
