#!/usr/bin/env bash
set -euo pipefail

PACKAGE_ROOT="$(cd "$(dirname "$0")" && pwd)"
MANIFEST_FILE="${PACKAGE_ROOT}/package-manifest.env"

CHECK_ONLY=0
SECURITY_PREFLIGHT=0
UPGRADE=0
START_AFTER_INSTALL=1
INSTALL_ROLE=""
INSTALL_DIR=""
INSTANCE_NAME=""
DATABASE_MODE=""
DATABASE_URL_INPUT=""
SERVICE_USER="${SERVICE_USER:-streamserver}"
SERVICE_GROUP="${SERVICE_GROUP:-streamserver}"
UNIT_BASENAME=""
RESERVED_LOCAL_TCP_PORTS=""

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
  ./install.sh [--check-only|--security-preflight] [--upgrade] [--role ROLE]
               [--install-dir DIR] [--instance-name NAME]
               [--database-url URL] [--no-start]

角色:
  control-plane
  worker-host-cpu
  worker-host-gpu
  all-in-one-host-cpu
  all-in-one-host-gpu

安全检查:
  --check-only              校验安装包；配合 --install-dir 时同时检查现有生产安全配置
  --security-preflight      只检查 --install-dir 中的 auth/admin、HTTP TLS、gRPC mTLS
  --upgrade                 升级前用包内新 media-core 对现有数据库和 TLS 配置做只读预检

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
      --security-preflight)
        SECURITY_PREFLIGHT=1
        shift
        ;;
      --upgrade)
        UPGRADE=1
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

prompt_password_with_confirmation() {
  local message="$1"
  local password
  local confirm
  while true; do
    password="$(prompt_secret "${message}")"
    [ -n "${password}" ] || {
      echo "密码不能为空。" >&2
      continue
    }
    confirm="$(prompt_secret "再次输入以确认")"
    if [ "${password}" = "${confirm}" ]; then
      printf '%s' "${password}"
      return 0
    fi
    echo "两次输入不一致，请重新输入。" >&2
  done
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
  local default_dir="/home/streamserver"
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

confirm_existing_install_target() {
  if [ -e "${INSTALL_DIR}/.env" ]; then
    prompt_yes_no "检测到 ${INSTALL_DIR} 中已有 native/Docker 部署配置，将备份并覆盖运行程序，是否继续？" "N" \
      || fail "用户取消安装"
    return 0
  fi
  if [ -d "${INSTALL_DIR}" ] && find "${INSTALL_DIR}" -mindepth 1 -maxdepth 1 | grep -q .; then
    prompt_yes_no "目录 ${INSTALL_DIR} 已存在且非空，是否继续写入 StreamServer 文件？" "N" \
      || fail "用户取消安装"
  fi
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

normalize_csv_labels() {
  local raw="${1:-}"
  local part
  local trimmed
  local joined=""
  local seen=","
  local parts=()
  IFS=',' read -r -a parts <<<"${raw}"
  for part in "${parts[@]}"; do
    trimmed="$(printf '%s' "${part}" | sed 's/^[[:space:]]*//;s/[[:space:]]*$//')"
    [ -n "${trimmed}" ] || continue
    case "${trimmed}" in
      *[!A-Za-z0-9_.-]*)
        fail "节点标签只能包含字母、数字、下划线、点和连字符: ${trimmed}"
        ;;
    esac
    case "${seen}" in
      *",${trimmed},"*) continue ;;
    esac
    seen="${seen}${trimmed},"
    if [ -n "${joined}" ]; then
      joined="${joined},${trimmed}"
    else
      joined="${trimmed}"
    fi
  done
  printf '%s' "${joined}"
}

extra_agent_labels_from_existing() {
  local raw="${1:-}"
  local part
  local trimmed
  local joined=""
  local parts=()
  IFS=',' read -r -a parts <<<"${raw}"
  for part in "${parts[@]}"; do
    trimmed="$(printf '%s' "${part}" | sed 's/^[[:space:]]*//;s/[[:space:]]*$//')"
    [ -n "${trimmed}" ] || continue
    case "${trimmed}" in
      cpu|gpu) continue ;;
    esac
    if [ -n "${joined}" ]; then
      joined="${joined},${trimmed}"
    else
      joined="${trimmed}"
    fi
  done
  normalize_csv_labels "${joined}"
}

collect_agent_labels() {
  local default_label="$1"
  local existing_labels="${2:-${default_label}}"
  local extra_default
  local extra_labels
  printf '当前节点默认会写入算力标签: %s\n' "${default_label}" >&2
  extra_default="$(extra_agent_labels_from_existing "${existing_labels}")"
  extra_labels="$(prompt "额外节点标签（英文逗号分隔，可留空）" "${extra_default}")"
  extra_labels="$(extra_agent_labels_from_existing "${extra_labels}")"
  normalize_csv_labels "${default_label},${extra_labels}"
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

existing_env_value() {
  local env_file="$1"
  local key="$2"
  [ -f "${env_file}" ] || return 1
  awk -v key="${key}" '
    BEGIN { found = 0; collecting = 0; value = "" }
    collecting {
      if (substr($0, length($0), 1) == "\047") {
        value = value "\n" substr($0, 1, length($0) - 1)
        print value
        found = 1
        exit
      }
      value = value "\n" $0
      next
    }
    $0 ~ /^[[:space:]]*#/ { next }
    {
      line = $0
      sub(/^[[:space:]]*/, "", line)
    }
    index(line, key "=") == 1 {
      raw = substr(line, length(key) + 2)
      if (substr(raw, 1, 1) == "\047") {
        raw = substr(raw, 2)
        if (length(raw) > 0 && substr(raw, length(raw), 1) == "\047") {
          print substr(raw, 1, length(raw) - 1)
          found = 1
          exit
        }
        value = raw
        collecting = 1
        next
      }
      print raw
      found = 1
      exit
    }
    END { exit found ? 0 : 1 }
  ' "${env_file}"
}

env_key_exists() {
  local env_file="$1"
  local key="$2"
  existing_env_value "${env_file}" "${key}" >/dev/null
}

env_value_or_default() {
  local env_file="$1"
  local key="$2"
  local default_value="$3"
  local value
  if value="$(existing_env_value "${env_file}" "${key}")"; then
    printf '%s' "${value}"
  else
    printf '%s' "${default_value}"
  fi
}

security_env_value() {
  local env_file="$1"
  local key="$2"
  existing_env_value "${env_file}" "${key}" 2>/dev/null || true
}

resolve_security_path() {
  local env_file="$1"
  local value="$2"
  local env_dir
  [ -n "${value}" ] || return 0
  case "${value}" in
    /*) printf '%s' "${value}" ;;
    *)
      env_dir="$(cd "$(dirname "${env_file}")" && pwd -P)"
      printf '%s/%s' "${env_dir}" "${value}"
      ;;
  esac
}

is_loopback_socket_addr() {
  case "$1" in
    127.*:*|\[::1\]:*) return 0 ;;
    *) return 1 ;;
  esac
}

validate_x509_certificate() {
  local cert_path="$1"
  [ -r "${cert_path}" ] || return 1
  openssl x509 -in "${cert_path}" -noout -checkend 0 >/dev/null 2>&1
}

validate_x509_ca_certificate() {
  local cert_path="$1"
  validate_x509_certificate "${cert_path}" || return 1
  openssl x509 -in "${cert_path}" -noout -text 2>/dev/null | grep -q 'CA:TRUE'
}

validate_private_key() {
  local key_path="$1"
  [ -r "${key_path}" ] || return 1
  openssl pkey -in "${key_path}" -noout >/dev/null 2>&1
}

validate_public_key() {
  local key_path="$1"
  [ -r "${key_path}" ] || return 1
  openssl pkey -pubin -in "${key_path}" -noout >/dev/null 2>&1
}

validate_certificate_key_pair() {
  local cert_path="$1"
  local key_path="$2"
  local cert_fingerprint
  local key_fingerprint
  validate_x509_certificate "${cert_path}" || return 1
  validate_private_key "${key_path}" || return 1
  cert_fingerprint="$(openssl x509 -in "${cert_path}" -pubkey -noout 2>/dev/null \
    | openssl pkey -pubin -outform DER 2>/dev/null \
    | sha256sum | awk '{print $1}')" || return 1
  key_fingerprint="$(openssl pkey -in "${key_path}" -pubout -outform DER 2>/dev/null \
    | sha256sum | awk '{print $1}')" || return 1
  [ -n "${cert_fingerprint}" ] && [ "${cert_fingerprint}" = "${key_fingerprint}" ]
}

validate_private_public_key_pair() {
  local private_key_path="$1"
  local public_key_path="$2"
  local private_fingerprint
  local public_fingerprint
  validate_private_key "${private_key_path}" || return 1
  validate_public_key "${public_key_path}" || return 1
  private_fingerprint="$(openssl pkey -in "${private_key_path}" -pubout -outform DER 2>/dev/null \
    | sha256sum | awk '{print $1}')" || return 1
  public_fingerprint="$(openssl pkey -pubin -in "${public_key_path}" -outform DER 2>/dev/null \
    | sha256sum | awk '{print $1}')" || return 1
  [ -n "${private_fingerprint}" ] && [ "${private_fingerprint}" = "${public_fingerprint}" ]
}

security_preflight_env() {
  local env_file="$1"
  local core_bin="${2:-}"
  local role auth_mode database_url jwt_private jwt_public jwt_external
  local http_addr http_cert http_key
  local grpc_cert grpc_key grpc_client_ca
  local agent_endpoint agent_cert agent_key agent_ca agent_domain
  local failures=0

  if [ ! -f "${env_file}" ]; then
    printf '[MISSING] configuration: %s does not exist\n' "${env_file}" >&2
    return 1
  fi
  role="$(security_env_value "${env_file}" INSTALL_ROLE)"
  [ -n "${core_bin}" ] || core_bin="$(cd "$(dirname "${env_file}")" && pwd)/bin/media-core"
  case "${role}" in
    control-plane|worker-host-cpu|worker-host-gpu|all-in-one-host-cpu|all-in-one-host-gpu) ;;
    *)
      printf '[MISSING] configuration: INSTALL_ROLE is missing or unsupported\n' >&2
      return 1
      ;;
  esac

  if role_has_core "${role}"; then
    auth_mode="$(security_env_value "${env_file}" AUTH_MODE)"
    database_url="$(security_env_value "${env_file}" DATABASE_URL)"
    jwt_private="$(security_env_value "${env_file}" AUTH_JWT_PRIVATE_KEY_PATH)"
    jwt_public="$(security_env_value "${env_file}" AUTH_JWT_PUBLIC_KEY_PATH)"
    jwt_private="$(resolve_security_path "${env_file}" "${jwt_private}")"
    jwt_public="$(resolve_security_path "${env_file}" "${jwt_public}")"
    jwt_external="$(security_env_value "${env_file}" JWT_PUBLIC_KEY)"
    case "${auth_mode}" in
      local_password)
        if [ -z "${database_url}" ]; then
          printf '[MISSING] auth/admin: DATABASE_URL is not configured\n' >&2
          failures=$((failures + 1))
        elif ! validate_private_public_key_pair "${jwt_private}" "${jwt_public}"; then
          printf '[INVALID] auth/admin: local_password JWT private/public key pair is missing or invalid\n' >&2
          failures=$((failures + 1))
        elif [ ! -x "${core_bin}" ]; then
          printf '[UNKNOWN] auth/admin: media-core admin probe is unavailable\n' >&2
          failures=$((failures + 1))
        elif ! env \
          STREAMSERVER_ENV=production \
          DATABASE_URL="${database_url}" \
          AUTH_MODE=local_password \
          AUTH_JWT_PRIVATE_KEY_PATH="${jwt_private}" \
          AUTH_JWT_PUBLIC_KEY_PATH="${jwt_public}" \
          "${core_bin}" auth check-config >/dev/null 2>&1; then
          printf '[INVALID] auth/admin: local_password JWT configuration is not valid RSA or Ed25519 PEM\n' >&2
          failures=$((failures + 1))
        elif env \
          STREAMSERVER_ENV=production \
          DATABASE_URL="${database_url}" \
          AUTH_MODE=local_password \
          AUTH_JWT_PRIVATE_KEY_PATH="${jwt_private}" \
          AUTH_JWT_PUBLIC_KEY_PATH="${jwt_public}" \
          "${core_bin}" auth check-admin >/dev/null 2>&1; then
          printf '[OK] auth/admin: local_password keys and enabled administrator verified\n'
        else
          printf '[MISSING] auth/admin: enabled administrator could not be confirmed\n' >&2
          failures=$((failures + 1))
        fi
        ;;
      external_jwt)
        if [ -z "${jwt_external}" ]; then
          printf '[MISSING] auth/admin: JWT_PUBLIC_KEY is required for external_jwt\n' >&2
          failures=$((failures + 1))
        elif [ -z "${database_url}" ]; then
          printf '[MISSING] auth/admin: DATABASE_URL is not configured\n' >&2
          failures=$((failures + 1))
        elif [ ! -x "${core_bin}" ]; then
          printf '[UNKNOWN] auth/admin: media-core auth configuration probe is unavailable\n' >&2
          failures=$((failures + 1))
        elif env \
          STREAMSERVER_ENV=production \
          DATABASE_URL="${database_url}" \
          AUTH_MODE=external_jwt \
          JWT_PUBLIC_KEY="${jwt_external}" \
          "${core_bin}" auth check-config >/dev/null 2>&1; then
          printf '[OK] auth/admin: external_jwt public key verified\n'
        else
          printf '[INVALID] auth/admin: external_jwt public key is not a valid RSA or Ed25519 PEM key\n' >&2
          failures=$((failures + 1))
        fi
        ;;
      *)
        printf '[MISSING] auth/admin: production AUTH_MODE must be local_password or external_jwt\n' >&2
        failures=$((failures + 1))
        ;;
    esac

    http_addr="$(security_env_value "${env_file}" CORE_HTTP_ADDR)"
    http_cert="$(security_env_value "${env_file}" CORE_HTTP_TLS_CERT_PATH)"
    http_key="$(security_env_value "${env_file}" CORE_HTTP_TLS_KEY_PATH)"
    http_cert="$(resolve_security_path "${env_file}" "${http_cert}")"
    http_key="$(resolve_security_path "${env_file}" "${http_key}")"
    if [ -z "${http_cert}" ] && [ -z "${http_key}" ]; then
      if is_loopback_socket_addr "${http_addr}"; then
        printf '[OK] HTTP TLS: plaintext listener is restricted to loopback\n'
      else
        printf '[MISSING] HTTP TLS: non-loopback CORE_HTTP_ADDR requires certificate and key\n' >&2
        failures=$((failures + 1))
      fi
    elif [ -z "${http_cert}" ] || [ -z "${http_key}" ]; then
      printf '[MISSING] HTTP TLS: certificate and key must be configured together\n' >&2
      failures=$((failures + 1))
    elif validate_certificate_key_pair "${http_cert}" "${http_key}"; then
      printf '[OK] HTTP TLS: certificate and matching private key verified\n'
    else
      printf '[INVALID] HTTP TLS: certificate or matching private key is invalid\n' >&2
      failures=$((failures + 1))
    fi

    grpc_cert="$(security_env_value "${env_file}" CORE_GRPC_TLS_CERT_PATH)"
    grpc_key="$(security_env_value "${env_file}" CORE_GRPC_TLS_KEY_PATH)"
    grpc_client_ca="$(security_env_value "${env_file}" CORE_GRPC_TLS_CLIENT_CA_PATH)"
    grpc_cert="$(resolve_security_path "${env_file}" "${grpc_cert}")"
    grpc_key="$(resolve_security_path "${env_file}" "${grpc_key}")"
    grpc_client_ca="$(resolve_security_path "${env_file}" "${grpc_client_ca}")"
    if [ -z "${grpc_cert}" ] || [ -z "${grpc_key}" ] || [ -z "${grpc_client_ca}" ]; then
      printf '[MISSING] gRPC mTLS: server certificate, key and client CA are all required\n' >&2
      failures=$((failures + 1))
    elif validate_certificate_key_pair "${grpc_cert}" "${grpc_key}" \
      && validate_x509_ca_certificate "${grpc_client_ca}"; then
      printf '[OK] gRPC mTLS: server identity and client CA verified\n'
    else
      printf '[INVALID] gRPC mTLS: server identity or client CA is invalid\n' >&2
      failures=$((failures + 1))
    fi
  fi

  if role_has_worker "${role}"; then
    agent_endpoint="$(security_env_value "${env_file}" AGENT_CORE_ENDPOINT)"
    agent_cert="$(security_env_value "${env_file}" AGENT_CERT_PATH)"
    agent_key="$(security_env_value "${env_file}" AGENT_KEY_PATH)"
    agent_ca="$(security_env_value "${env_file}" AGENT_CA_PATH)"
    agent_cert="$(resolve_security_path "${env_file}" "${agent_cert}")"
    agent_key="$(resolve_security_path "${env_file}" "${agent_key}")"
    agent_ca="$(resolve_security_path "${env_file}" "${agent_ca}")"
    agent_domain="$(security_env_value "${env_file}" AGENT_TLS_DOMAIN_NAME)"
    if [[ "${agent_endpoint}" != https://* ]] \
      || [ -z "${agent_cert}" ] || [ -z "${agent_key}" ] \
      || [ -z "${agent_ca}" ] || [ -z "${agent_domain}" ]; then
      printf '[MISSING] worker mTLS: HTTPS endpoint, client certificate/key, CA and TLS domain are required\n' >&2
      failures=$((failures + 1))
    elif validate_certificate_key_pair "${agent_cert}" "${agent_key}" \
      && validate_x509_ca_certificate "${agent_ca}"; then
      printf '[OK] worker mTLS: client identity, CA, endpoint and TLS domain verified\n'
    else
      printf '[INVALID] worker mTLS: client identity or CA is invalid\n' >&2
      failures=$((failures + 1))
    fi
  fi

  if [ "${failures}" -ne 0 ]; then
    printf 'security preflight failed with %s issue(s); services will not be started\n' "${failures}" >&2
    return 1
  fi
  printf 'security preflight passed\n'
}

validate_port_number() {
  local key="$1"
  local value="$2"
  local allow_zero="${3:-false}"
  case "${value}" in
    ''|*[!0-9]*)
      fail "${key} 必须是 0-65535 之间的整数"
      ;;
  esac
  if [ "${allow_zero}" = "true" ] && [ "${value}" = "0" ]; then
    return 0
  fi
  if [ "${value}" -lt 1 ] || [ "${value}" -gt 65535 ]; then
    fail "${key} 必须是 1-65535 之间的端口"
  fi
}

validate_port_range() {
  local key="$1"
  local value="$2"
  local start_port
  local end_port
  case "${value}" in
    *-*) ;;
    *) fail "${key} 必须使用 start-end 格式" ;;
  esac
  start_port="${value%%-*}"
  end_port="${value#*-}"
  validate_port_number "${key}" "${start_port}" true
  validate_port_number "${key}" "${end_port}" true
  if [ "${start_port}" -gt "${end_port}" ]; then
    fail "${key} 的起始端口不能大于结束端口"
  fi
}

validate_non_negative_integer() {
  local key="$1"
  local value="$2"
  case "${value}" in
    ''|*[!0-9]*)
      fail "${key} 必须是大于等于 0 的整数"
      ;;
  esac
}

validate_positive_integer() {
  local key="$1"
  local value="$2"
  case "${value}" in
    ''|*[!0-9]*)
      fail "${key} 必须是大于 0 的整数"
      ;;
  esac
  if [ "${value}" -lt 1 ]; then
    fail "${key} 必须是大于 0 的整数"
  fi
}

validate_percent_value() {
  local key="$1"
  local value="$2"
  if ! awk -v value="${value}" 'BEGIN {
    if (value !~ /^([0-9]+)(\.[0-9]+)?$/) {
      exit 1
    }
    numeric = value + 0
    if (numeric < 0 || numeric > 100) {
      exit 1
    }
  }'; then
    fail "${key} 必须是 0-100 之间的数字"
  fi
}

validate_upload_extensions() {
  local raw="$1"
  local extension
  local upload_extensions=()
  [ -n "${raw}" ] || fail "UPLOAD_ALLOWED_EXTENSIONS 不能为空"
  IFS=',' read -r -a upload_extensions <<<"${raw}"
  for extension in "${upload_extensions[@]}"; do
    extension="$(printf '%s' "${extension}" | tr '[:upper:]' '[:lower:]' | sed 's/^[[:space:]]*//;s/[[:space:]]*$//;s/^\\.//')"
    case "${extension}" in
      ''|*/*|*\\*|*.*)
        fail "UPLOAD_ALLOWED_EXTENSIONS 只能包含不带点号的文件扩展名"
        ;;
    esac
  done
}

prompt_non_negative_integer() {
  local key="$1"
  local label="$2"
  local default_value="$3"
  local answer
  while true; do
    answer="$(prompt_non_empty "${label}" "${default_value}")"
    if validate_non_negative_integer "${key}" "${answer}"; then
      printf '%s' "${answer}"
      return 0
    fi
  done
}

prompt_positive_integer() {
  local key="$1"
  local label="$2"
  local default_value="$3"
  local answer
  while true; do
    answer="$(prompt_non_empty "${label}" "${default_value}")"
    if validate_positive_integer "${key}" "${answer}"; then
      printf '%s' "${answer}"
      return 0
    fi
  done
}

describe_tcp_port_usage() {
  local port="$1"
  if command -v ss >/dev/null 2>&1; then
    ss -H -ltnp "sport = :${port}" 2>/dev/null | sed '/^[[:space:]]*$/d' || true
    return 0
  fi
  if command -v lsof >/dev/null 2>&1; then
    lsof -nP -iTCP:"${port}" -sTCP:LISTEN 2>/dev/null || true
  fi
}

session_reserved_tcp_port_usage() {
  local port="$1"
  case " ${RESERVED_LOCAL_TCP_PORTS} " in
    *" ${port} "*) printf '%s' "当前安装流程已为其他组件预留端口 ${port}" ;;
  esac
}

describe_local_tcp_port_conflict() {
  local port="$1"
  local skip_host_check="${2:-false}"
  local usage
  if [ "${skip_host_check}" != "true" ]; then
    usage="$(describe_tcp_port_usage "${port}")"
    if [ -n "${usage}" ]; then
      printf '%s' "${usage}"
      return 0
    fi
  fi
  usage="$(session_reserved_tcp_port_usage "${port}")"
  [ -n "${usage}" ] && printf '%s' "${usage}"
}

reserve_local_tcp_port() {
  local port="$1"
  [ "${port}" = "0" ] && return 0
  validate_port_number "reserved_tcp_port" "${port}"
  case " ${RESERVED_LOCAL_TCP_PORTS} " in
    *" ${port} "*) return 0 ;;
  esac
  if [ -n "${RESERVED_LOCAL_TCP_PORTS}" ]; then
    RESERVED_LOCAL_TCP_PORTS="${RESERVED_LOCAL_TCP_PORTS} ${port}"
  else
    RESERVED_LOCAL_TCP_PORTS="${port}"
  fi
}

find_next_available_tcp_port() {
  local start_port="$1"
  local skip_host_check="${2:-false}"
  local candidate=$((start_port + 1))
  while [ "${candidate}" -le 65535 ]; do
    if [ -z "$(describe_local_tcp_port_conflict "${candidate}" "${skip_host_check}")" ]; then
      printf '%s' "${candidate}"
      return 0
    fi
    candidate=$((candidate + 1))
  done
  fail "从端口 ${start_port} 开始向后未找到空闲 TCP 端口，请手动清理端口占用后重试"
}

print_tcp_port_usage_details() {
  local usage="$1"
  [ -n "${usage}" ] || return 0
  echo "占用程序信息:" >&2
  printf '%s\n' "${usage}" | sed 's/^/  /' >&2
}

prompt_local_tcp_port() {
  local env_file="$1"
  local key="$2"
  local label="$3"
  local built_in_default="$4"
  local allow_zero="${5:-false}"
  local skip_host_check="false"
  local default_value
  local answer
  local usage
  [ "${allow_zero}" = "true" ] || validate_port_number "${key}" "${built_in_default}"
  [ "${allow_zero}" = "true" ] && validate_port_number "${key}" "${built_in_default}" true
  default_value="$(env_value_or_default "${env_file}" "${key}" "${built_in_default}")"
  if env_key_exists "${env_file}" "${key}"; then
    skip_host_check="true"
  fi
  while true; do
    answer="$(prompt_non_empty "${label}" "${default_value}")"
    validate_port_number "${key}" "${answer}" "${allow_zero}"
    if [ "${answer}" = "0" ]; then
      printf '%s' "${answer}"
      return 0
    fi
    usage="$(describe_local_tcp_port_conflict "${answer}" "${skip_host_check}")"
    if [ -z "${usage}" ]; then
      reserve_local_tcp_port "${answer}"
      printf '%s' "${answer}"
      return 0
    fi
    printf '端口 %s 已被占用，不能直接使用。\n' "${answer}" >&2
    print_tcp_port_usage_details "${usage}"
    default_value="$(find_next_available_tcp_port "${answer}" "${skip_host_check}")"
    printf '已临时选中空闲端口 %s 作为当前默认值，请确认或改成其他端口。\n' "${default_value}" >&2
  done
}

prompt_remote_port() {
  local key="$1"
  local label="$2"
  local default_value="$3"
  local allow_zero="${4:-false}"
  local answer
  while true; do
    answer="$(prompt_non_empty "${label}" "${default_value}")"
    if validate_port_number "${key}" "${answer}" "${allow_zero}"; then
      printf '%s' "${answer}"
      return 0
    fi
  done
}

prompt_port_range() {
  local key="$1"
  local label="$2"
  local default_value="$3"
  local answer
  while true; do
    answer="$(prompt_non_empty "${label}" "${default_value}")"
    if validate_port_range "${key}" "${answer}"; then
      printf '%s' "${answer}"
      return 0
    fi
  done
}

discover_ipv4_interfaces() {
  command -v ip >/dev/null 2>&1 || return 0
  ip -o -4 addr show up scope global 2>/dev/null \
    | awk '!seen[$2]++ { split($4, cidr, "/"); print $2 "|" cidr[1] }'
}

detect_primary_interface_entry() {
  command -v ip >/dev/null 2>&1 || return 0
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
    printf '  %d) %s (%s)\n' "${index}" "${entry%%|*}" "${entry#*|}" >&2
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
  if [ "${#entries[@]}" -eq 0 ]; then
    local name
    local ip_value
    name="$(prompt "${label}名称（可留空）" "${default_name}")"
    ip_value="$(prompt_non_empty "${label} IP" "${default_ip}")"
    printf '%s|%s' "${name}" "${ip_value}"
    return 0
  fi
  while true; do
    printf '%s 可用网卡（输入编号或网卡名）:\n' "${label}" >&2
    print_interface_options "${entries[@]}"
    answer="$(prompt "${label}" "${default_name}")"
    if selected="$(resolve_interface_choice "${answer}" "${entries[@]}")"; then
      printf '%s' "${selected}"
      return 0
    fi
    if [ -n "${default_name}" ] && [ "${answer}" = "${default_name}" ] && [ -n "${default_ip}" ]; then
      printf '%s|%s' "${default_name}" "${default_ip}"
      return 0
    fi
    printf '无效选择，请输入上面的编号或网卡名。\n' >&2
  done
}

configure_host_interfaces() {
  local env_file="$1"
  local fallback_ip="$2"
  local default_entry
  local default_name
  local default_ip
  local primary_entry
  local multicast_entry
  local entries=()
  mapfile -t entries < <(discover_ipv4_interfaces)
  default_entry="$(detect_primary_interface_entry)"
  if [ -z "${default_entry}" ] && [ "${#entries[@]}" -gt 0 ]; then
    default_entry="${entries[0]}"
  fi
  default_name="${default_entry%%|*}"
  default_ip="${default_entry#*|}"
  [ -n "${default_ip}" ] || default_ip="${fallback_ip}"
  default_name="$(env_value_or_default "${env_file}" "AGENT_PRIMARY_INTERFACE_NAME" "${default_name}")"
  default_ip="$(env_value_or_default "${env_file}" "AGENT_PRIMARY_INTERFACE_IP" "${default_ip}")"

  echo "host 工作节点需要分别选择主网卡和组播网卡。" >&2
  echo "建议：普通流量走主网卡；真实组播收发优先使用独立组播网卡，没有时可与主网卡相同。" >&2
  primary_entry="$(prompt_interface_selection "主网卡" "${default_name}" "${default_ip}" "${entries[@]}")"
  AGENT_PRIMARY_INTERFACE_NAME="${primary_entry%%|*}"
  AGENT_PRIMARY_INTERFACE_IP="${primary_entry#*|}"

  default_name="$(env_value_or_default "${env_file}" "AGENT_MULTICAST_INTERFACE_NAME" "${AGENT_PRIMARY_INTERFACE_NAME}")"
  default_ip="$(env_value_or_default "${env_file}" "AGENT_MULTICAST_INTERFACE_IP" "${AGENT_PRIMARY_INTERFACE_IP}")"
  multicast_entry="$(prompt_interface_selection "组播网卡" "${default_name}" "${default_ip}" "${entries[@]}")"
  AGENT_MULTICAST_INTERFACE_NAME="${multicast_entry%%|*}"
  AGENT_MULTICAST_INTERFACE_IP="${multicast_entry#*|}"
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
  for item in .env bin ui runtime zlm docs certs systemd uninstall.sh; do
    [ -e "${INSTALL_DIR}/${item}" ] || continue
    cp -R "${INSTALL_DIR}/${item}" "${backup_dir}/${item}"
  done
  log "已备份现有部署: ${backup_dir}"
}

is_output_root_mountpoint() {
  local output_root="$1"
  if grep -F " ${output_root} " /proc/self/mountinfo >/dev/null 2>&1; then
    return 0
  fi
  return 1
}

create_output_layout_if_local() {
  local output_root="${INSTALL_DIR}/data/zlm/www/output"
  if is_output_root_mountpoint "${output_root}"; then
    log "检测到 output 目录是挂载点，跳过创建 output/mp4 和 output/hls: ${output_root}"
    return 0
  fi
  mkdir -p "${output_root}/mp4" "${output_root}/hls"
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
    "${INSTALL_DIR}/data/zlm/www/record" \
    "${INSTALL_DIR}/data/zlm/www/snap"
  create_output_layout_if_local
}

fix_output_permissions() {
  local output_root="${INSTALL_DIR}/data/zlm/www/output"
  if is_output_root_mountpoint "${output_root}"; then
    log "检测到 output 目录是挂载点，跳过修正 output 权限: ${output_root}"
    return 0
  fi
  mkdir -p "${output_root}/mp4" "${output_root}/hls"

  local item
  for item in "${output_root}" "${output_root}/mp4" "${output_root}/hls"; do
    [ -e "${item}" ] || continue
    chown "${SERVICE_USER}:${SERVICE_GROUP}" "${item}"
    chmod 2775 "${item}"
  done

  for item in "${output_root}"/mp4/node-*-mp4 "${output_root}"/hls/node-*-hls; do
    [ -d "${item}" ] || continue
    chown "${SERVICE_USER}:${SERVICE_GROUP}" "${item}"
    chmod 2775 "${item}"
  done
}

copy_package_assets() {
  install_binary MEDIA_CORE_BINARY_PATH media-core
  install_binary MEDIA_GATEWAY_BINARY_PATH media-gateway
  install_binary STREAMSERVER_CONFIG_BINARY_PATH streamserver-config
  if role_has_worker "${INSTALL_ROLE}"; then
    install_binary MEDIA_AGENT_BINARY_PATH media-agent
    local ffmpeg_variant="cpu"
    role_is_gpu "${INSTALL_ROLE}" && ffmpeg_variant="gpu"
    install_tree "${PACKAGE_ROOT}/runtime/ffmpeg/${ffmpeg_variant}" "${INSTALL_DIR}/runtime/ffmpeg/${ffmpeg_variant}"
    install_tree "${PACKAGE_ROOT}/runtime/zlm" "${INSTALL_DIR}/runtime/zlm"
    mkdir -p "${INSTALL_DIR}/runtime/zlm/lib/log"
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
  if [[ "${value}" == *$'\n'* ]]; then
    [[ "${value}" != *"'"* ]] || fail "${key} 的跨行值不能包含单引号"
    printf "%s='%s'\n" "${key}" "${value}" >>"${file}"
  else
    printf '%s=%s\n' "${key}" "${value}" >>"${file}"
  fi
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
  local existing_env_file="${INSTALL_DIR}/.env"
  local existing_postgres_password
  local generated_password
  if ! role_has_core "${INSTALL_ROLE}"; then
    return 0
  fi
  if [ "${DATABASE_MODE}" = "external" ]; then
    POSTGRES_DB="$(env_value_or_default "${existing_env_file}" "POSTGRES_DB" "streamserver")"
    POSTGRES_USER="$(env_value_or_default "${existing_env_file}" "POSTGRES_USER" "postgres")"
    POSTGRES_PASSWORD="$(env_value_or_default "${existing_env_file}" "POSTGRES_PASSWORD" "")"
    POSTGRES_PORT="$(env_value_or_default "${existing_env_file}" "POSTGRES_PORT" "5432")"
    DATABASE_URL="${DATABASE_URL_INPUT}"
    return 0
  fi
  generated_password="$(generate_secret)"
  if [ "${BUNDLE_POSTGRES_RUNTIME:-false}" = "true" ] && prompt_yes_no "是否使用包内 PostgreSQL runtime？选择 N 则输入外部 DATABASE_URL" "Y"; then
    DATABASE_MODE="bundled"
    POSTGRES_DB="$(prompt_non_empty "PostgreSQL 数据库名" "$(env_value_or_default "${existing_env_file}" "POSTGRES_DB" "streamserver")")"
    POSTGRES_USER="$(prompt_non_empty "PostgreSQL 用户名" "$(env_value_or_default "${existing_env_file}" "POSTGRES_USER" "postgres")")"
    existing_postgres_password="$(env_value_or_default "${existing_env_file}" "POSTGRES_PASSWORD" "")"
    POSTGRES_PASSWORD="$(prompt "PostgreSQL 密码（留空沿用现有值或自动生成）" "")"
    [ -n "${POSTGRES_PASSWORD}" ] || POSTGRES_PASSWORD="${existing_postgres_password}"
    [ -n "${POSTGRES_PASSWORD}" ] || POSTGRES_PASSWORD="${generated_password}"
    POSTGRES_PORT="$(prompt_local_tcp_port "${existing_env_file}" "POSTGRES_PORT" "数据库宿主机监听端口" "5432")"
    DATABASE_URL="postgresql://${POSTGRES_USER}:${POSTGRES_PASSWORD}@127.0.0.1:${POSTGRES_PORT}/${POSTGRES_DB}"
  else
    DATABASE_MODE="external"
    DATABASE_URL="$(prompt_non_empty "外部 DATABASE_URL" "${DATABASE_URL_INPUT}")"
    POSTGRES_DB="$(env_value_or_default "${existing_env_file}" "POSTGRES_DB" "streamserver")"
    POSTGRES_USER="$(env_value_or_default "${existing_env_file}" "POSTGRES_USER" "postgres")"
    POSTGRES_PASSWORD="$(env_value_or_default "${existing_env_file}" "POSTGRES_PASSWORD" "")"
    POSTGRES_PORT="$(env_value_or_default "${existing_env_file}" "POSTGRES_PORT" "5432")"
  fi
}

configure_core_values() {
  local existing_env_file="${INSTALL_DIR}/.env"
  local existing_hook_secret
  local existing_auth_mode
  local generated_secret
  if ! role_has_core "${INSTALL_ROLE}"; then
    return 0
  fi
  CORE_HTTP_PORT="$(prompt_local_tcp_port "${existing_env_file}" "CORE_HTTP_PORT" "控制面板网页和 HTTP API 端口" "8080")"
  CORE_GRPC_PORT="$(prompt_local_tcp_port "${existing_env_file}" "CORE_GRPC_PORT" "控制面板内部通信端口" "50051")"
  CORE_HTTP_TLS_CERT_PATH="$(prompt "Core HTTP TLS 证书路径（留空时仅允许 loopback 明文）" "$(env_value_or_default "${existing_env_file}" "CORE_HTTP_TLS_CERT_PATH" "")")"
  CORE_HTTP_TLS_KEY_PATH="$(prompt "Core HTTP TLS 私钥路径" "$(env_value_or_default "${existing_env_file}" "CORE_HTTP_TLS_KEY_PATH" "")")"
  if [ -n "${CORE_HTTP_TLS_CERT_PATH}" ] && [ -n "${CORE_HTTP_TLS_KEY_PATH}" ]; then
    CORE_HTTP_ADDR="$(prompt_non_empty "Core HTTP 监听地址" "$(env_value_or_default "${existing_env_file}" "CORE_HTTP_ADDR" "127.0.0.1:${CORE_HTTP_PORT}")")"
    CORE_HTTP_SCHEME="https"
  else
    CORE_HTTP_ADDR="127.0.0.1:${CORE_HTTP_PORT}"
    CORE_HTTP_SCHEME="http"
  fi
  CORE_GRPC_TLS_CERT_PATH="$(prompt "Core gRPC TLS 服务端证书路径（production 必填）" "$(env_value_or_default "${existing_env_file}" "CORE_GRPC_TLS_CERT_PATH" "")")"
  CORE_GRPC_TLS_KEY_PATH="$(prompt "Core gRPC TLS 服务端私钥路径（production 必填）" "$(env_value_or_default "${existing_env_file}" "CORE_GRPC_TLS_KEY_PATH" "")")"
  CORE_GRPC_TLS_CLIENT_CA_PATH="$(prompt "Core gRPC 客户端 CA 路径（production 必填）" "$(env_value_or_default "${existing_env_file}" "CORE_GRPC_TLS_CLIENT_CA_PATH" "")")"
  CORE_GRPC_ADDR="$(prompt_non_empty "Core gRPC 监听地址" "$(env_value_or_default "${existing_env_file}" "CORE_GRPC_ADDR" "127.0.0.1:${CORE_GRPC_PORT}")")"
  generated_secret="$(generate_secret)"
  existing_hook_secret="$(env_value_or_default "${existing_env_file}" "HOOK_SHARED_SECRET" "")"
  HOOK_SHARED_SECRET="$(prompt "ZLM Hook/API 密钥（留空沿用现有值或自动生成）" "")"
  [ -n "${HOOK_SHARED_SECRET}" ] || HOOK_SHARED_SECRET="${existing_hook_secret}"
  [ -n "${HOOK_SHARED_SECRET}" ] || HOOK_SHARED_SECRET="${generated_secret}"
  HOOK_SOURCE_ALLOWLIST="$(prompt "Hook 源 IP 白名单，逗号分隔（可留空）" "$(env_value_or_default "${existing_env_file}" "HOOK_SOURCE_ALLOWLIST" "")")"
  STORAGE_ALLOWLIST="$(prompt_non_empty "本地媒体文件访问白名单，逗号分隔" "$(env_value_or_default "${existing_env_file}" "STORAGE_ALLOWLIST" "${INSTALL_DIR}/data/media/work,${INSTALL_DIR}/data/zlm/www")")"
  existing_auth_mode="$(env_value_or_default "${existing_env_file}" "AUTH_MODE" "local_password")"
  AUTH_MODE="${existing_auth_mode}"
  AUTH_ENABLED="true"
  JWT_PUBLIC_KEY="$(env_value_or_default "${existing_env_file}" "JWT_PUBLIC_KEY" "")"
  AUTH_JWT_PRIVATE_KEY_PATH="$(env_value_or_default "${existing_env_file}" "AUTH_JWT_PRIVATE_KEY_PATH" "")"
  AUTH_JWT_PUBLIC_KEY_PATH="$(env_value_or_default "${existing_env_file}" "AUTH_JWT_PUBLIC_KEY_PATH" "")"
  AUTH_ACCESS_TOKEN_TTL="$(env_value_or_default "${existing_env_file}" "AUTH_ACCESS_TOKEN_TTL" "15m")"
  AUTH_REFRESH_TOKEN_TTL="$(env_value_or_default "${existing_env_file}" "AUTH_REFRESH_TOKEN_TTL" "7d")"
  ADMIN_USERNAME=""
  ADMIN_PASSWORD=""
  ADMIN_BOOTSTRAP_REQUIRED=0
  if [ -f "${existing_env_file}" ] && [ "${existing_auth_mode}" = "local_password" ]; then
    log "保留现有 production 认证模式和 JWT 密钥: local_password"
  elif [ -f "${existing_env_file}" ] && [ "${existing_auth_mode}" = "external_jwt" ]; then
    log "保留现有 production 认证模式和公钥: external_jwt"
  else
    AUTH_MODE="local_password"
    AUTH_ENABLED="true"
    JWT_PUBLIC_KEY=""
    ADMIN_BOOTSTRAP_REQUIRED=1
    ADMIN_USERNAME="$(prompt_non_empty "管理员用户名" "admin")"
    ADMIN_PASSWORD="$(prompt_password_with_confirmation "管理员密码")"
    openssl genpkey -algorithm Ed25519 -out "${INSTALL_DIR}/certs/auth/jwt-ed25519-private.pem" >/dev/null 2>&1
    openssl pkey -in "${INSTALL_DIR}/certs/auth/jwt-ed25519-private.pem" -pubout -out "${INSTALL_DIR}/certs/auth/jwt-ed25519-public.pem" >/dev/null 2>&1
    chmod 600 "${INSTALL_DIR}/certs/auth/jwt-ed25519-private.pem"
    chmod 644 "${INSTALL_DIR}/certs/auth/jwt-ed25519-public.pem"
    AUTH_JWT_PRIVATE_KEY_PATH="${INSTALL_DIR}/certs/auth/jwt-ed25519-private.pem"
    AUTH_JWT_PUBLIC_KEY_PATH="${INSTALL_DIR}/certs/auth/jwt-ed25519-public.pem"
  fi
}

configure_zlm_port_values() {
  local existing_env_file="$1"
  ZLM_HTTP_PORT="$(prompt_local_tcp_port "${existing_env_file}" "ZLM_HTTP_PORT" "ZLM HTTP 监听端口" "80")"
  ZLM_HTTPS_PORT="$(prompt_local_tcp_port "${existing_env_file}" "ZLM_HTTPS_PORT" "ZLM HTTPS 监听端口（0 表示关闭）" "0" true)"
  ZLM_RTMP_PORT="$(prompt_local_tcp_port "${existing_env_file}" "ZLM_RTMP_PORT" "ZLM RTMP 监听端口" "1935")"
  ZLM_RTMPS_PORT="$(prompt_local_tcp_port "${existing_env_file}" "ZLM_RTMPS_PORT" "ZLM RTMPS 监听端口（0 表示关闭）" "0" true)"
  ZLM_RTSP_PORT="$(prompt_local_tcp_port "${existing_env_file}" "ZLM_RTSP_PORT" "ZLM RTSP 监听端口" "554")"
  ZLM_RTSPS_PORT="$(prompt_local_tcp_port "${existing_env_file}" "ZLM_RTSPS_PORT" "ZLM RTSPS 监听端口（0 表示关闭）" "0" true)"
  ZLM_RTP_PROXY_PORT="$(prompt_local_tcp_port "${existing_env_file}" "ZLM_RTP_PROXY_PORT" "ZLM RTP Proxy 监听端口（0 表示关闭）" "10000" true)"
  ZLM_RTP_PROXY_PORT_RANGE="$(prompt_port_range "ZLM_RTP_PROXY_PORT_RANGE" "ZLM RTP Proxy 随机端口范围（start-end，0-0 表示关闭）" "$(env_value_or_default "${existing_env_file}" "ZLM_RTP_PROXY_PORT_RANGE" "30000-30500")")"
  ZLM_RTC_SIGNALING_PORT="$(prompt_local_tcp_port "${existing_env_file}" "ZLM_RTC_SIGNALING_PORT" "ZLM WebRTC signaling 端口（0 表示关闭）" "8000" true)"
  ZLM_RTC_SIGNALING_SSL_PORT="$(prompt_local_tcp_port "${existing_env_file}" "ZLM_RTC_SIGNALING_SSL_PORT" "ZLM WebRTC signaling SSL 端口（0 表示关闭）" "0" true)"
  ZLM_RTC_ICE_PORT="$(prompt_local_tcp_port "${existing_env_file}" "ZLM_RTC_ICE_PORT" "ZLM WebRTC ICE UDP 端口（0 表示关闭）" "0" true)"
  ZLM_RTC_ICE_TCP_PORT="$(prompt_local_tcp_port "${existing_env_file}" "ZLM_RTC_ICE_TCP_PORT" "ZLM WebRTC ICE TCP 端口（0 表示关闭）" "0" true)"
  ZLM_RTC_PORT="$(prompt_local_tcp_port "${existing_env_file}" "ZLM_RTC_PORT" "ZLM WebRTC UDP 端口（0 表示关闭）" "0" true)"
  ZLM_RTC_TCP_PORT="$(prompt_local_tcp_port "${existing_env_file}" "ZLM_RTC_TCP_PORT" "ZLM WebRTC TCP 端口（0 表示关闭）" "0" true)"
  ZLM_RTC_PORT_RANGE="$(prompt_port_range "ZLM_RTC_PORT_RANGE" "ZLM WebRTC 端口范围（start-end，0-0 表示关闭）" "$(env_value_or_default "${existing_env_file}" "ZLM_RTC_PORT_RANGE" "0-0")")"
  ZLM_SRT_PORT="$(prompt_local_tcp_port "${existing_env_file}" "ZLM_SRT_PORT" "ZLM SRT 监听端口（0 表示关闭）" "0" true)"
  ZLM_SHELL_PORT="$(prompt_local_tcp_port "${existing_env_file}" "ZLM_SHELL_PORT" "ZLM Shell 监听端口（0 表示关闭）" "0" true)"
  ZLM_ONVIF_PORT="$(prompt_local_tcp_port "${existing_env_file}" "ZLM_ONVIF_PORT" "ZLM ONVIF 监听端口（0 表示关闭）" "0" true)"
}

configure_worker_values() {
  local existing_env_file="${INSTALL_DIR}/.env"
  local default_ip
  local existing_hook_secret
  local base_label="cpu"
  default_ip="$(detect_default_ip)"
  [ -n "${default_ip}" ] || default_ip="127.0.0.1"
  NODE_ID="$(prompt_non_empty "节点 UUID（留空自动生成）" "$(env_value_or_default "${existing_env_file}" "NODE_ID" "$(generate_uuid)")")"
  AGENT_NODE_NAME="$(prompt_non_empty "节点名称" "$(env_value_or_default "${existing_env_file}" "AGENT_NODE_NAME" "$(hostname -s 2>/dev/null || echo streamserver-node)")")"
  configure_host_interfaces "${existing_env_file}" "${default_ip}"
  default_ip="${AGENT_PRIMARY_INTERFACE_IP}"
  PUBLIC_HOST="$(prompt_non_empty "当前工作节点对外可访问的主机名或 IP" "$(env_value_or_default "${existing_env_file}" "PUBLIC_HOST" "${default_ip}")")"
  if role_has_core "${INSTALL_ROLE}"; then
    CORE_HTTP_HOST="127.0.0.1"
    CORE_GRPC_HOST="127.0.0.1"
  else
    CORE_HTTP_HOST="$(prompt_non_empty "control-plane HTTP 地址或域名" "$(env_value_or_default "${existing_env_file}" "CORE_HTTP_HOST" "")")"
    CORE_HTTP_PORT="$(prompt_remote_port "CORE_HTTP_PORT" "control-plane HTTP 端口" "$(env_value_or_default "${existing_env_file}" "CORE_HTTP_PORT" "8080")")"
    CORE_GRPC_HOST="$(prompt_non_empty "control-plane gRPC 地址或域名" "$(env_value_or_default "${existing_env_file}" "CORE_GRPC_HOST" "${CORE_HTTP_HOST}")")"
    CORE_GRPC_PORT="$(prompt_remote_port "CORE_GRPC_PORT" "control-plane gRPC 端口" "$(env_value_or_default "${existing_env_file}" "CORE_GRPC_PORT" "50051")")"
    existing_hook_secret="$(env_value_or_default "${existing_env_file}" "HOOK_SHARED_SECRET" "")"
    HOOK_SHARED_SECRET="$(prompt "ZLM Hook/API 密钥（需与 control-plane 一致）" "")"
    [ -n "${HOOK_SHARED_SECRET}" ] || HOOK_SHARED_SECRET="${existing_hook_secret}"
    [ -n "${HOOK_SHARED_SECRET}" ] || fail "worker 角色必须提供与 control-plane 一致的 Hook/API 密钥"
  fi
  AGENT_HTTP_PORT="$(prompt_local_tcp_port "${existing_env_file}" "AGENT_HTTP_PORT" "工作节点本地接口端口" "8081")"
  configure_zlm_port_values "${existing_env_file}"
  if ! role_has_core "${INSTALL_ROLE}"; then
    CORE_HTTP_SCHEME="https"
  fi
  AGENT_CERT_PATH="$(prompt "Agent mTLS 客户端证书路径（production 必填）" "$(env_value_or_default "${existing_env_file}" "AGENT_CERT_PATH" "")")"
  AGENT_KEY_PATH="$(prompt "Agent mTLS 客户端私钥路径（production 必填）" "$(env_value_or_default "${existing_env_file}" "AGENT_KEY_PATH" "")")"
  AGENT_CA_PATH="$(prompt "Agent 信任的 Core CA 路径（production 必填）" "$(env_value_or_default "${existing_env_file}" "AGENT_CA_PATH" "")")"
  AGENT_TLS_DOMAIN_NAME="$(prompt_non_empty "Core gRPC TLS 域名" "$(env_value_or_default "${existing_env_file}" "AGENT_TLS_DOMAIN_NAME" "${CORE_GRPC_HOST}")")"
  AGENT_NETWORK_MODE="host"
  AGENT_ACCELERATION_MODE="cpu"
  if role_is_gpu "${INSTALL_ROLE}"; then
    AGENT_ACCELERATION_MODE="gpu"
    base_label="gpu"
  fi
  AGENT_LABELS="$(collect_agent_labels "${base_label}" "$(env_value_or_default "${existing_env_file}" "AGENT_LABELS" "${base_label}")")"
  AGENT_MAX_LIVE_RUNTIME_SLOTS="$(prompt_non_negative_integer "AGENT_MAX_LIVE_RUNTIME_SLOTS" "直播任务并发上限（0 表示不限）" "$(env_value_or_default "${existing_env_file}" "AGENT_MAX_LIVE_RUNTIME_SLOTS" "0")")"
  AGENT_MAX_VOD_RUNTIME_SLOTS="$(prompt_non_negative_integer "AGENT_MAX_VOD_RUNTIME_SLOTS" "点播任务并发上限（0 表示不限）" "$(env_value_or_default "${existing_env_file}" "AGENT_MAX_VOD_RUNTIME_SLOTS" "0")")"
  AGENT_RUNTIME_MANAGER_START_LIMIT="$(prompt_positive_integer "AGENT_RUNTIME_MANAGER_START_LIMIT" "运行时启动并发上限" "$(env_value_or_default "${existing_env_file}" "AGENT_RUNTIME_MANAGER_START_LIMIT" "8")")"
  AGENT_RUNTIME_MANAGER_STOP_LIMIT="$(prompt_positive_integer "AGENT_RUNTIME_MANAGER_STOP_LIMIT" "运行时停止并发上限" "$(env_value_or_default "${existing_env_file}" "AGENT_RUNTIME_MANAGER_STOP_LIMIT" "16")")"
  AGENT_RUNTIME_MANAGER_RECORDING_LIMIT="$(prompt_positive_integer "AGENT_RUNTIME_MANAGER_RECORDING_LIMIT" "录制状态巡检并发上限" "$(env_value_or_default "${existing_env_file}" "AGENT_RUNTIME_MANAGER_RECORDING_LIMIT" "12")")"
  AGENT_RUNTIME_MANAGER_ADOPT_LIMIT="$(prompt_positive_integer "AGENT_RUNTIME_MANAGER_ADOPT_LIMIT" "孤儿任务接管并发上限" "$(env_value_or_default "${existing_env_file}" "AGENT_RUNTIME_MANAGER_ADOPT_LIMIT" "1")")"
  AGENT_RUNTIME_LOG_TAIL_BYTES="$(prompt_positive_integer "AGENT_RUNTIME_LOG_TAIL_BYTES" "运行诊断日志 tail 保存字节数" "$(env_value_or_default "${existing_env_file}" "AGENT_RUNTIME_LOG_TAIL_BYTES" "8192")")"
  AGENT_RUNTIME_LOG_MAX_FILE_BYTES="$(prompt_positive_integer "AGENT_RUNTIME_LOG_MAX_FILE_BYTES" "单个运行诊断日志文件最大字节数" "$(env_value_or_default "${existing_env_file}" "AGENT_RUNTIME_LOG_MAX_FILE_BYTES" "134217728")")"
  AGENT_RUNTIME_LOG_RETENTION_DAYS="$(prompt_positive_integer "AGENT_RUNTIME_LOG_RETENTION_DAYS" "运行诊断日志本地保留天数" "$(env_value_or_default "${existing_env_file}" "AGENT_RUNTIME_LOG_RETENTION_DAYS" "7")")"
  AGENT_MP4_RECORD_SEGMENT_SEC="$(prompt_positive_integer "AGENT_MP4_RECORD_SEGMENT_SEC" "MP4 录制分片时长（秒）" "$(env_value_or_default "${existing_env_file}" "AGENT_MP4_RECORD_SEGMENT_SEC" "7200")")"
  AGENT_HLS_RECORD_SEGMENT_SEC="$(prompt_non_empty "HLS 录制分片时长（秒，30 或 60）" "$(env_value_or_default "${existing_env_file}" "AGENT_HLS_RECORD_SEGMENT_SEC" "60")")"
  case "${AGENT_HLS_RECORD_SEGMENT_SEC}" in
    30|60) ;;
    *) fail "AGENT_HLS_RECORD_SEGMENT_SEC 必须是 30 或 60" ;;
  esac
  if prompt_yes_no "是否启用产物磁盘清理？" "$( [ "$(env_value_or_default "${existing_env_file}" "AGENT_ARTIFACT_CLEANUP_ENABLED" "true")" = "true" ] && printf Y || printf N )"; then
    AGENT_ARTIFACT_CLEANUP_ENABLED="true"
  else
    AGENT_ARTIFACT_CLEANUP_ENABLED="false"
  fi
  AGENT_ARTIFACT_CLEANUP_THRESHOLD_PERCENT="$(prompt_non_empty "产物磁盘清理阈值百分比" "$(env_value_or_default "${existing_env_file}" "AGENT_ARTIFACT_CLEANUP_THRESHOLD_PERCENT" "85")")"
  validate_percent_value "AGENT_ARTIFACT_CLEANUP_THRESHOLD_PERCENT" "${AGENT_ARTIFACT_CLEANUP_THRESHOLD_PERCENT}"
  AGENT_ARTIFACT_CLEANUP_STRATEGY="$(prompt_non_empty "产物磁盘清理策略（delete_oldest_then_reject/reject_only）" "$(env_value_or_default "${existing_env_file}" "AGENT_ARTIFACT_CLEANUP_STRATEGY" "delete_oldest_then_reject")")"
  case "${AGENT_ARTIFACT_CLEANUP_STRATEGY}" in
    delete_oldest_then_reject|reject_only) ;;
    *) fail "AGENT_ARTIFACT_CLEANUP_STRATEGY 必须是 delete_oldest_then_reject/reject_only 之一" ;;
  esac
  AGENT_ARTIFACT_CLEANUP_CHECK_INTERVAL_SEC="$(prompt_positive_integer "AGENT_ARTIFACT_CLEANUP_CHECK_INTERVAL_SEC" "产物磁盘清理检查间隔（秒）" "$(env_value_or_default "${existing_env_file}" "AGENT_ARTIFACT_CLEANUP_CHECK_INTERVAL_SEC" "30")")"
  WORK_ROOT="$(prompt_non_empty "工作目录" "$(env_value_or_default "${existing_env_file}" "WORK_ROOT" "${INSTALL_DIR}/data/media/work")")"
  UPLOAD_MAX_BYTES="$(prompt_positive_integer "UPLOAD_MAX_BYTES" "上传文件最大字节数" "$(env_value_or_default "${existing_env_file}" "UPLOAD_MAX_BYTES" "10737418240")")"
  UPLOAD_ALLOWED_EXTENSIONS="$(prompt_non_empty "允许上传扩展名（英文逗号分隔，不带点号）" "$(env_value_or_default "${existing_env_file}" "UPLOAD_ALLOWED_EXTENSIONS" "mp4,mov,m4v,mkv,webm,ts,m2ts,mts,flv")")"
  validate_upload_extensions "${UPLOAD_ALLOWED_EXTENSIONS}"
  UPLOAD_PROBE_TIMEOUT_SEC="$(prompt_positive_integer "UPLOAD_PROBE_TIMEOUT_SEC" "上传探测超时时间（秒）" "$(env_value_or_default "${existing_env_file}" "UPLOAD_PROBE_TIMEOUT_SEC" "30")")"
  PUBLIC_MEDIA_BASE_URL="$(prompt "公开媒体访问基准 URL（可留空）" "$(env_value_or_default "${existing_env_file}" "PUBLIC_MEDIA_BASE_URL" "")")"
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
    write_env_entry "${env_file}" CORE_HTTP_ADDR "${CORE_HTTP_ADDR}"
    write_env_entry "${env_file}" CORE_HTTP_PORT "${CORE_HTTP_PORT}"
    write_env_entry "${env_file}" CORE_HTTP_TLS_CERT_PATH "${CORE_HTTP_TLS_CERT_PATH}"
    write_env_entry "${env_file}" CORE_HTTP_TLS_KEY_PATH "${CORE_HTTP_TLS_KEY_PATH}"
    write_env_entry "${env_file}" CORE_GRPC_ADDR "${CORE_GRPC_ADDR}"
    write_env_entry "${env_file}" CORE_GRPC_PORT "${CORE_GRPC_PORT}"
    write_env_entry "${env_file}" CORE_GRPC_TLS_CERT_PATH "${CORE_GRPC_TLS_CERT_PATH}"
    write_env_entry "${env_file}" CORE_GRPC_TLS_KEY_PATH "${CORE_GRPC_TLS_KEY_PATH}"
    write_env_entry "${env_file}" CORE_GRPC_TLS_CLIENT_CA_PATH "${CORE_GRPC_TLS_CLIENT_CA_PATH}"
    write_env_entry "${env_file}" CORE_INSECURE_DEV false
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
    write_env_entry "${env_file}" CORE_HTTP_SCHEME "${CORE_HTTP_SCHEME}"
    write_env_entry "${env_file}" AGENT_CORE_ENDPOINT "https://${CORE_GRPC_HOST}:${CORE_GRPC_PORT:-50051}"
    write_env_entry "${env_file}" AGENT_CERT_PATH "${AGENT_CERT_PATH}"
    write_env_entry "${env_file}" AGENT_KEY_PATH "${AGENT_KEY_PATH}"
    write_env_entry "${env_file}" AGENT_CA_PATH "${AGENT_CA_PATH}"
    write_env_entry "${env_file}" AGENT_TLS_DOMAIN_NAME "${AGENT_TLS_DOMAIN_NAME}"
    write_env_entry "${env_file}" PUBLIC_HOST "${PUBLIC_HOST}"
    write_env_entry "${env_file}" AGENT_STREAM_ADDR "http://${PUBLIC_HOST}:${ZLM_HTTP_PORT}"
    write_env_entry "${env_file}" AGENT_HTTP_ADDR "0.0.0.0:${AGENT_HTTP_PORT}"
    write_env_entry "${env_file}" AGENT_HTTP_PORT "${AGENT_HTTP_PORT}"
    write_env_entry "${env_file}" HOOK_SHARED_SECRET "${HOOK_SHARED_SECRET}"
    write_env_entry "${env_file}" ZLM_API_HOST "${PUBLIC_HOST}"
    write_env_entry "${env_file}" ZLM_API_BASE "http://${PUBLIC_HOST}:${ZLM_HTTP_PORT}"
    write_env_entry "${env_file}" ZLM_API_SECRET "${HOOK_SHARED_SECRET:-${ZLM_API_SECRET:-}}"
    write_env_entry "${env_file}" ZLM_API_ALLOW_IP_RANGE "::1,127.0.0.1,10.0.0.0-10.255.255.255,172.16.0.0-172.31.255.255,192.168.0.0-192.168.255.255"
    write_env_entry "${env_file}" ZLM_HOOK_SHARED_SECRET "${HOOK_SHARED_SECRET:-${ZLM_API_SECRET:-}}"
    write_env_entry "${env_file}" ZLM_SERVER_ID "${NODE_ID}"
    write_env_entry "${env_file}" ZLM_HOOK_BASE "${CORE_HTTP_SCHEME}://${CORE_HTTP_HOST}:${CORE_HTTP_PORT:-8080}/internal/hooks/zlm/${NODE_ID}"
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
    write_env_entry "${env_file}" AGENT_MAX_LIVE_RUNTIME_SLOTS "${AGENT_MAX_LIVE_RUNTIME_SLOTS}"
    write_env_entry "${env_file}" AGENT_MAX_VOD_RUNTIME_SLOTS "${AGENT_MAX_VOD_RUNTIME_SLOTS}"
    write_env_entry "${env_file}" AGENT_RUNTIME_MANAGER_START_LIMIT "${AGENT_RUNTIME_MANAGER_START_LIMIT}"
    write_env_entry "${env_file}" AGENT_RUNTIME_MANAGER_STOP_LIMIT "${AGENT_RUNTIME_MANAGER_STOP_LIMIT}"
    write_env_entry "${env_file}" AGENT_RUNTIME_MANAGER_RECORDING_LIMIT "${AGENT_RUNTIME_MANAGER_RECORDING_LIMIT}"
    write_env_entry "${env_file}" AGENT_RUNTIME_MANAGER_ADOPT_LIMIT "${AGENT_RUNTIME_MANAGER_ADOPT_LIMIT}"
    write_env_entry "${env_file}" AGENT_RUNTIME_LOG_TAIL_BYTES "${AGENT_RUNTIME_LOG_TAIL_BYTES}"
    write_env_entry "${env_file}" AGENT_RUNTIME_LOG_MAX_FILE_BYTES "${AGENT_RUNTIME_LOG_MAX_FILE_BYTES}"
    write_env_entry "${env_file}" AGENT_RUNTIME_LOG_RETENTION_DAYS "${AGENT_RUNTIME_LOG_RETENTION_DAYS}"
    write_env_entry "${env_file}" AGENT_MP4_RECORD_SEGMENT_SEC "${AGENT_MP4_RECORD_SEGMENT_SEC}"
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

load_installed_env() {
  set -a
  # shellcheck disable=SC1091
  . "${INSTALL_DIR}/.env"
  set +a
}

run_streamserver_config_tui_if_requested() {
  local env_file="${INSTALL_DIR}/.env"
  local config_bin="${INSTALL_DIR}/bin/streamserver-config"
  [ -x "${config_bin}" ] || return 0
  if [ -t 0 ] && [ -t 1 ] && prompt_yes_no "是否现在打开高级配置界面？" "N"; then
    "${config_bin}" --env "${env_file}" --no-restart-prompt
    load_installed_env
  fi
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
  chown -R "${SERVICE_USER}:${SERVICE_GROUP}" "${data_dir}" "${INSTALL_DIR}/data/postgres-run" "${INSTALL_DIR}/runtime/postgres"
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
  [ "${ADMIN_BOOTSTRAP_REQUIRED:-0}" -eq 1 ] || return 0
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
  local core_scheme="http"
  local core_curl_tls=()
  if [ -n "${CORE_HTTP_TLS_CERT_PATH:-}" ]; then
    core_scheme="https"
    # The configured server certificate can be CA-signed; there is no separate HTTP CA setting.
    # This probe is loopback-only and verifies readiness, while clients still validate normally.
    core_curl_tls=(-k)
  fi
  if [ -n "${CORE_HTTP_PORT:-}" ] && systemctl list-unit-files "${SYSTEMD_CORE_UNIT:-missing}" >/dev/null 2>&1; then
    curl -fsS "${core_curl_tls[@]}" "${core_scheme}://127.0.0.1:${CORE_HTTP_PORT}/health/ready" >/dev/null && echo "[OK] media-core"
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
  chown "${SERVICE_USER}:${SERVICE_GROUP}" "${INSTALL_DIR}"
  for item in bin runtime ui zlm docs certs systemd uninstall.sh .env; do
    [ -e "${INSTALL_DIR}/${item}" ] && chown -R "${SERVICE_USER}:${SERVICE_GROUP}" "${INSTALL_DIR}/${item}"
  done
  for item in data data/media data/media/work data/media/logs data/postgres data/postgres-run data/zlm data/zlm/www data/zlm/www/record data/zlm/www/snap; do
    [ -e "${INSTALL_DIR}/${item}" ] && chown "${SERVICE_USER}:${SERVICE_GROUP}" "${INSTALL_DIR}/${item}"
  done
  fix_output_permissions
  chmod 755 "${INSTALL_DIR}" "${INSTALL_DIR}/bin"
}

start_services_if_requested() {
  [ "${START_AFTER_INSTALL}" -eq 1 ] || return 0
  systemctl start "${UNIT_BASENAME}.target"
  log "已启动 native 服务。"
  log "状态: ${INSTALL_DIR}/bin/streamserverctl status"
  log "健康检查: ${INSTALL_DIR}/bin/streamserverctl health"
}

prepare_production_security_state() {
  if role_has_core "${INSTALL_ROLE}"; then
    if [ "${DATABASE_MODE}" = "bundled" ]; then
      systemctl start "${UNIT_BASENAME}-postgres.service"
      wait_for_postgres
      ensure_database_exists
    fi
    bootstrap_local_admin_if_needed
  fi

  if ! security_preflight_env "${INSTALL_DIR}/.env"; then
    if role_has_core "${INSTALL_ROLE}" && [ "${DATABASE_MODE}" = "bundled" ]; then
      systemctl stop "${UNIT_BASENAME}-postgres.service" >/dev/null 2>&1 || true
    fi
    fail "production security preflight failed; no Core/Agent service was started"
  fi

  if [ "${START_AFTER_INSTALL}" -eq 0 ] \
    && role_has_core "${INSTALL_ROLE}" \
    && [ "${DATABASE_MODE}" = "bundled" ]; then
    systemctl stop "${UNIT_BASENAME}-postgres.service"
  fi
}

confirm_start_after_install() {
  [ "${START_AFTER_INSTALL}" -eq 1 ] || return 0
  if ! prompt_yes_no "是否立即启动 native 服务？" "Y"; then
    START_AFTER_INSTALL=0
    log "已选择暂不启动服务。后续可执行: ${INSTALL_DIR}/bin/streamserverctl start"
  fi
}

main() {
  local package_core_bin
  parse_args "$@"
  if [ "${CHECK_ONLY}" -eq 1 ] && [ "${SECURITY_PREFLIGHT}" -eq 1 ]; then
    fail "--check-only and --security-preflight cannot be used together"
  fi
  load_manifest
  ensure_prerequisites
  verify_package_checksums
  assert_no_docker_assets
  package_core_bin="${PACKAGE_ROOT}/${MEDIA_CORE_BINARY_PATH}"
  if [ "${SECURITY_PREFLIGHT}" -eq 1 ]; then
    [ -n "${INSTALL_DIR}" ] || fail "--security-preflight requires --install-dir"
    security_preflight_env "${INSTALL_DIR}/.env" "${package_core_bin}" \
      || fail "installed production security preflight failed"
    exit 0
  fi
  if [ "${CHECK_ONLY}" -eq 1 ]; then
    if [ -n "${INSTALL_DIR}" ]; then
      security_preflight_env "${INSTALL_DIR}/.env" "${package_core_bin}" \
        || fail "check-only found production security gaps"
    fi
    log "check-only 通过。"
    exit 0
  fi
  if [ "${UPGRADE}" -eq 1 ]; then
    [ -n "${INSTALL_DIR}" ] || fail "--upgrade requires --install-dir"
    [ -f "${INSTALL_DIR}/.env" ] || fail "--upgrade requires an existing native .env"
    security_preflight_env "${INSTALL_DIR}/.env" "${package_core_bin}" \
      || fail "upgrade blocked until auth/admin and TLS gaps are migrated"
    [ -n "${INSTALL_ROLE}" ] || INSTALL_ROLE="$(existing_env_value "${INSTALL_DIR}/.env" INSTALL_ROLE)"
    [ -n "${INSTANCE_NAME}" ] || INSTANCE_NAME="$(existing_env_value "${INSTALL_DIR}/.env" INSTANCE_NAME)"
  fi
  ensure_root_for_install
  select_role
  collect_basic_inputs
  confirm_existing_install_target
  configure_database
  prepare_layout
  copy_package_assets
  configure_core_values
  if role_has_worker "${INSTALL_ROLE}"; then
    configure_worker_values
  fi
  write_env_file
  run_streamserver_config_tui_if_requested
  write_streamserverctl
  install_uninstaller
  ensure_service_user
  initialize_postgres_if_needed
  fix_permissions
  install_systemd_units
  confirm_start_after_install
  prepare_production_security_state
  start_services_if_requested
  log "安装完成: ${INSTALL_DIR}"
  log "卸载: ${INSTALL_DIR}/uninstall.sh"
}

main "$@"
