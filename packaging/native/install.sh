#!/usr/bin/env bash
set -euo pipefail
set +a
set +x
unset ADMIN_PASSWORD
unset AGENT_ENROLLMENT_TOKEN

# This switch exists only for function-level contract tests that source a
# main-less copy of this file. It must never relax a real root installer.
if [ "${BASH_SOURCE[0]}" = "$0" ] \
  && [ -n "${EMULATED_SECURITY_METADATA+x}" ]; then
  printf '%s\n' \
    '[streamserver-native-install] ERROR: EMULATED_SECURITY_METADATA is test-only and is refused by the native installer' >&2
  exit 1
fi
unset EMULATED_SECURITY_METADATA

PACKAGE_ROOT="$(cd "$(dirname "$0")" && pwd)"
MANIFEST_FILE="${PACKAGE_ROOT}/package-manifest.env"

CHECK_ONLY=0
SECURITY_PREFLIGHT=0
UPGRADE=0
START_AFTER_INSTALL=1
INSTALL_ROLE=""
INSTALL_DIR=""
INSTANCE_NAME=""
INSTALL_ROLE_WAS_EXPLICIT=0
INSTANCE_NAME_WAS_EXPLICIT=0
DATABASE_MODE=""
DATABASE_URL_INPUT=""
SERVICE_USER="${SERVICE_USER:-streamserver}"
SERVICE_GROUP="${SERVICE_GROUP:-streamserver}"
UNIT_BASENAME=""
RESERVED_LOCAL_TCP_PORTS=""
INTERACTIVE_INSTALL=0
INITIAL_ADMIN_PASSWORD_READY=0
ADMIN_HANDOFF_DELIVERED_NAME="admin-handoff.delivered"
ADMIN_HANDOFF_STATE_ROOT="/var/lib/streamserver-native-installer"
SYSTEMD_UNIT_ROOT="/etc/systemd/system"
INSTALL_TRANSACTION_LOCK_FD=""
INSTALL_TRANSACTION_LOCK_PATH=""
INSTALL_TRANSACTION_EXTERNAL_LOCKS=0
INSTALL_TRANSACTION_GLOBAL_LOCK_PATH=""
INSTALL_TRANSACTION_PATH_LOCK_PATH=""
INSTALL_LOCK_WRAPPER_SIGNAL_STATUS=0
INSTALL_TRANSACTION_INSTALL_DIR_IDENTITY=""
INSTALL_TRANSACTION_PARENT_CHAIN_IDENTITY=""
INSTALL_TRANSACTION_STATE_ROOT_IDENTITY=""
INSTALL_TRANSACTION_STATE_DIR_IDENTITY=""
ADMIN_HANDOFF_DELIVERED_READY=0
UPGRADE_TARGET_WAS_ACTIVE=0
UPGRADE_SERVICES_QUIESCED=0
UPGRADE_RESTORE_ON_FAILURE=0
UPGRADE_PREFLIGHT_POSTGRES_STARTED=0
UPGRADE_TRANSACTION_STATE="none"
UPGRADE_TRANSACTION_DIR=""
UPGRADE_TRANSACTION_ID=""
UPGRADE_TRANSACTION_PHASE_FILE=""
UPGRADE_ACTIVE_UNITS=()
UPGRADE_ACTIVE_MAIN_PIDS=()
UPGRADE_SERVICE_STATE_CAPTURED=0
INSTALLER_TEMP_FILES=()
LAST_INSTALLER_TEMP_FILE=""
SECURITY_PROBE_RUNTIME_ROOT=""
SECURITY_PROBE_DIR=""
SECURITY_PROBE_DIR_IDENTITY=""
SECURITY_PROBE_CORE_BIN=""
SECURITY_PROBE_AGENT_BIN=""
VERIFIED_PACKAGE_CORE_SHA256=""
VERIFIED_PACKAGE_AGENT_SHA256=""
VERIFIED_PACKAGE_CHECKSUM_FILE_IDENTITY=""
VERIFIED_PACKAGE_CHECKSUM_FILE_SHA256=""
VERIFIED_PACKAGE_CHECKSUM_SNAPSHOT=""
VERIFIED_PACKAGE_CHECKSUM_SNAPSHOT_IDENTITY=""
VERIFIED_PACKAGE_TREE_FINGERPRINT=""
LOCKED_PACKAGE_EXPECTED_CHECKSUM_SHA256=""
LOCKED_PACKAGE_EXPECTED_TREE_FINGERPRINT=""
LOCKED_PACKAGE_STAGING_DIR=""
LOCKED_PACKAGE_STAGING_IDENTITY=""
UPGRADE_BOOT_FENCE_ACTIVE=0
UPGRADE_BOOT_FENCE_MARKER=""
UPGRADE_BOOT_FENCE_LEASE=""
UPGRADE_BOOT_FENCE_LEASE_FD=""

clear_security_probe_state() {
  SECURITY_PROBE_DIR=""
  SECURITY_PROBE_DIR_IDENTITY=""
  SECURITY_PROBE_CORE_BIN=""
  SECURITY_PROBE_AGENT_BIN=""
}

cleanup_security_probe_binaries() {
  local probe_dir="${SECURITY_PROBE_DIR:-}"
  local expected_identity="${SECURITY_PROBE_DIR_IDENTITY:-}"
  [ -n "${probe_dir}" ] || {
    clear_security_probe_state
    return 0
  }
  case "${probe_dir}" in
    "${SECURITY_PROBE_RUNTIME_ROOT}"/probe.*) ;;
    *)
      printf '[streamserver-native-install] WARNING: refused to clean an unexpected security probe path\n' >&2
      return 1
      ;;
  esac
  if [ ! -e "${probe_dir}" ] && [ ! -L "${probe_dir}" ]; then
    clear_security_probe_state
    return 0
  fi
  if [ -L "${probe_dir}" ] || [ ! -d "${probe_dir}" ]; then
    printf '[streamserver-native-install] WARNING: refused to clean a replaced security probe directory\n' >&2
    return 1
  fi
  if [ -z "${expected_identity}" ]; then
    if [ "$(stat -c '%u:%a' -- "${probe_dir}" 2>/dev/null || true)" = "$(id -u):700" ] && rmdir -- "${probe_dir}" 2>/dev/null; then
      sync -f "${SECURITY_PROBE_RUNTIME_ROOT}" >/dev/null 2>&1 || true
      clear_security_probe_state
      return 0
    fi
    printf '[streamserver-native-install] WARNING: refused to clean an unidentified security probe directory\n' >&2
    return 1
  fi
  if [ "$(stat -Lc '%d:%i' -- "${probe_dir}" 2>/dev/null || true)" != "${expected_identity}" ]; then
    printf '[streamserver-native-install] WARNING: refused to clean a replaced security probe directory\n' >&2
    return 1
  fi
  rm -f -- "${probe_dir}/.media-core.tmp" "${probe_dir}/.media-agent.tmp" 2>/dev/null \
    || {
      printf '[streamserver-native-install] WARNING: failed to remove security probe temporary executables\n' >&2
      return 1
    }
  rm -f -- "${probe_dir}/media-core" "${probe_dir}/media-agent" 2>/dev/null \
    || {
      printf '[streamserver-native-install] WARNING: failed to remove security probe executables\n' >&2
      return 1
    }
  if ! rmdir -- "${probe_dir}" 2>/dev/null; then
    printf '[streamserver-native-install] WARNING: security probe directory was not empty during cleanup\n' >&2
    return 1
  fi
  sync -f "${SECURITY_PROBE_RUNTIME_ROOT}" >/dev/null 2>&1 || true
  clear_security_probe_state
}

cleanup_locked_package_staging() {
  local staging_dir="${LOCKED_PACKAGE_STAGING_DIR:-}"
  local expected_identity="${LOCKED_PACKAGE_STAGING_IDENTITY:-}"
  local state_root
  [ -n "${staging_dir}" ] || return 0
  state_root="$(admin_handoff_state_root_path 2>/dev/null)" || return 1
  case "${staging_dir}" in
    "${state_root}"/package-staging.*) ;;
    *)
      printf '[streamserver-native-install] WARNING: refused to clean an unexpected package staging path\n' >&2
      return 1
      ;;
  esac
  if [ ! -e "${staging_dir}" ] && [ ! -L "${staging_dir}" ]; then
    LOCKED_PACKAGE_STAGING_DIR=""
    LOCKED_PACKAGE_STAGING_IDENTITY=""
    return 0
  fi
  [ ! -L "${staging_dir}" ] && [ -d "${staging_dir}" ] \
    && [ "$(stat -Lc '%d:%i' -- "${staging_dir}" 2>/dev/null || true)" = "${expected_identity}" ] \
    && [ "$(stat -c '%u:%a' -- "${staging_dir}" 2>/dev/null || true)" = "0:700" ] \
    || {
      printf '[streamserver-native-install] WARNING: refused to clean a replaced package staging directory\n' >&2
      return 1
    }
  rm -rf -- "${staging_dir}" || return 1
  sync -f "${state_root}" >/dev/null 2>&1 || true
  LOCKED_PACKAGE_STAGING_DIR=""
  LOCKED_PACKAGE_STAGING_IDENTITY=""
}

cleanup_upgrade_boot_fence_lease_only() {
  local lease="${UPGRADE_BOOT_FENCE_LEASE:-}"
  local lease_fd="${UPGRADE_BOOT_FENCE_LEASE_FD:-}"
  local expected_lease
  if [ -n "${lease_fd}" ]; then
    [[ "${lease_fd}" =~ ^[0-9]+$ ]] || return 1
    exec {UPGRADE_BOOT_FENCE_LEASE_FD}>&- || return 1
    UPGRADE_BOOT_FENCE_LEASE_FD=""
  fi
  [ -n "${lease}" ] || return 0
  expected_lease="$(upgrade_boot_fence_lease_path 2>/dev/null)" || return 1
  [ "${lease}" = "${expected_lease}" ] || return 1
  if [ -e "${lease}" ] || [ -L "${lease}" ]; then
    [ ! -L "${lease}" ] && [ -f "${lease}" ] \
      && [ "$(stat -c '%u:%a:%h' -- "${lease}" 2>/dev/null || true)" = 0:600:1 ] \
      || return 1
    rm -f -- "${lease}" || return 1
  fi
  UPGRADE_BOOT_FENCE_LEASE=""
}

cleanup_admin_password() {
  local temporary_file
  unset ADMIN_PASSWORD
  INITIAL_ADMIN_PASSWORD_READY=0
  for temporary_file in "${INSTALLER_TEMP_FILES[@]}"; do
    [ -z "${temporary_file}" ] || rm -f -- "${temporary_file}" 2>/dev/null || true
  done
}

cleanup_installer_ephemeral_state() {
  cleanup_admin_password
  cleanup_security_probe_binaries || true
  case "${UPGRADE_TRANSACTION_STATE:-none}" in
    presealed|armed) ;;
    *) cleanup_upgrade_boot_fence_lease_only || true ;;
  esac
  cleanup_locked_package_staging || true
}

handle_admin_password_signal() {
  local exit_status="$1"
  cleanup_installer_ephemeral_state
  exit "${exit_status}"
}

trap cleanup_installer_ephemeral_state EXIT
trap 'handle_admin_password_signal 129' HUP
trap 'handle_admin_password_signal 130' INT
trap 'handle_admin_password_signal 143' TERM

log() {
  printf '[streamserver-native-install] %s\n' "$*"
}

fail() {
  cleanup_installer_ephemeral_state
  printf '[streamserver-native-install] ERROR: %s\n' "$*" >&2
  exit 1
}

secure_installer_tmp_root() {
  local tmp_root
  local normalized
  local resolved
  # A privileged installer must never trust caller-controlled TMPDIR.  Keep
  # every security inventory in the same root-only runtime directory used by
  # the upgrade lease.  EUID is shell-owned and cannot be forged by replacing
  # the `id` command in a package or test double.
  if [ "${EUID}" -eq 0 ] && [ "${EMULATED_SECURITY_METADATA:-0}" -ne 1 ]; then
    tmp_root=/run/streamserver-native-installer
    if [ ! -e "${tmp_root}" ] && [ ! -L "${tmp_root}" ]; then
      install -d -o root -g root -m 0700 -- "${tmp_root}" \
        || fail "cannot create the secure native installer runtime directory"
    fi
    [ ! -L "${tmp_root}" ] && [ -d "${tmp_root}" ] \
      && [ "$(stat -c '%u:%g:%a' -- "${tmp_root}" 2>/dev/null || true)" = 0:0:700 ] \
      || fail "secure native installer runtime directory is unsafe"
  else
    tmp_root="${TMPDIR:-/tmp}"
    [ ! -L "${tmp_root}" ] && [ -d "${tmp_root}" ] \
      && [ -w "${tmp_root}" ] && [ -x "${tmp_root}" ] \
      || fail "native installer temporary directory is unavailable"
    normalized="$(realpath -ms -- "${tmp_root}" 2>/dev/null)" \
      && resolved="$(realpath -e -- "${tmp_root}" 2>/dev/null)" \
      && [ "${normalized}" = "${resolved}" ] \
      || fail "native installer temporary directory contains a symbolic link"
    tmp_root="${resolved}"
  fi
  printf '%s' "${tmp_root}"
}

require_cmd() {
  command -v "$1" >/dev/null 2>&1 || fail "缺少命令: $1"
}

manifest_keys() {
  printf '%s\n' \
    BUNDLE_VERSION BUNDLE_VARIANT BUNDLE_GPU_SUPPORT \
    BUNDLE_WORKER_SUPPORT BUNDLE_POSTGRES_RUNTIME DEPLOY_MODE \
    MEDIA_CORE_BINARY_PATH MEDIA_AGENT_BINARY_PATH \
    MEDIA_GATEWAY_BINARY_PATH STREAMSERVER_CONFIG_BINARY_PATH \
    MEDIA_CORE_UI_PATH FFMPEG_CPU_BINARY_PATH FFPROBE_CPU_BINARY_PATH \
    FFMPEG_CPU_LIB_PATH FFMPEG_GPU_BINARY_PATH FFPROBE_GPU_BINARY_PATH \
    FFMPEG_GPU_LIB_PATH ZLM_BINARY_PATH ZLM_DEFAULT_PEM_PATH \
    ZLM_LIB_PATH POSTGRES_RUNTIME_PATH POSTGRES_BIN_PATH \
    POSTGRES_LIB_PATH POSTGRES_EXTENSION_MANIFEST_PATH
}

manifest_key_is_allowed() {
  case "$1" in
    BUNDLE_VERSION|BUNDLE_VARIANT|BUNDLE_GPU_SUPPORT|BUNDLE_WORKER_SUPPORT|\
    BUNDLE_POSTGRES_RUNTIME|DEPLOY_MODE|MEDIA_CORE_BINARY_PATH|\
    MEDIA_AGENT_BINARY_PATH|MEDIA_GATEWAY_BINARY_PATH|\
    STREAMSERVER_CONFIG_BINARY_PATH|MEDIA_CORE_UI_PATH|\
    FFMPEG_CPU_BINARY_PATH|FFPROBE_CPU_BINARY_PATH|FFMPEG_CPU_LIB_PATH|\
    FFMPEG_GPU_BINARY_PATH|FFPROBE_GPU_BINARY_PATH|FFMPEG_GPU_LIB_PATH|\
    ZLM_BINARY_PATH|ZLM_DEFAULT_PEM_PATH|ZLM_LIB_PATH|\
    POSTGRES_RUNTIME_PATH|POSTGRES_BIN_PATH|POSTGRES_LIB_PATH|\
    POSTGRES_EXTENSION_MANIFEST_PATH) return 0 ;;
    *) return 1 ;;
  esac
}

manifest_path_key_status() {
  case "$1" in
    *_PATH) return 0 ;;
    *) return 1 ;;
  esac
}

load_manifest() {
  local line
  local key
  local value
  local expected_key
  local -A seen=()
  [ -f "${MANIFEST_FILE}" ] && [ ! -L "${MANIFEST_FILE}" ] \
    || fail "缺少或不安全的 ${MANIFEST_FILE}"
  [ "$(stat -c '%h' -- "${MANIFEST_FILE}")" = 1 ] \
    || fail "package manifest must have exactly one hard link"
  while IFS= read -r line || [ -n "${line}" ]; do
    [[ "${line}" != *$'\r'* ]] \
      || fail "package manifest contains a carriage return"
    [[ "${line}" =~ ^([A-Z][A-Z0-9_]*)=([^[:space:]]+)$ ]] \
      || fail "package manifest contains an invalid assignment"
    key="${BASH_REMATCH[1]}"
    value="${BASH_REMATCH[2]}"
    manifest_key_is_allowed "${key}" \
      || fail "package manifest contains an unknown key: ${key}"
    [ -z "${seen[${key}]+x}" ] \
      || fail "package manifest contains a duplicate key: ${key}"
    seen["${key}"]=1
    if manifest_path_key_status "${key}"; then
      validate_package_relative_path "${value}" \
        || fail "package manifest contains an invalid relative path: ${key}"
    else
      case "${key}" in
        BUNDLE_VERSION)
          [[ "${value}" =~ ^[A-Za-z0-9._+@:-]{1,128}$ ]] \
            || fail "package manifest contains an invalid bundle version"
          ;;
        BUNDLE_VARIANT)
          case "${value}" in cpu-only|gpu-enabled|control-plane-minimal) ;; *)
            fail "package manifest contains an invalid bundle variant" ;;
          esac
          ;;
        BUNDLE_GPU_SUPPORT|BUNDLE_WORKER_SUPPORT|BUNDLE_POSTGRES_RUNTIME)
          case "${value}" in true|false) ;; *)
            fail "package manifest contains an invalid capability flag: ${key}" ;;
          esac
          ;;
        DEPLOY_MODE)
          [ "${value}" = native ] \
            || fail "package manifest must declare DEPLOY_MODE=native"
          ;;
      esac
    fi
    printf -v "${key}" '%s' "${value}"
  done <"${MANIFEST_FILE}"
  while IFS= read -r expected_key; do
    [ -n "${seen[${expected_key}]+x}" ] \
      || fail "package manifest is missing required key: ${expected_key}"
  done < <(manifest_keys)
  [ "${#seen[@]}" -eq 24 ] \
    || fail "package manifest field count is invalid"
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
        INSTALL_ROLE_WAS_EXPLICIT=1
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
        INSTANCE_NAME_WAS_EXPLICIT=1
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
  require_cmd systemd-run
  require_cmd sed
  require_cmd awk
  require_cmd cmp
  require_cmd sort
  require_cmd uniq
  require_cmd flock
  require_cmd install
  require_cmd realpath
  require_cmd runuser
  require_cmd stat
  require_cmd sync
  require_cmd timeout
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

validate_package_relative_path() {
  local relative_path="$1"
  [ -n "${relative_path}" ] || return 1
  [[ "${relative_path}" != /* ]] || return 1
  [[ "${relative_path}" != . ]] && [[ "${relative_path}" != .. ]] || return 1
  [[ "${relative_path}" != ../* ]] || return 1
  [[ "${relative_path}" != */../* ]] && [[ "${relative_path}" != */.. ]] || return 1
  [[ "${relative_path}" =~ ^[A-Za-z0-9._+/-]+$ ]]
}

verified_package_checksum_for_path() {
  local relative_path="$1"
  local checksum_file="${VERIFIED_PACKAGE_CHECKSUM_SNAPSHOT:-}"
  local -a matches=()
  validate_package_relative_path "${relative_path}" || fail "package executable path is invalid"
  [ -f "${checksum_file}" ] && [ ! -L "${checksum_file}" ] \
    || fail "verified SHA256SUMS snapshot is unavailable"
  mapfile -t matches < <(
    awk -v plain="${relative_path}" -v dotted="./${relative_path}" '
      NF == 2 &&
      length($1) == 64 &&
      $1 !~ /[^0-9A-Fa-f]/ &&
      ($2 == plain || $2 == dotted || $2 == "*" plain || $2 == "*" dotted) {
        print tolower($1)
      }
    ' "${checksum_file}"
  )
  [ "${#matches[@]}" -eq 1 ] || fail "package executable checksum must appear exactly once in SHA256SUMS"
  [[ "${matches[0]}" =~ ^[0-9a-fA-F]{64}$ ]] || fail "package executable checksum must be a SHA256 digest"
  printf '%s' "${matches[0],,}"
}

verify_package_checksums() {
  local checksum_file="${PACKAGE_ROOT}/SHA256SUMS"
  local checksum_snapshot
  local checksum_snapshot_root
  local checksum_identity_before
  local checksum_snapshot_identity
  local checksum_snapshot_sha256
  [ -f "${checksum_file}" ] && [ ! -L "${checksum_file}" ] || fail "缺少或不安全的 SHA256SUMS"
  [ "$(stat -c '%h' -- "${checksum_file}")" = 1 ] || fail "SHA256SUMS must have exactly one hard link"
  admin_handoff_no_symlink_boundary_status "${checksum_file}" \
    || fail "SHA256SUMS path contains a symbolic link"
  checksum_identity_before="$(stat -Lc '%d:%i' -- "${checksum_file}")" || fail "cannot capture SHA256SUMS identity"
  checksum_snapshot_root="$(secure_installer_tmp_root)"
  checksum_snapshot="$(umask 077; mktemp "${checksum_snapshot_root%/}/streamserver-native-sha256.XXXXXX")" \
    || fail "cannot allocate a verified SHA256SUMS snapshot"
  INSTALLER_TEMP_FILES+=("${checksum_snapshot}")
  cp --reflink=never -- "${checksum_file}" "${checksum_snapshot}" \
    || fail "cannot snapshot SHA256SUMS"
  chmod 0600 "${checksum_snapshot}"
  [ -f "${checksum_snapshot}" ] && [ ! -L "${checksum_snapshot}" ] \
    || fail "verified SHA256SUMS snapshot is unsafe"
  [ "$(stat -c '%u:%a:%h' -- "${checksum_snapshot}")" = "$(id -u):600:1" ] \
    || fail "verified SHA256SUMS snapshot metadata is unsafe"
  checksum_snapshot_identity="$(stat -Lc '%d:%i' -- "${checksum_snapshot}")" \
    || fail "cannot capture verified SHA256SUMS snapshot identity"
  checksum_snapshot_sha256="$(sha256sum "${checksum_snapshot}" | awk '{print $1}')" \
    || fail "cannot hash verified SHA256SUMS snapshot"
  [ "$(stat -Lc '%d:%i' -- "${checksum_file}")" = "${checksum_identity_before}" ] \
    || fail "SHA256SUMS changed while being snapshotted"
  [ "$(sha256sum "${checksum_file}" | awk '{print $1}')" = "${checksum_snapshot_sha256}" ] \
    || fail "SHA256SUMS content changed while being snapshotted"
  VERIFIED_PACKAGE_CHECKSUM_FILE_IDENTITY="${checksum_identity_before}"
  VERIFIED_PACKAGE_CHECKSUM_FILE_SHA256="${checksum_snapshot_sha256}"
  VERIFIED_PACKAGE_CHECKSUM_SNAPSHOT="${checksum_snapshot}"
  VERIFIED_PACKAGE_CHECKSUM_SNAPSHOT_IDENTITY="${checksum_snapshot_identity}"
  assert_verified_package_checksum_manifest_unchanged
  assert_package_checksum_coverage "${checksum_snapshot}"
  assert_control_tree_safe "${PACKAGE_ROOT}" structural
  (cd "${PACKAGE_ROOT}" && sha256sum --strict -c "${checksum_snapshot}" >/dev/null) \
    || fail "package SHA256SUMS verification failed"
  assert_verified_package_checksum_manifest_unchanged
  VERIFIED_PACKAGE_TREE_FINGERPRINT="$(upgrade_entry_fingerprint "${PACKAGE_ROOT}" content)" \
    || fail "cannot fingerprint the verified native package"
  [[ "${VERIFIED_PACKAGE_TREE_FINGERPRINT}" =~ ^[0-9a-f]{64}$ ]] \
    || fail "verified native package fingerprint is invalid"
  VERIFIED_PACKAGE_CORE_SHA256="$(verified_package_checksum_for_path "${MEDIA_CORE_BINARY_PATH:-}")"
  if [ -n "${MEDIA_AGENT_BINARY_PATH:-}" ]; then
    VERIFIED_PACKAGE_AGENT_SHA256="$(verified_package_checksum_for_path "${MEDIA_AGENT_BINARY_PATH}")"
  else
    VERIFIED_PACKAGE_AGENT_SHA256=""
  fi
  log "包内 SHA256SUMS 校验通过"
}

assert_package_checksum_coverage() {
  local checksum_file="$1"
  local checksum_paths
  local package_paths
  local duplicate_path
  local tmp_root
  tmp_root="$(secure_installer_tmp_root)"
  checksum_paths="$(mktemp "${tmp_root%/}/streamserver-checksum-paths.XXXXXX")" \
    || fail "cannot allocate package checksum inventory"
  package_paths="$(mktemp "${tmp_root%/}/streamserver-package-paths.XXXXXX")" \
    || {
      rm -f -- "${checksum_paths}" >/dev/null 2>&1 || true
      fail "cannot allocate package file inventory"
    }
  chmod 600 "${checksum_paths}" "${package_paths}"
  if ! awk '
    {
      if ($0 !~ /^[0-9A-Fa-f]{64} [ *][A-Za-z0-9._+\/-]+$/) exit 2
      path = substr($0, 67)
      if (path ~ /^\.\//) path = substr(path, 3)
      if (path == "" || path == "." || path == ".." ||
          path ~ /^\// || path ~ /^\.\.\// || path ~ /\/\.\.\// ||
          path ~ /\/\.\.$/) exit 2
      print path
    }
  ' "${checksum_file}" | LC_ALL=C sort >"${checksum_paths}"; then
    rm -f -- "${checksum_paths}" "${package_paths}" >/dev/null 2>&1 || true
    fail "package SHA256SUMS contains an invalid path record"
  fi
  duplicate_path="$(uniq -d "${checksum_paths}" | head -n 1)"
  [ -z "${duplicate_path}" ] || {
    rm -f -- "${checksum_paths}" "${package_paths}" >/dev/null 2>&1 || true
    fail "package SHA256SUMS contains a duplicate path: ${duplicate_path}"
  }
  if ! find -P "${PACKAGE_ROOT}" -type f ! -path "${PACKAGE_ROOT}/SHA256SUMS" \
    -printf '%P\n' | LC_ALL=C sort >"${package_paths}"; then
    rm -f -- "${checksum_paths}" "${package_paths}" >/dev/null 2>&1 || true
    fail "cannot enumerate native package files"
  fi
  if ! cmp -s -- "${checksum_paths}" "${package_paths}"; then
    rm -f -- "${checksum_paths}" "${package_paths}" >/dev/null 2>&1 || true
    fail "package SHA256SUMS does not cover the exact regular-file inventory"
  fi
  rm -f -- "${checksum_paths}" "${package_paths}" \
    || fail "cannot remove package checksum inventory"
}

stage_verified_package_root() {
  local source_root="${PACKAGE_ROOT}"
  local expected_checksum="${VERIFIED_PACKAGE_CHECKSUM_FILE_SHA256}"
  local expected_tree="${VERIFIED_PACKAGE_TREE_FINGERPRINT}"
  local state_root
  local source_before
  local source_after
  local staged_tree
  local staged_checksum
  [ "$(id -u)" -eq 0 ] \
    || fail "native package staging requires root"
  [[ "${expected_checksum}" =~ ^[0-9a-f]{64}$ ]] \
    && [[ "${expected_tree}" =~ ^[0-9a-f]{64}$ ]] \
    || fail "verified package identity is unavailable for locked staging"
  [ -d "${source_root}" ] && [ ! -L "${source_root}" ] \
    || fail "verified package root is not a real directory"
  assert_control_tree_safe "${source_root}" structural
  assert_verified_package_checksum_manifest_unchanged
  source_before="$(upgrade_entry_fingerprint "${source_root}" content)" \
    || fail "cannot fingerprint the package before locked staging"
  [ "${source_before}" = "${expected_tree}" ] \
    || fail "native package changed after checksum verification"

  state_root="$(admin_handoff_state_root_path)"
  admin_handoff_assert_secure_directory "${state_root}"
  LOCKED_PACKAGE_STAGING_DIR="$(umask 077; mktemp -d "${state_root}/package-staging.XXXXXX")" \
    || fail "cannot allocate root-only native package staging"
  chmod 700 "${LOCKED_PACKAGE_STAGING_DIR}"
  LOCKED_PACKAGE_STAGING_IDENTITY="$(stat -Lc '%d:%i' -- "${LOCKED_PACKAGE_STAGING_DIR}")" \
    || fail "cannot capture native package staging identity"
  cp -a --no-dereference --reflink=auto -- \
    "${source_root}/." "${LOCKED_PACKAGE_STAGING_DIR}/" \
    || fail "cannot copy the verified package into root-only staging"

  source_after="$(upgrade_entry_fingerprint "${source_root}" content)" \
    || fail "cannot fingerprint the package after locked staging"
  [ "${source_after}" = "${expected_tree}" ] \
    || fail "native package changed while being copied into locked staging"
  staged_tree="$(upgrade_entry_fingerprint "${LOCKED_PACKAGE_STAGING_DIR}" content)" \
    || fail "cannot fingerprint the root-only staged package"
  [ "${staged_tree}" = "${expected_tree}" ] \
    || fail "root-only staged package differs from the verified package"
  staged_checksum="$(sha256sum "${LOCKED_PACKAGE_STAGING_DIR}/SHA256SUMS" | awk '{print $1}')" \
    || fail "cannot hash the staged package checksum manifest"
  [ "${staged_checksum}" = "${expected_checksum}" ] \
    || fail "staged package checksum manifest differs from the verified manifest"
  (cd "${LOCKED_PACKAGE_STAGING_DIR}" \
    && sha256sum --strict -c "${VERIFIED_PACKAGE_CHECKSUM_SNAPSHOT}" >/dev/null) \
    || fail "root-only staged package checksum verification failed"

  chown -R -h root:root -- "${LOCKED_PACKAGE_STAGING_DIR}" \
    || fail "cannot seal root-only package staging ownership"
  chmod 700 "${LOCKED_PACKAGE_STAGING_DIR}" \
    || fail "cannot seal root-only package staging mode"
  assert_control_tree_safe "${LOCKED_PACKAGE_STAGING_DIR}" strict
  PACKAGE_ROOT="${LOCKED_PACKAGE_STAGING_DIR}"
  MANIFEST_FILE="${PACKAGE_ROOT}/package-manifest.env"
  assert_package_checksum_coverage "${VERIFIED_PACKAGE_CHECKSUM_SNAPSHOT}"
  [ "$(upgrade_entry_fingerprint "${PACKAGE_ROOT}" content)" = "${expected_tree}" ] \
    || fail "sealed package staging identity changed"
  sync -f "${state_root}" \
    || fail "cannot publish the root-only package staging"
}

assert_locked_package_identity() {
  local expected_checksum="$1"
  local expected_tree="$2"
  [[ "${expected_checksum}" =~ ^[0-9a-f]{64}$ ]] \
    && [[ "${expected_tree}" =~ ^[0-9a-f]{64}$ ]] \
    || fail "locked package plan identity is invalid"
  [ "${VERIFIED_PACKAGE_CHECKSUM_FILE_SHA256}" = "${expected_checksum}" ] \
    && [ "${VERIFIED_PACKAGE_TREE_FINGERPRINT}" = "${expected_tree}" ] \
    || fail "locked package does not match the pre-lock verified identity"
  [ "$(stat -c '%u:%a' -- "${PACKAGE_ROOT}")" = "0:700" ] \
    || fail "locked package root metadata is unsafe"
  assert_control_tree_safe "${PACKAGE_ROOT}" strict
}

assert_verified_package_checksum_manifest_unchanged() {
  local checksum_file="${PACKAGE_ROOT}/SHA256SUMS"
  local checksum_snapshot="${VERIFIED_PACKAGE_CHECKSUM_SNAPSHOT:-}"
  [[ "${VERIFIED_PACKAGE_CHECKSUM_FILE_IDENTITY}" =~ ^[0-9]+:[0-9]+$ ]] || fail "verified SHA256SUMS identity is unavailable"
  [[ "${VERIFIED_PACKAGE_CHECKSUM_FILE_SHA256}" =~ ^[0-9a-f]{64}$ ]] || fail "verified SHA256SUMS digest is unavailable"
  [[ "${VERIFIED_PACKAGE_CHECKSUM_SNAPSHOT_IDENTITY}" =~ ^[0-9]+:[0-9]+$ ]] || fail "verified SHA256SUMS snapshot identity is unavailable"
  [ -f "${checksum_file}" ] && [ ! -L "${checksum_file}" ] || fail "verified SHA256SUMS was replaced"
  [ "$(stat -c '%h' -- "${checksum_file}")" = 1 ] || fail "verified SHA256SUMS hard-link count changed"
  [ "$(stat -Lc '%d:%i' -- "${checksum_file}")" = "${VERIFIED_PACKAGE_CHECKSUM_FILE_IDENTITY}" ] || fail "verified SHA256SUMS identity changed"
  [ "$(sha256sum "${checksum_file}" | awk '{print $1}')" = "${VERIFIED_PACKAGE_CHECKSUM_FILE_SHA256}" ] || fail "verified SHA256SUMS content changed"
  [ -f "${checksum_snapshot}" ] && [ ! -L "${checksum_snapshot}" ] \
    || fail "verified SHA256SUMS snapshot was replaced"
  [ "$(stat -c '%u:%a:%h' -- "${checksum_snapshot}")" = "$(id -u):600:1" ] \
    || fail "verified SHA256SUMS snapshot metadata changed"
  [ "$(stat -Lc '%d:%i' -- "${checksum_snapshot}")" = "${VERIFIED_PACKAGE_CHECKSUM_SNAPSHOT_IDENTITY}" ] \
    || fail "verified SHA256SUMS snapshot identity changed"
  [ "$(sha256sum "${checksum_snapshot}" | awk '{print $1}')" = "${VERIFIED_PACKAGE_CHECKSUM_FILE_SHA256}" ] \
    || fail "verified SHA256SUMS snapshot content changed"
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

is_strong_url_safe_secret() {
  local value="${1:-}"
  [[ "${value}" =~ ^[A-Za-z0-9._~-]+$ ]] \
    && [ "${#value}" -ge 32 ] \
    && [ "${#value}" -le 256 ]
}

generate_distinct_secret() {
  local candidate existing
  local collision
  local attempts=0
  while [ "${attempts}" -lt 32 ]; do
    attempts=$((attempts + 1))
    candidate="$(generate_secret)" || fail "cannot generate a local service credential"
    is_strong_url_safe_secret "${candidate}" \
      || fail "generated local service credential is invalid"
    collision=0
    for existing in "$@"; do
      if [ -n "${existing}" ] && [ "${candidate}" = "${existing}" ]; then
        collision=1
        break
      fi
    done
    if [ "${collision}" -eq 0 ]; then
      printf '%s' "${candidate}"
      return 0
    fi
  done
  fail "cannot generate distinct local service credentials"
}

generate_one_time_admin_password() {
  local password
  password="$(openssl rand -hex 18)" || fail "无法生成一次性管理员初始密码"
  [[ "${password}" =~ ^[0-9a-f]{36}$ ]] || fail "一次性管理员初始密码生成结果无效"
  printf '%s' "${password}"
}

generate_admin_handoff_id() {
  local handoff_id
  handoff_id="$(generate_uuid)" || fail "无法生成管理员密码交付 ID"
  [[ "${handoff_id}" =~ ^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$ ]] \
    || fail "管理员密码交付 ID 格式无效"
  printf '%s' "${handoff_id}"
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

env_key_occurrence_count() {
  local env_file="$1"
  local key="$2"
  awk -v key="${key}" '
    {
      line = $0
      sub(/^[[:space:]]*/, "", line)
    }
    index(line, key "=") == 1 { count += 1 }
    END { print count + 0 }
  ' "${env_file}"
}

strict_identity_env_value() {
  local env_file="$1"
  local key="$2"
  local raw
  local value
  [ ! -L "${env_file}" ] && [ -f "${env_file}" ] \
    || fail "upgrade identity environment must be a regular file, not a symbolic link"
  raw="$(awk -v key="${key}" '
    {
      line = $0
      sub(/^[[:space:]]*/, "", line)
    }
    index(line, key "=") == 1 {
      count += 1
      value = substr(line, length(key) + 2)
    }
    END {
      if (count != 1) exit 1
      print value
    }
  ' "${env_file}")" \
    || fail "upgrade requires ${key} to appear exactly once in the existing environment"
  case "${raw}" in
    \'*\')
      value="${raw:1:${#raw}-2}"
      [[ "${value}" != *"'"* ]] \
        || fail "upgrade ${key} uses unsupported quote syntax"
      ;;
    *"'"*) fail "upgrade ${key} uses unsupported quote syntax" ;;
    *) value="${raw}" ;;
  esac
  [[ "${value}" =~ ^[A-Za-z0-9_.@-]+$ ]] \
    || fail "upgrade ${key} contains an invalid identity value"
  printf '%s' "${value}"
}

require_unique_env_key() {
  local env_file="$1"
  local key="$2"
  [ "$(env_key_occurrence_count "${env_file}" "${key}")" = "1" ] \
    || fail "upgrade requires exactly one ${key} entry in the existing environment"
}

unit_basename_for_instance() {
  case "$1" in
    ss-*) printf '%s' "$1" ;;
    *) printf 'ss-%s' "$1" ;;
  esac
}

trusted_systemd_path_status() {
  local path="$1"
  local expected_type="$2"
  local mode
  [ ! -L "${path}" ] || return 1
  case "${expected_type}" in
    directory) [ -d "${path}" ] || return 1 ;;
    file) [ -f "${path}" ] || return 1 ;;
    *) return 1 ;;
  esac
  [ "$(stat -c '%u' -- "${path}" 2>/dev/null)" = "0" ] || return 1
  mode="$(stat -c '%a' -- "${path}" 2>/dev/null)" || return 1
  (( (8#${mode} & 8#022) == 0 ))
}

trusted_unit_exact_line() {
  local file="$1"
  local expected="$2"
  [ "$(grep -Fxc -- "${expected}" "${file}" 2>/dev/null || true)" = "1" ]
}

trusted_unit_single_directive() {
  local file="$1"
  local directive="$2"
  local expected="$3"
  local actual
  actual="$(awk -v prefix="${directive}=" '
    index($0, prefix) == 1 {count += 1; value = $0}
    END {if (count != 1) exit 1; print value}
  ' "${file}")" || return 1
  [ "${actual}" = "${expected}" ]
}

trusted_unit_exec_line() {
  local file="$1"
  local expected="$2"
  local actual
  actual="$(awk '/^ExecStart=/{count += 1; value = $0} END {if (count != 1) exit 1; print value}' "${file}")" \
    || return 1
  case "${actual}" in
    "${expected}"|"${expected} "*) return 0 ;;
    *) return 1 ;;
  esac
}

trusted_unit_environment_line_count() {
  local file="$1"
  awk 'index($0, "Environment=") == 1 {count += 1} END {print count + 0}' \
    "${file}"
}

validate_trusted_upgrade_unit_fragment() {
  local kind="$1"
  local file="${SYSTEMD_UNIT_ROOT}/${UNIT_BASENAME}-${kind}.service"
  local expected_exec
  local legacy_exec=""
  local expected_environment_count=0
  local expected_working_directory="${INSTALL_DIR}"
  case "${kind}" in
    core)
      expected_exec="ExecStart=/usr/bin/env STREAMSERVER_ENV=production STREAMSERVER_UI_DIR=${INSTALL_DIR}/ui ${INSTALL_DIR}/bin/media-core"
      legacy_exec="ExecStart=${INSTALL_DIR}/bin/media-core"
      expected_environment_count=2
      ;;
    agent)
      expected_exec="ExecStart=/usr/bin/env STREAMSERVER_ENV=production ${INSTALL_DIR}/bin/media-agent"
      legacy_exec="ExecStart=${INSTALL_DIR}/bin/media-agent"
      expected_environment_count=1
      ;;
    zlm)
      expected_exec="ExecStart=${INSTALL_DIR}/bin/zlm-mediaserver"
      expected_working_directory="${INSTALL_DIR}/runtime/zlm"
      ;;
    postgres) expected_exec="ExecStart=${INSTALL_DIR}/bin/postgres" ;;
    *) fail "unsupported native systemd component identity: ${kind}" ;;
  esac
  trusted_systemd_path_status "${file}" file \
    || fail "upgrade requires a secure root-owned systemd ${kind} fragment"
  [ ! -e "${file}.d" ] && [ ! -L "${file}.d" ] \
    || fail "upgrade refuses systemd drop-ins for native ${kind} identity verification"
  trusted_unit_single_directive \
    "${file}" EnvironmentFile "EnvironmentFile=${INSTALL_DIR}/.env" \
    || fail "trusted systemd ${kind} unit must contain exactly one matching EnvironmentFile"
  trusted_unit_single_directive \
    "${file}" WorkingDirectory "WorkingDirectory=${expected_working_directory}" \
    || fail "trusted systemd ${kind} unit has an unexpected WorkingDirectory"
  if [ -n "${legacy_exec}" ]; then
    if trusted_unit_single_directive "${file}" ExecStart "${expected_exec}"; then
      [ "$(trusted_unit_environment_line_count "${file}")" -eq 0 ] \
        || fail "trusted systemd ${kind} unit mixes pinned ExecStart with legacy Environment directives"
    elif trusted_unit_single_directive "${file}" ExecStart "${legacy_exec}"; then
      [ "$(trusted_unit_environment_line_count "${file}")" -eq "${expected_environment_count}" ] \
        && trusted_unit_exact_line \
          "${file}" 'Environment=STREAMSERVER_ENV=production' \
        || fail "trusted legacy systemd ${kind} unit has unexpected Environment directives"
      if [ "${kind}" = core ]; then
        trusted_unit_exact_line \
          "${file}" "Environment=STREAMSERVER_UI_DIR=${INSTALL_DIR}/ui" \
          || fail "trusted legacy systemd core unit has an unexpected UI environment directive"
      fi
    else
      fail "trusted systemd ${kind} unit has an unexpected ExecStart"
    fi
  else
    trusted_unit_exec_line "${file}" "${expected_exec}" \
      || fail "trusted systemd ${kind} unit has an unexpected ExecStart"
  fi
  trusted_unit_single_directive \
    "${file}" WantedBy "WantedBy=${UNIT_BASENAME}.target" \
    || fail "trusted systemd ${kind} unit is not attached to the expected target"
  if [ "${kind}" = agent ]; then
    case "${INSTALL_ROLE}" in
      *-gpu)
        trusted_unit_exact_line "${file}" 'ExecStartPre=/usr/bin/nvidia-smi' \
          && [ "$(grep -Fc 'h264_nvenc' "${file}" 2>/dev/null || true)" = "1" ] \
          && [ "$(grep -Fc 'hevc_nvenc' "${file}" 2>/dev/null || true)" = "1" ] \
          || fail "trusted agent unit does not prove the requested GPU topology"
        ;;
      *-cpu)
        if grep -Eq 'nvidia-smi|h264_nvenc|hevc_nvenc' "${file}"; then
          fail "trusted agent unit has GPU preflight for a requested CPU topology"
        fi
        ;;
    esac
  fi
}

discover_explicit_upgrade_unit_identity() {
  local kind
  local core_count=0
  local agent_count=0
  local zlm_count=0
  local postgres_count=0
  trusted_systemd_path_status "${SYSTEMD_UNIT_ROOT}" directory \
    || fail "upgrade requires a secure root-owned systemd unit directory"
  trusted_systemd_path_status "${SYSTEMD_UNIT_ROOT}/${UNIT_BASENAME}.target" file \
    || fail "upgrade requires a secure root-owned native systemd target"
  [ ! -e "${SYSTEMD_UNIT_ROOT}/${UNIT_BASENAME}.target.d" ] \
    && [ ! -L "${SYSTEMD_UNIT_ROOT}/${UNIT_BASENAME}.target.d" ] \
    || fail "upgrade refuses systemd target drop-ins during identity verification"
  trusted_unit_single_directive \
    "${SYSTEMD_UNIT_ROOT}/${UNIT_BASENAME}.target" \
    WantedBy 'WantedBy=multi-user.target' \
    || fail "trusted native target has an unexpected install topology"
  for kind in core agent zlm postgres; do
    if [ -e "${SYSTEMD_UNIT_ROOT}/${UNIT_BASENAME}-${kind}.service" ] \
      || [ -L "${SYSTEMD_UNIT_ROOT}/${UNIT_BASENAME}-${kind}.service" ]; then
      validate_trusted_upgrade_unit_fragment "${kind}"
      case "${kind}" in
        core) core_count=1 ;;
        agent) agent_count=1 ;;
        zlm) zlm_count=1 ;;
        postgres) postgres_count=1 ;;
      esac
    fi
  done
  TRUSTED_UNIT_BASENAME="${UNIT_BASENAME}"
  TRUSTED_CORE_UNIT_COUNT="${core_count}"
  TRUSTED_AGENT_UNIT_COUNT="${agent_count}"
  TRUSTED_ZLM_UNIT_COUNT="${zlm_count}"
  TRUSTED_POSTGRES_UNIT_COUNT="${postgres_count}"
}

validate_role_against_trusted_units() {
  local role="$1"
  case "${role}" in
    control-plane)
      [ "${TRUSTED_CORE_UNIT_COUNT}" -eq 1 ] \
        && [ "${TRUSTED_AGENT_UNIT_COUNT}" -eq 0 ] \
        && [ "${TRUSTED_ZLM_UNIT_COUNT}" -eq 0 ]
      ;;
    worker-host-cpu|worker-host-gpu)
      [ "${TRUSTED_CORE_UNIT_COUNT}" -eq 0 ] \
        && [ "${TRUSTED_AGENT_UNIT_COUNT}" -eq 1 ] \
        && [ "${TRUSTED_ZLM_UNIT_COUNT}" -eq 1 ] \
        && [ "${TRUSTED_POSTGRES_UNIT_COUNT}" -eq 0 ]
      ;;
    all-in-one-host-cpu|all-in-one-host-gpu)
      [ "${TRUSTED_CORE_UNIT_COUNT}" -eq 1 ] \
        && [ "${TRUSTED_AGENT_UNIT_COUNT}" -eq 1 ] \
        && [ "${TRUSTED_ZLM_UNIT_COUNT}" -eq 1 ]
      ;;
    *) return 1 ;;
  esac || fail "persisted INSTALL_ROLE does not match root-managed systemd units"
}

require_upgrade_systemd_identity() {
  local env_file="$1"
  local key="$2"
  local expected="$3"
  local actual
  actual="$(strict_identity_env_value "${env_file}" "${key}")"
  [ "${actual}" = "${expected}" ] \
    || fail "upgrade ${key} identity does not match the existing instance"
}

prepare_upgrade_cli_identity() {
  prepare_upgrade_cli_lock_identity
  discover_explicit_upgrade_unit_identity
  validate_role_against_trusted_units "${INSTALL_ROLE}"
}

prepare_upgrade_cli_lock_identity() {
  [ ! -L "${INSTALL_DIR}" ] && [ -d "${INSTALL_DIR}" ] \
    || fail "--upgrade requires a real installation directory"
  [ "${INSTALL_ROLE_WAS_EXPLICIT}" -eq 1 ] \
    || fail "--upgrade requires an explicit --role"
  [ "${INSTANCE_NAME_WAS_EXPLICIT}" -eq 1 ] \
    || fail "--upgrade requires an explicit --instance-name"
  [ "$(sanitize_instance_name "${INSTANCE_NAME}")" = "${INSTANCE_NAME}" ] \
    && [ -n "${INSTANCE_NAME}" ] \
    || fail "--instance-name is invalid"
  validate_role_supported "${INSTALL_ROLE}"
  UNIT_BASENAME="$(unit_basename_for_instance "${INSTANCE_NAME}")"
}

validate_upgrade_recovery_cli_identity() {
  local snapshot_env="${UPGRADE_TRANSACTION_DIR}/install/.env"
  local state_file="${UPGRADE_TRANSACTION_DIR}/install-state/.env.state"
  [ ! -L "${state_file}" ] && [ -f "${state_file}" ] \
    && [ "$(<"${state_file}")" = file ] \
    || fail "native upgrade recovery environment state is invalid"
  [ "$(strict_identity_env_value "${snapshot_env}" INSTALL_ROLE)" = "${INSTALL_ROLE}" ] \
    || fail "upgrade recovery --role does not match the durable transaction"
  [ "$(strict_identity_env_value "${snapshot_env}" INSTANCE_NAME)" = "${INSTANCE_NAME}" ] \
    || fail "upgrade recovery --instance-name does not match the durable transaction"
  require_upgrade_systemd_identity \
    "${snapshot_env}" SYSTEMD_TARGET "${UNIT_BASENAME}.target"
  require_upgrade_systemd_identity \
    "${snapshot_env}" SYSTEMD_CORE_UNIT "${UNIT_BASENAME}-core.service"
  require_upgrade_systemd_identity \
    "${snapshot_env}" SYSTEMD_AGENT_UNIT "${UNIT_BASENAME}-agent.service"
  require_upgrade_systemd_identity \
    "${snapshot_env}" SYSTEMD_ZLM_UNIT "${UNIT_BASENAME}-zlm.service"
  require_upgrade_systemd_identity \
    "${snapshot_env}" SYSTEMD_POSTGRES_UNIT "${UNIT_BASENAME}-postgres.service"
}

seal_legacy_upgrade_environment() {
  local env_file="${INSTALL_DIR}/.env"
  local source_env_file="${env_file}"
  local temporary_file
  assert_control_path_not_symlink "${INSTALL_DIR}"
  [ -d "${INSTALL_DIR}" ] \
    || fail "upgrade installation root must be a directory"
  [ ! -L "${env_file}" ] && [ -f "${env_file}" ] \
    || fail "upgrade environment must be a regular file, not a symbolic link"
  if [ "${UPGRADE_TRANSACTION_STATE:-none}" = presealed ]; then
    source_env_file="${UPGRADE_TRANSACTION_DIR}/install/.env"
    [ ! -L "${source_env_file}" ] && [ -f "${source_env_file}" ] \
      || fail "native upgrade preseal environment baseline is unsafe"
  fi
  chown root:root "${INSTALL_DIR}"
  chmod 755 "${INSTALL_DIR}"
  begin_atomic_target_write "${env_file}"
  temporary_file="${LAST_INSTALLER_TEMP_FILE}"
  cp -- "${source_env_file}" "${temporary_file}"
  finish_atomic_target_write "${temporary_file}" "${env_file}" 600 root:root
}

upgrade_env_fixed_port_conflicts() {
  local env_file="$1"
  local role="$2"
  local candidate="$3"
  local check_host="${4:-true}"
  local key value range_start range_end
  local -a keys=(
    AGENT_HTTP_PORT AGENT_MANAGEMENT_PORT
    ZLM_HTTP_PORT ZLM_HTTPS_PORT ZLM_RTMP_PORT ZLM_RTMPS_PORT
    ZLM_RTSP_PORT ZLM_RTSPS_PORT ZLM_RTP_PROXY_PORT
    ZLM_RTC_SIGNALING_PORT ZLM_RTC_SIGNALING_SSL_PORT
    ZLM_RTC_ICE_PORT ZLM_RTC_ICE_TCP_PORT ZLM_RTC_PORT ZLM_RTC_TCP_PORT
    ZLM_SRT_PORT ZLM_SHELL_PORT ZLM_ONVIF_PORT
  )
  if role_has_core "${role}"; then
    keys+=(POSTGRES_PORT CORE_HTTP_PORT CORE_GRPC_PORT)
  fi
  for key in "${keys[@]}"; do
    [ "${key}" != AGENT_ZLM_HOOK_PORT ] || continue
    [ "$(env_key_occurrence_count "${env_file}" "${key}")" -eq 1 ] || continue
    value="$(existing_env_value "${env_file}" "${key}")"
    [[ "${value}" =~ ^[0-9]+$ ]] || continue
    [ "${value}" = 0 ] || [ "${value}" != "${candidate}" ] || return 0
  done
  for key in ZLM_RTP_PROXY_PORT_RANGE ZLM_RTC_PORT_RANGE; do
    [ "$(env_key_occurrence_count "${env_file}" "${key}")" -eq 1 ] || continue
    value="$(existing_env_value "${env_file}" "${key}")"
    [[ "${value}" =~ ^([0-9]+)-([0-9]+)$ ]] || continue
    range_start="${BASH_REMATCH[1]}"
    range_end="${BASH_REMATCH[2]}"
    if [ "${range_start}" -ne 0 ] \
      && [ "${candidate}" -ge "${range_start}" ] \
      && [ "${candidate}" -le "${range_end}" ]; then
      return 0
    fi
  done
  if [ "${check_host}" = true ] \
    && [ -n "$(describe_tcp_port_usage "${candidate}")" ]; then
    return 0
  fi
  return 1
}

select_upgrade_zlm_hook_port() {
  local env_file="$1"
  local role="$2"
  local current=""
  local candidate=18082
  if [ "$(env_key_occurrence_count "${env_file}" AGENT_ZLM_HOOK_PORT)" -eq 1 ]; then
    current="$(existing_env_value "${env_file}" AGENT_ZLM_HOOK_PORT)"
    if [[ "${current}" =~ ^[0-9]+$ ]] \
      && [ "${current}" -ge 1 ] && [ "${current}" -le 65535 ] \
      && ! upgrade_env_fixed_port_conflicts "${env_file}" "${role}" "${current}" false; then
      printf '%s' "${current}"
      return 0
    fi
  fi
  while [ "${candidate}" -le 65535 ]; do
    if ! upgrade_env_fixed_port_conflicts "${env_file}" "${role}" "${candidate}" true; then
      printf '%s' "${candidate}"
      return 0
    fi
    candidate=$((candidate + 1))
  done
  fail "upgrade could not allocate a loopback ZLMediaKit hook port"
}

safe_upgrade_zlm_api_secret() {
  local env_file="$1"
  local value=""
  local core_hook_secret=""
  local agent_hook_secret=""

  if [ "$(env_key_occurrence_count "${env_file}" HOOK_SHARED_SECRET)" -eq 1 ]; then
    core_hook_secret="$(existing_env_value "${env_file}" HOOK_SHARED_SECRET)"
  fi
  if [ "$(env_key_occurrence_count "${env_file}" ZLM_HOOK_SHARED_SECRET)" -eq 1 ]; then
    agent_hook_secret="$(existing_env_value "${env_file}" ZLM_HOOK_SHARED_SECRET)"
  fi
  if [ "$(env_key_occurrence_count "${env_file}" ZLM_API_SECRET)" -eq 1 ]; then
    value="$(existing_env_value "${env_file}" ZLM_API_SECRET)"
  fi
  if ! is_strong_url_safe_secret "${value}" \
    || { [ -n "${core_hook_secret}" ] && [ "${value}" = "${core_hook_secret}" ]; } \
    || { [ -n "${agent_hook_secret}" ] && [ "${value}" = "${agent_hook_secret}" ]; }; then
    generate_distinct_secret "${core_hook_secret}" "${agent_hook_secret}"
    return 0
  fi
  printf '%s' "${value}"
}

safe_upgrade_zlm_hook_secret() {
  local env_file="$1"
  local selected_api_secret="${2:-}"
  local value=""
  local core_hook_secret=""

  if [ "$(env_key_occurrence_count "${env_file}" HOOK_SHARED_SECRET)" -eq 1 ]; then
    core_hook_secret="$(existing_env_value "${env_file}" HOOK_SHARED_SECRET)"
  fi
  if [ -z "${selected_api_secret}" ] \
    && [ "$(env_key_occurrence_count "${env_file}" ZLM_API_SECRET)" -eq 1 ]; then
    selected_api_secret="$(existing_env_value "${env_file}" ZLM_API_SECRET)"
  fi
  if [ "$(env_key_occurrence_count "${env_file}" ZLM_HOOK_SHARED_SECRET)" -eq 1 ]; then
    value="$(existing_env_value "${env_file}" ZLM_HOOK_SHARED_SECRET)"
  fi
  if ! is_strong_url_safe_secret "${value}" \
    || { [ -n "${core_hook_secret}" ] && [ "${value}" = "${core_hook_secret}" ]; } \
    || { [ -n "${selected_api_secret}" ] && [ "${value}" = "${selected_api_secret}" ]; }; then
    generate_distinct_secret "${core_hook_secret}" "${selected_api_secret}"
    return 0
  fi
  printf '%s' "${value}"
}

migrate_legacy_zlm_api_endpoint() {
  local env_file="${INSTALL_DIR}/.env"
  local role
  local zlm_http_port=""
  local expected_zlm_api_base=""
  local expected_zlm_api_allow_ip_range="::1,127.0.0.1,10.0.0.0-10.255.255.255,172.16.0.0-172.31.255.255,192.168.0.0-192.168.255.255"
  local hook_port=""
  local hook_addr=""
  local hook_base=""
  local hook_secret=""
  local zlm_api_secret=""
  local remove_worker_core_hook=0
  local needs_migration=0
  local temporary_file
  local key

  role="$(strict_identity_env_value "${env_file}" INSTALL_ROLE)"
  if role_has_worker "${role}"; then
    [ "$(env_key_occurrence_count "${env_file}" ZLM_HTTP_PORT)" -eq 1 ] \
      || fail "upgrade requires ZLM_HTTP_PORT to appear exactly once before ZLM endpoint migration"
    zlm_http_port="$(existing_env_value "${env_file}" ZLM_HTTP_PORT)"
    validate_port_number ZLM_HTTP_PORT "${zlm_http_port}"
    expected_zlm_api_base="http://127.0.0.1:${zlm_http_port}"
    hook_port="$(select_upgrade_zlm_hook_port "${env_file}" "${role}")"
    hook_addr="127.0.0.1:${hook_port}"
    hook_base="http://127.0.0.1:${hook_port}/internal/zlm-hooks"
    zlm_api_secret="$(safe_upgrade_zlm_api_secret "${env_file}")"
    hook_secret="$(safe_upgrade_zlm_hook_secret "${env_file}" "${zlm_api_secret}")"
    [ "${zlm_api_secret}" != "${hook_secret}" ] \
      || fail "upgrade could not separate the ZLM API and Agent hook credentials"
    if ! role_has_core "${role}"; then
      remove_worker_core_hook=1
    fi
    for key in \
      ZLM_API_BASE ZLM_API_SECRET ZLM_API_ALLOW_IP_RANGE \
      AGENT_ZLM_HOOK_ADDR AGENT_ZLM_HOOK_PORT \
      AGENT_ZLM_HOOK_QUEUE_CAPACITY AGENT_ZLM_HOOK_TIMEOUT_SEC \
      ZLM_HOOK_SHARED_SECRET ZLM_HOOK_BASE; do
      [ "$(env_key_occurrence_count "${env_file}" "${key}")" -eq 1 ] \
        || needs_migration=1
    done
    [ "$(existing_env_value "${env_file}" ZLM_API_BASE 2>/dev/null || true)" = \
      "${expected_zlm_api_base}" ] || needs_migration=1
    [ "$(existing_env_value "${env_file}" ZLM_API_SECRET 2>/dev/null || true)" = \
      "${zlm_api_secret}" ] || needs_migration=1
    [ "$(existing_env_value "${env_file}" ZLM_API_ALLOW_IP_RANGE 2>/dev/null || true)" = \
      "${expected_zlm_api_allow_ip_range}" ] || needs_migration=1
    [ "$(existing_env_value "${env_file}" AGENT_ZLM_HOOK_ADDR 2>/dev/null || true)" = \
      "${hook_addr}" ] || needs_migration=1
    [ "$(existing_env_value "${env_file}" AGENT_ZLM_HOOK_PORT 2>/dev/null || true)" = \
      "${hook_port}" ] || needs_migration=1
    [ "$(existing_env_value "${env_file}" AGENT_ZLM_HOOK_QUEUE_CAPACITY 2>/dev/null || true)" = 64 ] \
      || needs_migration=1
    [ "$(existing_env_value "${env_file}" AGENT_ZLM_HOOK_TIMEOUT_SEC 2>/dev/null || true)" = 4 ] \
      || needs_migration=1
    [ "$(existing_env_value "${env_file}" ZLM_HOOK_SHARED_SECRET 2>/dev/null || true)" = \
      "${hook_secret}" ] || needs_migration=1
    [ "$(existing_env_value "${env_file}" ZLM_HOOK_BASE 2>/dev/null || true)" = \
      "${hook_base}" ] || needs_migration=1
    [ "$(env_key_occurrence_count "${env_file}" ZLM_API_HOST)" -eq 0 ] \
      || needs_migration=1
    if [ "${remove_worker_core_hook}" -eq 1 ]; then
      [ "$(env_key_occurrence_count "${env_file}" HOOK_SHARED_SECRET)" -eq 0 ] \
        || needs_migration=1
    fi
  elif role_has_core "${role}"; then
    for key in \
      ZLM_API_HOST ZLM_API_BASE ZLM_API_SECRET ZLM_API_ALLOW_IP_RANGE \
      AGENT_ZLM_HOOK_ADDR AGENT_ZLM_HOOK_PORT \
      AGENT_ZLM_HOOK_QUEUE_CAPACITY AGENT_ZLM_HOOK_TIMEOUT_SEC \
      ZLM_HOOK_SHARED_SECRET ZLM_HOOK_BASE; do
      [ "$(env_key_occurrence_count "${env_file}" "${key}")" -eq 0 ] \
        || needs_migration=1
    done
  else
    fail "cannot migrate ZLM endpoint for unsupported native role ${role}"
  fi

  [ "${needs_migration}" -eq 1 ] || return 0
  begin_atomic_target_write "${env_file}"
  temporary_file="${LAST_INSTALLER_TEMP_FILE}"
  awk -v remove_worker_hook="${remove_worker_core_hook}" '
    BEGIN { skipping = 0 }
    skipping {
      if (length($0) > 0 && substr($0, length($0), 1) == "\047") skipping = 0
      next
    }
    {
      line = $0
      sub(/^[[:space:]]*/, "", line)
      key = line
      sub(/=.*/, "", key)
    }
    key == "ZLM_API_HOST" || key == "ZLM_API_BASE" || key == "ZLM_API_SECRET" \
      || key == "ZLM_API_ALLOW_IP_RANGE" \
      || key == "AGENT_ZLM_HOOK_ADDR" || key == "AGENT_ZLM_HOOK_PORT" \
      || key == "AGENT_ZLM_HOOK_QUEUE_CAPACITY" \
      || key == "AGENT_ZLM_HOOK_TIMEOUT_SEC" \
      || key == "ZLM_HOOK_SHARED_SECRET" || key == "ZLM_HOOK_BASE" \
      || (remove_worker_hook == 1 && key == "HOOK_SHARED_SECRET") {
      raw = substr(line, index(line, "=") + 1)
      if (substr(raw, 1, 1) == "\047" \
          && (length(raw) == 1 || substr(raw, length(raw), 1) != "\047")) skipping = 1
      next
    }
    { print }
    END { if (skipping) exit 2 }
  ' "${env_file}" >"${temporary_file}" \
    || fail "cannot safely migrate legacy ZLM endpoint fields"
  if role_has_worker "${role}"; then
    write_env_entry "${temporary_file}" ZLM_API_BASE "${expected_zlm_api_base}"
    write_env_entry "${temporary_file}" ZLM_API_SECRET "${zlm_api_secret}"
    write_env_entry "${temporary_file}" ZLM_API_ALLOW_IP_RANGE \
      "${expected_zlm_api_allow_ip_range}"
    write_env_entry "${temporary_file}" AGENT_ZLM_HOOK_ADDR "${hook_addr}"
    write_env_entry "${temporary_file}" AGENT_ZLM_HOOK_PORT "${hook_port}"
    write_env_entry "${temporary_file}" AGENT_ZLM_HOOK_QUEUE_CAPACITY 64
    write_env_entry "${temporary_file}" AGENT_ZLM_HOOK_TIMEOUT_SEC 4
    write_env_entry "${temporary_file}" ZLM_HOOK_SHARED_SECRET "${hook_secret}"
    write_env_entry "${temporary_file}" ZLM_HOOK_BASE "${hook_base}"
  fi
  finish_atomic_target_write "${temporary_file}" "${env_file}" 600 root:root
  log "WARNING: migrated legacy ZLM control endpoint and hook ingress to the authenticated Agent-local loopback policy"
}

validate_sealed_upgrade_environment_identity() {
  local env_file="${INSTALL_DIR}/.env"
  local requested_role="${INSTALL_ROLE}"
  local requested_instance_name="${INSTANCE_NAME}"
  local existing_role
  local existing_instance_name

  existing_role="$(strict_identity_env_value "${env_file}" INSTALL_ROLE)"
  existing_instance_name="$(strict_identity_env_value "${env_file}" INSTANCE_NAME)"
  validate_role_supported "${existing_role}"
  validate_role_against_trusted_units "${existing_role}"

  if [ "${requested_role}" != "${existing_role}" ]; then
    fail "--role must exactly match the existing native installation"
  fi
  if [ "${requested_instance_name}" != "${existing_instance_name}" ]; then
    fail "--instance-name must exactly match the existing native installation"
  fi

  INSTALL_ROLE="${existing_role}"
  INSTANCE_NAME="${existing_instance_name}"
  [ "${UNIT_BASENAME}" = "${TRUSTED_UNIT_BASENAME}" ] \
    || fail "persisted INSTANCE_NAME does not match root-managed systemd units"
  require_upgrade_systemd_identity \
    "${env_file}" SYSTEMD_TARGET "${UNIT_BASENAME}.target"
  require_upgrade_systemd_identity \
    "${env_file}" SYSTEMD_CORE_UNIT "${UNIT_BASENAME}-core.service"
  require_upgrade_systemd_identity \
    "${env_file}" SYSTEMD_AGENT_UNIT "${UNIT_BASENAME}-agent.service"
  require_upgrade_systemd_identity \
    "${env_file}" SYSTEMD_ZLM_UNIT "${UNIT_BASENAME}-zlm.service"
  require_upgrade_systemd_identity \
    "${env_file}" SYSTEMD_POSTGRES_UNIT "${UNIT_BASENAME}-postgres.service"
}

admin_handoff_state_dir() {
  [ -d "${INSTALL_DIR}" ] || fail "管理员密码交付状态要求安装目录已存在"
  printf '%s/%s' "$(admin_handoff_state_root_path)" "$(admin_handoff_install_dir_fingerprint)"
}

pending_admin_handoff_path() {
  printf '%s/admin-handoff.pending' "$(admin_handoff_state_dir)"
}

delivered_admin_handoff_path() {
  printf '%s/%s' "$(admin_handoff_state_dir)" "${ADMIN_HANDOFF_DELIVERED_NAME}"
}

ensure_admin_handoff_state_dir() {
  local state_root
  local state_root_parent
  local state_dir
  state_root="$(admin_handoff_state_root_path)"
  state_root_parent="$(dirname "${state_root}")"
  state_dir="$(admin_handoff_state_dir)"

  admin_handoff_assert_no_symlink_boundary "${state_root}"
  [ -d "${state_root_parent}" ] \
    || fail "administrator handoff state parent must already exist"
  [ "$(id -u)" -eq 0 ] \
    || fail "administrator handoff state mutation requires root"
  admin_handoff_assert_secure_root_ancestors "${state_root_parent}"
  if [ ! -e "${state_root}" ]; then
    [ ! -L "${state_root}" ] \
      || fail "administrator handoff state path contains a symbolic link"
    install -d -o root -g root -m 0700 -- "${state_root}"
    sync -f "${state_root_parent}"
  fi
  admin_handoff_assert_secure_directory "${state_root}"

  admin_handoff_assert_no_symlink_boundary "${state_dir}"
  if [ ! -e "${state_dir}" ]; then
    [ ! -L "${state_dir}" ] \
      || fail "administrator handoff state path contains a symbolic link"
    install -d -o root -g root -m 0700 -- "${state_dir}"
    sync -f "${state_root}"
  fi
  admin_handoff_assert_secure_directory "${state_dir}"
}

normalize_install_dir_for_transaction() {
  local lexical_path
  local resolved_path
  local canonical_path
  [ -n "${INSTALL_DIR}" ] || fail "native installation root cannot be empty"
  case "${INSTALL_DIR}" in
    /*) ;;
    *) fail "native installation root must be an absolute path" ;;
  esac
  lexical_path="$(realpath -ms -- "${INSTALL_DIR}")" \
    || fail "native installation root cannot be normalized"
  resolved_path="$(realpath -m -- "${INSTALL_DIR}")" \
    || fail "native installation root cannot be resolved"
  [ "${lexical_path}" = "${resolved_path}" ] \
    || fail "native installation root path must not traverse a symbolic link"
  [ "${resolved_path}" != / ] \
    || fail "native installation root cannot be the filesystem root"
  if [ -d "${resolved_path}" ]; then
    canonical_path="$(cd "${resolved_path}" && pwd -P)"
    [ "${canonical_path}" = "${resolved_path}" ] \
      || fail "native installation root changed while being normalized"
  fi
  INSTALL_DIR="${resolved_path}"
}

prepare_install_root_for_transaction() {
  local require_existing="$1"
  local parent
  local mode
  normalize_install_dir_for_transaction
  parent="$(dirname "${INSTALL_DIR}")"
  [ -d "${parent}" ] && [ ! -L "${parent}" ] \
    || fail "native installation root parent must be a real directory"
  admin_handoff_assert_no_symlink_boundary "${parent}"
  admin_handoff_assert_secure_root_ancestors "${parent}"
  if [ "${require_existing}" = true ]; then
    [ -d "${INSTALL_DIR}" ] && [ ! -L "${INSTALL_DIR}" ] \
      || fail "upgrade requires an existing real installation directory"
    return 0
  fi
  if [ ! -e "${INSTALL_DIR}" ] && [ ! -L "${INSTALL_DIR}" ]; then
    install -d -o root -g root -m 0755 -- "${INSTALL_DIR}"
    sync -f "${parent}"
  fi
  [ -d "${INSTALL_DIR}" ] && [ ! -L "${INSTALL_DIR}" ] \
    || fail "fresh native installation root must be a real directory"
  if find "${INSTALL_DIR}" -mindepth 1 -maxdepth 1 -print -quit | grep -q .; then
    fail "fresh native installation root must be empty; use --upgrade for an existing deployment"
  fi
  if [ "$(id -u)" -eq 0 ] && [ "${EMULATED_SECURITY_METADATA:-0}" -ne 1 ]; then
    [ "$(stat -c '%u' -- "${INSTALL_DIR}")" = 0 ] \
      || fail "fresh native installation root must be owned by root"
    mode="$(stat -c '%a' -- "${INSTALL_DIR}")" \
      || fail "cannot inspect fresh native installation root mode"
    (( (8#${mode} & 8#022) == 0 )) \
      || fail "fresh native installation root must not be group/world writable"
  fi
}

install_transaction_path_identity() {
  stat -Lc '%d:%i' -- "$1"
}

install_transaction_parent_chain_identity() {
  local path
  local identity
  path="$(dirname "${INSTALL_DIR}")"
  while true; do
    [ ! -L "${path}" ] && [ -d "${path}" ] || return 1
    identity="$(stat -c '%d:%i:%u:%a' -- "${path}")" || return 1
    printf '%s=%s\n' "${path}" "${identity}"
    [ "${path}" = / ] && break
    path="$(dirname "${path}")"
  done
}

run_command_with_installer_flocks() {
  local global_lock="$1"
  local path_lock="$2"
  local command_status
  shift 2
  [ "$#" -gt 0 ] || return 64
  INSTALL_LOCK_WRAPPER_SIGNAL_STATUS=0
  trap 'record_install_lock_wrapper_signal 129' HUP
  trap 'record_install_lock_wrapper_signal 130' INT
  trap 'record_install_lock_wrapper_signal 143' TERM
  if (
    # Lock parents must survive the same terminal signal that asks the
    # mutation child to roll back. The child explicitly restores defaults;
    # util-linux flock keeps both descriptors and -o closes them before exec.
    trap '' HUP INT TERM
    flock -n -E 75 -o "${global_lock}" \
      flock -n -E 75 -o "${path_lock}" \
      env --default-signal=HUP,INT,TERM "$@"
  ); then
    command_status=0
  else
    command_status=$?
  fi
  restore_primary_installer_signal_traps
  if [ "${INSTALL_LOCK_WRAPPER_SIGNAL_STATUS}" -ne 0 ]; then
    return "${INSTALL_LOCK_WRAPPER_SIGNAL_STATUS}"
  fi
  return "${command_status}"
}

record_install_lock_wrapper_signal() {
  local signal_status="$1"
  if [ "${INSTALL_LOCK_WRAPPER_SIGNAL_STATUS}" -eq 0 ]; then
    INSTALL_LOCK_WRAPPER_SIGNAL_STATUS="${signal_status}"
  fi
}

restore_primary_installer_signal_traps() {
  trap 'handle_admin_password_signal 129' HUP
  trap 'handle_admin_password_signal 130' INT
  trap 'handle_admin_password_signal 143' TERM
}

emit_locked_install_plan() {
  printf '%s\0' \
    streamserver-native-install-plan-v2 \
    15 \
    "${INSTALL_DIR}" \
    "${INSTANCE_NAME}" \
    "${INSTALL_ROLE}" \
    "${UNIT_BASENAME}" \
    "${UPGRADE}" \
    "${START_AFTER_INSTALL}" \
    "${DATABASE_MODE}" \
    "${DATABASE_URL_INPUT}" \
    "${INSTALL_ROLE_WAS_EXPLICIT}" \
    "${INSTANCE_NAME_WAS_EXPLICIT}" \
    "${INTERACTIVE_INSTALL}" \
    "${VERIFIED_PACKAGE_CHECKSUM_FILE_SHA256}" \
    "${VERIFIED_PACKAGE_TREE_FINGERPRINT}"
}

load_locked_install_plan_from_fd() {
  local plan_fd="$1"
  local -a fields=()
  [[ "${plan_fd}" =~ ^[0-9]+$ ]] || return 1
  [ -p "/proc/self/fd/${plan_fd}" ] || return 1
  if ! mapfile -d '' -t fields <&"${plan_fd}"; then
    exec {plan_fd}<&-
    return 1
  fi
  exec {plan_fd}<&-
  [ "${#fields[@]}" -eq 15 ] || return 1
  [ "${fields[0]}" = streamserver-native-install-plan-v2 ] || return 1
  [ "${fields[1]}" = 15 ] || return 1
  INSTALL_DIR="${fields[2]}"
  INSTANCE_NAME="${fields[3]}"
  INSTALL_ROLE="${fields[4]}"
  UNIT_BASENAME="${fields[5]}"
  UPGRADE="${fields[6]}"
  START_AFTER_INSTALL="${fields[7]}"
  DATABASE_MODE="${fields[8]}"
  DATABASE_URL_INPUT="${fields[9]}"
  INSTALL_ROLE_WAS_EXPLICIT="${fields[10]}"
  INSTANCE_NAME_WAS_EXPLICIT="${fields[11]}"
  INTERACTIVE_INSTALL="${fields[12]}"
  LOCKED_PACKAGE_EXPECTED_CHECKSUM_SHA256="${fields[13]}"
  LOCKED_PACKAGE_EXPECTED_TREE_FINGERPRINT="${fields[14]}"
  [[ "${UPGRADE}" =~ ^[01]$ ]] \
    && [[ "${START_AFTER_INSTALL}" =~ ^[01]$ ]] \
    && [[ "${INSTALL_ROLE_WAS_EXPLICIT}" =~ ^[01]$ ]] \
    && [[ "${INSTANCE_NAME_WAS_EXPLICIT}" =~ ^[01]$ ]] \
    && [[ "${INTERACTIVE_INSTALL}" =~ ^[01]$ ]] || return 1
  case "${DATABASE_MODE}" in
    ''|bundled|external) ;;
    *) return 1 ;;
  esac
  [ -n "${INSTALL_DIR}" ] \
    && [ -n "${INSTANCE_NAME}" ] \
    && [ -n "${INSTALL_ROLE}" ] \
    && [ -n "${UNIT_BASENAME}" ] || return 1
  [ "$(sanitize_instance_name "${INSTANCE_NAME}")" = "${INSTANCE_NAME}" ] \
    || return 1
  [ "$(unit_basename_for_instance "${INSTANCE_NAME}")" = "${UNIT_BASENAME}" ] \
    || return 1
  [[ "${LOCKED_PACKAGE_EXPECTED_CHECKSUM_SHA256}" =~ ^[0-9a-f]{64}$ ]] \
    && [[ "${LOCKED_PACKAGE_EXPECTED_TREE_FINGERPRINT}" =~ ^[0-9a-f]{64}$ ]] \
    || return 1
}

installer_lock_root_path() {
  printf '%s/locks' "$(admin_handoff_state_root_path)"
}

ensure_installer_lock_root() {
  local state_root
  local state_root_parent
  local lock_root
  state_root="$(admin_handoff_state_root_path)"
  state_root_parent="$(dirname "${state_root}")"
  lock_root="$(installer_lock_root_path)"
  [ "$(id -u)" -eq 0 ] || fail "native installer lock creation requires root"
  admin_handoff_assert_no_symlink_boundary "${state_root}"
  [ -d "${state_root_parent}" ] \
    || fail "native installer lock state parent must already exist"
  admin_handoff_assert_secure_root_ancestors "${state_root_parent}"
  if [ ! -e "${state_root}" ] && [ ! -L "${state_root}" ]; then
    install -d -o root -g root -m 0700 -- "${state_root}"
    sync -f "${state_root_parent}"
  fi
  admin_handoff_assert_secure_directory "${state_root}"
  admin_handoff_assert_no_symlink_boundary "${lock_root}"
  if [ ! -e "${lock_root}" ] && [ ! -L "${lock_root}" ]; then
    install -d -o root -g root -m 0700 -- "${lock_root}"
    sync -f "${state_root}"
  fi
  admin_handoff_assert_secure_directory "${lock_root}"
}

derive_external_installer_lock_paths() {
  local lock_root
  local instance_fingerprint
  local path_fingerprint
  normalize_install_dir_for_transaction
  [ -n "${UNIT_BASENAME}" ] || fail "native installer instance lock identity is empty"
  lock_root="$(installer_lock_root_path)"
  instance_fingerprint="$(printf '%s' "${UNIT_BASENAME}" | sha256sum | awk '{print $1}')"
  path_fingerprint="$(printf '%s' "${INSTALL_DIR}" | sha256sum | awk '{print $1}')"
  [[ "${instance_fingerprint}" =~ ^[0-9a-f]{64}$ ]] \
    && [[ "${path_fingerprint}" =~ ^[0-9a-f]{64}$ ]] \
    || fail "cannot derive native installer lock identity"
  INSTALL_TRANSACTION_GLOBAL_LOCK_PATH="${lock_root}/instance-${instance_fingerprint}.lock"
  INSTALL_TRANSACTION_PATH_LOCK_PATH="${lock_root}/path-${path_fingerprint}.lock"
}

ensure_external_installer_lock_file() {
  local lock_file="$1"
  local lock_root
  lock_root="$(installer_lock_root_path)"
  case "${lock_file}" in
    "${lock_root}/instance-"*.lock|"${lock_root}/path-"*.lock) ;;
    *) fail "refused unsafe native installer lock path" ;;
  esac
  [ ! -L "${lock_file}" ] || fail "native installer lock must not be a symbolic link"
  if [ ! -e "${lock_file}" ]; then
    (umask 077; set -C; : >"${lock_file}") 2>/dev/null || true
    sync -f "${lock_root}"
  fi
  admin_handoff_assert_secure_file "${lock_file}" 600
}

prepare_external_installer_lock_files() {
  ensure_installer_lock_root
  derive_external_installer_lock_paths
  ensure_external_installer_lock_file "${INSTALL_TRANSACTION_GLOBAL_LOCK_PATH}"
  ensure_external_installer_lock_file "${INSTALL_TRANSACTION_PATH_LOCK_PATH}"
}

assert_external_installer_flocks_held() {
  local lock_file
  local probe_status
  admin_handoff_assert_secure_directory "$(installer_lock_root_path)"
  for lock_file in \
    "${INSTALL_TRANSACTION_GLOBAL_LOCK_PATH}" \
    "${INSTALL_TRANSACTION_PATH_LOCK_PATH}"; do
    admin_handoff_assert_secure_file "${lock_file}" 600
    probe_status=0
    flock -n -E 76 -o "${lock_file}" true || probe_status=$?
    [ "${probe_status}" -eq 76 ] \
      || fail "native installer external transaction lock is not held"
  done
}

run_readonly_check_with_external_flocks() {
  local readonly_mode="$1"
  local status
  local installer_script
  local expected_checksum="${VERIFIED_PACKAGE_CHECKSUM_FILE_SHA256}"
  local expected_tree="${VERIFIED_PACKAGE_TREE_FINGERPRINT}"
  case "${readonly_mode}" in
    security-preflight|check-only) ;;
    *) fail "invalid locked readonly diagnostic mode" ;;
  esac
  prepare_external_installer_lock_files
  stage_verified_package_root
  installer_script="${PACKAGE_ROOT}/install.sh"
  if run_command_with_installer_flocks \
    "${INSTALL_TRANSACTION_GLOBAL_LOCK_PATH}" \
    "${INSTALL_TRANSACTION_PATH_LOCK_PATH}" \
    bash "${installer_script}" --_locked-readonly-check-stage \
      "${readonly_mode}" "${INSTALL_DIR}" \
      "${expected_checksum}" "${expected_tree}"; then
    status=0
  else
    status=$?
  fi
  if [ "${status}" -eq 75 ]; then
    fail "another installer is already operating on this native instance or installation root"
  fi
  return "${status}"
}

run_locked_readonly_check_stage() {
  local readonly_mode="$1"
  local requested_install_dir="$2"
  local expected_checksum="$3"
  local expected_tree="$4"
  local persisted_instance_name
  case "${readonly_mode}" in
    security-preflight|check-only) ;;
    *) fail "invalid internal readonly diagnostic mode" ;;
  esac
  [ "$(id -u)" -eq 0 ] \
    || fail "locked installed-state diagnostics require root"
  INSTALL_DIR="${requested_install_dir}"
  load_manifest
  ensure_prerequisites
  verify_package_checksums
  assert_locked_package_identity "${expected_checksum}" "${expected_tree}"
  assert_no_docker_assets
  prepare_install_root_for_transaction true
  persisted_instance_name="$(existing_env_value "${INSTALL_DIR}/.env" INSTANCE_NAME)"
  [ -n "${persisted_instance_name}" ] \
    && [ "$(sanitize_instance_name "${persisted_instance_name}")" = "${persisted_instance_name}" ] \
    || fail "installed INSTANCE_NAME is invalid"
  INSTANCE_NAME="${persisted_instance_name}"
  UNIT_BASENAME="$(unit_basename_for_instance "${INSTANCE_NAME}")"
  prepare_external_installer_lock_files
  INSTALL_TRANSACTION_EXTERNAL_LOCKS=1
  acquire_install_transaction_lock
  [ "$(existing_env_value "${INSTALL_DIR}/.env" INSTANCE_NAME)" = "${INSTANCE_NAME}" ] \
    || fail "installed INSTANCE_NAME changed while acquiring the diagnostic lock"
  select_readonly_security_probe_runtime_root
  prepare_package_security_probe_binaries
  case "${readonly_mode}" in
    security-preflight)
      security_preflight_env \
        "${INSTALL_DIR}/.env" "${SECURITY_PROBE_CORE_BIN}" "${SECURITY_PROBE_AGENT_BIN}" \
        || fail "installed production security preflight failed"
      ;;
    check-only)
      security_preflight_env \
        "${INSTALL_DIR}/.env" "${SECURITY_PROBE_CORE_BIN}" "${SECURITY_PROBE_AGENT_BIN}" \
        || fail "check-only found production security gaps"
      ;;
  esac
  cleanup_security_probe_binaries \
    || fail "failed to clean package security probe binaries"
}

run_install_with_external_flocks() {
  local plan_fd
  local status
  local installer_script
  prepare_external_installer_lock_files
  stage_verified_package_root
  installer_script="${PACKAGE_ROOT}/install.sh"
  exec {plan_fd}< <(emit_locked_install_plan)
  if run_command_with_installer_flocks \
    "${INSTALL_TRANSACTION_GLOBAL_LOCK_PATH}" \
    "${INSTALL_TRANSACTION_PATH_LOCK_PATH}" \
    bash "${installer_script}" --_locked-install-stage "${plan_fd}"; then
    status=0
  else
    status=$?
  fi
  exec {plan_fd}<&-
  return "${status}"
}

assert_fresh_instance_namespace_available() {
  local deadline=$((SECONDS + 30))
  local unit
  local load_state
  local fragment_path
  [ "${UPGRADE}" -eq 0 ] || return 0
  while IFS= read -r unit; do
    if [ -e "${SYSTEMD_UNIT_ROOT}/${unit}" ] \
      || [ -L "${SYSTEMD_UNIT_ROOT}/${unit}" ]; then
      fail "fresh install instance already owns a systemd namespace: ${unit}"
    fi
    load_state="$(bounded_upgrade_systemctl "${deadline}" \
      show --property LoadState --value "${unit}" 2>/dev/null)" \
      || fail "cannot verify the systemd instance namespace: ${unit}"
    fragment_path="$(bounded_upgrade_systemctl "${deadline}" \
      show --property FragmentPath --value "${unit}" 2>/dev/null)" \
      || fail "cannot verify the systemd unit fragment namespace: ${unit}"
    [[ "${load_state}" != *$'\n'* ]] \
      && [[ "${load_state}" != *$'\r'* ]] \
      && [[ "${fragment_path}" != *$'\n'* ]] \
      && [[ "${fragment_path}" != *$'\r'* ]] \
      || fail "systemd returned an ambiguous native instance namespace"
    [ "${load_state}" = not-found ] && [ -z "${fragment_path}" ] \
      || fail "fresh install instance collides with a loaded systemd namespace: ${unit}"
  done < <(upgrade_transaction_unit_names)
}

assert_install_transaction_lock_held() {
  local lock_fd_path
  local current_install_identity
  local current_parent_chain_identity
  local current_state_root_identity
  local current_state_dir_identity
  local path_lock_identity
  local fd_lock_identity
  if [ "${INSTALL_TRANSACTION_EXTERNAL_LOCKS:-0}" -eq 1 ]; then
    assert_external_installer_flocks_held
    [ -n "${INSTALL_TRANSACTION_LOCK_PATH}" ] \
      && [ "${INSTALL_TRANSACTION_LOCK_PATH}" = \
        "${INSTALL_TRANSACTION_PATH_LOCK_PATH}" ] \
      || fail "native installer path lock identity is unavailable"
  else
    [ -n "${INSTALL_TRANSACTION_LOCK_FD}" ] \
      || fail "native installer transaction lock is not held"
    lock_fd_path="/proc/self/fd/${INSTALL_TRANSACTION_LOCK_FD}"
    [ -e "${lock_fd_path}" ] \
      || fail "native installer transaction lock descriptor is unavailable"
  fi
  normalize_install_dir_for_transaction
  current_install_identity="$(install_transaction_path_identity "${INSTALL_DIR}")" \
    || fail "cannot revalidate native installation root identity"
  [ "${current_install_identity}" = "${INSTALL_TRANSACTION_INSTALL_DIR_IDENTITY}" ] \
    || fail "native installation root identity changed while locked"
  admin_handoff_assert_secure_root_ancestors "$(dirname "${INSTALL_DIR}")"
  current_parent_chain_identity="$(install_transaction_parent_chain_identity)" \
    || fail "cannot revalidate native installation parent ancestry"
  [ "${current_parent_chain_identity}" = \
    "${INSTALL_TRANSACTION_PARENT_CHAIN_IDENTITY}" ] \
    || fail "native installation parent ancestry changed while locked"
  admin_handoff_assert_no_symlink_boundary "$(admin_handoff_state_root_path)"
  admin_handoff_assert_no_symlink_boundary "$(admin_handoff_state_dir)"
  admin_handoff_assert_secure_directory "$(admin_handoff_state_root_path)"
  admin_handoff_assert_secure_directory "$(admin_handoff_state_dir)"
  current_state_root_identity="$(install_transaction_path_identity \
    "$(admin_handoff_state_root_path)")" \
    || fail "cannot revalidate native installer state root identity"
  current_state_dir_identity="$(install_transaction_path_identity \
    "$(admin_handoff_state_dir)")" \
    || fail "cannot revalidate native installer state directory identity"
  [ "${current_state_root_identity}" = "${INSTALL_TRANSACTION_STATE_ROOT_IDENTITY}" ] \
    && [ "${current_state_dir_identity}" = "${INSTALL_TRANSACTION_STATE_DIR_IDENTITY}" ] \
    || fail "native installer transaction state directory identity changed while locked"
  admin_handoff_assert_secure_file "${INSTALL_TRANSACTION_LOCK_PATH}" 600
  path_lock_identity="$(install_transaction_path_identity \
    "${INSTALL_TRANSACTION_LOCK_PATH}")" \
    || fail "cannot inspect native installer transaction lock path"
  if [ "${INSTALL_TRANSACTION_EXTERNAL_LOCKS:-0}" -eq 0 ]; then
    fd_lock_identity="$(install_transaction_path_identity "${lock_fd_path}")" \
      || fail "cannot inspect native installer transaction lock descriptor"
    [ "${path_lock_identity}" = "${fd_lock_identity}" ] \
      || fail "native installer transaction lock path was replaced after acquisition"
  fi
}

acquire_install_transaction_lock() {
  local lock_file
  if [ "${INSTALL_TRANSACTION_EXTERNAL_LOCKS:-0}" -eq 1 ]; then
    if [ -n "${INSTALL_TRANSACTION_INSTALL_DIR_IDENTITY}" ]; then
      assert_install_transaction_lock_held
      return 0
    fi
    assert_external_installer_flocks_held
    normalize_install_dir_for_transaction
    [ -d "${INSTALL_DIR}" ] && [ ! -L "${INSTALL_DIR}" ] \
      || fail "native installer transaction lock requires a real installation root"
    ensure_admin_handoff_state_dir
    INSTALL_TRANSACTION_INSTALL_DIR_IDENTITY="$(install_transaction_path_identity \
      "${INSTALL_DIR}")" || fail "cannot capture native installation root identity"
    INSTALL_TRANSACTION_PARENT_CHAIN_IDENTITY="$(install_transaction_parent_chain_identity)" \
      || fail "cannot capture native installation parent ancestry"
    INSTALL_TRANSACTION_STATE_ROOT_IDENTITY="$(install_transaction_path_identity \
      "$(admin_handoff_state_root_path)")" \
      || fail "cannot capture native installer state root identity"
    INSTALL_TRANSACTION_STATE_DIR_IDENTITY="$(install_transaction_path_identity \
      "$(admin_handoff_state_dir)")" \
      || fail "cannot capture native installer state directory identity"
    INSTALL_TRANSACTION_LOCK_PATH="${INSTALL_TRANSACTION_PATH_LOCK_PATH}"
    assert_install_transaction_lock_held
    return 0
  fi
  [ -n "${INSTALL_TRANSACTION_LOCK_FD}" ] && {
    assert_install_transaction_lock_held
    return 0
  }
  normalize_install_dir_for_transaction
  [ -d "${INSTALL_DIR}" ] && [ ! -L "${INSTALL_DIR}" ] \
    || fail "native installer transaction lock requires a real installation root"
  ensure_admin_handoff_state_dir
  lock_file="$(admin_handoff_state_dir)/installer.lock"
  [ ! -L "${lock_file}" ] \
    || fail "administrator handoff lock must not be a symbolic link"
  if [ ! -e "${lock_file}" ]; then
    (umask 077; set -C; : >"${lock_file}") 2>/dev/null || true
    sync -f "$(admin_handoff_state_dir)"
  fi
  admin_handoff_assert_secure_file "${lock_file}" 600
  INSTALL_TRANSACTION_INSTALL_DIR_IDENTITY="$(install_transaction_path_identity \
    "${INSTALL_DIR}")" || fail "cannot capture native installation root identity"
  INSTALL_TRANSACTION_PARENT_CHAIN_IDENTITY="$(install_transaction_parent_chain_identity)" \
    || fail "cannot capture native installation parent ancestry"
  INSTALL_TRANSACTION_STATE_ROOT_IDENTITY="$(install_transaction_path_identity \
    "$(admin_handoff_state_root_path)")" \
    || fail "cannot capture native installer state root identity"
  INSTALL_TRANSACTION_STATE_DIR_IDENTITY="$(install_transaction_path_identity \
    "$(admin_handoff_state_dir)")" \
    || fail "cannot capture native installer state directory identity"
  INSTALL_TRANSACTION_LOCK_PATH="${lock_file}"
  exec {INSTALL_TRANSACTION_LOCK_FD}<>"${lock_file}"
  flock -n "${INSTALL_TRANSACTION_LOCK_FD}" \
    || fail "another installer is already operating on this native installation root"
  assert_install_transaction_lock_held
}

acquire_admin_handoff_lock() {
  acquire_install_transaction_lock
}

admin_handoff_install_dir_fingerprint() {
  local canonical_install_dir
  canonical_install_dir="$(cd "${INSTALL_DIR}" && pwd -P)"
  printf '%s' "${canonical_install_dir}" | sha256sum | awk '{print $1}'
}

admin_handoff_state_root_path() {
  local state_root="${ADMIN_HANDOFF_STATE_ROOT%/}"
  local lexical_path
  [ -n "${state_root}" ] || state_root="/"
  case "${state_root}" in
    /*) ;;
    *) fail "administrator handoff state root must be an absolute path" ;;
  esac
  lexical_path="$(realpath -ms -- "${state_root}")" \
    || fail "administrator handoff state root cannot be normalized"
  [ "${state_root}" = "${lexical_path}" ] \
    || fail "administrator handoff state root must be a normalized absolute path"
  [ "${state_root}" != "/" ] \
    || fail "administrator handoff state root cannot be the filesystem root"
  printf '%s' "${state_root}"
}

admin_handoff_assert_no_symlink_boundary() {
  admin_handoff_no_symlink_boundary_status "$1" \
    || fail "administrator handoff state path contains a symbolic link or cannot be resolved"
}

admin_handoff_no_symlink_boundary_status() {
  local path="$1"
  local lexical_path
  local resolved_path
  lexical_path="$(realpath -ms -- "${path}" 2>/dev/null)" || return 1
  resolved_path="$(realpath -m -- "${path}" 2>/dev/null)" || return 1
  [ "${lexical_path}" = "${resolved_path}" ]
}

admin_handoff_assert_secure_root_ancestors() {
  admin_handoff_secure_root_ancestors_status "$1" \
    || fail "administrator handoff state ancestor must be a secure root-owned directory"
}

admin_handoff_secure_root_ancestors_status() {
  local path="$1"
  local mode
  [ "$(id -u)" -eq 0 ] || return 0
  while true; do
    [ ! -L "${path}" ] && [ -d "${path}" ] || return 1
    [ "$(stat -c '%u' -- "${path}" 2>/dev/null)" = "0" ] || return 1
    mode="$(stat -c '%a' -- "${path}" 2>/dev/null)" || return 1
    (( (8#${mode} & 8#022) == 0 )) || return 1
    [ "${path}" = "/" ] && break
    path="$(dirname "${path}")"
  done
}

admin_handoff_secure_directory_status() {
  local path="$1"
  [ ! -L "${path}" ] && [ -d "${path}" ] && [ -x "${path}" ] \
    && [ "$(stat -c '%u' -- "${path}" 2>/dev/null)" = "0" ] \
    && [ "$(stat -c '%a' -- "${path}" 2>/dev/null)" = "700" ]
}

admin_handoff_assert_secure_directory() {
  admin_handoff_secure_directory_status "$1" \
    || fail "administrator handoff state directory must be owned by root with mode 0700 and must not be a symbolic link"
}

admin_handoff_secure_file_status() {
  local path="$1"
  local expected_mode="$2"
  [ ! -L "${path}" ] && [ -f "${path}" ] \
    && [ "$(stat -c '%u' -- "${path}" 2>/dev/null)" = "0" ] \
    && [ "$(stat -c '%a' -- "${path}" 2>/dev/null)" = "${expected_mode}" ]
}

admin_handoff_assert_secure_file() {
  admin_handoff_secure_file_status "$1" "$2" \
    || fail "administrator handoff state file must be owned by root with the required mode and must not be a symbolic link"
}

admin_handoff_marker_probe() {
  local marker_file="$1"
  local state_root
  local state_root_parent
  local state_dir
  state_root="$(admin_handoff_state_root_path 2>/dev/null)" || return 2
  state_root_parent="$(dirname "${state_root}")"
  state_dir="$(admin_handoff_state_dir 2>/dev/null)" || return 2

  admin_handoff_no_symlink_boundary_status "${state_root}" || return 2
  [ -d "${state_root_parent}" ] && [ -x "${state_root_parent}" ] || return 2
  admin_handoff_secure_root_ancestors_status "${state_root_parent}" || return 2
  if [ ! -e "${state_root}" ] && [ ! -L "${state_root}" ]; then
    return 1
  fi
  admin_handoff_secure_directory_status "${state_root}" || return 2
  admin_handoff_no_symlink_boundary_status "${state_dir}" || return 2
  if [ ! -e "${state_dir}" ] && [ ! -L "${state_dir}" ]; then
    return 1
  fi
  admin_handoff_secure_directory_status "${state_dir}" || return 2
  if [ ! -e "${marker_file}" ] && [ ! -L "${marker_file}" ]; then
    return 1
  fi
  admin_handoff_secure_file_status "${marker_file}" 600 || return 2
  return 0
}

admin_handoff_public_key_path() {
  local value="${AUTH_JWT_PUBLIC_KEY_PATH:-}"
  if [ -z "${value}" ] && [ -f "${INSTALL_DIR}/.env" ]; then
    value="$(existing_env_value "${INSTALL_DIR}/.env" AUTH_JWT_PUBLIC_KEY_PATH 2>/dev/null || true)"
  fi
  [ -n "${value}" ] || value="${INSTALL_DIR}/certs/auth/jwt-ed25519-public.pem"
  resolve_security_path "${INSTALL_DIR}/.env" "${value}"
}

admin_handoff_public_key_fingerprint() {
  local public_key_path
  public_key_path="$(admin_handoff_public_key_path)"
  if [ "$(id -u)" -eq 0 ] && [ "${EMULATED_SECURITY_METADATA:-0}" -ne 1 ]; then
    runuser -u "${SERVICE_USER}" -- \
      openssl pkey -pubin -in "${public_key_path}" -outform DER 2>/dev/null \
      | sha256sum | awk '{print $1}'
  else
    validate_public_key "${public_key_path}" \
      || fail "管理员密码交付状态无法验证 JWT 公钥"
    openssl pkey -pubin -in "${public_key_path}" -outform DER 2>/dev/null \
      | sha256sum | awk '{print $1}'
  fi
}

normalize_admin_username_for_handoff() {
  local username
  username="$(printf '%s' "$1" | tr '[:upper:]' '[:lower:]')"
  [[ "${username}" =~ ^[a-z0-9._@-]+$ ]] \
    || fail "管理员用户名包含不支持的字符"
  printf '%s' "${username}"
}

write_pending_admin_handoff_marker() {
  local username="$1"
  local handoff_id
  local jwt_public_key_fingerprint
  local marker_file
  local temporary_file
  ensure_admin_handoff_state_dir
  [ ! -e "$(delivered_admin_handoff_path)" ] \
    && [ ! -L "$(delivered_admin_handoff_path)" ] \
    || fail "管理员密码已交付标记尚未完成清理"
  marker_file="$(pending_admin_handoff_path)"
  temporary_file="${marker_file}.tmp.$$"
  username="$(normalize_admin_username_for_handoff "${username}")"
  handoff_id="$(generate_admin_handoff_id)"
  if ! jwt_public_key_fingerprint="$(admin_handoff_public_key_fingerprint)"; then
    fail "administrator handoff JWT public key is not readable by the service account"
  fi
  [[ "${jwt_public_key_fingerprint}" =~ ^[0-9a-f]{64}$ ]] \
    || fail "administrator handoff JWT public key fingerprint is invalid"
  [ ! -e "${marker_file}" ] && [ ! -L "${marker_file}" ] \
    || fail "administrator handoff pending marker already exists"
  [ ! -e "${temporary_file}" ] && [ ! -L "${temporary_file}" ] \
    || fail "administrator handoff temporary marker already exists"
  (
    umask 077
    printf '%s\n' \
      'STREAMSERVER_ADMIN_HANDOFF_VERSION=3' \
      'PURPOSE=admin-password-handoff' \
      "ADMIN_USERNAME=${username}" \
      "HANDOFF_ID=${handoff_id}" \
      "INSTALL_DIR_SHA256=$(admin_handoff_install_dir_fingerprint)" \
      "JWT_PUBLIC_KEY_SHA256=${jwt_public_key_fingerprint}" >"${temporary_file}"
  )
  sync -f "${temporary_file}"
  admin_handoff_assert_secure_file "${temporary_file}" 600
  mv -- "${temporary_file}" "${marker_file}"
  sync -f "$(admin_handoff_state_dir)"
}

read_admin_handoff_username() {
  local marker_file="$1"
  local version
  local purpose
  local username
  local handoff_id
  local jwt_public_key_fingerprint
  local field
  admin_handoff_marker_probe "${marker_file}" \
    || fail "administrator handoff marker is absent, inaccessible, or insecure"
  [ "$(wc -l <"${marker_file}" | tr -d '[:space:]')" = "6" ] \
    || fail "管理员密码交付标记包含未知、缺失或重复字段"
  for field in STREAMSERVER_ADMIN_HANDOFF_VERSION PURPOSE ADMIN_USERNAME HANDOFF_ID INSTALL_DIR_SHA256 JWT_PUBLIC_KEY_SHA256; do
    [ "$(grep -Ec "^${field}=" "${marker_file}")" = "1" ] \
      || fail "administrator handoff marker contains an unknown, missing, or duplicate field"
  done
  version="$(existing_env_value "${marker_file}" STREAMSERVER_ADMIN_HANDOFF_VERSION)"
  purpose="$(existing_env_value "${marker_file}" PURPOSE)"
  username="$(existing_env_value "${marker_file}" ADMIN_USERNAME)"
  handoff_id="$(existing_env_value "${marker_file}" HANDOFF_ID)"
  jwt_public_key_fingerprint="$(existing_env_value "${marker_file}" JWT_PUBLIC_KEY_SHA256)"
  [ "${version}" = "3" ] \
    && [ "${purpose}" = "admin-password-handoff" ] \
    && [ "$(existing_env_value "${marker_file}" INSTALL_DIR_SHA256)" = "$(admin_handoff_install_dir_fingerprint)" ] \
    || fail "管理员密码交付标记格式无效"
  [[ "${handoff_id}" =~ ^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$ ]] \
    || fail "administrator handoff ID is invalid"
  [[ "${jwt_public_key_fingerprint}" =~ ^[0-9a-f]{64}$ ]] \
    && [ "${jwt_public_key_fingerprint}" = "$(admin_handoff_public_key_fingerprint)" ] \
    || fail "administrator handoff JWT public key fingerprint does not match the current key"
  normalize_admin_username_for_handoff "${username}"
}

read_pending_admin_handoff_username() {
  read_admin_handoff_username "$(pending_admin_handoff_path)"
}

read_admin_handoff_id() {
  local marker_file="$1"
  local handoff_id
  read_admin_handoff_username "${marker_file}" >/dev/null
  handoff_id="$(existing_env_value "${marker_file}" HANDOFF_ID)"
  [[ "${handoff_id}" =~ ^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$ ]] \
    || fail "administrator handoff ID is invalid"
  printf '%s' "${handoff_id}"
}

pending_admin_handoff_probe() {
  local marker_file
  marker_file="$(pending_admin_handoff_path 2>/dev/null)" || return 2
  admin_handoff_marker_probe "${marker_file}"
}

delivered_admin_handoff_probe() {
  local marker_file
  marker_file="$(delivered_admin_handoff_path 2>/dev/null)" || return 2
  admin_handoff_marker_probe "${marker_file}"
}

pending_admin_handoff_exists() {
  local status=0
  pending_admin_handoff_probe || status=$?
  case "${status}" in
    0) return 0 ;;
    1) return 1 ;;
    *) fail "administrator handoff pending state is inaccessible or insecure" ;;
  esac
}

delivered_admin_handoff_exists() {
  local status=0
  delivered_admin_handoff_probe || status=$?
  case "${status}" in
    0) return 0 ;;
    1) return 1 ;;
    *) fail "administrator handoff delivered state is inaccessible or insecure" ;;
  esac
}

acknowledge_admin_handoff_delivery() {
  local pending_file
  local delivered_file
  pending_file="$(pending_admin_handoff_path)"
  delivered_file="$(delivered_admin_handoff_path)"
  read_admin_handoff_username "${pending_file}" >/dev/null
  [ ! -e "${delivered_file}" ] && [ ! -L "${delivered_file}" ] \
    || fail "administrator handoff delivered marker already exists"
  mv -- "${pending_file}" "${delivered_file}"
  sync -f "$(admin_handoff_state_dir)"
}

clear_delivered_admin_handoff_marker() {
  local delivered_file
  delivered_file="$(delivered_admin_handoff_path)"
  read_admin_handoff_username "${delivered_file}" >/dev/null
  rm -- "${delivered_file}"
  sync -f "$(admin_handoff_state_dir)"
}

security_env_value() {
  local env_file="$1"
  local key="$2"
  local count
  count="$(env_key_occurrence_count "${env_file}" "${key}")" || return 2
  case "${count}" in
    0) return 0 ;;
    1) existing_env_value "${env_file}" "${key}" ;;
    *)
      printf '[INVALID] configuration: %s must appear at most once\n' "${key}" >&2
      return 2
      ;;
  esac
}

assert_security_env_keys_unique() {
  local env_file="$1"
  local key
  for key in \
    INSTALL_ROLE AUTH_MODE DATABASE_URL \
    AUTH_JWT_PRIVATE_KEY_PATH AUTH_JWT_PUBLIC_KEY_PATH JWT_PUBLIC_KEY \
    AUTH_ACCESS_TOKEN_TTL AUTH_REFRESH_TOKEN_TTL \
    CORE_HTTP_ADDR CORE_HTTP_PUBLIC_HOST \
    CORE_HTTP_TLS_CERT_PATH CORE_HTTP_TLS_KEY_PATH \
    CORE_GRPC_ADDR CORE_GRPC_TLS_DOMAIN_NAME \
    CORE_GRPC_TLS_CERT_PATH CORE_GRPC_TLS_KEY_PATH \
    CORE_GRPC_TLS_CLIENT_CA_PATH CORE_GRPC_TLS_SERVER_CA_PATH \
    CORE_AGENT_CA_CERT_PATH CORE_AGENT_CA_KEY_PATH \
    CORE_AGENT_CAPABILITY_JWT_PRIVATE_KEY_PATH \
    CORE_AGENT_CAPABILITY_JWT_PUBLIC_KEY_PATH CORE_AGENT_CAPABILITY_TTL_SEC \
    CORE_INSTANCE_ID CORE_AGENT_MANAGEMENT_CLIENT_CERT_PATH \
    CORE_AGENT_MANAGEMENT_CLIENT_KEY_PATH CORE_AGENT_MANAGEMENT_CA_PATH \
    SOURCE_GATEWAY_BASE_URL SOURCE_GATEWAY_TLS_INSECURE_SKIP_VERIFY \
    SOURCE_GATEWAY_PREFETCH_POLL_MS SOURCE_GATEWAY_PREFETCH_TIMEOUT_MS \
    NODE_ID AGENT_NODE_ID AGENT_CORE_ENDPOINT AGENT_IDENTITY_DIR \
    AGENT_CERT_PATH AGENT_KEY_PATH AGENT_CA_PATH AGENT_TLS_DOMAIN_NAME \
    AGENT_MANAGEMENT_ADDR AGENT_MANAGEMENT_PORT \
    AGENT_ZLM_HOOK_ADDR AGENT_ZLM_HOOK_PORT AGENT_ZLM_HOOK_QUEUE_CAPACITY \
    AGENT_ZLM_HOOK_TIMEOUT_SEC ZLM_HOOK_SHARED_SECRET ZLM_HOOK_BASE \
    ZLM_HTTP_PORT ZLM_API_HOST ZLM_API_BASE ZLM_API_SECRET \
    ZLM_API_ALLOW_IP_RANGE; do
    [ "$(env_key_occurrence_count "${env_file}" "${key}")" -le 1 ] || {
      printf '[INVALID] configuration: %s must appear at most once\n' "${key}" >&2
      return 1
    }
  done
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

validate_private_public_key_pair_for_service() {
  if [ "$(id -u)" -ne 0 ] || [ "${EMULATED_SECURITY_METADATA:-0}" -eq 1 ]; then
    validate_private_public_key_pair "$1" "$2"
    return
  fi
  runuser -u "${SERVICE_USER}" -- bash -c '
    set -euo pipefail
    private="$(openssl pkey -in "$1" -pubout -outform DER 2>/dev/null | sha256sum | awk "{print \$1}")"
    public="$(openssl pkey -pubin -in "$2" -outform DER 2>/dev/null | sha256sum | awk "{print \$1}")"
    [ -n "${private}" ] && [ "${private}" = "${public}" ]
  ' streamserver-key-check "$1" "$2"
}

validate_certificate_key_pair_for_service() {
  if [ "$(id -u)" -ne 0 ] || [ "${EMULATED_SECURITY_METADATA:-0}" -eq 1 ]; then
    validate_certificate_key_pair "$1" "$2"
    return
  fi
  runuser -u "${SERVICE_USER}" -- bash -c '
    set -euo pipefail
    openssl x509 -in "$1" -noout >/dev/null 2>&1
    cert="$(openssl x509 -in "$1" -pubkey -noout 2>/dev/null | openssl pkey -pubin -outform DER 2>/dev/null | sha256sum | awk "{print \$1}")"
    key="$(openssl pkey -in "$2" -pubout -outform DER 2>/dev/null | sha256sum | awk "{print \$1}")"
    [ -n "${cert}" ] && [ "${cert}" = "${key}" ]
  ' streamserver-cert-check "$1" "$2"
}

validate_x509_ca_certificate_for_service() {
  if [ "$(id -u)" -ne 0 ] || [ "${EMULATED_SECURITY_METADATA:-0}" -eq 1 ]; then
    validate_x509_ca_certificate "$1"
    return
  fi
  runuser -u "${SERVICE_USER}" -- bash -c '
    set -euo pipefail
    openssl x509 -in "$1" -noout >/dev/null 2>&1
    openssl x509 -in "$1" -noout -text 2>/dev/null | grep -q "CA:TRUE"
  ' streamserver-ca-check "$1"
}

validate_certificate_directly_issued_by_ca_for_service() {
  local certificate_path="$1"
  local ca_path="$2"
  local validation_script='
    set -euo pipefail
    certificate_issuer="$(openssl x509 -in "$1" -noout -issuer -nameopt RFC2253)"
    ca_subject="$(openssl x509 -in "$2" -noout -subject -nameopt RFC2253)"
    [ "${certificate_issuer#issuer=}" = "${ca_subject#subject=}" ]
    openssl verify -trusted "$2" -no-CAfile -no-CApath -partial_chain "$1" >/dev/null
  '
  if [ "$(id -u)" -ne 0 ] || [ "${EMULATED_SECURITY_METADATA:-0}" -eq 1 ]; then
    bash -c "${validation_script}" streamserver-direct-issuer-check \
      "${certificate_path}" "${ca_path}"
    return
  fi
  runuser -u "${SERVICE_USER}" -- bash -c "${validation_script}" \
    streamserver-direct-issuer-check "${certificate_path}" "${ca_path}"
}

validate_exact_uri_san_for_service() {
  local certificate_path="$1"
  local expected_uri="$2"
  local validation_script='
    set -euo pipefail
    san="$(openssl x509 -in "$1" -noout -ext subjectAltName 2>/dev/null)"
    [ "$(printf "%s\n" "${san}" | grep -Eo "DNS:|IP Address:|URI:|email:" | wc -l)" -eq 1 ]
    printf "%s\n" "${san}" | grep -Fq "URI:$2"
  '
  if [ "$(id -u)" -ne 0 ] || [ "${EMULATED_SECURITY_METADATA:-0}" -eq 1 ]; then
    bash -c "${validation_script}" streamserver-uri-san-check \
      "${certificate_path}" "${expected_uri}"
    return
  fi
  runuser -u "${SERVICE_USER}" -- bash -c "${validation_script}" \
    streamserver-uri-san-check "${certificate_path}" "${expected_uri}"
}

validate_server_leaf_profile_for_service() {
  local certificate_path="$1"
  local validation_script='
    set -euo pipefail
    certificate_path="$1"
    require_exact_extension() {
      local extension_name="$1"
      local expected_header="$2"
      local expected_value="$3"
      local extension_output
      local extension_header
      local extension_value
      extension_output="$(openssl x509 -in "${certificate_path}" -noout \
        -ext "${extension_name}" 2>/dev/null)"
      extension_header="$(printf "%s\n" "${extension_output}" \
        | sed -n "1{s/^[[:space:]]*//;s/[[:space:]]*$//;p;}")"
      extension_value="$(printf "%s\n" "${extension_output}" \
        | sed -e "1d" -e "s/^[[:space:]]*//" \
          -e "s/[[:space:]]*$//" -e "/^$/d")"
      [ "${extension_header}" = "${expected_header}" ]
      [ "${extension_value}" = "${expected_value}" ]
    }
    openssl x509 -in "${certificate_path}" -noout -checkend 0 >/dev/null
    require_exact_extension basicConstraints \
      "X509v3 Basic Constraints: critical" "CA:FALSE"
    require_exact_extension keyUsage \
      "X509v3 Key Usage: critical" "Digital Signature"
    require_exact_extension extendedKeyUsage \
      "X509v3 Extended Key Usage: critical" "TLS Web Server Authentication"
  '
  if [ "$(id -u)" -ne 0 ] || [ "${EMULATED_SECURITY_METADATA:-0}" -eq 1 ]; then
    bash -c "${validation_script}" streamserver-server-leaf-profile-check \
      "${certificate_path}"
    return
  fi
  runuser -u "${SERVICE_USER}" -- bash -c "${validation_script}" \
    streamserver-server-leaf-profile-check "${certificate_path}"
}

validate_certificate_san_name_for_service() {
  local certificate_path="$1"
  local expected_name="$2"
  local expected_kind=dns
  local validation_script='
    set -euo pipefail
    match_output=""
    san="$(openssl x509 -in "$1" -noout -ext subjectAltName 2>/dev/null)"
    printf "%s\n" "${san}" \
      | grep -Eq "^[[:space:]]*X509v3 Subject Alternative Name:"
    case "$3" in
      ip)
        match_output="$(openssl x509 -in "$1" -noout -checkip "$2")"
        [ "${match_output}" = "IP $2 does match certificate" ]
        ;;
      dns)
        match_output="$(openssl x509 -in "$1" -noout -checkhost "$2")"
        [ "${match_output}" = "Hostname $2 does match certificate" ]
        ;;
      *) exit 1 ;;
    esac
  '
  validate_internal_pki_host "${expected_name}" || return 1
  if validate_ipv4_literal "${expected_name}" \
    || validate_ipv6_literal "${expected_name}"; then
    expected_kind=ip
  fi
  if [ "$(id -u)" -ne 0 ] || [ "${EMULATED_SECURITY_METADATA:-0}" -eq 1 ]; then
    bash -c "${validation_script}" streamserver-certificate-san-name-check \
      "${certificate_path}" "${expected_name}" "${expected_kind}"
    return
  fi
  runuser -u "${SERVICE_USER}" -- bash -c "${validation_script}" \
    streamserver-certificate-san-name-check \
    "${certificate_path}" "${expected_name}" "${expected_kind}"
}

validate_ed25519_private_public_key_pair_for_service() {
  local private_key_path="$1"
  local public_key_path="$2"
  local validation_script='
    set -euo pipefail
    private="$(openssl pkey -in "$1" -pubout -outform DER 2>/dev/null | sha256sum | awk "{print \$1}")"
    public="$(openssl pkey -pubin -in "$2" -outform DER 2>/dev/null | sha256sum | awk "{print \$1}")"
    [ -n "${private}" ] && [ "${private}" = "${public}" ]
    openssl pkey -pubin -in "$2" -text_pub -noout 2>/dev/null | grep -qi ED25519
  '
  if [ "$(id -u)" -ne 0 ] || [ "${EMULATED_SECURITY_METADATA:-0}" -eq 1 ]; then
    bash -c "${validation_script}" streamserver-ed25519-check \
      "${private_key_path}" "${public_key_path}"
    return
  fi
  runuser -u "${SERVICE_USER}" -- bash -c "${validation_script}" \
    streamserver-ed25519-check "${private_key_path}" "${public_key_path}"
}

validate_distinct_ca_roots_for_service() {
  local validation_script='
    set -euo pipefail
    cert_fingerprints="$(for root in "$@"; do
      openssl x509 -in "${root}" -outform DER 2>/dev/null | sha256sum | awk "{print \$1}"
    done)"
    key_fingerprints="$(for root in "$@"; do
      openssl x509 -in "${root}" -pubkey -noout 2>/dev/null \
        | openssl pkey -pubin -outform DER 2>/dev/null \
        | sha256sum | awk "{print \$1}"
    done)"
    [ "$(printf "%s\n" "${cert_fingerprints}" | sort -u | wc -l)" -eq 3 ]
    [ "$(printf "%s\n" "${key_fingerprints}" | sort -u | wc -l)" -eq 3 ]
  '
  if [ "$(id -u)" -ne 0 ] || [ "${EMULATED_SECURITY_METADATA:-0}" -eq 1 ]; then
    bash -c "${validation_script}" streamserver-root-separation-check "$@"
    return
  fi
  runuser -u "${SERVICE_USER}" -- bash -c "${validation_script}" \
    streamserver-root-separation-check "$@"
}

validate_ca_present_in_bundle_for_service() {
  local ca_path="$1"
  local bundle_path="$2"
  local validation_script='
    set -euo pipefail
    openssl verify -CAfile "$2" -no-CApath "$1" >/dev/null
  '
  if [ "$(id -u)" -ne 0 ] || [ "${EMULATED_SECURITY_METADATA:-0}" -eq 1 ]; then
    bash -c "${validation_script}" streamserver-ca-bundle-check \
      "${ca_path}" "${bundle_path}"
    return
  fi
  runuser -u "${SERVICE_USER}" -- bash -c "${validation_script}" \
    streamserver-ca-bundle-check "${ca_path}" "${bundle_path}"
}

is_canonical_non_nil_uuid() {
  local value="$1"
  [[ "${value}" =~ ^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$ ]] \
    && [ "${value}" != "00000000-0000-0000-0000-000000000000" ]
}

run_agent_identity_check_for_service() {
  local agent_bin="$1"
  local identity_dir="$2"
  local node_id="$3"
  (
    unset ADMIN_PASSWORD AGENT_ENROLLMENT_TOKEN
    if [ "$(id -u)" -eq 0 ] && [ "${EMULATED_SECURITY_METADATA:-0}" -ne 1 ]; then
      runuser -u "${SERVICE_USER}" -- env -i \
        PATH=/usr/sbin:/usr/bin:/sbin:/bin LANG=C.UTF-8 \
        "${agent_bin}" identity check \
        --node-id "${node_id}" --identity-dir "${identity_dir}"
    else
      env -i PATH=/usr/sbin:/usr/bin:/sbin:/bin LANG=C.UTF-8 \
        "${agent_bin}" identity check \
        --node-id "${node_id}" --identity-dir "${identity_dir}"
    fi
  )
}

select_readonly_security_probe_runtime_root() {
  [ -n "${SECURITY_PROBE_RUNTIME_ROOT:-}" ] || SECURITY_PROBE_RUNTIME_ROOT="/opt"
}

prepare_security_probe_runtime_root() {
  local runtime_root="${SECURITY_PROBE_RUNTIME_ROOT:-${INSTALL_DIR}}"
  local lexical_path
  local resolved_path
  local mode
  if [ -z "${runtime_root}" ] || [[ "${runtime_root}" != /* ]]; then
    fail "security probe runtime root must be an absolute path"
  fi
  lexical_path="$(realpath -ms -- "${runtime_root}")" || fail "security probe runtime root cannot be normalized"
  resolved_path="$(realpath -m -- "${runtime_root}")" || fail "security probe runtime root cannot be resolved"
  [ "${lexical_path}" = "${resolved_path}" ] || fail "security probe runtime root must not traverse a symbolic link"
  SECURITY_PROBE_RUNTIME_ROOT="${resolved_path}"
  if [ ! -d "${SECURITY_PROBE_RUNTIME_ROOT}" ] || [ -L "${SECURITY_PROBE_RUNTIME_ROOT}" ]; then
    fail "security probe runtime root must be a real directory"
  fi
  if [ "$(id -u)" -eq 0 ] && [ "${EMULATED_SECURITY_METADATA:-0}" -ne 1 ]; then
    admin_handoff_assert_no_symlink_boundary "${SECURITY_PROBE_RUNTIME_ROOT}"
    admin_handoff_assert_secure_root_ancestors "${SECURITY_PROBE_RUNTIME_ROOT}"
    [ "$(stat -c '%u' -- "${SECURITY_PROBE_RUNTIME_ROOT}")" = 0 ] || fail "security probe runtime root must be owned by root"
    mode="$(stat -c '%a' -- "${SECURITY_PROBE_RUNTIME_ROOT}")" || fail "cannot inspect security probe runtime root mode"
    if (( (8#${mode} & 8#022) != 0 )); then
      fail "security probe runtime root must not be group/world writable"
    fi
  else
    [ -x "${SECURITY_PROBE_RUNTIME_ROOT}" ] || fail "security probe runtime root is not traversable"
  fi
}

stage_verified_package_executable() {
  local relative_path="$1"
  local expected_sha256="$2"
  local target_name="$3"
  local output_variable="$4"
  local package_root_resolved
  local source_path
  local source_resolved
  local source_sha256
  local temporary_file
  local temporary_identity
  local target_path
  local target_sha256
  validate_package_relative_path "${relative_path}" || fail "security probe executable path is invalid"
  [[ "${expected_sha256}" =~ ^[0-9a-f]{64}$ ]] || fail "verified security probe checksum is unavailable"
  case "${target_name}" in
    media-core|media-agent) ;;
    *) fail "security probe executable name is invalid" ;;
  esac
  case "${output_variable}" in
    SECURITY_PROBE_CORE_BIN|SECURITY_PROBE_AGENT_BIN) ;;
    *) fail "security probe output variable is invalid" ;;
  esac
  package_root_resolved="$(cd "${PACKAGE_ROOT}" && pwd -P)" || fail "package root cannot be resolved for security probe staging"
  source_path="${PACKAGE_ROOT}/${relative_path}"
  admin_handoff_no_symlink_boundary_status "${source_path}" \
    || fail "package security probe executable path contains a symbolic link"
  if [ ! -f "${source_path}" ] || [ -L "${source_path}" ] || [ ! -x "${source_path}" ]; then
    fail "verified package security probe executable is missing or unsafe"
  fi
  [ "$(stat -c '%h' -- "${source_path}")" = 1 ] || fail "package security probe executable must have exactly one hard link"
  source_resolved="$(realpath -e -- "${source_path}")" || fail "package security probe executable cannot be resolved"
  case "${source_resolved}" in
    "${package_root_resolved}"/*) ;;
    *) fail "package security probe executable escapes the package root" ;;
  esac
  source_sha256="$(sha256sum "${source_path}" | awk '{print $1}')" || fail "cannot hash package security probe executable"
  [ "${source_sha256}" = "${expected_sha256}" ] || fail "package security probe executable changed after checksum verification"

  temporary_file="${SECURITY_PROBE_DIR}/.${target_name}.tmp"
  INSTALLER_TEMP_FILES+=("${temporary_file}")
  (umask 077; set -C; : >"${temporary_file}") 2>/dev/null || fail "cannot allocate security probe executable"
  temporary_identity="$(stat -Lc '%d:%i' -- "${temporary_file}")" || fail "cannot capture security probe temporary identity"
  cp --reflink=auto -- "${source_path}" "${temporary_file}" || fail "cannot stage verified package security probe executable"
  [ "$(stat -Lc '%d:%i' -- "${temporary_file}")" = "${temporary_identity}" ] || fail "security probe temporary executable was replaced while being written"
  [ "$(sha256sum "${source_path}" | awk '{print $1}')" = "${expected_sha256}" ] || fail "package security probe executable changed while being staged"
  chmod 0555 "${temporary_file}"
  if [ "$(id -u)" -eq 0 ] && [ "${EMULATED_SECURITY_METADATA:-0}" -ne 1 ]; then
    chown root:root "${temporary_file}"
  fi
  sync -f "${temporary_file}"
  target_path="${SECURITY_PROBE_DIR}/${target_name}"
  if [ -e "${target_path}" ] || [ -L "${target_path}" ]; then
    fail "security probe target unexpectedly exists"
  fi
  mv -T -- "${temporary_file}" "${target_path}" || fail "cannot publish security probe executable"
  sync -f "${SECURITY_PROBE_DIR}"
  if [ ! -f "${target_path}" ] || [ -L "${target_path}" ]; then
    fail "published security probe executable metadata is invalid"
  fi
  [ "$(stat -c '%h:%a' -- "${target_path}")" = "1:555" ] || fail "published security probe executable metadata is invalid"
  if [ "$(id -u)" -eq 0 ] && [ "${EMULATED_SECURITY_METADATA:-0}" -ne 1 ]; then
    [ "$(stat -c '%u:%g' -- "${target_path}")" = "0:0" ] || fail "published security probe executable must be root-owned"
  fi
  target_sha256="$(sha256sum "${target_path}" | awk '{print $1}')" || fail "cannot hash published security probe executable"
  [ "${target_sha256}" = "${expected_sha256}" ] || fail "published security probe executable checksum is invalid"
  printf -v "${output_variable}" '%s' "${target_path}"
}

prepare_package_security_probe_binaries() {
  local probe_nonce
  local service_gid
  cleanup_security_probe_binaries \
    || fail "failed to clean a previous package security probe"
  assert_verified_package_checksum_manifest_unchanged
  prepare_security_probe_runtime_root
  probe_nonce="$(openssl rand -hex 16)" || fail "cannot generate a security probe directory nonce"
  SECURITY_PROBE_DIR="${SECURITY_PROBE_RUNTIME_ROOT}/probe.${probe_nonce}"
  (umask 077; mkdir -- "${SECURITY_PROBE_DIR}") || fail "cannot allocate a security probe directory"
  SECURITY_PROBE_DIR_IDENTITY="$(stat -Lc '%d:%i' -- "${SECURITY_PROBE_DIR}")" || fail "cannot capture security probe directory identity"
  if [ "$(id -u)" -eq 0 ] && [ "${EMULATED_SECURITY_METADATA:-0}" -ne 1 ]; then
    service_gid="$(getent group "${SERVICE_GROUP}" | cut -d: -f3)" || fail "installed service group is unavailable for security preflight"
    [[ "${service_gid}" =~ ^[0-9]+$ ]] || fail "installed service group identity is invalid"
    chown "root:${SERVICE_GROUP}" "${SECURITY_PROBE_DIR}"
    chmod 0750 "${SECURITY_PROBE_DIR}"
    [ "$(stat -c '%u:%g:%a' -- "${SECURITY_PROBE_DIR}")" = "0:${service_gid}:750" ] || fail "security probe directory permissions are invalid"
  else
    chmod 0700 "${SECURITY_PROBE_DIR}"
  fi
  stage_verified_package_executable "${MEDIA_CORE_BINARY_PATH:-}" "${VERIFIED_PACKAGE_CORE_SHA256}" media-core SECURITY_PROBE_CORE_BIN
  stage_verified_package_executable "${MEDIA_AGENT_BINARY_PATH:-}" "${VERIFIED_PACKAGE_AGENT_SHA256}" media-agent SECURITY_PROBE_AGENT_BIN
  assert_verified_package_checksum_manifest_unchanged
}

security_preflight_env() {
  set +x
  local env_file="$1"
  local core_bin="${2:-}"
  local preflight_scope="${4:-full}"
  local INSTALL_DIR
  local role auth_mode database_url jwt_private jwt_public jwt_external
  local http_addr http_public_host http_cert http_key
  local grpc_domain grpc_cert grpc_key grpc_client_ca
  local grpc_server_ca agent_signing_ca agent_signing_key
  local capability_private capability_public capability_ttl core_instance_id
  local management_client_cert management_client_key management_client_ca
  local agent_endpoint agent_identity_dir agent_node_id agent_domain agent_bin
  local zlm_http_port zlm_api_base zlm_api_secret expected_zlm_api_base
  local zlm_api_allow_ip_range core_hook_secret
  local zlm_hook_addr zlm_hook_port zlm_hook_base zlm_hook_secret
  local zlm_hook_queue_capacity zlm_hook_timeout
  local failures=0
  local handoff_probe_status=0
  local delivered_probe_status=0

  if [ ! -f "${env_file}" ]; then
    printf '[MISSING] configuration: %s does not exist\n' "${env_file}" >&2
    return 1
  fi
  if [ "$(env_key_occurrence_count "${env_file}" CORE_INSECURE_DEV)" -gt 0 ]; then
    printf '[INVALID] configuration: CORE_INSECURE_DEV is unsupported; use media-core --insecure-dev only for local development\n' >&2
    return 1
  fi
  if [ "$(env_key_occurrence_count "${env_file}" STREAMSERVER_ENV)" -gt 0 ]; then
    printf '[INVALID] configuration: STREAMSERVER_ENV is reserved by the native service launcher\n' >&2
    return 1
  fi
  assert_security_env_keys_unique "${env_file}" || return 1
  INSTALL_DIR="$(cd "$(dirname "${env_file}")" && pwd -P)"
  role="$(security_env_value "${env_file}" INSTALL_ROLE)"
  [ -n "${core_bin}" ] || core_bin="$(cd "$(dirname "${env_file}")" && pwd)/bin/media-core"
  agent_bin="${3:-$(cd "$(dirname "${env_file}")" && pwd)/bin/media-agent}"
  case "${preflight_scope}" in
    full|upgrade-gate) ;;
    core-only)
      role_has_core "${role}" || {
        printf '[INVALID] configuration: Core-only preflight requires a Core role\n' >&2
        return 1
      }
      ;;
    *)
      printf '[INVALID] configuration: unsupported security preflight scope\n' >&2
      return 1
      ;;
  esac
  case "${role}" in
    control-plane|worker-host-cpu|worker-host-gpu|all-in-one-host-cpu|all-in-one-host-gpu) ;;
    *)
      printf '[MISSING] configuration: INSTALL_ROLE is missing or unsupported\n' >&2
      return 1
      ;;
  esac
  if [ "${preflight_scope}" != core-only ] && role_has_worker "${role}"; then
    zlm_http_port="$(security_env_value "${env_file}" ZLM_HTTP_PORT)"
    zlm_api_base="$(security_env_value "${env_file}" ZLM_API_BASE)"
    zlm_api_secret="$(security_env_value "${env_file}" ZLM_API_SECRET)"
    zlm_api_allow_ip_range="$(security_env_value "${env_file}" ZLM_API_ALLOW_IP_RANGE)"
    expected_zlm_api_base="http://127.0.0.1:${zlm_http_port}"
    zlm_hook_addr="$(security_env_value "${env_file}" AGENT_ZLM_HOOK_ADDR)"
    zlm_hook_port="$(security_env_value "${env_file}" AGENT_ZLM_HOOK_PORT)"
    zlm_hook_base="$(security_env_value "${env_file}" ZLM_HOOK_BASE)"
    zlm_hook_secret="$(security_env_value "${env_file}" ZLM_HOOK_SHARED_SECRET)"
    zlm_hook_queue_capacity="$(security_env_value "${env_file}" AGENT_ZLM_HOOK_QUEUE_CAPACITY)"
    zlm_hook_timeout="$(security_env_value "${env_file}" AGENT_ZLM_HOOK_TIMEOUT_SEC)"
    core_hook_secret=""
    if role_has_core "${role}"; then
      core_hook_secret="$(security_env_value "${env_file}" HOOK_SHARED_SECRET)"
    elif [ "$(env_key_occurrence_count "${env_file}" HOOK_SHARED_SECRET)" -ne 0 ]; then
      printf '[INVALID] Agent/ZLM control: worker-only configuration must not contain the Core hook credential\n' >&2
      failures=$((failures + 1))
    fi
    if [ "$(env_key_occurrence_count "${env_file}" ZLM_API_HOST)" -ne 0 ]; then
      printf '[INVALID] Agent/ZLM control: legacy ZLM_API_HOST must be removed\n' >&2
      failures=$((failures + 1))
    elif [[ ! "${zlm_http_port}" =~ ^[0-9]+$ ]] \
      || [ "${zlm_http_port}" -lt 1 ] || [ "${zlm_http_port}" -gt 65535 ] \
      || [ "${zlm_api_base}" != "${expected_zlm_api_base}" ] \
      || [ "${zlm_api_allow_ip_range}" != "::1,127.0.0.1,10.0.0.0-10.255.255.255,172.16.0.0-172.31.255.255,192.168.0.0-192.168.255.255" ] \
      || ! is_strong_url_safe_secret "${zlm_api_secret}" \
      || [ "${zlm_api_secret}" = "${zlm_hook_secret}" ] \
      || { [ -n "${core_hook_secret}" ] && [ "${zlm_api_secret}" = "${core_hook_secret}" ]; }; then
      printf '[INVALID] Agent/ZLM control: Agent API target must be loopback, the shared HTTP listener must retain the native media CIDRs, and the API credential must be strong and independent\n' >&2
      failures=$((failures + 1))
    else
      printf '[OK] Agent/ZLM control: Agent uses loopback while the shared HTTP listener retains native media CIDRs\n'
    fi
    if [[ ! "${zlm_hook_port}" =~ ^[0-9]+$ ]] \
      || [ "${zlm_hook_port}" -lt 1 ] || [ "${zlm_hook_port}" -gt 65535 ] \
      || [ "${zlm_hook_addr}" != "127.0.0.1:${zlm_hook_port}" ] \
      || [ "${zlm_hook_base}" != "http://127.0.0.1:${zlm_hook_port}/internal/zlm-hooks" ]; then
      printf '[INVALID] Agent/ZLM hook: ingress and ZLM target must use the same loopback port\n' >&2
      failures=$((failures + 1))
    elif [[ ! "${zlm_hook_secret}" =~ ^[A-Za-z0-9._~-]+$ ]] \
      || [ "${#zlm_hook_secret}" -lt 32 ] || [ "${#zlm_hook_secret}" -gt 256 ] \
      || [ "${zlm_hook_secret}" = "${zlm_api_secret}" ] \
      || { [ -n "${core_hook_secret}" ] && [ "${zlm_hook_secret}" = "${core_hook_secret}" ]; }; then
      printf '[INVALID] Agent/ZLM hook: shared secret must be a strong URL-safe token\n' >&2
      failures=$((failures + 1))
    elif [[ ! "${zlm_hook_queue_capacity}" =~ ^[0-9]+$ ]] \
      || [ "${zlm_hook_queue_capacity}" -lt 1 ] \
      || [ "${zlm_hook_queue_capacity}" -gt 1024 ] \
      || [[ ! "${zlm_hook_timeout}" =~ ^[0-9]+$ ]] \
      || [ "${zlm_hook_timeout}" -lt 1 ] || [ "${zlm_hook_timeout}" -ge 5 ]; then
      printf '[INVALID] Agent/ZLM hook: queue must be 1..1024 and relay timeout must be 1..4 seconds\n' >&2
      failures=$((failures + 1))
    else
      printf '[OK] Agent/ZLM hook: authenticated loopback relay is bounded\n'
    fi
  elif ! role_has_worker "${role}"; then
    if [ "$(env_key_occurrence_count "${env_file}" ZLM_API_HOST)" -ne 0 ] \
      || [ "$(env_key_occurrence_count "${env_file}" ZLM_API_BASE)" -ne 0 ] \
      || [ "$(env_key_occurrence_count "${env_file}" ZLM_API_SECRET)" -ne 0 ] \
      || [ "$(env_key_occurrence_count "${env_file}" ZLM_API_ALLOW_IP_RANGE)" -ne 0 ]; then
      printf '[INVALID] Agent/ZLM control: control-plane-only configuration must not contain a ZLM direct endpoint\n' >&2
      failures=$((failures + 1))
    fi
  fi
  pending_admin_handoff_probe || handoff_probe_status=$?
  case "${handoff_probe_status}" in
    0)
      if [ "${INITIAL_ADMIN_PASSWORD_READY:-0}" -ne 1 ]; then
        if [ "${preflight_scope}" = upgrade-gate ] && [ "${INTERACTIVE_INSTALL:-0}" -eq 1 ]; then
          printf '[PENDING] auth/admin: valid administrator handoff will be recovered after application quiesce\n'
        else
          printf '[UNKNOWN] auth/admin: one-time administrator password delivery is pending; resume the interactive installer\n' >&2
          failures=$((failures + 1))
        fi
      fi
      ;;
    1) ;;
    *)
      printf '[UNKNOWN] auth/admin: administrator handoff state is inaccessible or insecure; run this preflight as root\n' >&2
      failures=$((failures + 1))
      ;;
  esac

  delivered_admin_handoff_probe || delivered_probe_status=$?
  case "${delivered_probe_status}" in
    0)
      if [ "${handoff_probe_status}" -eq 0 ]; then
        printf '[UNKNOWN] auth/admin: pending and delivered administrator handoff markers both exist\n' >&2
        failures=$((failures + 1))
      elif ! (read_admin_handoff_username "$(delivered_admin_handoff_path)" >/dev/null 2>&1); then
        printf '[UNKNOWN] auth/admin: delivered administrator handoff state is malformed or does not match the current key\n' >&2
        failures=$((failures + 1))
      else
        printf '[OK] auth/admin: delivered administrator handoff state is valid\n'
      fi
      ;;
    1) ;;
    *)
      printf '[UNKNOWN] auth/admin: delivered administrator handoff state is inaccessible or insecure; run this preflight as root\n' >&2
      failures=$((failures + 1))
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
        elif ! validate_private_public_key_pair_for_service "${jwt_private}" "${jwt_public}"; then
          printf '[INVALID] auth/admin: local_password JWT private/public key pair is missing or invalid\n' >&2
          failures=$((failures + 1))
        elif [ ! -x "${core_bin}" ]; then
          printf '[UNKNOWN] auth/admin: media-core admin probe is unavailable\n' >&2
          failures=$((failures + 1))
        elif ! run_core_auth_from_installed_env \
          "${env_file}" "${core_bin}" auth check-config >/dev/null 2>&1; then
          printf '[INVALID] auth/admin: local_password JWT configuration is not valid RSA or Ed25519 PEM\n' >&2
          failures=$((failures + 1))
        elif [ "${preflight_scope}" = upgrade-gate ] && [ "${handoff_probe_status}" -eq 0 ]; then
          printf '[PENDING] auth/admin: enabled administrator check is deferred until handoff recovery\n'
        elif run_core_auth_from_installed_env \
          "${env_file}" "${core_bin}" auth check-admin >/dev/null 2>&1; then
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
        elif run_core_auth_from_installed_env \
          "${env_file}" "${core_bin}" auth check-config >/dev/null 2>&1; then
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
    http_public_host="$(security_env_value "${env_file}" CORE_HTTP_PUBLIC_HOST)"
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
    elif [ -z "${http_cert}" ] || [ -z "${http_key}" ] \
      || [ -z "${http_public_host}" ]; then
      printf '[MISSING] HTTP TLS: certificate, key and CORE_HTTP_PUBLIC_HOST must be configured together\n' >&2
      failures=$((failures + 1))
    elif validate_certificate_key_pair_for_service "${http_cert}" "${http_key}" \
      && validate_server_leaf_profile_for_service "${http_cert}" \
      && validate_certificate_san_name_for_service \
        "${http_cert}" "${http_public_host}"; then
      printf '[OK] HTTP TLS: server profile, public-host SAN and matching private key verified\n'
    else
      printf '[INVALID] HTTP TLS: certificate profile, public-host SAN or matching private key is invalid\n' >&2
      failures=$((failures + 1))
    fi

    grpc_cert="$(security_env_value "${env_file}" CORE_GRPC_TLS_CERT_PATH)"
    grpc_key="$(security_env_value "${env_file}" CORE_GRPC_TLS_KEY_PATH)"
    grpc_client_ca="$(security_env_value "${env_file}" CORE_GRPC_TLS_CLIENT_CA_PATH)"
    grpc_domain="$(security_env_value "${env_file}" CORE_GRPC_TLS_DOMAIN_NAME)"
    grpc_cert="$(resolve_security_path "${env_file}" "${grpc_cert}")"
    grpc_key="$(resolve_security_path "${env_file}" "${grpc_key}")"
    grpc_client_ca="$(resolve_security_path "${env_file}" "${grpc_client_ca}")"
    if [ -z "${grpc_cert}" ] || [ -z "${grpc_key}" ] \
      || [ -z "${grpc_client_ca}" ] || [ -z "${grpc_domain}" ]; then
      printf '[MISSING] gRPC mTLS: server certificate, key, client CA and TLS name are all required\n' >&2
      failures=$((failures + 1))
    elif validate_certificate_key_pair_for_service "${grpc_cert}" "${grpc_key}" \
      && validate_server_leaf_profile_for_service "${grpc_cert}" \
      && validate_certificate_san_name_for_service "${grpc_cert}" "${grpc_domain}" \
      && validate_x509_ca_certificate_for_service "${grpc_client_ca}"; then
      printf '[OK] gRPC mTLS: server profile, SAN identity and client CA verified\n'
    else
      printf '[INVALID] gRPC mTLS: server profile, SAN identity or client CA is invalid\n' >&2
      failures=$((failures + 1))
    fi

    grpc_server_ca="$(security_env_value "${env_file}" CORE_GRPC_TLS_SERVER_CA_PATH)"
    agent_signing_ca="$(security_env_value "${env_file}" CORE_AGENT_CA_CERT_PATH)"
    agent_signing_key="$(security_env_value "${env_file}" CORE_AGENT_CA_KEY_PATH)"
    capability_private="$(security_env_value "${env_file}" CORE_AGENT_CAPABILITY_JWT_PRIVATE_KEY_PATH)"
    capability_public="$(security_env_value "${env_file}" CORE_AGENT_CAPABILITY_JWT_PUBLIC_KEY_PATH)"
    capability_ttl="$(security_env_value "${env_file}" CORE_AGENT_CAPABILITY_TTL_SEC)"
    core_instance_id="$(security_env_value "${env_file}" CORE_INSTANCE_ID)"
    management_client_cert="$(security_env_value "${env_file}" CORE_AGENT_MANAGEMENT_CLIENT_CERT_PATH)"
    management_client_key="$(security_env_value "${env_file}" CORE_AGENT_MANAGEMENT_CLIENT_KEY_PATH)"
    management_client_ca="$(security_env_value "${env_file}" CORE_AGENT_MANAGEMENT_CA_PATH)"
    grpc_server_ca="$(resolve_security_path "${env_file}" "${grpc_server_ca}")"
    agent_signing_ca="$(resolve_security_path "${env_file}" "${agent_signing_ca}")"
    agent_signing_key="$(resolve_security_path "${env_file}" "${agent_signing_key}")"
    capability_private="$(resolve_security_path "${env_file}" "${capability_private}")"
    capability_public="$(resolve_security_path "${env_file}" "${capability_public}")"
    management_client_cert="$(resolve_security_path "${env_file}" "${management_client_cert}")"
    management_client_key="$(resolve_security_path "${env_file}" "${management_client_key}")"
    management_client_ca="$(resolve_security_path "${env_file}" "${management_client_ca}")"
    if [ -z "${grpc_domain}" ] || [ -z "${grpc_server_ca}" ] \
      || [ -z "${agent_signing_ca}" ] || [ -z "${agent_signing_key}" ] \
      || [ -z "${capability_private}" ] || [ -z "${capability_public}" ] \
      || [ -z "${capability_ttl}" ] || [ -z "${core_instance_id}" ] \
      || [ -z "${management_client_cert}" ] || [ -z "${management_client_key}" ] \
      || [ -z "${management_client_ca}" ]; then
      printf '[MISSING] internal PKI: three trust roots, signing material, capability keys and Core identity are required\n' >&2
      failures=$((failures + 1))
    elif ! is_canonical_non_nil_uuid "${core_instance_id}" \
      || [[ ! "${capability_ttl}" =~ ^[0-9]+$ ]] \
      || [ "${capability_ttl}" -lt 10 ] || [ "${capability_ttl}" -gt 120 ] \
      || ! validate_x509_ca_certificate_for_service "${grpc_server_ca}" \
      || ! validate_certificate_key_pair_for_service "${agent_signing_ca}" "${agent_signing_key}" \
      || ! validate_x509_ca_certificate_for_service "${agent_signing_ca}" \
      || ! validate_ed25519_private_public_key_pair_for_service \
        "${capability_private}" "${capability_public}" \
      || ! validate_certificate_key_pair_for_service \
        "${management_client_cert}" "${management_client_key}" \
      || ! validate_x509_ca_certificate_for_service "${management_client_ca}" \
      || ! validate_certificate_directly_issued_by_ca_for_service \
        "${grpc_cert}" "${grpc_server_ca}" \
      || ! validate_certificate_san_name_for_service "${grpc_cert}" "${grpc_domain}" \
      || ! validate_certificate_directly_issued_by_ca_for_service \
        "${management_client_cert}" "${management_client_ca}" \
      || ! validate_exact_uri_san_for_service "${management_client_cert}" \
        "spiffe://streamserver/core/${core_instance_id}" \
      || ! validate_ca_present_in_bundle_for_service \
        "${agent_signing_ca}" "${grpc_client_ca}" \
      || ! validate_distinct_ca_roots_for_service \
        "${agent_signing_ca}" "${grpc_server_ca}" "${management_client_ca}"; then
      printf '[INVALID] internal PKI: identities, direct issuers, trust separation or capability policy is invalid\n' >&2
      failures=$((failures + 1))
    else
      printf '[OK] internal PKI: three distinct roots and all Core/Agent trust material verified\n'
    fi
  fi

  if [ "${preflight_scope}" != core-only ] && role_has_worker "${role}"; then
    agent_endpoint="$(security_env_value "${env_file}" AGENT_CORE_ENDPOINT)"
    agent_identity_dir="$(security_env_value "${env_file}" AGENT_IDENTITY_DIR)"
    agent_identity_dir="$(resolve_security_path "${env_file}" "${agent_identity_dir}")"
    agent_node_id="$(security_env_value "${env_file}" AGENT_NODE_ID)"
    [ -n "${agent_node_id}" ] || agent_node_id="$(security_env_value "${env_file}" NODE_ID)"
    agent_domain="$(security_env_value "${env_file}" AGENT_TLS_DOMAIN_NAME)"
    if [[ "${agent_endpoint}" != https://* ]] \
      || [ -z "${agent_identity_dir}" ] || [ -z "${agent_node_id}" ] \
      || [ -z "${agent_domain}" ]; then
      printf '[MISSING] worker mTLS: HTTPS endpoint, enrolled identity, node ID and TLS domain are required\n' >&2
      failures=$((failures + 1))
    elif [ ! -x "${agent_bin}" ]; then
      printf '[UNKNOWN] worker mTLS: media-agent identity probe is unavailable\n' >&2
      failures=$((failures + 1))
    elif ! is_canonical_non_nil_uuid "${agent_node_id}"; then
      printf '[INVALID] worker mTLS: Agent node ID is not a canonical non-nil UUID\n' >&2
      failures=$((failures + 1))
    elif run_agent_identity_check_for_service \
      "${agent_bin}" "${agent_identity_dir}" "${agent_node_id}"; then
      printf '[OK] worker mTLS: enrolled identity, endpoint and TLS domain verified\n'
    else
      printf '[INVALID] worker mTLS: enrolled identity is missing, expired or inconsistent\n' >&2
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

assign_local_tcp_port() {
  local variable_name="$1"
  shift
  local selected_port
  [[ "${variable_name}" =~ ^[A-Za-z_][A-Za-z0-9_]*$ ]] \
    || fail "invalid local TCP port variable name"
  selected_port="$(prompt_local_tcp_port "$@")"
  reserve_local_tcp_port "${selected_port}"
  printf -v "${variable_name}" '%s' "${selected_port}"
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
  local default_agent_tls_domain
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

begin_atomic_target_write() {
  local target="$1"
  local target_dir
  local target_name
  local mode
  target_dir="$(dirname "${target}")"
  target_name="$(basename "${target}")"
  [ ! -L "${target_dir}" ] && [ -d "${target_dir}" ] \
    || fail "native control parent must be a real directory: ${target_dir}"
  [ ! -L "${target}" ] \
    || fail "native control target must not be a symbolic link: ${target}"
  if [ -e "${target}" ] && [ ! -f "${target}" ]; then
    fail "native control target must be a regular file: ${target}"
  fi
  if [ "$(id -u)" -eq 0 ] && [ "${EMULATED_SECURITY_METADATA:-0}" -ne 1 ]; then
    [ "$(stat -c '%u' -- "${target_dir}" 2>/dev/null)" = "0" ] \
      || fail "native control parent must be root-owned: ${target_dir}"
    mode="$(stat -c '%a' -- "${target_dir}" 2>/dev/null)" \
      || fail "cannot inspect native control parent mode: ${target_dir}"
    (( (8#${mode} & 8#022) == 0 )) \
      || fail "native control parent must not be group/world writable: ${target_dir}"
  fi
  LAST_INSTALLER_TEMP_FILE="$(mktemp "${target_dir}/.${target_name}.tmp.XXXXXX")"
  INSTALLER_TEMP_FILES+=("${LAST_INSTALLER_TEMP_FILE}")
}

finish_atomic_target_write() {
  local temporary_file="$1"
  local target="$2"
  local mode="$3"
  local owner_group="${4:-root:root}"
  [ ! -L "${temporary_file}" ] && [ -f "${temporary_file}" ] \
    || fail "native control temporary file is invalid: ${temporary_file}"
  chmod "${mode}" "${temporary_file}"
  if [ "$(id -u)" -eq 0 ] && [ "${EMULATED_SECURITY_METADATA:-0}" -ne 1 ]; then
    chown -h "${owner_group}" "${temporary_file}"
  fi
  sync -f "${temporary_file}"
  # Reject an existing link explicitly. A post-check swap is still not
  # followed because rename replaces the directory entry atomically.
  [ ! -L "${target}" ] \
    || fail "native control target must not be a symbolic link: ${target}"
  if [ -e "${target}" ] && [ ! -f "${target}" ]; then
    fail "native control target must be a regular file: ${target}"
  fi
  mv -f -- "${temporary_file}" "${target}"
  sync -f "$(dirname "${target}")"
}

ensure_control_directory() {
  local path="$1"
  local mode="${2:-755}"
  local owner_group="${3:-root:root}"
  local relative
  local component
  local current="${INSTALL_DIR}"
  local -a components=()
  [[ "${mode}" =~ ^[0-7]{3,4}$ ]] \
    || fail "native control directory mode is invalid: ${mode}"
  case "${path}" in
    "${INSTALL_DIR}") relative="" ;;
    "${INSTALL_DIR}"/*) relative="${path#"${INSTALL_DIR}"/}" ;;
    *) fail "native control directory escapes the installation root: ${path}" ;;
  esac
  [ ! -L "${INSTALL_DIR}" ] && [ -d "${INSTALL_DIR}" ] \
    || fail "native installation root must be a real directory"
  if [ -z "${relative}" ]; then
    if [ "$(id -u)" -eq 0 ] && [ "${EMULATED_SECURITY_METADATA:-0}" -ne 1 ]; then
      chown -h "${owner_group}" "${INSTALL_DIR}"
    fi
    chmod "${mode}" "${INSTALL_DIR}"
    return 0
  fi
  IFS='/' read -r -a components <<<"${relative}"
  for component in "${components[@]}"; do
    [ -n "${component}" ] && [ "${component}" != . ] && [ "${component}" != .. ] \
      || fail "native control directory contains an invalid component: ${path}"
    current="${current}/${component}"
    [ ! -L "${current}" ] \
      || fail "native control directory must not contain a symbolic link: ${current}"
    if [ ! -e "${current}" ]; then
      install -d -m "${mode}" -- "${current}"
    fi
    [ -d "${current}" ] \
      || fail "native control path component must be a directory: ${current}"
    if [ "$(id -u)" -eq 0 ] && [ "${EMULATED_SECURITY_METADATA:-0}" -ne 1 ]; then
      chown -h "${owner_group}" "${current}"
    fi
    chmod "${mode}" "${current}"
  done
}

copy_file_atomically() {
  local source="$1"
  local target="$2"
  local mode="${3:-755}"
  local owner_group="${4:-root:root}"
  local temp
  # 二进制和脚本先写临时文件再 mv，避免安装中断时留下半写入目标。
  begin_atomic_target_write "${target}"
  temp="${LAST_INSTALLER_TEMP_FILE}"
  cp "${source}" "${temp}"
  finish_atomic_target_write "${temp}" "${target}" "${mode}" "${owner_group}"
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
  local target_parent
  local staging_parent
  local staging
  local source_before
  local source_after
  local staging_fingerprint
  [ ! -L "${source}" ] && [ -d "${source}" ] \
    || fail "native package tree source must be a real directory: ${source}"
  assert_control_tree_safe "${source}" structural
  [ ! -L "${target}" ] \
    || fail "native control tree target must not be a symbolic link: ${target}"
  if [ -e "${target}" ]; then
    [ -d "${target}" ] \
      || fail "native control tree target must be a directory: ${target}"
    assert_control_tree_safe "${target}"
  fi
  target_parent="$(dirname "${target}")"
  ensure_control_directory "${target_parent}"
  staging_parent="$(mktemp -d "${target}.installing.XXXXXX")" \
    || fail "cannot create native package tree staging directory"
  chmod 700 "${staging_parent}"
  staging="${staging_parent}/tree"
  source_before="$(upgrade_entry_fingerprint "${source}")" \
    || fail "cannot fingerprint native package tree source"
  if ! (
    umask 022
    cp -a --no-dereference -- "${source}" "${staging}"
    assert_control_tree_safe "${staging}" structural
    source_after="$(upgrade_entry_fingerprint "${source}")"
    staging_fingerprint="$(upgrade_entry_fingerprint "${staging}")"
    [ "${source_before}" = "${source_after}" ]
    [ "${source_before}" = "${staging_fingerprint}" ]
    if [ "$(id -u)" -eq 0 ] \
      && [ "${EMULATED_SECURITY_METADATA:-0}" -ne 1 ]; then
      chown -R -h root:root "${staging}"
    fi
    chmod -R u+rwX,go+rX,go-w "${staging}"
    assert_control_tree_safe "${staging}"
  ); then
    rm -rf -- "${staging_parent}" >/dev/null 2>&1 || true
    fail "cannot stage a verified native package tree: ${source}"
  fi
  rm -rf "${target}"
  mv -- "${staging}" "${target}" \
    || fail "cannot publish verified native package tree: ${target}"
  rmdir -- "${staging_parent}" \
    || fail "cannot remove native package tree staging parent"
  assert_control_tree_safe "${target}"
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
  local temporary_file
  [ -x "${binary}" ] || fail "缺少 runtime 二进制: ${binary}"
  begin_atomic_target_write "${target}"
  temporary_file="${LAST_INSTALLER_TEMP_FILE}"
  cat >"${temporary_file}" <<EOF
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
  finish_atomic_target_write "${temporary_file}" "${target}" 755
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

upgrade_transaction_install_items() {
  printf '%s\n' \
    .env bin ui runtime zlm docs certs systemd uninstall.sh .installer-backups
}

upgrade_transaction_unit_names() {
  printf '%s\n' \
    "${UNIT_BASENAME}.target" \
    "${UNIT_BASENAME}-postgres.service" \
    "${UNIT_BASENAME}-core.service" \
    "${UNIT_BASENAME}-zlm.service" \
    "${UNIT_BASENAME}-agent.service"
}

upgrade_entry_fingerprint() {
  local root="$1"
  local fingerprint_mode="${2:-full}"
  local inventory
  local records
  local relative
  local path
  local entry_type
  local content_hash
  local tmp_root
  case "${fingerprint_mode}" in full|content) ;; *) return 1 ;; esac
  [ -e "${root}" ] || [ -L "${root}" ] || return 1
  tmp_root="$(secure_installer_tmp_root)" || return 1
  inventory="$(mktemp "${tmp_root%/}/streamserver-upgrade-paths.XXXXXX")" \
    || return 1
  records="$(mktemp "${tmp_root%/}/streamserver-upgrade-records.XXXXXX")" \
    || {
      rm -f -- "${inventory}" >/dev/null 2>&1 || true
      return 1
    }
  chmod 600 "${inventory}" "${records}" || {
    rm -f -- "${inventory}" "${records}" >/dev/null 2>&1 || true
    return 1
  }
  if [ -d "${root}" ] && [ ! -L "${root}" ]; then
    if ! find -P "${root}" -printf '%P\0' | LC_ALL=C sort -z >"${inventory}"; then
      rm -f -- "${inventory}" "${records}" >/dev/null 2>&1 || true
      return 1
    fi
  else
    printf '\0' >"${inventory}" || {
      rm -f -- "${inventory}" "${records}" >/dev/null 2>&1 || true
      return 1
    }
  fi
  while IFS= read -r -d '' relative; do
    if [ -n "${relative}" ]; then
      path="${root}/${relative}"
    else
      path="${root}"
    fi
    if [ -L "${path}" ]; then
      entry_type=l
    elif [ -f "${path}" ]; then
      entry_type=f
    elif [ -d "${path}" ]; then
      entry_type=d
    else
      rm -f -- "${inventory}" "${records}" >/dev/null 2>&1 || true
      return 1
    fi
    {
      printf '%s\0%s\0' "${relative}" "${entry_type}"
      if [ "${fingerprint_mode}" = full ]; then
        if [ "${entry_type}" = d ]; then
          # A directory's st_size is an allocation detail and is not preserved
          # by cp -a across filesystems (or even by a fresh copy on the same
          # filesystem). The sorted inventory below already commits to every
          # child, while mode/owner/link-count/mtime retain the meaningful
          # directory metadata required by the rollback guard.
          stat -c '%f\0%u\0%g\0%h\0-\0%y\0' -- "${path}" || exit 1
        else
          stat -c '%f\0%u\0%g\0%h\0%s\0%y\0' -- "${path}" || exit 1
        fi
      fi
      case "${entry_type}" in
        f)
          content_hash="$(sha256sum -- "${path}" | awk '{print $1}')" \
            || exit 1
          printf '%s\0' "${content_hash}"
          ;;
        l) readlink -z -- "${path}" || exit 1 ;;
        d) printf '\0' ;;
      esac
    } >>"${records}" || {
      rm -f -- "${inventory}" "${records}" >/dev/null 2>&1 || true
      return 1
    }
  done <"${inventory}"
  content_hash="$(sha256sum -- "${records}" | awk '{print $1}')" || {
    rm -f -- "${inventory}" "${records}" >/dev/null 2>&1 || true
    return 1
  }
  rm -f -- "${inventory}" "${records}" || return 1
  printf '%s' "${content_hash}"
}

copy_upgrade_transaction_entry() {
  local source="$1"
  local destination="$2"
  cp -a --no-dereference --reflink=auto -- "${source}" "${destination}"
}

snapshot_upgrade_transaction_entry() {
  local source="$1"
  local snapshot="$2"
  local state_file="$3"
  local source_before
  local source_after
  local snapshot_fingerprint
  if [ ! -e "${source}" ] && [ ! -L "${source}" ]; then
    printf '%s\n' absent >"${state_file}"
    return 0
  fi
  [ ! -L "${source}" ] \
    || fail "upgrade transaction refuses a symbolic-link source"
  if [ -f "${source}" ]; then
    printf '%s\n' file >"${state_file}"
  elif [ -d "${source}" ]; then
    assert_control_tree_safe "${source}" structural
    printf '%s\n' directory >"${state_file}"
  else
    fail "upgrade transaction source must be a regular file or directory"
  fi
  source_before="$(upgrade_entry_fingerprint "${source}")" \
    || fail "cannot fingerprint native upgrade transaction source"
  copy_upgrade_transaction_entry "${source}" "${snapshot}" \
    || fail "cannot create native upgrade transaction snapshot"
  if [ -d "${snapshot}" ] && [ ! -L "${snapshot}" ]; then
    assert_control_tree_safe "${snapshot}" structural
  elif [ ! -L "${snapshot}" ] && [ -f "${snapshot}" ]; then
    :
  else
    fail "native upgrade transaction copy has an unsafe type"
  fi
  source_after="$(upgrade_entry_fingerprint "${source}")" \
    && snapshot_fingerprint="$(upgrade_entry_fingerprint "${snapshot}")" \
    || fail "cannot verify native upgrade transaction snapshot"
  [ "${source_before}" = "${source_after}" ] \
    && [ "${source_before}" = "${snapshot_fingerprint}" ] \
    || fail "native upgrade transaction source changed while it was copied"
}

verify_upgrade_transaction_entry_matches_source() {
  local source="$1"
  local snapshot="$2"
  local state_file="$3"
  local state
  local fingerprint_mode="${4:-full}"
  local source_fingerprint
  local snapshot_fingerprint
  [ ! -L "${state_file}" ] && [ -f "${state_file}" ] \
    && [ "$(wc -l <"${state_file}" | tr -d '[:space:]')" = 1 ] || return 1
  state="$(<"${state_file}")"
  case "${state}" in
    absent)
      [ ! -e "${source}" ] && [ ! -L "${source}" ] \
        && [ ! -e "${snapshot}" ] && [ ! -L "${snapshot}" ]
      ;;
    file)
      [ ! -L "${source}" ] && [ -f "${source}" ] \
        && [ ! -L "${snapshot}" ] && [ -f "${snapshot}" ] || return 1
      source_fingerprint="$(upgrade_entry_fingerprint "${source}" "${fingerprint_mode}")" \
        && snapshot_fingerprint="$(upgrade_entry_fingerprint "${snapshot}" "${fingerprint_mode}")" \
        && [ "${source_fingerprint}" = "${snapshot_fingerprint}" ]
      ;;
    directory)
      [ ! -L "${source}" ] && [ -d "${source}" ] \
        && [ ! -L "${snapshot}" ] && [ -d "${snapshot}" ] || return 1
      (assert_control_tree_safe "${source}" structural) >/dev/null 2>&1 \
        && (assert_control_tree_safe "${snapshot}" structural) >/dev/null 2>&1 \
        || return 1
      source_fingerprint="$(upgrade_entry_fingerprint "${source}" "${fingerprint_mode}")" \
        && snapshot_fingerprint="$(upgrade_entry_fingerprint "${snapshot}" "${fingerprint_mode}")" \
        && [ "${source_fingerprint}" = "${snapshot_fingerprint}" ]
      ;;
    *) return 1 ;;
  esac
}

bounded_upgrade_command() {
  local deadline="$1"
  local remaining
  local timeout_sec
  shift
  [[ "${deadline}" =~ ^[0-9]+$ ]] || return 1
  remaining=$((deadline - SECONDS))
  [ "${remaining}" -gt 0 ] || return 124
  timeout_sec="${remaining}"
  [ "${timeout_sec}" -le 10 ] || timeout_sec=10
  timeout --signal=TERM --kill-after=1s "${timeout_sec}s" "$@"
}

bounded_upgrade_systemctl() {
  local deadline="$1"
  shift
  bounded_upgrade_command "${deadline}" systemctl "$@"
}

upgrade_boot_fence_marker_path() {
  printf '%s/upgrade-boot-fence' "$(admin_handoff_state_dir)"
}

upgrade_boot_fence_lease_path() {
  printf '/run/streamserver-native-installer/upgrade-%s.lease' \
    "$(admin_handoff_install_dir_fingerprint)"
}

upgrade_boot_fence_dropin_path() {
  local unit="$1"
  printf '%s/%s.d/90-streamserver-upgrade-fence.conf' \
    "${SYSTEMD_UNIT_ROOT}" "${unit}"
}

upgrade_boot_fence_guard_path() {
  local content_hash
  content_hash="$(upgrade_boot_fence_guard_content | sha256sum | awk '{print $1}')" \
    || fail "cannot fingerprint native upgrade boot fence guard"
  [[ "${content_hash}" =~ ^[0-9a-f]{64}$ ]] \
    || fail "native upgrade boot fence guard fingerprint is invalid"
  printf '/usr/local/libexec/streamserver-native-installer/upgrade-fence-guard-%s' \
    "${content_hash}"
}

upgrade_boot_fence_watchdog_unit_name() {
  local guard_hash
  guard_hash="$(upgrade_boot_fence_guard_path)"
  guard_hash="${guard_hash##*-}"
  printf 'streamserver-native-upgrade-watchdog-%s-%s.service' \
    "$(admin_handoff_install_dir_fingerprint)" "${guard_hash:0:16}"
}

upgrade_boot_fence_watchdog_unit_path() {
  printf '%s/%s' "${SYSTEMD_UNIT_ROOT}" \
    "$(upgrade_boot_fence_watchdog_unit_name)"
}

upgrade_boot_fence_guard_content() {
  cat <<'STREAMSERVER_UPGRADE_FENCE_GUARD'
#!/bin/bash
set -u
PATH=/usr/sbin:/usr/bin:/sbin:/bin
export PATH

mode="${1:-}"
marker="${2:-}"
lease="${3:-}"
shift 3 || exit 64

[[ "${marker}" =~ ^/[A-Za-z0-9_./-]+$ ]] || exit 64
[[ "${lease}" =~ ^/[A-Za-z0-9_./-]+$ ]] || exit 64

lease_owner_alive() {
  local owner_pid
  local owner_start
  local extra
  local actual_start
  [ ! -L "${lease}" ] && [ -f "${lease}" ] || return 1
  [ "$(/usr/bin/stat -c '%u:%a:%h' -- "${lease}" 2>/dev/null)" = \
    "${EUID}:600:1" ] || return 1
  IFS=' ' read -r owner_pid owner_start extra <"${lease}" || return 1
  [ -z "${extra:-}" ] \
    && [[ "${owner_pid}" =~ ^[1-9][0-9]*$ ]] \
    && [[ "${owner_start}" =~ ^[1-9][0-9]*$ ]] || return 1
  actual_start="$(/usr/bin/awk '{print $22}' \
    "/proc/${owner_pid}/stat" 2>/dev/null)" || return 1
  [ "${actual_start}" = "${owner_start}" ]
}

case "${mode}" in
  check)
    if [ ! -e "${marker}" ] && [ ! -L "${marker}" ]; then
      exit 0
    fi
    [ ! -L "${marker}" ] && [ -f "${marker}" ] || exit 1
    lease_owner_alive || exit 1
    /usr/bin/flock --nonblock --conflict-exit-code 75 \
      "${lease}" /bin/true
    status=$?
    [ "${status}" -eq 75 ]
    ;;
  watch)
    [ "$#" -gt 0 ] || exit 64
    while :; do
      if [ -e "${marker}" ] || [ -L "${marker}" ]; then
        if lease_owner_alive; then
          /bin/sleep 1
          continue
        fi
        /usr/bin/systemctl stop -- "$@" || exit 1
        for unit in "$@"; do
          state="$(/usr/bin/systemctl show --property ActiveState --value \
            "${unit}" 2>/dev/null)" || exit 1
          [ "${state}" = inactive ] || [ "${state}" = failed ] || exit 1
        done
      fi
      /bin/sleep 1
    done
    ;;
  *) exit 64 ;;
esac
STREAMSERVER_UPGRADE_FENCE_GUARD
}

validate_upgrade_boot_fence_guard() {
  local guard
  local expected_hash
  local actual_hash
  guard="$(upgrade_boot_fence_guard_path)"
  [ ! -L "${guard}" ] && [ -f "${guard}" ] \
    && [ "$(stat -c '%u:%g:%a:%h' -- "${guard}" 2>/dev/null || true)" = 0:0:700:1 ] \
    || fail "native upgrade boot fence guard is unsafe"
  expected_hash="$(upgrade_boot_fence_guard_content | sha256sum | awk '{print $1}')"
  actual_hash="$(sha256sum "${guard}" | awk '{print $1}')" \
    || fail "cannot hash native upgrade boot fence guard"
  [ "${actual_hash}" = "${expected_hash}" ] \
    || fail "native upgrade boot fence guard content is invalid"
}

ensure_upgrade_boot_fence_guard() {
  local guard
  local guard_root
  local temporary_guard
  guard="$(upgrade_boot_fence_guard_path)"
  guard_root="${guard%/*}"
  if [ ! -e "${guard_root}" ] && [ ! -L "${guard_root}" ]; then
    install -d -o root -g root -m 0755 -- "${guard_root}"
    sync -f "$(dirname "${guard_root}")"
  fi
  admin_handoff_assert_no_symlink_boundary "${guard_root}"
  admin_handoff_assert_secure_root_ancestors "$(dirname "${guard_root}")"
  [ ! -L "${guard_root}" ] && [ -d "${guard_root}" ] \
    && [ "$(stat -c '%u:%g:%a' -- "${guard_root}" 2>/dev/null || true)" = 0:0:755 ] \
    || fail "native upgrade boot fence guard directory is unsafe"
  if [ -e "${guard}" ] || [ -L "${guard}" ]; then
    validate_upgrade_boot_fence_guard
    return 0
  fi
  temporary_guard="${guard}.$$.$RANDOM"
  (umask 077; upgrade_boot_fence_guard_content >"${temporary_guard}") \
    || fail "cannot create native upgrade boot fence guard"
  chown root:root "${temporary_guard}"
  chmod 700 "${temporary_guard}"
  sync -f "${temporary_guard}"
  mv -f -- "${temporary_guard}" "${guard}"
  sync -f "${guard_root}"
  validate_upgrade_boot_fence_guard
}

render_upgrade_boot_fence_watchdog_unit() {
  local marker="$1"
  local lease="$2"
  local guard
  local unit
  guard="$(upgrade_boot_fence_guard_path)"
  printf '%s\n' \
    '[Unit]' \
    'Description=StreamServer native upgrade transaction watchdog' \
    '[Service]' \
    'Type=exec' \
    'Restart=on-failure' \
    'RestartSec=1s'
  printf 'ExecStart=+%s watch %s %s' "${guard}" "${marker}" "${lease}"
  while IFS= read -r unit; do
    printf ' %s' "${unit}"
  done < <(upgrade_rollback_units)
  printf '\n'
}

install_upgrade_boot_fence_watchdog_unit() {
  local marker="$1"
  local lease="$2"
  local unit_path
  local temporary_unit
  local expected
  unit_path="$(upgrade_boot_fence_watchdog_unit_path)"
  trusted_systemd_path_status "${SYSTEMD_UNIT_ROOT}" directory \
    || fail "systemd unit directory is unsafe for the native upgrade watchdog"
  expected="$(render_upgrade_boot_fence_watchdog_unit "${marker}" "${lease}")"
  if [ -e "${unit_path}" ] || [ -L "${unit_path}" ]; then
    [ ! -L "${unit_path}" ] && [ -f "${unit_path}" ] \
      && [ "$(stat -c '%u:%g:%a:%h' -- "${unit_path}")" = 0:0:644:1 ] \
      && [ "$(<"${unit_path}")" = "${expected}" ] \
      || fail "native upgrade watchdog runtime unit is unsafe"
    return 0
  fi
  temporary_unit="${unit_path}.$$.$RANDOM"
  (umask 077; printf '%s\n' "${expected}" >"${temporary_unit}") \
    || fail "cannot create native upgrade watchdog runtime unit"
  chown root:root "${temporary_unit}"
  chmod 644 "${temporary_unit}"
  sync -f "${temporary_unit}"
  mv -f -- "${temporary_unit}" "${unit_path}"
  sync -f "${SYSTEMD_UNIT_ROOT}"
}

start_upgrade_boot_fence_watchdog() {
  local deadline=$((SECONDS + 30))
  local watchdog
  local state
  watchdog="$(upgrade_boot_fence_watchdog_unit_name)"
  bounded_upgrade_systemctl "${deadline}" restart "${watchdog}" \
    || fail "cannot start native upgrade transaction watchdog"
  state="$(bounded_upgrade_systemctl "${deadline}" \
    show --property ActiveState --value "${watchdog}")" \
    || fail "cannot inspect native upgrade transaction watchdog"
  [ "${state}" = active ] \
    || fail "native upgrade transaction watchdog is not active"
}

remove_upgrade_boot_fence_watchdog_unit() {
  local deadline=$((SECONDS + 30))
  local watchdog
  local unit_path
  watchdog="$(upgrade_boot_fence_watchdog_unit_name)"
  unit_path="$(upgrade_boot_fence_watchdog_unit_path)"
  bounded_upgrade_systemctl "${deadline}" stop "${watchdog}" \
    >/dev/null 2>&1 || true
  if [ -e "${unit_path}" ] || [ -L "${unit_path}" ]; then
    [ ! -L "${unit_path}" ] && [ -f "${unit_path}" ] \
      && [ "$(stat -c '%u:%g:%a:%h' -- "${unit_path}")" = 0:0:644:1 ] \
      || return 1
    rm -f -- "${unit_path}" || return 1
    sync -f "${SYSTEMD_UNIT_ROOT}" || return 1
  fi
}

render_upgrade_boot_fence_dropin() {
  local unit="$1"
  local marker="$2"
  local lease="$3"
  local guard
  local watchdog
  [[ "${unit}" =~ ^[A-Za-z0-9_.@-]+$ ]] \
    || fail "native upgrade boot fence unit name is invalid"
  [[ "${marker}" =~ ^/[A-Za-z0-9_./-]+$ ]] \
    && [[ "${lease}" =~ ^/[A-Za-z0-9_./-]+$ ]] \
    || fail "native upgrade boot fence path cannot be represented safely"
  guard="$(upgrade_boot_fence_guard_path)"
  watchdog="$(upgrade_boot_fence_watchdog_unit_name)"
  printf \
    '[Unit]\nConditionPathExists=|!%s\nConditionPathExists=|%s\n' \
    "${marker}" "${lease}"
  case "${unit}" in
    *.service)
      printf \
        '[Service]\nExecCondition=+%s check %s %s\n' \
        "${guard}" "${marker}" "${lease}"
      ;;
    *.target)
      printf 'Requires=%s\nAfter=%s\n' "${watchdog}" "${watchdog}"
      ;;
    *) fail "native upgrade boot fence only supports service and target units" ;;
  esac
}

validate_upgrade_boot_fence_files() {
  local marker="$1"
  local lease="$2"
  local unit
  local dropin
  local expected
  validate_upgrade_boot_fence_guard
  admin_handoff_assert_secure_file "${marker}" 600
  [[ "$(<"${marker}")" =~ ^[0-9]+-[0-9]+-[0-9]+$ ]] \
    || fail "native upgrade boot fence marker is malformed"
  while IFS= read -r unit; do
    dropin="$(upgrade_boot_fence_dropin_path "${unit}")"
    expected="$(render_upgrade_boot_fence_dropin "${unit}" "${marker}" "${lease}")"
    trusted_systemd_path_status "$(dirname "${dropin}")" directory \
      || fail "native upgrade boot fence drop-in directory is unsafe: ${unit}"
    admin_handoff_assert_secure_file "${dropin}" 644
    [ "$(<"${dropin}")" = "${expected}" ] \
      || fail "native upgrade boot fence drop-in is malformed: ${unit}"
    [ "$(find "$(dirname "${dropin}")" -mindepth 1 -maxdepth 1 -printf '%f\n' | wc -l | tr -d '[:space:]')" = 1 ] \
      || fail "native upgrade boot fence directory contains an unexpected drop-in: ${unit}"
  done < <(upgrade_transaction_unit_names)
}

cleanup_orphan_upgrade_boot_fence_dropins() {
  local marker="$1"
  local lease="$2"
  local unit
  local dropin
  local dropin_dir
  local expected
  local changed=0
  local entry_count
  local deadline=$((SECONDS + 30))
  while IFS= read -r unit; do
    dropin="$(upgrade_boot_fence_dropin_path "${unit}")"
    dropin_dir="$(dirname "${dropin}")"
    expected="$(render_upgrade_boot_fence_dropin "${unit}" "${marker}" "${lease}")"
    if [ ! -e "${dropin_dir}" ] && [ ! -L "${dropin_dir}" ]; then
      continue
    fi
    trusted_systemd_path_status "${dropin_dir}" directory \
      || fail "orphaned native upgrade boot fence directory is unsafe: ${unit}"
    entry_count="$(find "${dropin_dir}" -mindepth 1 -maxdepth 1 -printf '%f\n' \
      | wc -l | tr -d '[:space:]')"
    if [ "${entry_count}" = 0 ]; then
      rmdir -- "${dropin_dir}" \
        || fail "cannot remove empty orphaned native upgrade boot fence directory"
      changed=1
      continue
    fi
    [ "${entry_count}" = 1 ] \
      || fail "orphaned native upgrade boot fence directory contains unexpected entries: ${unit}"
    admin_handoff_assert_secure_file "${dropin}" 644
    [ "$(<"${dropin}")" = "${expected}" ] \
      || fail "orphaned native upgrade boot fence drop-in is malformed: ${unit}"
    rm -f -- "${dropin}" || fail "cannot remove orphaned native upgrade boot fence drop-in"
    rmdir -- "${dropin_dir}" || fail "cannot remove orphaned native upgrade boot fence directory"
    changed=1
  done < <(upgrade_transaction_unit_names)
  sync -f "${SYSTEMD_UNIT_ROOT}" \
    || fail "cannot durably remove orphaned boot fence directories"
  if [ -e "${lease}" ] || [ -L "${lease}" ]; then
    admin_handoff_assert_secure_file "${lease}" 600
    [ "$(stat -c '%h' -- "${lease}")" = 1 ] \
      || fail "orphaned native upgrade boot fence lease has multiple hard links"
    rm -f -- "${lease}" \
      || fail "cannot remove orphaned native upgrade boot fence lease"
  fi
  if [ "${changed}" -eq 1 ]; then
    bounded_upgrade_systemctl "${deadline}" daemon-reload \
      || fail "cannot reload systemd after orphaned boot fence cleanup"
  fi
  if [ -e "$(upgrade_boot_fence_watchdog_unit_path)" ] \
    || [ -L "$(upgrade_boot_fence_watchdog_unit_path)" ]; then
    remove_upgrade_boot_fence_watchdog_unit \
      || fail "cannot remove orphaned native upgrade watchdog unit"
    bounded_upgrade_systemctl "${deadline}" daemon-reload \
      || fail "cannot reload systemd after orphaned watchdog cleanup"
  fi
}

create_upgrade_boot_fence_lease() {
  local lease="$1"
  local lease_root="${lease%/*}"
  local owner_pid="${BASHPID}"
  local owner_start
  [ -z "${UPGRADE_BOOT_FENCE_LEASE_FD:-}" ] \
    || fail "native upgrade boot fence lease descriptor is already active"
  UPGRADE_BOOT_FENCE_LEASE="${lease}"
  if [ ! -e "${lease_root}" ] && [ ! -L "${lease_root}" ]; then
    install -d -o root -g root -m 0700 -- "${lease_root}"
  fi
  admin_handoff_assert_secure_directory "${lease_root}"
  [ ! -e "${lease}" ] && [ ! -L "${lease}" ] \
    || fail "native upgrade boot fence lease already exists"
  owner_start="$(awk '{print $22}' "/proc/${owner_pid}/stat" 2>/dev/null)" \
    || fail "cannot capture native upgrade installer process identity"
  [[ "${owner_start}" =~ ^[1-9][0-9]*$ ]] \
    || fail "native upgrade installer process identity is invalid"
  (umask 077; set -C; printf '%s %s\n' "${owner_pid}" "${owner_start}" >"${lease}") \
    || fail "cannot create native upgrade boot fence lease"
  chown root:root "${lease}"
  chmod 600 "${lease}"
  exec {UPGRADE_BOOT_FENCE_LEASE_FD}<>"${lease}" \
    || fail "cannot open native upgrade boot fence lease"
  flock -n "${UPGRADE_BOOT_FENCE_LEASE_FD}" \
    || fail "another process owns the native upgrade boot fence lease"
}

arm_upgrade_boot_fence() {
  local marker
  local lease
  local marker_tmp
  local unit
  local dropin
  local dropin_dir
  local dropin_tmp
  local deadline=$((SECONDS + 60))
  [ "${UPGRADE_TRANSACTION_STATE}" = presealed ] \
    || fail "native upgrade boot fence must be published before the armed decision"
  marker="$(upgrade_boot_fence_marker_path)"
  lease="$(upgrade_boot_fence_lease_path)"
  [ ! -e "${marker}" ] && [ ! -L "${marker}" ] \
    || fail "native upgrade boot fence already exists"
  create_upgrade_boot_fence_lease "${lease}"
  ensure_upgrade_boot_fence_guard
  install_upgrade_boot_fence_watchdog_unit "${marker}" "${lease}"
  while IFS= read -r unit; do
    dropin="$(upgrade_boot_fence_dropin_path "${unit}")"
    dropin_dir="$(dirname "${dropin}")"
    [ ! -e "${dropin_dir}" ] && [ ! -L "${dropin_dir}" ] \
      || fail "native upgrade refuses a pre-existing systemd drop-in directory: ${unit}"
    install -d -o root -g root -m 0755 -- "${dropin_dir}"
    dropin_tmp="${dropin_dir}/.90-streamserver-upgrade-fence.$$.$RANDOM"
    render_upgrade_boot_fence_dropin "${unit}" "${marker}" "${lease}" >"${dropin_tmp}"
    chown root:root "${dropin_tmp}"
    chmod 644 "${dropin_tmp}"
    sync -f "${dropin_tmp}"
    mv -f -- "${dropin_tmp}" "${dropin}"
    sync -f "${dropin_dir}"
  done < <(upgrade_transaction_unit_names)
  sync -f "${SYSTEMD_UNIT_ROOT}" \
    || fail "cannot durably publish native upgrade boot fence directories"
  bounded_upgrade_systemctl "${deadline}" daemon-reload \
    || fail "cannot load the native upgrade boot fence"
  start_upgrade_boot_fence_watchdog
  marker_tmp="${marker}.$$.$RANDOM"
  (umask 077; printf '%s\n' "${UPGRADE_TRANSACTION_ID}" >"${marker_tmp}") \
    || fail "cannot create native upgrade boot fence marker"
  chown root:root "${marker_tmp}"
  chmod 600 "${marker_tmp}"
  sync -f "${marker_tmp}"
  mv -f -- "${marker_tmp}" "${marker}"
  sync -f "${marker}"
  sync -f "$(dirname "${marker}")"
  validate_upgrade_boot_fence_files "${marker}" "${lease}"
  UPGRADE_BOOT_FENCE_ACTIVE=1
  UPGRADE_BOOT_FENCE_MARKER="${marker}"
  UPGRADE_BOOT_FENCE_LEASE="${lease}"
}

resume_upgrade_boot_fence_for_recovery() {
  local marker
  local lease
  local lease_root
  assert_install_transaction_lock_held
  marker="$(upgrade_boot_fence_marker_path)"
  lease="$(upgrade_boot_fence_lease_path)"
  lease_root="${lease%/*}"
  # The lease is only a same-process boot bypass.  If a prior installer was
  # killed, both installer flocks becoming available proves that process no
  # longer owns the transaction.  Remove its /run residue before consulting
  # any durable recovery data so every subsequent rejection remains fenced.
  if [ -e "${lease}" ] || [ -L "${lease}" ]; then
    local owner_pid
    local owner_start
    local owner_extra
    local actual_start=""
    admin_handoff_assert_secure_file "${lease}" 600
    [ "$(stat -c '%h' -- "${lease}")" = 1 ] \
      || fail "stale native upgrade boot fence lease has multiple hard links"
    IFS=' ' read -r owner_pid owner_start owner_extra <"${lease}" \
      || fail "stale native upgrade boot fence lease is malformed"
    [ -z "${owner_extra:-}" ] \
      && [[ "${owner_pid}" =~ ^[1-9][0-9]*$ ]] \
      && [[ "${owner_start}" =~ ^[1-9][0-9]*$ ]] \
      || fail "stale native upgrade boot fence owner identity is invalid"
    actual_start="$(awk '{print $22}' "/proc/${owner_pid}/stat" 2>/dev/null || true)"
    [ "${actual_start}" != "${owner_start}" ] \
      || fail "another process still owns the native upgrade boot fence lease"
    rm -f -- "${lease}" \
      || fail "cannot remove stale native upgrade boot fence lease"
    if [ -d "${lease_root}" ] && [ ! -L "${lease_root}" ]; then
      sync -f "${lease_root}" \
        || fail "cannot synchronize stale native upgrade boot fence lease removal"
    fi
  fi
  UPGRADE_BOOT_FENCE_LEASE=""
  UPGRADE_BOOT_FENCE_LEASE_FD=""
  if [ ! -e "${marker}" ] && [ ! -L "${marker}" ]; then
    cleanup_orphan_upgrade_boot_fence_dropins "${marker}" "${lease}"
    return 0
  fi
  ensure_upgrade_boot_fence_guard
  validate_upgrade_boot_fence_files "${marker}" "${lease}"
  UPGRADE_BOOT_FENCE_ACTIVE=1
  UPGRADE_BOOT_FENCE_MARKER="${marker}"
}

activate_upgrade_boot_fence_lease_for_recovery() {
  local marker="${UPGRADE_BOOT_FENCE_MARKER:-}"
  local lease
  local marker_id
  local deadline=$((SECONDS + 30))
  [ "${UPGRADE_BOOT_FENCE_ACTIVE:-0}" -eq 1 ] && [ -n "${marker}" ] \
    || return 0
  lease="$(upgrade_boot_fence_lease_path)"
  validate_upgrade_boot_fence_files "${marker}" "${lease}"
  marker_id="$(<"${marker}")"
  [ "${marker_id}" = "${UPGRADE_TRANSACTION_ID}" ] \
    || fail "native upgrade boot fence does not match the durable transaction"
  ensure_upgrade_boot_fence_guard
  install_upgrade_boot_fence_watchdog_unit "${marker}" "${lease}"
  bounded_upgrade_systemctl "${deadline}" daemon-reload \
    || fail "cannot activate the validated native upgrade recovery lease"
  start_upgrade_boot_fence_watchdog
  UPGRADE_BOOT_FENCE_LEASE="${lease}"
  create_upgrade_boot_fence_lease "${lease}"
}

clear_upgrade_boot_fence() {
  local marker
  local lease
  local unit
  local dropin
  local dropin_dir
  local deadline=$((SECONDS + 60))
  marker="$(upgrade_boot_fence_marker_path)"
  lease="$(upgrade_boot_fence_lease_path)"
  if [ -e "${marker}" ] || [ -L "${marker}" ]; then
    validate_upgrade_boot_fence_files "${marker}" "${lease}"
    rm -f -- "${marker}" || return 1
    sync -f "$(dirname "${marker}")" || return 1
  fi
  while IFS= read -r unit; do
    dropin="$(upgrade_boot_fence_dropin_path "${unit}")"
    dropin_dir="$(dirname "${dropin}")"
    if [ -e "${dropin}" ] || [ -L "${dropin}" ]; then
      [ ! -L "${dropin}" ] && [ -f "${dropin}" ] || return 1
      rm -f -- "${dropin}" || return 1
    fi
    if [ -d "${dropin_dir}" ] && [ ! -L "${dropin_dir}" ]; then
      rmdir -- "${dropin_dir}" || return 1
    elif [ -e "${dropin_dir}" ] || [ -L "${dropin_dir}" ]; then
      return 1
    fi
  done < <(upgrade_transaction_unit_names)
  sync -f "${SYSTEMD_UNIT_ROOT}" || return 1
  # First make systemd forget the target's Requires=watchdog relationship.
  # Stopping the watchdog while that cached edge exists would also deactivate
  # an otherwise healthy target after a successful commit.
  bounded_upgrade_systemctl "${deadline}" daemon-reload || return 1
  remove_upgrade_boot_fence_watchdog_unit || return 1
  bounded_upgrade_systemctl "${deadline}" daemon-reload || return 1
  rm -f -- "${lease}" || return 1
  if [ -n "${UPGRADE_BOOT_FENCE_LEASE_FD:-}" ]; then
    exec {UPGRADE_BOOT_FENCE_LEASE_FD}>&- || return 1
  fi
  UPGRADE_BOOT_FENCE_ACTIVE=0
  UPGRADE_BOOT_FENCE_MARKER=""
  UPGRADE_BOOT_FENCE_LEASE=""
  UPGRADE_BOOT_FENCE_LEASE_FD=""
}

capture_upgrade_unit_enablement() {
  local unit="$1"
  local deadline="${2:-$((SECONDS + 10))}"
  local state=""
  state="$(bounded_upgrade_systemctl \
    "${deadline}" is-enabled "${unit}" 2>/dev/null)" || true
  state="$(printf '%s\n' "${state}" | sed -n '1p')"
  [ -n "${state}" ] || return 1
  case "${state}" in
    enabled|enabled-runtime|disabled|masked|masked-runtime|static|indirect|generated|transient|linked|linked-runtime|alias|not-found) ;;
    *) return 1 ;;
  esac
  printf '%s' "${state}"
}

sync_upgrade_transaction_snapshot() {
  local path
  local state_dir
  local file_list
  local directory_list
  state_dir="$(admin_handoff_state_dir)" || return 1
  file_list="$(mktemp "${state_dir}/.upgrade-snapshot-files.XXXXXX")" \
    || return 1
  directory_list="$(mktemp "${state_dir}/.upgrade-snapshot-directories.XXXXXX")" \
    || {
      rm -f -- "${file_list}" >/dev/null 2>&1 || true
      return 1
    }
  chmod 600 "${file_list}" "${directory_list}" || {
    rm -f -- "${file_list}" "${directory_list}" >/dev/null 2>&1 || true
    return 1
  }
  if ! find "${UPGRADE_TRANSACTION_DIR}" -type f -print0 >"${file_list}"; then
    rm -f -- "${file_list}" "${directory_list}" >/dev/null 2>&1 || true
    return 1
  fi
  while IFS= read -r -d '' path; do
    sync -f "${path}" || {
      rm -f -- "${file_list}" "${directory_list}" >/dev/null 2>&1 || true
      return 1
    }
  done <"${file_list}"
  if ! find "${UPGRADE_TRANSACTION_DIR}" -depth -type d -print0 \
    >"${directory_list}"; then
    rm -f -- "${file_list}" "${directory_list}" >/dev/null 2>&1 || true
    return 1
  fi
  while IFS= read -r -d '' path; do
    sync -f "${path}" || {
      rm -f -- "${file_list}" "${directory_list}" >/dev/null 2>&1 || true
      return 1
    }
  done <"${directory_list}"
  rm -f -- "${file_list}" "${directory_list}" || return 1
}

write_upgrade_transaction_phase() {
  local phase="$1"
  local temporary_phase
  case "${phase}" in
    building|presealed|armed|committed|restored) ;;
    *) return 1 ;;
  esac
  [ -n "${UPGRADE_TRANSACTION_DIR}" ] \
    && [ ! -L "${UPGRADE_TRANSACTION_DIR}" ] \
    && [ -d "${UPGRADE_TRANSACTION_DIR}" ] || return 1
  temporary_phase="${UPGRADE_TRANSACTION_DIR}/.phase.$$.$RANDOM"
  (umask 077 && printf '%s\n' "${phase}" >"${temporary_phase}") \
    || return 1
  chmod 600 "${temporary_phase}" || {
    rm -f -- "${temporary_phase}" >/dev/null 2>&1 || true
    return 1
  }
  sync -f "${temporary_phase}" || {
    rm -f -- "${temporary_phase}" >/dev/null 2>&1 || true
    return 1
  }
  mv -f -- "${temporary_phase}" "${UPGRADE_TRANSACTION_DIR}/phase" \
    || return 1
  UPGRADE_TRANSACTION_PHASE_FILE="${UPGRADE_TRANSACTION_DIR}/phase"
  sync -f "${UPGRADE_TRANSACTION_PHASE_FILE}" || return 1
  sync -f "${UPGRADE_TRANSACTION_DIR}"
}

current_system_boot_id() {
  local boot_id
  [ ! -L /proc/sys/kernel/random/boot_id ] \
    && [ -f /proc/sys/kernel/random/boot_id ] || return 1
  boot_id="$(</proc/sys/kernel/random/boot_id)"
  [[ "${boot_id}" =~ ^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$ ]] \
    || return 1
  printf '%s' "${boot_id}"
}

persist_upgrade_transaction_boot_id() {
  local boot_id
  boot_id="$(current_system_boot_id)" \
    || fail "cannot capture the system boot identity for native upgrade fencing"
  (umask 077; printf '%s\n' "${boot_id}" \
    >"${UPGRADE_TRANSACTION_DIR}/boot-id") \
    || fail "cannot persist the native upgrade boot identity"
  chmod 600 "${UPGRADE_TRANSACTION_DIR}/boot-id"
}

write_upgrade_transaction_snapshot_kind() {
  local kind="$1"
  local temporary_kind
  case "${kind}" in minimal|full) ;; *) return 1 ;; esac
  temporary_kind="${UPGRADE_TRANSACTION_DIR}/.snapshot-kind.$$.$RANDOM"
  (umask 077; printf '%s\n' "${kind}" >"${temporary_kind}") \
    && chmod 600 "${temporary_kind}" \
    && sync -f "${temporary_kind}" \
    && mv -f -- "${temporary_kind}" "${UPGRADE_TRANSACTION_DIR}/snapshot-kind" \
    && sync -f "${UPGRADE_TRANSACTION_DIR}/snapshot-kind" \
    && sync -f "${UPGRADE_TRANSACTION_DIR}" \
    || {
      rm -f -- "${temporary_kind}" >/dev/null 2>&1 || true
      return 1
    }
}

read_upgrade_transaction_snapshot_kind() {
  local kind_file="${UPGRADE_TRANSACTION_DIR}/snapshot-kind"
  local kind
  [ ! -L "${kind_file}" ] && [ -f "${kind_file}" ] \
    && [ "$(stat -c '%a:%h' -- "${kind_file}")" = 600:1 ] \
    && [ "$(wc -l <"${kind_file}" | tr -d '[:space:]')" = 1 ] \
    || return 1
  kind="$(<"${kind_file}")"
  case "${kind}" in minimal|full) printf '%s' "${kind}" ;; *) return 1 ;; esac
}

upgrade_transaction_crossed_reboot() {
  local boot_file="${UPGRADE_TRANSACTION_DIR}/boot-id"
  local captured
  local current
  [ ! -L "${boot_file}" ] && [ -f "${boot_file}" ] \
    && [ "$(stat -c '%a:%h' -- "${boot_file}")" = 600:1 ] \
    || fail "native upgrade boot identity file is unsafe"
  if [ "$(id -u)" -eq 0 ] \
    && [ "${EMULATED_SECURITY_METADATA:-0}" -ne 1 ]; then
    [ "$(stat -c '%u' -- "${boot_file}")" = 0 ] \
      || fail "native upgrade boot identity must be root-owned"
  fi
  [ "$(wc -l <"${boot_file}" | tr -d '[:space:]')" = 1 ] \
    || fail "native upgrade boot identity is malformed"
  captured="$(<"${boot_file}")"
  current="$(current_system_boot_id)" \
    || fail "cannot read the current system boot identity"
  [[ "${captured}" =~ ^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$ ]] \
    || fail "captured native upgrade boot identity is invalid"
  [ "${captured}" != "${current}" ]
}

read_upgrade_transaction_phase() {
  local phase_file="${UPGRADE_TRANSACTION_DIR}/phase"
  local phase
  [ -n "${UPGRADE_TRANSACTION_DIR}" ] \
    && [ ! -L "${phase_file}" ] \
    && [ -f "${phase_file}" ] || return 1
  [ "$(wc -l <"${phase_file}" | tr -d '[:space:]')" = 1 ] \
    || return 1
  phase="$(<"${phase_file}")"
  case "${phase}" in
    building|presealed|armed|committed|restored) printf '%s' "${phase}" ;;
    *) return 1 ;;
  esac
}

read_upgrade_transaction_phase_from_dir() {
  local transaction_dir="$1"
  local phase_file="${transaction_dir}/phase"
  local phase
  admin_handoff_assert_secure_directory "${transaction_dir}"
  if ! (assert_control_tree_safe "${transaction_dir}" structural) >/dev/null 2>&1; then
    fail "unsafe native upgrade transaction tree cannot be recovered"
  fi
  admin_handoff_assert_secure_file "${phase_file}" 600
  [ "$(wc -l <"${phase_file}" | tr -d '[:space:]')" = 1 ] \
    || fail "native upgrade transaction phase is malformed"
  phase="$(<"${phase_file}")"
  case "${phase}" in
    building|presealed|armed|committed|restored) printf '%s' "${phase}" ;;
    *) fail "native upgrade transaction phase is unknown" ;;
  esac
}

read_upgrade_transaction_id_from_dir() {
  local transaction_dir="$1"
  local id_file="${transaction_dir}/transaction-id"
  local transaction_id
  admin_handoff_assert_secure_file "${id_file}" 600
  [ "$(wc -l <"${id_file}" | tr -d '[:space:]')" = 1 ] \
    || fail "native upgrade transaction ID is malformed"
  transaction_id="$(<"${id_file}")"
  [[ "${transaction_id}" =~ ^[0-9]+-[0-9]+-[0-9]+$ ]] \
    || fail "native upgrade transaction ID is invalid"
  printf '%s' "${transaction_id}"
}

garbage_collect_upgrade_transaction_tree() {
  local transaction_dir="$1"
  local expected_phase="$2"
  local expected_id="$3"
  local observed_phase
  local observed_id
  local state_dir
  local decision_file
  local temporary_decision
  local quarantine
  observed_phase="$(read_upgrade_transaction_phase_from_dir "${transaction_dir}")"
  observed_id="$(read_upgrade_transaction_id_from_dir "${transaction_dir}")"
  case "${expected_phase}" in committed|restored) ;; *)
    fail "native upgrade transaction garbage-collection phase is not terminal" ;;
  esac
  [ "${observed_phase}" = "${expected_phase}" ] \
    || fail "native upgrade transaction garbage-collection phase mismatch"
  [ "${observed_id}" = "${expected_id}" ] \
    || fail "native upgrade transaction garbage-collection identity mismatch"

  state_dir="$(admin_handoff_state_dir)"
  decision_file="${state_dir}/upgrade-transaction.terminal.${expected_id}"
  quarantine="${state_dir}/upgrade-transaction.gc.${expected_phase}.${expected_id}"
  if [ -e "${decision_file}" ] || [ -L "${decision_file}" ]; then
    admin_handoff_assert_secure_file "${decision_file}" 600
    [ "$(<"${decision_file}")" = "${expected_phase}" ] \
      || fail "native upgrade terminal decision conflicts with the resolved tree"
  else
    temporary_decision="${state_dir}/.upgrade-transaction-terminal.$$.$RANDOM"
    (umask 077 && printf '%s\n' "${expected_phase}" >"${temporary_decision}") \
      && chmod 600 "${temporary_decision}" \
      && sync -f "${temporary_decision}" \
      && mv -f -- "${temporary_decision}" "${decision_file}" \
      && sync -f "${decision_file}" \
      && sync -f "${state_dir}" \
      || {
        rm -f -- "${temporary_decision}" >/dev/null 2>&1 || true
        return 1
      }
  fi
  [ ! -e "${quarantine}" ] && [ ! -L "${quarantine}" ] \
    || return 1
  mv -- "${transaction_dir}" "${quarantine}" \
    || return 1
  sync -f "${state_dir}" \
    || return 1
  if rm -rf -- "${quarantine}" && sync -f "${state_dir}"; then
    rm -f -- "${decision_file}" \
      || printf '[streamserver-native-install] WARNING: terminal decision marker was retained\n' >&2
    sync -f "${state_dir}" >/dev/null 2>&1 || true
  else
    printf '[streamserver-native-install] WARNING: resolved upgrade transaction quarantine was retained\n' >&2
  fi
}

garbage_collect_quarantined_upgrade_transaction() {
  local quarantine="$1"
  local expected_phase="$2"
  local expected_id="$3"
  local state_dir
  local decision_file
  state_dir="$(admin_handoff_state_dir)"
  decision_file="${state_dir}/upgrade-transaction.terminal.${expected_id}"
  case "${quarantine}" in
    "${state_dir}/upgrade-transaction.gc.${expected_phase}.${expected_id}") ;;
    *) fail "native upgrade garbage quarantine path is invalid" ;;
  esac
  case "${expected_phase}" in committed|restored) ;; *)
    fail "native upgrade garbage quarantine phase is invalid" ;;
  esac
  [[ "${expected_id}" =~ ^[0-9]+-[0-9]+-[0-9]+$ ]] \
    || fail "native upgrade garbage quarantine ID is invalid"
  admin_handoff_assert_secure_directory "${quarantine}"
  admin_handoff_assert_secure_file "${decision_file}" 600
  [ "$(<"${decision_file}")" = "${expected_phase}" ] \
    || fail "native upgrade garbage quarantine has no matching terminal decision"
  if rm -rf -- "${quarantine}" && sync -f "${state_dir}"; then
    rm -f -- "${decision_file}" \
      || printf '[streamserver-native-install] WARNING: terminal decision marker was retained\n' >&2
    sync -f "${state_dir}" >/dev/null 2>&1 || true
  else
    printf '[streamserver-native-install] WARNING: native upgrade garbage quarantine was retained\n' >&2
    return 1
  fi
}

inspect_building_upgrade_transaction_phase() {
  local transaction_dir="$1"
  local phase_file="${transaction_dir}/phase"
  local phase
  admin_handoff_assert_secure_directory "${transaction_dir}"
  (assert_control_tree_safe "${transaction_dir}" structural) >/dev/null 2>&1 \
    || fail "unsafe native upgrade transaction build tree cannot be recovered"
  if [ ! -e "${phase_file}" ] && [ ! -L "${phase_file}" ]; then
    printf '%s' partial
    return 0
  fi
  admin_handoff_assert_secure_file "${phase_file}" 600
  [ "$(wc -l <"${phase_file}" | tr -d '[:space:]')" = 1 ] \
    || fail "native upgrade transaction build phase is malformed"
  phase="$(<"${phase_file}")"
  case "${phase}" in
    building|presealed|armed) printf '%s' "${phase}" ;;
    *) fail "native upgrade transaction build phase is invalid" ;;
  esac
}

discard_unpublished_building_upgrade_transaction() {
  local transaction_dir="$1"
  local state_dir="$2"
  local observed_phase
  observed_phase="$(inspect_building_upgrade_transaction_phase "${transaction_dir}")"
  case "${observed_phase}" in partial|building) ;; *) return 1 ;; esac
  rm -rf -- "${transaction_dir}" || return 1
  sync -f "${state_dir}"
}

garbage_collect_resolved_upgrade_transactions() {
  local state_dir
  local fixed_dir
  local entry
  local entry_name
  local phase
  local transaction_id
  local tombstone
  local decision_phase
  local changed=0
  state_dir="$(admin_handoff_state_dir)"
  admin_handoff_assert_secure_directory "${state_dir}"
  fixed_dir="${state_dir}/upgrade-transaction"

  if [ -e "${fixed_dir}" ] || [ -L "${fixed_dir}" ]; then
    [ ! -L "${fixed_dir}" ] && [ -d "${fixed_dir}" ] \
      || fail "unsafe fixed native upgrade transaction cannot be recovered"
    phase="$(read_upgrade_transaction_phase_from_dir "${fixed_dir}")"
    transaction_id="$(read_upgrade_transaction_id_from_dir "${fixed_dir}")"
    case "${phase}" in
      committed|restored)
        tombstone="${state_dir}/upgrade-transaction.${phase}.${transaction_id}"
        [ ! -e "${tombstone}" ] && [ ! -L "${tombstone}" ] \
          || fail "resolved native upgrade transaction tombstone already exists"
        mv -- "${fixed_dir}" "${tombstone}" \
          || fail "cannot publish resolved native upgrade transaction tombstone"
        sync -f "${state_dir}" \
          || printf '[streamserver-native-install] WARNING: resolved transaction rename could not be fsynced\n' >&2
        UPGRADE_TRANSACTION_DIR="${tombstone}"
        UPGRADE_TRANSACTION_ID="${transaction_id}"
        UPGRADE_TRANSACTION_STATE="${phase}"
        validate_upgrade_recovery_cli_identity
        complete_terminal_upgrade_transaction \
          || fail "cannot complete a terminal native upgrade transaction"
        UPGRADE_TRANSACTION_STATE=none
        UPGRADE_TRANSACTION_DIR=""
        UPGRADE_TRANSACTION_ID=""
        UPGRADE_TRANSACTION_PHASE_FILE=""
        changed=1
        ;;
      presealed)
        UPGRADE_TRANSACTION_DIR="${fixed_dir}"
        UPGRADE_TRANSACTION_ID="${transaction_id}"
        UPGRADE_TRANSACTION_STATE=presealed
        validate_upgrade_recovery_cli_identity
        validate_upgrade_transaction_entry_snapshot \
          "${UPGRADE_TRANSACTION_DIR}/install/.env" \
          "${UPGRADE_TRANSACTION_DIR}/install-state/.env.state"
        validate_upgrade_install_root_metadata_state
        trap cleanup_installer_state EXIT
        restore_upgrade_preseal_guard \
          || fail "cannot recover a presealed native upgrade transaction"
        changed=1
        ;;
      armed)
        UPGRADE_TRANSACTION_DIR="${fixed_dir}"
        UPGRADE_TRANSACTION_ID="${transaction_id}"
        UPGRADE_TRANSACTION_STATE=armed
        validate_upgrade_recovery_cli_identity
        validate_upgrade_transaction_snapshot_for_restore \
          || fail "cannot validate an armed native upgrade transaction before recovery lease activation"
        trap cleanup_installer_state EXIT
        activate_upgrade_boot_fence_lease_for_recovery
        recover_fixed_armed_upgrade_transaction \
          || fail "cannot recover an armed native upgrade transaction"
        changed=1
        ;;
      building)
        fail "a fixed-name building transaction violates the native transaction protocol"
        ;;
    esac
  fi

  for entry in \
    "${state_dir}"/upgrade-transaction.building.* \
    "${state_dir}"/upgrade-transaction.committed.* \
    "${state_dir}"/upgrade-transaction.restored.*; do
    [ -e "${entry}" ] || [ -L "${entry}" ] || continue
    entry_name="${entry##*/}"
    case "${entry_name}" in
      upgrade-transaction.building.*) phase=building ;;
      upgrade-transaction.committed.*) phase=committed ;;
      upgrade-transaction.restored.*) phase=restored ;;
      *) fail "unsafe native upgrade transaction tombstone name" ;;
    esac
    transaction_id="${entry_name#upgrade-transaction.${phase}.}"
    [[ "${transaction_id}" =~ ^[0-9]+-[0-9]+-[0-9]+$ ]] \
      || fail "native upgrade transaction tombstone ID is invalid"
    if [ "${phase}" = building ]; then
      decision_phase="$(inspect_building_upgrade_transaction_phase "${entry}")"
      case "${decision_phase}" in
        partial|building)
          discard_unpublished_building_upgrade_transaction \
            "${entry}" "${state_dir}" \
            || fail "cannot discard an unpublished native upgrade transaction"
          ;;
        presealed)
          [ "$(read_upgrade_transaction_id_from_dir "${entry}")" = "${transaction_id}" ] \
            || fail "native upgrade preseal transaction ID mismatch"
          UPGRADE_TRANSACTION_DIR="${entry}"
          UPGRADE_TRANSACTION_ID="${transaction_id}"
          UPGRADE_TRANSACTION_STATE=presealed
          validate_upgrade_recovery_cli_identity
          validate_upgrade_transaction_entry_snapshot \
            "${UPGRADE_TRANSACTION_DIR}/install/.env" \
            "${UPGRADE_TRANSACTION_DIR}/install-state/.env.state"
          validate_upgrade_install_root_metadata_state
          trap cleanup_installer_state EXIT
          restore_upgrade_preseal_guard \
            || fail "cannot recover a presealed native upgrade transaction"
          ;;
        armed)
          [ "$(read_upgrade_transaction_id_from_dir "${entry}")" = "${transaction_id}" ] \
            || fail "native unpublished armed transaction ID mismatch"
          UPGRADE_TRANSACTION_DIR="${entry}"
          UPGRADE_TRANSACTION_ID="${transaction_id}"
          UPGRADE_TRANSACTION_STATE=armed
          validate_upgrade_recovery_cli_identity
          validate_upgrade_transaction_entry_snapshot \
            "${UPGRADE_TRANSACTION_DIR}/install/.env" \
            "${UPGRADE_TRANSACTION_DIR}/install-state/.env.state"
          validate_upgrade_install_root_metadata_state
          trap cleanup_installer_state EXIT
          restore_upgrade_preseal_guard \
            || fail "cannot recover an unpublished armed native upgrade transaction"
          ;;
        *) fail "native upgrade transaction build phase is invalid" ;;
      esac
    else
      UPGRADE_TRANSACTION_DIR="${entry}"
      UPGRADE_TRANSACTION_ID="${transaction_id}"
      UPGRADE_TRANSACTION_STATE="${phase}"
      validate_upgrade_recovery_cli_identity
      complete_terminal_upgrade_transaction \
        || fail "cannot complete a terminal native upgrade tombstone"
      UPGRADE_TRANSACTION_STATE=none
      UPGRADE_TRANSACTION_DIR=""
      UPGRADE_TRANSACTION_ID=""
      UPGRADE_TRANSACTION_PHASE_FILE=""
    fi
    changed=1
  done

  for entry in "${state_dir}"/upgrade-transaction.gc.*; do
    [ -e "${entry}" ] || [ -L "${entry}" ] || continue
    entry_name="${entry##*/}"
    case "${entry_name}" in
      upgrade-transaction.gc.committed.*) phase=committed ;;
      upgrade-transaction.gc.restored.*) phase=restored ;;
      *) fail "unsafe native upgrade garbage quarantine name" ;;
    esac
    transaction_id="${entry_name#upgrade-transaction.gc.${phase}.}"
    garbage_collect_quarantined_upgrade_transaction \
      "${entry}" "${phase}" "${transaction_id}"
    changed=1
  done

  for entry in "${state_dir}"/upgrade-transaction.terminal.*; do
    [ -e "${entry}" ] || [ -L "${entry}" ] || continue
    entry_name="${entry##*/}"
    transaction_id="${entry_name#upgrade-transaction.terminal.}"
    [[ "${transaction_id}" =~ ^[0-9]+-[0-9]+-[0-9]+$ ]] \
      || fail "native upgrade terminal decision ID is invalid"
    admin_handoff_assert_secure_file "${entry}" 600
    [ "$(wc -l <"${entry}" | tr -d '[:space:]')" = 1 ] \
      || fail "native upgrade terminal decision is malformed"
    decision_phase="$(<"${entry}")"
    case "${decision_phase}" in committed|restored) ;; *)
      fail "native upgrade terminal decision phase is invalid" ;;
    esac
    if [ -e "${state_dir}/upgrade-transaction.gc.${decision_phase}.${transaction_id}" ] \
      || [ -L "${state_dir}/upgrade-transaction.gc.${decision_phase}.${transaction_id}" ] \
      || [ -e "${state_dir}/upgrade-transaction.${decision_phase}.${transaction_id}" ] \
      || [ -L "${state_dir}/upgrade-transaction.${decision_phase}.${transaction_id}" ]; then
      continue
    fi
    rm -f -- "${entry}" \
      || printf '[streamserver-native-install] WARNING: terminal decision marker was retained\n' >&2
    changed=1
  done
  if [ "${changed}" -eq 1 ]; then
    sync -f "${state_dir}" \
      || printf '[streamserver-native-install] WARNING: terminal transaction garbage collection could not be fsynced\n' >&2
  fi
}

read_upgrade_terminal_decision() {
  local state_dir
  local decision_file
  local phase
  [[ "${UPGRADE_TRANSACTION_ID}" =~ ^[0-9]+-[0-9]+-[0-9]+$ ]] \
    || return 1
  state_dir="$(admin_handoff_state_dir)" || return 1
  decision_file="${state_dir}/upgrade-transaction.terminal.${UPGRADE_TRANSACTION_ID}"
  [ ! -L "${decision_file}" ] && [ -f "${decision_file}" ] || return 1
  [ "$(wc -l <"${decision_file}" | tr -d '[:space:]')" = 1 ] \
    || return 1
  phase="$(<"${decision_file}")"
  case "${phase}" in
    committed|restored) printf '%s' "${phase}" ;;
    *) return 1 ;;
  esac
}

capture_upgrade_install_root_metadata() {
  local state_file="${UPGRADE_TRANSACTION_DIR}/install-root.state"
  local uid
  local gid
  local mode
  local mtime
  local device
  local inode
  [ ! -L "${INSTALL_DIR}" ] && [ -d "${INSTALL_DIR}" ] \
    || fail "cannot snapshot unsafe native installation root metadata"
  uid="$(stat -c '%u' -- "${INSTALL_DIR}")" \
    && gid="$(stat -c '%g' -- "${INSTALL_DIR}")" \
    && mode="$(stat -c '%a' -- "${INSTALL_DIR}")" \
    && mtime="$(stat -c '%y' -- "${INSTALL_DIR}")" \
    && device="$(stat -c '%d' -- "${INSTALL_DIR}")" \
    && inode="$(stat -c '%i' -- "${INSTALL_DIR}")" \
    || fail "cannot inspect native installation root metadata"
  [[ "${uid}" =~ ^[0-9]+$ ]] \
    && [[ "${gid}" =~ ^[0-9]+$ ]] \
    && [[ "${mode}" =~ ^[0-7]{3,4}$ ]] \
    && [ -n "${mtime}" ] \
    && [[ "${mtime}" != *$'\n'* ]] \
    && [[ "${mtime}" != *$'\r'* ]] \
    && [[ "${device}" =~ ^[0-9]+$ ]] \
    && [[ "${inode}" =~ ^[0-9]+$ ]] \
    || fail "native installation root metadata is invalid"
  (umask 077 && printf 'UID=%s\nGID=%s\nMODE=%s\nMTIME=%s\nDEVICE=%s\nINODE=%s\n' \
    "${uid}" "${gid}" "${mode}" "${mtime}" "${device}" "${inode}" \
    >"${state_file}") \
    || fail "cannot persist native installation root metadata"
  chmod 600 "${state_file}"
}

upgrade_install_root_metadata_value() {
  local state_file="$1"
  local key="$2"
  awk -v key="${key}" '
    index($0, key "=") == 1 {
      count += 1
      value = substr($0, length(key) + 2)
    }
    END {
      if (count != 1) exit 1
      print value
    }
  ' "${state_file}"
}

verify_upgrade_install_root_metadata_unchanged() {
  local state_file="${UPGRADE_TRANSACTION_DIR}/install-root.state"
  local expected
  local observed
  local key
  local format
  [ ! -L "${state_file}" ] && [ -f "${state_file}" ] \
    && [ "$(stat -c '%a' -- "${state_file}")" = 600 ] \
    && [ "$(wc -l <"${state_file}" | tr -d '[:space:]')" = 6 ] || return 1
  for key in UID GID MODE MTIME DEVICE INODE; do
    case "${key}" in
      UID) format=%u ;;
      GID) format=%g ;;
      MODE) format=%a ;;
      MTIME) format=%y ;;
      DEVICE) format=%d ;;
      INODE) format=%i ;;
    esac
    expected="$(upgrade_install_root_metadata_value "${state_file}" "${key}")" \
      && observed="$(stat -c "${format}" -- "${INSTALL_DIR}")" || return 1
    [ "${observed}" = "${expected}" ] || return 1
  done
}

restore_upgrade_install_root_metadata() {
  local state_file="${UPGRADE_TRANSACTION_DIR}/install-root.state"
  local uid
  local gid
  local mode
  local mtime
  local device
  local inode
  [ ! -L "${state_file}" ] && [ -f "${state_file}" ] \
    && [ "$(stat -c '%a' -- "${state_file}")" = 600 ] \
    && [ "$(wc -l <"${state_file}" | tr -d '[:space:]')" = 6 ] || return 1
  uid="$(upgrade_install_root_metadata_value "${state_file}" UID)" \
    && gid="$(upgrade_install_root_metadata_value "${state_file}" GID)" \
    && mode="$(upgrade_install_root_metadata_value "${state_file}" MODE)" \
    && mtime="$(upgrade_install_root_metadata_value "${state_file}" MTIME)" \
    && device="$(upgrade_install_root_metadata_value "${state_file}" DEVICE)" \
    && inode="$(upgrade_install_root_metadata_value "${state_file}" INODE)" \
    || return 1
  [[ "${uid}" =~ ^[0-9]+$ ]] \
    && [[ "${gid}" =~ ^[0-9]+$ ]] \
    && [[ "${mode}" =~ ^[0-7]{3,4}$ ]] \
    && [ -n "${mtime}" ] \
    && [[ "${mtime}" != *$'\n'* ]] \
    && [[ "${mtime}" != *$'\r'* ]] \
    && [[ "${device}" =~ ^[0-9]+$ ]] \
    && [[ "${inode}" =~ ^[0-9]+$ ]] || return 1
  [ ! -L "${INSTALL_DIR}" ] && [ -d "${INSTALL_DIR}" ] || return 1
  [ "$(stat -c '%d:%i' -- "${INSTALL_DIR}")" = "${device}:${inode}" ] || return 1
  if [ "$(id -u)" -eq 0 ] && [ "${EMULATED_SECURITY_METADATA:-0}" -ne 1 ]; then
    chown "${uid}:${gid}" "${INSTALL_DIR}" || return 1
  else
    [ "$(stat -c '%u:%g' -- "${INSTALL_DIR}")" = "${uid}:${gid}" ] || return 1
  fi
  chmod "${mode}" "${INSTALL_DIR}" || return 1
  touch -m -d "${mtime}" "${INSTALL_DIR}" || return 1
  sync -f "${INSTALL_DIR}"
}

restore_upgrade_preseal_guard() {
  local disk_phase
  disk_phase="$(read_upgrade_transaction_phase 2>/dev/null)" \
    || disk_phase="${UPGRADE_TRANSACTION_STATE}"
  case "${disk_phase}" in presealed|armed) ;; *) return 1 ;; esac
  assert_install_transaction_lock_held || return 1
  restore_upgrade_transaction_entry \
    "${UPGRADE_TRANSACTION_DIR}/install/.env" \
    "${UPGRADE_TRANSACTION_DIR}/install-state/.env.state" \
    "${INSTALL_DIR}/.env" || return 1
  restore_upgrade_install_root_metadata || return 1
  finalize_upgrade_transaction_terminal restored || return 1
  complete_terminal_upgrade_transaction || return 1
  UPGRADE_TRANSACTION_STATE=none
  UPGRADE_TRANSACTION_DIR=""
  UPGRADE_TRANSACTION_PHASE_FILE=""
  UPGRADE_TRANSACTION_ID=""
}

recover_fixed_armed_upgrade_transaction() {
  local disk_phase
  disk_phase="$(read_upgrade_transaction_phase 2>/dev/null)" || return 1
  [ "${disk_phase}" = armed ] || return 1
  assert_install_transaction_lock_held || return 1
  load_persisted_upgrade_service_state || return 1
  UPGRADE_RESTORE_ON_FAILURE=1
  restore_upgrade_transaction || return 1
  UPGRADE_TRANSACTION_STATE=none
  UPGRADE_TRANSACTION_DIR=""
  UPGRADE_TRANSACTION_PHASE_FILE=""
  UPGRADE_TRANSACTION_ID=""
  UPGRADE_RESTORE_ON_FAILURE=0
}

begin_upgrade_preseal_guard() {
  local state_dir
  local building_dir
  local fixed_dir
  local caller_umask
  [ "${UPGRADE}" -eq 1 ] || return 0
  assert_install_transaction_lock_held
  [ "${UPGRADE_TRANSACTION_STATE}" = none ] \
    || fail "native upgrade transaction is already active"
  state_dir="$(admin_handoff_state_dir)"
  fixed_dir="${state_dir}/upgrade-transaction"
  if [ -e "${fixed_dir}" ] || [ -L "${fixed_dir}" ] \
    || compgen -G "${state_dir}/upgrade-transaction.building.*" >/dev/null \
    || compgen -G "${state_dir}/upgrade-transaction.committed.*" >/dev/null \
    || compgen -G "${state_dir}/upgrade-transaction.restored.*" >/dev/null \
    || compgen -G "${state_dir}/upgrade-transaction.gc.*" >/dev/null \
    || compgen -G "${state_dir}/upgrade-transaction.terminal.*" >/dev/null; then
    garbage_collect_resolved_upgrade_transactions
  fi
  [ ! -e "${fixed_dir}" ] && [ ! -L "${fixed_dir}" ] \
    || fail "an unresolved native upgrade transaction already exists"
  UPGRADE_TRANSACTION_ID="$$-${BASHPID}-${RANDOM}"
  building_dir="${state_dir}/upgrade-transaction.building.${UPGRADE_TRANSACTION_ID}"
  [ ! -e "${building_dir}" ] && [ ! -L "${building_dir}" ] \
    || fail "native upgrade transaction build path already exists"
  UPGRADE_TRANSACTION_DIR="${building_dir}"
  UPGRADE_TRANSACTION_STATE=building
  trap cleanup_installer_state EXIT
  caller_umask="$(umask)"
  umask 077
  mkdir -- "${UPGRADE_TRANSACTION_DIR}"
  chmod 700 "${UPGRADE_TRANSACTION_DIR}"
  mkdir -- \
    "${UPGRADE_TRANSACTION_DIR}/install" \
    "${UPGRADE_TRANSACTION_DIR}/install-state" \
    "${UPGRADE_TRANSACTION_DIR}/units" \
    "${UPGRADE_TRANSACTION_DIR}/unit-state" \
    "${UPGRADE_TRANSACTION_DIR}/enablement" \
    "${UPGRADE_TRANSACTION_DIR}/service-state" \
    "${UPGRADE_TRANSACTION_DIR}/handoff" \
    "${UPGRADE_TRANSACTION_DIR}/handoff-state"
  capture_upgrade_install_root_metadata
  (umask 077 && printf '%s\n' "${UPGRADE_TRANSACTION_ID}" \
    >"${UPGRADE_TRANSACTION_DIR}/transaction-id")
  chmod 600 "${UPGRADE_TRANSACTION_DIR}/transaction-id"
  persist_upgrade_transaction_boot_id
  write_upgrade_transaction_snapshot_kind minimal \
    || fail "cannot persist native upgrade minimal snapshot kind"
  write_upgrade_transaction_phase building \
    || fail "cannot persist native upgrade transaction build phase"
  snapshot_upgrade_transaction_entry \
    "${INSTALL_DIR}/.env" \
    "${UPGRADE_TRANSACTION_DIR}/install/.env" \
    "${UPGRADE_TRANSACTION_DIR}/install-state/.env.state"
  verify_upgrade_install_root_metadata_unchanged \
    || fail "native installation root changed while the preseal guard was captured"
  sync_upgrade_transaction_snapshot \
    || fail "cannot make native upgrade preseal guard durable"
  write_upgrade_transaction_phase presealed \
    || fail "cannot persist native upgrade preseal phase"
  sync -f "${state_dir}" \
    || fail "cannot make native upgrade preseal publication durable"
  umask "${caller_umask}"
  UPGRADE_TRANSACTION_STATE=presealed
  log "captured durable native upgrade preseal guard"
}

begin_upgrade_transaction() {
  local item
  local unit
  local marker
  local state_dir
  local enablement
  local fixed_dir
  local caller_umask
  local systemd_deadline
  [ "${UPGRADE}" -eq 1 ] || return 0
  assert_install_transaction_lock_held
  if [ "${UPGRADE_TRANSACTION_STATE}" = none ]; then
    begin_upgrade_preseal_guard
  fi
  [ "${UPGRADE_TRANSACTION_STATE}" = presealed ] \
    || fail "native upgrade preseal guard is not active"
  state_dir="$(admin_handoff_state_dir)"
  fixed_dir="${state_dir}/upgrade-transaction"
  [ ! -e "${fixed_dir}" ] && [ ! -L "${fixed_dir}" ] \
    || fail "an unresolved native upgrade transaction already exists"
  caller_umask="$(umask)"
  umask 077
  if [ "${UPGRADE_SERVICE_STATE_CAPTURED:-0}" -eq 1 ]; then
    persist_upgrade_service_state \
      || fail "cannot persist native upgrade service baseline"
  fi

  while IFS= read -r item; do
    [ "${item}" != .env ] || continue
    snapshot_upgrade_transaction_entry \
      "${INSTALL_DIR}/${item}" \
      "${UPGRADE_TRANSACTION_DIR}/install/${item}" \
      "${UPGRADE_TRANSACTION_DIR}/install-state/${item}.state"
  done < <(upgrade_transaction_install_items)

  systemd_deadline=$((SECONDS + 60))
  while IFS= read -r unit; do
    snapshot_upgrade_transaction_entry \
      "${SYSTEMD_UNIT_ROOT}/${unit}" \
      "${UPGRADE_TRANSACTION_DIR}/units/${unit}" \
      "${UPGRADE_TRANSACTION_DIR}/unit-state/${unit}.state"
    enablement="$(capture_upgrade_unit_enablement \
      "${unit}" "${systemd_deadline}")" \
      || fail "cannot capture native unit enablement before upgrade"
    printf '%s\n' "${enablement}" \
      >"${UPGRADE_TRANSACTION_DIR}/enablement/${unit}.state"
  done < <(upgrade_transaction_unit_names)

  for marker in admin-handoff.pending "${ADMIN_HANDOFF_DELIVERED_NAME}"; do
    if [ -e "${state_dir}/${marker}" ] || [ -L "${state_dir}/${marker}" ]; then
      admin_handoff_assert_secure_file "${state_dir}/${marker}" 600
    fi
    snapshot_upgrade_transaction_entry \
      "${state_dir}/${marker}" \
      "${UPGRADE_TRANSACTION_DIR}/handoff/${marker}" \
      "${UPGRADE_TRANSACTION_DIR}/handoff-state/${marker}.state"
  done
  while IFS= read -r item; do
    [ "${item}" != .env ] || continue
    verify_upgrade_transaction_entry_matches_source \
      "${INSTALL_DIR}/${item}" \
      "${UPGRADE_TRANSACTION_DIR}/install/${item}" \
      "${UPGRADE_TRANSACTION_DIR}/install-state/${item}.state" \
      || fail "native upgrade install tree changed while it was snapshotted"
  done < <(upgrade_transaction_install_items)
  while IFS= read -r unit; do
    verify_upgrade_transaction_entry_matches_source \
      "${SYSTEMD_UNIT_ROOT}/${unit}" \
      "${UPGRADE_TRANSACTION_DIR}/units/${unit}" \
      "${UPGRADE_TRANSACTION_DIR}/unit-state/${unit}.state" \
      || fail "native systemd unit changed while it was snapshotted"
  done < <(upgrade_transaction_unit_names)
  for marker in admin-handoff.pending "${ADMIN_HANDOFF_DELIVERED_NAME}"; do
    verify_upgrade_transaction_entry_matches_source \
      "${state_dir}/${marker}" \
      "${UPGRADE_TRANSACTION_DIR}/handoff/${marker}" \
      "${UPGRADE_TRANSACTION_DIR}/handoff-state/${marker}.state" \
      || fail "native handoff marker changed while it was snapshotted"
  done
  if [ "${UPGRADE_SERVICE_STATE_CAPTURED:-0}" -eq 1 ]; then
    verify_captured_upgrade_service_state_unchanged \
      || fail "native service state changed while the upgrade was snapshotted"
  fi
  assert_control_tree_safe "${UPGRADE_TRANSACTION_DIR}" structural
  sync_upgrade_transaction_snapshot \
    || fail "cannot make native upgrade transaction snapshot durable"
  write_upgrade_transaction_snapshot_kind full \
    || fail "cannot persist native upgrade full snapshot kind"
  if [ "${UPGRADE_SERVICE_STATE_CAPTURED:-0}" -eq 1 ]; then
    # Publish the persistent reboot fence while the on-disk decision is still
    # presealed. Only after every drop-in and the marker are durable may the
    # transaction become armed and permit arbitrary install-tree mutation.
    arm_upgrade_boot_fence
  fi
  write_upgrade_transaction_phase armed \
    || fail "cannot persist native upgrade transaction armed phase"
  mv -- "${UPGRADE_TRANSACTION_DIR}" "${fixed_dir}" \
    || fail "cannot publish native upgrade transaction snapshot"
  UPGRADE_TRANSACTION_DIR="${fixed_dir}"
  UPGRADE_TRANSACTION_PHASE_FILE="${fixed_dir}/phase"
  UPGRADE_TRANSACTION_STATE=armed
  sync -f "${state_dir}" \
    || fail "cannot make native upgrade transaction publication durable"
  umask "${caller_umask}"
  UPGRADE_TRANSACTION_STATE=armed
  log "captured root-only native upgrade transaction snapshot"
}

is_output_root_mountpoint() {
  local output_root="$1"
  if grep -F " ${output_root} " /proc/self/mountinfo >/dev/null 2>&1; then
    return 0
  fi
  return 1
}

managed_data_paths() {
  printf '%s\n' \
    "${INSTALL_DIR}/data" \
    "${INSTALL_DIR}/data/agent" \
    "${INSTALL_DIR}/data/agent/identity" \
    "${INSTALL_DIR}/data/media" \
    "${INSTALL_DIR}/data/media/work" \
    "${INSTALL_DIR}/data/media/logs" \
    "${INSTALL_DIR}/data/postgres" \
    "${INSTALL_DIR}/data/postgres-run" \
    "${INSTALL_DIR}/data/zlm" \
    "${INSTALL_DIR}/data/zlm/www" \
    "${INSTALL_DIR}/data/zlm/www/record" \
    "${INSTALL_DIR}/data/zlm/www/snap" \
    "${INSTALL_DIR}/data/zlm/www/output" \
    "${INSTALL_DIR}/data/zlm/www/output/mp4" \
    "${INSTALL_DIR}/data/zlm/www/output/hls"
}

path_is_symbolic_link_status() {
  [ -L "$1" ]
}

assert_managed_data_path_safe() {
  local path="$1"
  local relative
  local component
  local current="${INSTALL_DIR}"
  local -a components=()
  case "${path}" in
    "${INSTALL_DIR}"/*) relative="${path#"${INSTALL_DIR}"/}" ;;
    *) fail "managed native data path escapes the installation root: ${path}" ;;
  esac
  ! path_is_symbolic_link_status "${INSTALL_DIR}" \
    || fail "native installation root must not be a symbolic link"
  [ -d "${INSTALL_DIR}" ] \
    || fail "native installation root must be a directory"
  IFS='/' read -r -a components <<<"${relative}"
  for component in "${components[@]}"; do
    [ -n "${component}" ] && [ "${component}" != . ] && [ "${component}" != .. ] \
      || fail "managed native data path contains an invalid component: ${path}"
    current="${current}/${component}"
    ! path_is_symbolic_link_status "${current}" \
      || fail "managed native data path must not contain a symbolic link: ${current}"
    if [ -e "${current}" ] && [ ! -d "${current}" ]; then
      fail "managed native data path component must be a directory: ${current}"
    fi
  done
}

ensure_managed_data_directory() {
  local path="$1"
  local relative
  local component
  local current="${INSTALL_DIR}"
  local -a components=()
  assert_managed_data_path_safe "${path}"
  relative="${path#"${INSTALL_DIR}"/}"
  IFS='/' read -r -a components <<<"${relative}"
  for component in "${components[@]}"; do
    current="${current}/${component}"
    if [ ! -e "${current}" ] && [ ! -L "${current}" ]; then
      mkdir -- "${current}"
    fi
    ! path_is_symbolic_link_status "${current}" && [ -d "${current}" ] \
      || fail "managed native data directory changed during creation: ${current}"
  done
}

assert_managed_data_paths_safe() {
  local path
  while IFS= read -r path; do
    assert_managed_data_path_safe "${path}"
  done < <(managed_data_paths)
}

assert_postgres_password_file_safe() {
  local path="${INSTALL_DIR}/.postgres-pw"
  ! path_is_symbolic_link_status "${path}" \
    || fail "temporary PostgreSQL password path must not be a symbolic link"
  if [ -e "${path}" ] && [ ! -f "${path}" ]; then
    fail "temporary PostgreSQL password path must be a regular file"
  fi
}

create_output_layout_if_local() {
  local output_root="${INSTALL_DIR}/data/zlm/www/output"
  assert_managed_data_paths_safe
  if is_output_root_mountpoint "${output_root}"; then
    log "检测到 output 目录是挂载点，跳过创建 output/mp4 和 output/hls: ${output_root}"
    return 0
  fi
  ensure_managed_data_directory "${output_root}/mp4"
  ensure_managed_data_directory "${output_root}/hls"
}

prepare_layout() {
  assert_managed_data_paths_safe
  assert_postgres_password_file_safe
  ensure_control_directory "${INSTALL_DIR}/bin"
  ensure_control_directory "${INSTALL_DIR}/runtime"
  ensure_control_directory "${INSTALL_DIR}/ui"
  ensure_control_directory "${INSTALL_DIR}/zlm"
  ensure_control_directory "${INSTALL_DIR}/docs"
  # The service account validates the public JWT key while the fresh admin
  # handoff marker is created, before fix_permissions seals the whole tree.
  # Publish traversable root-owned directories now; the group has no write bit.
  ensure_control_directory \
    "${INSTALL_DIR}/certs/auth" 750 "root:${SERVICE_GROUP}"
  ensure_control_directory "${INSTALL_DIR}/systemd"
  ensure_managed_data_directory "${INSTALL_DIR}/data/agent"
  ensure_managed_data_directory "${INSTALL_DIR}/data/media/work"
  ensure_managed_data_directory "${INSTALL_DIR}/data/media/logs"
  ensure_managed_data_directory "${INSTALL_DIR}/data/postgres-run"
  ensure_managed_data_directory "${INSTALL_DIR}/data/zlm/www/record"
  ensure_managed_data_directory "${INSTALL_DIR}/data/zlm/www/snap"
  create_output_layout_if_local
}

fix_output_permissions() {
  local output_root="${INSTALL_DIR}/data/zlm/www/output"
  assert_managed_data_paths_safe
  if is_output_root_mountpoint "${output_root}"; then
    log "检测到 output 目录是挂载点，跳过修正 output 权限: ${output_root}"
    return 0
  fi
  ensure_managed_data_directory "${output_root}/mp4"
  ensure_managed_data_directory "${output_root}/hls"

  local item
  for item in "${output_root}" "${output_root}/mp4" "${output_root}/hls"; do
    [ -e "${item}" ] || continue
    chown -h "${SERVICE_USER}:${SERVICE_GROUP}" "${item}"
    chmod 2775 "${item}"
  done

  for item in "${output_root}"/mp4/node-*-mp4 "${output_root}"/hls/node-*-hls; do
    ! path_is_symbolic_link_status "${item}" \
      || fail "managed native output path must not be a symbolic link: ${item}"
    [ -d "${item}" ] || continue
    chown -h "${SERVICE_USER}:${SERVICE_GROUP}" "${item}"
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
  [[ "${value}" != *"'"* ]] \
    || fail "${key} 的值不能包含单引号"
  # The same file is consumed by systemd EnvironmentFile and non-evaluating
  # parsers. Always quote values so an upgrade cannot preserve shell syntax
  # from a formerly service-writable environment as executable text.
  printf "%s='%s'\n" "${key}" "${value}" >>"${file}"
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

required_upgrade_database_value() {
  local env_file="$1"
  local key="$2"
  local value
  require_unique_env_key "${env_file}" "${key}"
  value="$(existing_env_value "${env_file}" "${key}")" \
    || fail "upgrade requires ${key} in the existing environment"
  [ -n "${value}" ] && [[ "${value}" != *$'\n'* ]] && [[ "${value}" != *$'\r'* ]] \
    || fail "upgrade ${key} is empty or contains an unsupported line break"
  printf '%s' "${value}"
}

prepare_upgrade_database_configuration() {
  local env_file="${INSTALL_DIR}/.env"
  local existing_database_url
  [ "${UPGRADE}" -eq 1 ] || return 0
  role_has_core "${INSTALL_ROLE}" || return 0
  existing_database_url="$(required_upgrade_database_value \
    "${env_file}" DATABASE_URL)"
  case "${TRUSTED_POSTGRES_UNIT_COUNT:-}" in
    1)
      [ "${DATABASE_MODE:-}" != external ] && [ -z "${DATABASE_URL_INPUT:-}" ] \
        || fail "bundled PostgreSQL upgrades cannot switch to an external database"
      [ "${BUNDLE_POSTGRES_RUNTIME:-false}" = true ] \
        && [ -n "${POSTGRES_RUNTIME_PATH:-}" ] \
        && [ -d "${PACKAGE_ROOT}/${POSTGRES_RUNTIME_PATH}" ] \
        || fail "the upgrade package is missing the existing bundled PostgreSQL runtime"
      DATABASE_MODE=bundled
      POSTGRES_DB="$(required_upgrade_database_value "${env_file}" POSTGRES_DB)"
      POSTGRES_USER="$(required_upgrade_database_value "${env_file}" POSTGRES_USER)"
      POSTGRES_PASSWORD="$(required_upgrade_database_value "${env_file}" POSTGRES_PASSWORD)"
      POSTGRES_PORT="$(required_upgrade_database_value "${env_file}" POSTGRES_PORT)"
      validate_port_number POSTGRES_PORT "${POSTGRES_PORT}"
      DATABASE_URL="${existing_database_url}"
      ;;
    0)
      DATABASE_MODE=external
      if [ -n "${DATABASE_URL_INPUT:-}" ]; then
        [[ "${DATABASE_URL_INPUT}" != *$'\n'* ]] \
          && [[ "${DATABASE_URL_INPUT}" != *$'\r'* ]] \
          || fail "--database-url contains an unsupported line break"
        DATABASE_URL="${DATABASE_URL_INPUT}"
      else
        DATABASE_URL="${existing_database_url}"
      fi
      POSTGRES_DB="$(env_value_or_default "${env_file}" POSTGRES_DB streamserver)"
      POSTGRES_USER="$(env_value_or_default "${env_file}" POSTGRES_USER postgres)"
      POSTGRES_PASSWORD="$(env_value_or_default "${env_file}" POSTGRES_PASSWORD '')"
      POSTGRES_PORT="$(env_value_or_default "${env_file}" POSTGRES_PORT 5432)"
      validate_port_number POSTGRES_PORT "${POSTGRES_PORT}"
      ;;
    *) fail "trusted PostgreSQL unit topology is unavailable for upgrade" ;;
  esac
}

configure_database() {
  local existing_env_file="${INSTALL_DIR}/.env"
  local existing_postgres_password
  local generated_password
  if ! role_has_core "${INSTALL_ROLE}"; then
    return 0
  fi
  if [ "${UPGRADE}" -eq 1 ]; then
    case "${DATABASE_MODE}" in
      bundled|external) return 0 ;;
      *) fail "upgrade database topology was not prepared before quiesce" ;;
    esac
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
    assign_local_tcp_port POSTGRES_PORT "${existing_env_file}" \
      "POSTGRES_PORT" "数据库宿主机监听端口" "5432"
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

validate_ipv4_literal() {
  local value="$1"
  local octet
  local -a octets=()
  [[ "${value}" =~ ^([0-9]{1,3}\.){3}[0-9]{1,3}$ ]] || return 1
  IFS='.' read -r -a octets <<<"${value}"
  [ "${#octets[@]}" -eq 4 ] || return 1
  for octet in "${octets[@]}"; do
    [[ "${octet}" =~ ^(0|[1-9][0-9]{0,2})$ ]] || return 1
    [ "$((10#${octet}))" -le 255 ] || return 1
  done
}

validate_ipv6_literal() {
  local value="$1"
  local ipv4_tail
  local left
  local right
  local group
  local group_count=0
  local -a groups=()
  [[ "${value}" == *:* ]] || return 1
  case "${value}" in *%*) return 1 ;; esac

  if [[ "${value}" == *.* ]]; then
    ipv4_tail="${value##*:}"
    validate_ipv4_literal "${ipv4_tail}" || return 1
    value="${value%:*}:0:0"
  fi
  [[ "${value}" =~ ^[0-9A-Fa-f:]+$ ]] || return 1

  if [[ "${value}" == *::* ]]; then
    left="${value%%::*}"
    right="${value#*::}"
    [[ "${right}" != *::* ]] || return 1
    [[ -z "${left}" || "${left}" != :* && "${left}" != *: ]] || return 1
    [[ -z "${right}" || "${right}" != :* && "${right}" != *: ]] || return 1
    if [ -n "${left}" ]; then
      IFS=':' read -r -a groups <<<"${left}"
      for group in "${groups[@]}"; do
        [[ "${group}" =~ ^[0-9A-Fa-f]{1,4}$ ]] || return 1
        group_count=$((group_count + 1))
      done
    fi
    if [ -n "${right}" ]; then
      IFS=':' read -r -a groups <<<"${right}"
      for group in "${groups[@]}"; do
        [[ "${group}" =~ ^[0-9A-Fa-f]{1,4}$ ]] || return 1
        group_count=$((group_count + 1))
      done
    fi
    [ "${group_count}" -lt 8 ]
    return
  fi

  IFS=':' read -r -a groups <<<"${value}"
  [ "${#groups[@]}" -eq 8 ] || return 1
  for group in "${groups[@]}"; do
    [[ "${group}" =~ ^[0-9A-Fa-f]{1,4}$ ]] || return 1
  done
}

validate_dns_name() {
  local value="$1"
  local label
  local -a labels=()
  [ -n "${value}" ] && [ "${#value}" -le 253 ] || return 1
  [[ "${value}" != .* && "${value}" != *. && "${value}" != *..* ]] || return 1
  [[ "${value}" =~ ^[A-Za-z0-9.-]+$ ]] || return 1
  [[ "${value}" =~ ^[0-9.]+$ ]] && return 1
  IFS='.' read -r -a labels <<<"${value}"
  [ "${#labels[@]}" -ge 1 ] || return 1
  for label in "${labels[@]}"; do
    [ -n "${label}" ] && [ "${#label}" -le 63 ] || return 1
    [[ "${label}" =~ ^[A-Za-z0-9]([A-Za-z0-9-]*[A-Za-z0-9])?$ ]] || return 1
  done
}

validate_internal_pki_host() {
  local value="$1"
  [ -n "${value}" ] || return 1
  case "${value}" in
    *$'\n'*|*$'\r'*|*$'\t'*|*,*|*' '*|*'/'*|*'\\'*) return 1 ;;
  esac
  validate_ipv4_literal "${value}" && return 0
  if [[ "${value}" == *:* ]]; then
    validate_ipv6_literal "${value}"
    return
  fi
  validate_dns_name "${value}"
}

internal_pki_san_for_host() {
  local value="$1"
  validate_internal_pki_host "${value}" \
    || fail "internal PKI host is not a safe DNS name or IP address: ${value}"
  if validate_ipv4_literal "${value}" || validate_ipv6_literal "${value}"; then
    printf 'IP:%s' "${value}"
  else
    printf 'DNS:%s' "${value}"
  fi
}

generate_internal_ca() {
  local work_root="$1"
  local name="$2"
  local common_name="$3"
  local ca_dir="${work_root}/${name}"
  local start_date
  local end_date
  mkdir -p -- "${ca_dir}" || return 1
  install -d -m 0700 -- "${ca_dir}/newcerts" || return 1
  : >"${ca_dir}/index" || return 1
  printf '1000\n' >"${ca_dir}/serial" || return 1
  openssl genpkey -algorithm Ed25519 -out "${ca_dir}/ca.key" >/dev/null 2>&1 \
    || return 1
  openssl req -new -key "${ca_dir}/ca.key" \
    -subj "/CN=${common_name}" -out "${ca_dir}/ca.csr" >/dev/null 2>&1 \
    || return 1
  cat >"${ca_dir}/ca.cnf" <<EOF
[ ca ]
default_ca = root_ca

[ root_ca ]
dir = ${ca_dir}
database = \$dir/index
new_certs_dir = \$dir/newcerts
serial = \$dir/serial
certificate = \$dir/ca.pem
private_key = \$dir/ca.key
default_md = default
policy = policy_any
unique_subject = no

[ policy_any ]
commonName = supplied

[ v3_ca ]
basicConstraints = critical,CA:TRUE,pathlen:0
keyUsage = critical,keyCertSign
subjectKeyIdentifier = hash
authorityKeyIdentifier = keyid:always
EOF
  [ -f "${ca_dir}/ca.cnf" ] || return 1
  start_date="$(date -u -d '10 minutes ago' +%y%m%d%H%M%SZ)"
  end_date="$(date -u -d '3650 days' +%y%m%d%H%M%SZ)"
  openssl ca -selfsign -batch -notext -config "${ca_dir}/ca.cnf" \
    -extensions v3_ca -startdate "${start_date}" -enddate "${end_date}" \
    -keyfile "${ca_dir}/ca.key" -in "${ca_dir}/ca.csr" \
    -out "${ca_dir}/ca.pem" >/dev/null 2>&1 || return 1
  openssl verify -CAfile "${ca_dir}/ca.pem" "${ca_dir}/ca.pem" >/dev/null 2>&1 \
    || return 1
}

issue_internal_leaf() {
  local ca_dir="$1"
  local output_root="$2"
  local name="$3"
  local common_name="$4"
  local profile="$5"
  local subject_alt_name="$6"
  local leaf_dir="${output_root}/${name}"
  local start_date
  local end_date
  mkdir -p -- "${output_root}" || return 1
  install -d -m 0700 -- "${leaf_dir}" || return 1
  openssl genpkey -algorithm Ed25519 -out "${leaf_dir}/leaf.key" >/dev/null 2>&1 \
    || return 1
  openssl req -new -key "${leaf_dir}/leaf.key" \
    -subj "/CN=${common_name}" -out "${leaf_dir}/leaf.csr" >/dev/null 2>&1 \
    || return 1
  case "${profile}" in
    server)
      cat >"${leaf_dir}/leaf.cnf" <<EOF
[ leaf ]
basicConstraints = critical,CA:FALSE
keyUsage = critical,digitalSignature
extendedKeyUsage = critical,serverAuth
subjectAltName = ${subject_alt_name}
subjectKeyIdentifier = hash
authorityKeyIdentifier = keyid:always
EOF
      ;;
    client)
      cat >"${leaf_dir}/leaf.cnf" <<EOF
[ leaf ]
basicConstraints = critical,CA:FALSE
keyUsage = critical,digitalSignature
extendedKeyUsage = critical,clientAuth
subjectAltName = ${subject_alt_name}
subjectKeyIdentifier = hash
authorityKeyIdentifier = keyid:always
EOF
      ;;
    *) fail "unsupported internal PKI leaf profile: ${profile}" ;;
  esac
  start_date="$(date -u -d '5 minutes ago' +%y%m%d%H%M%SZ)"
  end_date="$(date -u -d '825 days' +%y%m%d%H%M%SZ)"
  openssl ca -batch -notext -config "${ca_dir}/ca.cnf" \
    -extfile "${leaf_dir}/leaf.cnf" -extensions leaf \
    -startdate "${start_date}" -enddate "${end_date}" \
    -in "${leaf_dir}/leaf.csr" -out "${leaf_dir}/leaf.pem" >/dev/null 2>&1 \
    || return 1
  openssl verify -CAfile "${ca_dir}/ca.pem" "${leaf_dir}/leaf.pem" >/dev/null 2>&1 \
    || return 1
}

install_internal_pki_file() {
  local source="$1"
  local target="$2"
  local mode="$3"
  local owner="${4:-root:${SERVICE_GROUP}}"
  local temporary_file
  begin_atomic_target_write "${target}"
  temporary_file="${LAST_INSTALLER_TEMP_FILE}"
  cp -- "${source}" "${temporary_file}"
  finish_atomic_target_write "${temporary_file}" "${target}" "${mode}" "${owner}"
}

ensure_core_internal_pki() {
  local existing_env_file="$1"
  local pki_root="${INSTALL_DIR}/certs/internal"
  local work_root
  local grpc_san
  local http_san
  local agent_root_fingerprint
  local server_root_fingerprint
  local management_root_fingerprint

  if [ -f "${existing_env_file}" ]; then
    for required_value in \
      CORE_INSTANCE_ID CORE_GRPC_TLS_CERT_PATH CORE_GRPC_TLS_KEY_PATH \
      CORE_GRPC_TLS_CLIENT_CA_PATH CORE_GRPC_TLS_SERVER_CA_PATH \
      CORE_AGENT_CA_CERT_PATH CORE_AGENT_CA_KEY_PATH \
      CORE_AGENT_CAPABILITY_JWT_PRIVATE_KEY_PATH \
      CORE_AGENT_CAPABILITY_JWT_PUBLIC_KEY_PATH \
      CORE_AGENT_MANAGEMENT_CLIENT_CERT_PATH \
      CORE_AGENT_MANAGEMENT_CLIENT_KEY_PATH CORE_AGENT_MANAGEMENT_CA_PATH; do
      [ -n "${!required_value:-}" ] \
        || fail "existing production install is missing ${required_value}; run the migration preflight"
    done
    return 0
  fi

  install -d -o root -g root -m 0700 -- "${pki_root}"
  work_root="$(mktemp -d "${pki_root}/.generate.XXXXXX")"
  chmod 0700 "${work_root}"
  grpc_san="$(internal_pki_san_for_host "${CORE_GRPC_TLS_DOMAIN_NAME}"),DNS:localhost,IP:127.0.0.1,IP:::1"
  http_san="$(internal_pki_san_for_host "${CORE_HTTP_PUBLIC_HOST}"),DNS:localhost,IP:127.0.0.1,IP:::1"
  generate_internal_ca "${work_root}" agent-issuer "SS Agent CA ${CORE_INSTANCE_ID}" || {
    rm -rf -- "${work_root}"
    fail "failed to generate the three-root Core internal PKI"
  }
  generate_internal_ca "${work_root}" control-server "SS Control CA ${CORE_INSTANCE_ID}" || {
    rm -rf -- "${work_root}"
    fail "failed to generate the three-root Core internal PKI"
  }
  generate_internal_ca "${work_root}" management-client "SS Mgmt CA ${CORE_INSTANCE_ID}" || {
    rm -rf -- "${work_root}"
    fail "failed to generate the three-root Core internal PKI"
  }
  issue_internal_leaf \
    "${work_root}/control-server" "${work_root}" core-grpc \
    "SS Core gRPC ${CORE_INSTANCE_ID}" server "${grpc_san}" || {
      rm -rf -- "${work_root}"
      fail "failed to generate the Core gRPC server identity"
    }
  issue_internal_leaf \
    "${work_root}/control-server" "${work_root}" core-http \
    "SS Core HTTP ${CORE_INSTANCE_ID}" server "${http_san}" || {
      rm -rf -- "${work_root}"
      fail "failed to generate the Core HTTP server identity"
    }
  issue_internal_leaf \
    "${work_root}/management-client" "${work_root}" core-management \
    "SS Core Mgmt ${CORE_INSTANCE_ID}" client \
    "URI:spiffe://streamserver/core/${CORE_INSTANCE_ID}" || {
      rm -rf -- "${work_root}"
      fail "failed to generate the Core management client identity"
    }
  openssl genpkey -algorithm Ed25519 \
    -out "${work_root}/agent-capability-private.pem" >/dev/null 2>&1 || {
      rm -rf -- "${work_root}"
      fail "failed to generate the Agent capability signing key"
    }
  openssl pkey -in "${work_root}/agent-capability-private.pem" -pubout \
    -out "${work_root}/agent-capability-public.pem" >/dev/null 2>&1 || {
      rm -rf -- "${work_root}"
      fail "failed to derive the Agent capability public key"
    }

  agent_root_fingerprint="$(openssl x509 -in "${work_root}/agent-issuer/ca.pem" \
    -outform DER | sha256sum | awk '{print $1}')"
  server_root_fingerprint="$(openssl x509 -in "${work_root}/control-server/ca.pem" \
    -outform DER | sha256sum | awk '{print $1}')"
  management_root_fingerprint="$(openssl x509 -in "${work_root}/management-client/ca.pem" \
    -outform DER | sha256sum | awk '{print $1}')"
  [ -n "${agent_root_fingerprint}" ] \
    && [ "${agent_root_fingerprint}" != "${server_root_fingerprint}" ] \
    && [ "${agent_root_fingerprint}" != "${management_root_fingerprint}" ] \
    && [ "${server_root_fingerprint}" != "${management_root_fingerprint}" ] || {
      rm -rf -- "${work_root}"
      fail "generated internal PKI roots are not distinct"
    }

  install_internal_pki_file \
    "${work_root}/agent-issuer/ca.pem" "${CORE_AGENT_CA_CERT_PATH}" 640
  install_internal_pki_file \
    "${work_root}/agent-issuer/ca.key" "${CORE_AGENT_CA_KEY_PATH}" 640
  install_internal_pki_file \
    "${work_root}/control-server/ca.pem" "${CORE_GRPC_TLS_SERVER_CA_PATH}" 640
  install_internal_pki_file \
    "${work_root}/control-server/ca.key" "${pki_root}/control-plane-server-ca-key.pem" 600 root:root
  install_internal_pki_file \
    "${work_root}/core-grpc/leaf.pem" "${CORE_GRPC_TLS_CERT_PATH}" 640
  install_internal_pki_file \
    "${work_root}/core-grpc/leaf.key" "${CORE_GRPC_TLS_KEY_PATH}" 640
  install_internal_pki_file \
    "${work_root}/core-http/leaf.pem" "${CORE_HTTP_TLS_CERT_PATH}" 640
  install_internal_pki_file \
    "${work_root}/core-http/leaf.key" "${CORE_HTTP_TLS_KEY_PATH}" 640
  install_internal_pki_file \
    "${work_root}/management-client/ca.pem" "${CORE_AGENT_MANAGEMENT_CA_PATH}" 640
  install_internal_pki_file \
    "${work_root}/management-client/ca.key" "${pki_root}/management-client-ca-key.pem" 600 root:root
  install_internal_pki_file \
    "${work_root}/core-management/leaf.pem" \
    "${CORE_AGENT_MANAGEMENT_CLIENT_CERT_PATH}" 640
  install_internal_pki_file \
    "${work_root}/core-management/leaf.key" \
    "${CORE_AGENT_MANAGEMENT_CLIENT_KEY_PATH}" 640
  install_internal_pki_file \
    "${work_root}/agent-capability-private.pem" \
    "${CORE_AGENT_CAPABILITY_JWT_PRIVATE_KEY_PATH}" 640
  install_internal_pki_file \
    "${work_root}/agent-capability-public.pem" \
    "${CORE_AGENT_CAPABILITY_JWT_PUBLIC_KEY_PATH}" 640
  rm -rf -- "${work_root}"
}

configure_core_values() {
  local existing_env_file="${INSTALL_DIR}/.env"
  local existing_hook_secret
  local existing_auth_mode
  local generated_secret
  local pending_admin_username=""
  local jwt_private_target
  local jwt_public_target
  local jwt_private_temp
  local jwt_public_temp
  local source_gateway_tls_skip
  local source_gateway_tls_default
  if [ "${INITIAL_ADMIN_PASSWORD_READY:-0}" -ne 1 ]; then
    unset ADMIN_PASSWORD
  fi
  if ! role_has_core "${INSTALL_ROLE}"; then
    return 0
  fi
  assign_local_tcp_port CORE_HTTP_PORT "${existing_env_file}" \
    "CORE_HTTP_PORT" "控制面板网页和 HTTP API 端口" "8080"
  assign_local_tcp_port CORE_GRPC_PORT "${existing_env_file}" \
    "CORE_GRPC_PORT" "控制面板内部通信端口" "50051"
  CORE_INSTANCE_ID="$(env_value_or_default "${existing_env_file}" "CORE_INSTANCE_ID" "$(generate_uuid)")"
  CORE_HTTP_PUBLIC_HOST="$(prompt_non_empty "Core HTTP 对外证书主机名或 IP" \
    "$(env_value_or_default "${existing_env_file}" "CORE_HTTP_PUBLIC_HOST" \
      "$(hostname -f 2>/dev/null || hostname -s)")")"
  validate_internal_pki_host "${CORE_HTTP_PUBLIC_HOST}" \
    || fail "Core HTTP 对外证书主机名或 IP 格式无效"
  CORE_GRPC_TLS_DOMAIN_NAME="$(prompt_non_empty "Core gRPC TLS 域名或 IP" \
    "$(env_value_or_default "${existing_env_file}" "CORE_GRPC_TLS_DOMAIN_NAME" \
      "${CORE_HTTP_PUBLIC_HOST}")")"
  validate_internal_pki_host "${CORE_GRPC_TLS_DOMAIN_NAME}" \
    || fail "Core gRPC TLS 域名或 IP 格式无效"

  CORE_HTTP_TLS_CERT_PATH="$(env_value_or_default "${existing_env_file}" \
    "CORE_HTTP_TLS_CERT_PATH" "${INSTALL_DIR}/certs/internal/core-http-server.pem")"
  CORE_HTTP_TLS_KEY_PATH="$(env_value_or_default "${existing_env_file}" \
    "CORE_HTTP_TLS_KEY_PATH" "${INSTALL_DIR}/certs/internal/core-http-server-key.pem")"
  CORE_GRPC_TLS_CERT_PATH="$(env_value_or_default "${existing_env_file}" \
    "CORE_GRPC_TLS_CERT_PATH" "${INSTALL_DIR}/certs/internal/core-grpc-server.pem")"
  CORE_GRPC_TLS_KEY_PATH="$(env_value_or_default "${existing_env_file}" \
    "CORE_GRPC_TLS_KEY_PATH" "${INSTALL_DIR}/certs/internal/core-grpc-server-key.pem")"
  CORE_GRPC_TLS_CLIENT_CA_PATH="$(env_value_or_default "${existing_env_file}" \
    "CORE_GRPC_TLS_CLIENT_CA_PATH" "${INSTALL_DIR}/certs/internal/agent-client-issuer-ca.pem")"
  CORE_GRPC_TLS_SERVER_CA_PATH="$(env_value_or_default "${existing_env_file}" \
    "CORE_GRPC_TLS_SERVER_CA_PATH" "${INSTALL_DIR}/certs/internal/control-plane-server-ca.pem")"
  CORE_AGENT_CA_CERT_PATH="$(env_value_or_default "${existing_env_file}" \
    "CORE_AGENT_CA_CERT_PATH" "${INSTALL_DIR}/certs/internal/agent-client-issuer-ca.pem")"
  CORE_AGENT_CA_KEY_PATH="$(env_value_or_default "${existing_env_file}" \
    "CORE_AGENT_CA_KEY_PATH" "${INSTALL_DIR}/certs/internal/agent-client-issuer-ca-key.pem")"
  CORE_AGENT_CAPABILITY_JWT_PRIVATE_KEY_PATH="$(env_value_or_default "${existing_env_file}" \
    "CORE_AGENT_CAPABILITY_JWT_PRIVATE_KEY_PATH" "${INSTALL_DIR}/certs/internal/agent-capability-private.pem")"
  CORE_AGENT_CAPABILITY_JWT_PUBLIC_KEY_PATH="$(env_value_or_default "${existing_env_file}" \
    "CORE_AGENT_CAPABILITY_JWT_PUBLIC_KEY_PATH" "${INSTALL_DIR}/certs/internal/agent-capability-public.pem")"
  CORE_AGENT_MANAGEMENT_CLIENT_CERT_PATH="$(env_value_or_default "${existing_env_file}" \
    "CORE_AGENT_MANAGEMENT_CLIENT_CERT_PATH" "${INSTALL_DIR}/certs/internal/core-management-client.pem")"
  CORE_AGENT_MANAGEMENT_CLIENT_KEY_PATH="$(env_value_or_default "${existing_env_file}" \
    "CORE_AGENT_MANAGEMENT_CLIENT_KEY_PATH" "${INSTALL_DIR}/certs/internal/core-management-client-key.pem")"
  CORE_AGENT_MANAGEMENT_CA_PATH="$(env_value_or_default "${existing_env_file}" \
    "CORE_AGENT_MANAGEMENT_CA_PATH" "${INSTALL_DIR}/certs/internal/management-client-ca.pem")"
  ensure_core_internal_pki "${existing_env_file}"
  CORE_HTTP_ADDR="$(prompt_non_empty "Core HTTP 监听地址" \
    "$(env_value_or_default "${existing_env_file}" "CORE_HTTP_ADDR" "127.0.0.1:${CORE_HTTP_PORT}")")"
  CORE_HTTP_SCHEME="https"
  CORE_GRPC_ADDR="$(prompt_non_empty "Core gRPC 监听地址" \
    "$(env_value_or_default "${existing_env_file}" "CORE_GRPC_ADDR" "127.0.0.1:${CORE_GRPC_PORT}")")"
  generated_secret="$(generate_secret)"
  existing_hook_secret="$(env_value_or_default "${existing_env_file}" "HOOK_SHARED_SECRET" "")"
  HOOK_SHARED_SECRET="$(prompt "ZLM Hook/API 密钥（留空沿用现有值或自动生成）" "")"
  [ -n "${HOOK_SHARED_SECRET}" ] || HOOK_SHARED_SECRET="${existing_hook_secret}"
  [ -n "${HOOK_SHARED_SECRET}" ] || HOOK_SHARED_SECRET="${generated_secret}"
  HOOK_SOURCE_ALLOWLIST="$(prompt "Hook 源 IP 白名单，逗号分隔（可留空）" "$(env_value_or_default "${existing_env_file}" "HOOK_SOURCE_ALLOWLIST" "")")"
  STORAGE_ALLOWLIST="$(prompt_non_empty "本地媒体文件访问白名单，逗号分隔" "$(env_value_or_default "${existing_env_file}" "STORAGE_ALLOWLIST" "${INSTALL_DIR}/data/media/work,${INSTALL_DIR}/data/zlm/www")")"
  SOURCE_GATEWAY_BASE_URL="$(prompt "Source Gateway HTTPS 基准地址（留空关闭）" "$(env_value_or_default "${existing_env_file}" "SOURCE_GATEWAY_BASE_URL" "")")"
  if [ -n "${SOURCE_GATEWAY_BASE_URL}" ]; then
    case "${SOURCE_GATEWAY_BASE_URL}" in
      https://*) ;;
      *) fail "SOURCE_GATEWAY_BASE_URL must use https" ;;
    esac
  fi
  source_gateway_tls_skip="$(env_value_or_default "${existing_env_file}" "SOURCE_GATEWAY_TLS_INSECURE_SKIP_VERIFY" "false")"
  case "${source_gateway_tls_skip}" in
    true) source_gateway_tls_default="Y" ;;
    false) source_gateway_tls_default="N" ;;
    *) fail "SOURCE_GATEWAY_TLS_INSECURE_SKIP_VERIFY must be true or false" ;;
  esac
  if prompt_yes_no "是否仅对 Source Gateway 跳过证书链、有效期和主机名验证？" "${source_gateway_tls_default}"; then
    SOURCE_GATEWAY_TLS_INSECURE_SKIP_VERIFY="true"
  else
    SOURCE_GATEWAY_TLS_INSECURE_SKIP_VERIFY="false"
  fi
  SOURCE_GATEWAY_PREFETCH_POLL_MS="$(prompt_positive_integer \
    "SOURCE_GATEWAY_PREFETCH_POLL_MS" "Source Gateway 预取轮询间隔（毫秒）" \
    "$(env_value_or_default "${existing_env_file}" "SOURCE_GATEWAY_PREFETCH_POLL_MS" "1000")")"
  SOURCE_GATEWAY_PREFETCH_TIMEOUT_MS="$(prompt_positive_integer \
    "SOURCE_GATEWAY_PREFETCH_TIMEOUT_MS" "Source Gateway 预取总超时（毫秒）" \
    "$(env_value_or_default "${existing_env_file}" "SOURCE_GATEWAY_PREFETCH_TIMEOUT_MS" "600000")")"
  [ "${SOURCE_GATEWAY_PREFETCH_TIMEOUT_MS}" -ge "${SOURCE_GATEWAY_PREFETCH_POLL_MS}" ] \
    || fail "SOURCE_GATEWAY_PREFETCH_TIMEOUT_MS must be greater than or equal to SOURCE_GATEWAY_PREFETCH_POLL_MS"
  existing_auth_mode="$(env_value_or_default "${existing_env_file}" "AUTH_MODE" "local_password")"
  AUTH_MODE="${existing_auth_mode}"
  AUTH_ENABLED="true"
  JWT_PUBLIC_KEY="$(env_value_or_default "${existing_env_file}" "JWT_PUBLIC_KEY" "")"
  AUTH_JWT_PRIVATE_KEY_PATH="$(env_value_or_default "${existing_env_file}" "AUTH_JWT_PRIVATE_KEY_PATH" "")"
  AUTH_JWT_PUBLIC_KEY_PATH="$(env_value_or_default "${existing_env_file}" "AUTH_JWT_PUBLIC_KEY_PATH" "")"
  AUTH_ACCESS_TOKEN_TTL="$(env_value_or_default "${existing_env_file}" "AUTH_ACCESS_TOKEN_TTL" "15m")"
  AUTH_REFRESH_TOKEN_TTL="$(env_value_or_default "${existing_env_file}" "AUTH_REFRESH_TOKEN_TTL" "7d")"
  ADMIN_USERNAME=""
  if [ "${INITIAL_ADMIN_PASSWORD_READY:-0}" -ne 1 ]; then
    unset ADMIN_PASSWORD
  fi
  ADMIN_BOOTSTRAP_REQUIRED=0
  if pending_admin_handoff_exists; then
    [ "${INTERACTIVE_INSTALL:-0}" -eq 1 ] \
      || fail "pending administrator password delivery requires an interactive terminal"
    [ "${existing_auth_mode}" = "local_password" ] \
      || fail "pending administrator password delivery requires AUTH_MODE=local_password"
    pending_admin_username="$(read_pending_admin_handoff_username)"
    AUTH_MODE="local_password"
    AUTH_ENABLED="true"
    ADMIN_BOOTSTRAP_REQUIRED=1
    ADMIN_USERNAME="${pending_admin_username}"
    if [ -z "${AUTH_JWT_PRIVATE_KEY_PATH}" ] && [ -z "${AUTH_JWT_PUBLIC_KEY_PATH}" ] \
      && [ -f "${INSTALL_DIR}/certs/auth/jwt-ed25519-private.pem" ] \
      && [ -f "${INSTALL_DIR}/certs/auth/jwt-ed25519-public.pem" ]; then
      AUTH_JWT_PRIVATE_KEY_PATH="${INSTALL_DIR}/certs/auth/jwt-ed25519-private.pem"
      AUTH_JWT_PUBLIC_KEY_PATH="${INSTALL_DIR}/certs/auth/jwt-ed25519-public.pem"
    fi
    log "恢复待完成的一次性管理员密码交付: ${ADMIN_USERNAME}"
  elif [ -f "${existing_env_file}" ] && [ "${existing_auth_mode}" = "local_password" ]; then
    log "保留现有 production 认证模式和 JWT 密钥: local_password"
  elif [ -f "${existing_env_file}" ] && [ "${existing_auth_mode}" = "external_jwt" ]; then
    log "保留现有 production 认证模式和公钥: external_jwt"
  else
    [ "${INTERACTIVE_INSTALL:-0}" -eq 1 ] \
      || fail "fresh local_password install requires an interactive terminal"
    AUTH_MODE="local_password"
    AUTH_ENABLED="true"
    JWT_PUBLIC_KEY=""
    ADMIN_BOOTSTRAP_REQUIRED=1
    ADMIN_USERNAME="$(normalize_admin_username_for_handoff "$(prompt_non_empty "管理员用户名" "admin")")"
    jwt_private_target="${INSTALL_DIR}/certs/auth/jwt-ed25519-private.pem"
    jwt_public_target="${INSTALL_DIR}/certs/auth/jwt-ed25519-public.pem"
    begin_atomic_target_write "${jwt_private_target}"
    jwt_private_temp="${LAST_INSTALLER_TEMP_FILE}"
    begin_atomic_target_write "${jwt_public_target}"
    jwt_public_temp="${LAST_INSTALLER_TEMP_FILE}"
    openssl genpkey -algorithm Ed25519 -out "${jwt_private_temp}" >/dev/null 2>&1
    openssl pkey -in "${jwt_private_temp}" -pubout -out "${jwt_public_temp}" >/dev/null 2>&1
    validate_private_public_key_pair "${jwt_private_temp}" "${jwt_public_temp}" \
      || fail "generated administrator JWT key pair is invalid"
    finish_atomic_target_write \
      "${jwt_private_temp}" "${jwt_private_target}" 600 "root:${SERVICE_GROUP}"
    finish_atomic_target_write \
      "${jwt_public_temp}" "${jwt_public_target}" 640 "root:${SERVICE_GROUP}"
    AUTH_JWT_PRIVATE_KEY_PATH="${jwt_private_target}"
    AUTH_JWT_PUBLIC_KEY_PATH="${jwt_public_target}"
    write_pending_admin_handoff_marker "${ADMIN_USERNAME}"
    unset ADMIN_PASSWORD
    ADMIN_PASSWORD="$(generate_one_time_admin_password)"
  fi
}

configure_zlm_port_values() {
  local existing_env_file="$1"
  assign_local_tcp_port ZLM_HTTP_PORT "${existing_env_file}" \
    "ZLM_HTTP_PORT" "ZLM HTTP 监听端口" "80"
  assign_local_tcp_port ZLM_HTTPS_PORT "${existing_env_file}" \
    "ZLM_HTTPS_PORT" "ZLM HTTPS 监听端口（0 表示关闭）" "0" true
  assign_local_tcp_port ZLM_RTMP_PORT "${existing_env_file}" \
    "ZLM_RTMP_PORT" "ZLM RTMP 监听端口" "1935"
  assign_local_tcp_port ZLM_RTMPS_PORT "${existing_env_file}" \
    "ZLM_RTMPS_PORT" "ZLM RTMPS 监听端口（0 表示关闭）" "0" true
  assign_local_tcp_port ZLM_RTSP_PORT "${existing_env_file}" \
    "ZLM_RTSP_PORT" "ZLM RTSP 监听端口" "554"
  assign_local_tcp_port ZLM_RTSPS_PORT "${existing_env_file}" \
    "ZLM_RTSPS_PORT" "ZLM RTSPS 监听端口（0 表示关闭）" "0" true
  assign_local_tcp_port ZLM_RTP_PROXY_PORT "${existing_env_file}" \
    "ZLM_RTP_PROXY_PORT" "ZLM RTP Proxy 监听端口（0 表示关闭）" "10000" true
  ZLM_RTP_PROXY_PORT_RANGE="$(prompt_port_range "ZLM_RTP_PROXY_PORT_RANGE" "ZLM RTP Proxy 随机端口范围（start-end，0-0 表示关闭）" "$(env_value_or_default "${existing_env_file}" "ZLM_RTP_PROXY_PORT_RANGE" "30000-30500")")"
  assign_local_tcp_port ZLM_RTC_SIGNALING_PORT "${existing_env_file}" \
    "ZLM_RTC_SIGNALING_PORT" "ZLM WebRTC signaling 端口（0 表示关闭）" "8000" true
  assign_local_tcp_port ZLM_RTC_SIGNALING_SSL_PORT "${existing_env_file}" \
    "ZLM_RTC_SIGNALING_SSL_PORT" "ZLM WebRTC signaling SSL 端口（0 表示关闭）" "0" true
  assign_local_tcp_port ZLM_RTC_ICE_PORT "${existing_env_file}" \
    "ZLM_RTC_ICE_PORT" "ZLM WebRTC ICE UDP 端口（0 表示关闭）" "0" true
  assign_local_tcp_port ZLM_RTC_ICE_TCP_PORT "${existing_env_file}" \
    "ZLM_RTC_ICE_TCP_PORT" "ZLM WebRTC ICE TCP 端口（0 表示关闭）" "0" true
  assign_local_tcp_port ZLM_RTC_PORT "${existing_env_file}" \
    "ZLM_RTC_PORT" "ZLM WebRTC UDP 端口（0 表示关闭）" "0" true
  assign_local_tcp_port ZLM_RTC_TCP_PORT "${existing_env_file}" \
    "ZLM_RTC_TCP_PORT" "ZLM WebRTC TCP 端口（0 表示关闭）" "0" true
  ZLM_RTC_PORT_RANGE="$(prompt_port_range "ZLM_RTC_PORT_RANGE" "ZLM WebRTC 端口范围（start-end，0-0 表示关闭）" "$(env_value_or_default "${existing_env_file}" "ZLM_RTC_PORT_RANGE" "0-0")")"
  assign_local_tcp_port ZLM_SRT_PORT "${existing_env_file}" \
    "ZLM_SRT_PORT" "ZLM SRT 监听端口（0 表示关闭）" "0" true
  assign_local_tcp_port ZLM_SHELL_PORT "${existing_env_file}" \
    "ZLM_SHELL_PORT" "ZLM Shell 监听端口（0 表示关闭）" "0" true
  assign_local_tcp_port ZLM_ONVIF_PORT "${existing_env_file}" \
    "ZLM_ONVIF_PORT" "ZLM ONVIF 监听端口（0 表示关闭）" "0" true
}

prompt_secret_from_tty() {
  local label="$1"
  local value
  [ "${INTERACTIVE_INSTALL:-0}" -eq 1 ] \
    || fail "${label} requires an interactive terminal"
  printf '%s: ' "${label}" >/dev/tty
  IFS= read -r -s value </dev/tty
  printf '\n' >/dev/tty
  [ -n "${value}" ] || fail "${label} must not be empty"
  printf '%s' "${value}"
}

run_agent_enrollment_if_needed() {
  set +x
  local identity_dir="${AGENT_IDENTITY_DIR:-${INSTALL_DIR}/data/agent/identity}"
  local identity_parent
  local current_path="${identity_dir}/current"
  local agent_bin="${INSTALL_DIR}/bin/media-agent"
  local token=""
  local use_runuser=0
  local -a enroll_args=()
  export -n AGENT_ENROLLMENT_TOKEN 2>/dev/null || true

  role_has_worker "${INSTALL_ROLE}" || return 0
  if [ -f "${current_path}" ] && [ ! -L "${current_path}" ]; then
    unset AGENT_ENROLLMENT_TOKEN
    return 0
  fi
  case "${identity_dir}" in
    /*) ;;
    *) fail "Agent identity directory must be an absolute path" ;;
  esac
  identity_parent="$(dirname "${identity_dir}")"
  [ ! -L "${identity_parent}" ] && [ -d "${identity_parent}" ] \
    || fail "Agent identity parent must be a real directory created before enrollment"
  if [ -e "${identity_dir}" ] || [ -L "${identity_dir}" ]; then
    [ ! -L "${identity_dir}" ] && [ -d "${identity_dir}" ] \
      || fail "Agent identity path must be a real directory"
  fi
  [ ! -L "${agent_bin}" ] && [ -f "${agent_bin}" ] && [ -x "${agent_bin}" ] \
    || fail "media-agent enrollment binary is unavailable"
  is_canonical_non_nil_uuid "${NODE_ID:-}" \
    || fail "Agent enrollment requires a canonical non-nil node UUID"
  [ -n "${AGENT_ENROLLMENT_CORE_URL:-}" ] \
    && [[ "${AGENT_ENROLLMENT_CORE_URL}" == https://* ]] \
    || fail "Agent enrollment requires an HTTPS Core URL"
  [ ! -L "${AGENT_ENROLLMENT_SERVER_CA_PATH:-}" ] \
    && [ -f "${AGENT_ENROLLMENT_SERVER_CA_PATH:-}" ] \
    && [ -r "${AGENT_ENROLLMENT_SERVER_CA_PATH:-}" ] \
    || fail "Agent enrollment requires a readable Core HTTP server CA"
  if [ "$(id -u)" -eq 0 ] && [ "${EMULATED_SECURITY_METADATA:-0}" -ne 1 ]; then
    use_runuser=1
    runuser -u "${SERVICE_USER}" -- test -x "${agent_bin}" \
      || fail "media-agent enrollment binary is not executable by the service user"
    runuser -u "${SERVICE_USER}" -- test -r "${AGENT_ENROLLMENT_SERVER_CA_PATH}" \
      || fail "Core HTTP server CA is not readable by the service user"
    runuser -u "${SERVICE_USER}" -- test -d "${identity_parent}" \
      || fail "Agent identity parent is not accessible by the service user"
    runuser -u "${SERVICE_USER}" -- test -w "${identity_parent}" \
      || fail "Agent identity parent is not writable by the service user"
  else
    [ -x "${agent_bin}" ] \
      && [ -r "${AGENT_ENROLLMENT_SERVER_CA_PATH}" ] \
      && [ -w "${identity_parent}" ] \
      || fail "Agent enrollment inputs are not accessible by the service user"
  fi
  enroll_args=(
    enroll
    --node-id "${NODE_ID}"
    --core-url "${AGENT_ENROLLMENT_CORE_URL}"
    --server-ca "${AGENT_ENROLLMENT_SERVER_CA_PATH}"
    --identity-dir "${identity_dir}"
    --token-stdin
  )
  token="${AGENT_ENROLLMENT_TOKEN:-}"
  if [ -z "${token}" ] && [ "${INTERACTIVE_INSTALL:-0}" -eq 1 ]; then
    token="$(prompt_secret_from_tty "10 分钟一次性 Agent enrollment token")"
  fi
  [ -n "${token}" ] \
    || fail "worker has no enrolled identity; create a 10-minute enrollment token on the running Core and rerun the installer"
  unset AGENT_ENROLLMENT_TOKEN
  export -n token 2>/dev/null || true
  if [ "${use_runuser}" -eq 1 ]; then
    printf '%s' "${token}" | runuser -u "${SERVICE_USER}" -- env -i \
      PATH=/usr/sbin:/usr/bin:/sbin:/bin LANG=C.UTF-8 \
      "${agent_bin}" "${enroll_args[@]}"
  else
    printf '%s' "${token}" | env -i \
      PATH=/usr/sbin:/usr/bin:/sbin:/bin LANG=C.UTF-8 \
      "${agent_bin}" "${enroll_args[@]}"
  fi
  unset token AGENT_ENROLLMENT_TOKEN
  [ -f "${current_path}" ] && [ ! -L "${current_path}" ] \
    || fail "Agent enrollment did not publish a current identity"
}

role_is_all_in_one() {
  case "$1" in
    all-in-one-host-cpu|all-in-one-host-gpu) return 0 ;;
    *) return 1 ;;
  esac
}

bootstrap_all_in_one_agent_identity_if_needed() (
  set +x
  local env_file="${INSTALL_DIR}/.env"
  local core_bin="${INSTALL_DIR}/bin/media-core"
  local identity_dir
  local current_path
  local http_port
  local grpc_port
  local http_cert
  local server_ca
  local token=""
  local bootstrap_unit="${UNIT_BASENAME}-enrollment-bootstrap.service"
  local bootstrap_deadline=$((SECONDS + 60))
  local remaining
  local curl_connect_timeout
  local curl_timeout

  role_is_all_in_one "${INSTALL_ROLE}" || return 0
  identity_dir="$(security_env_value "${env_file}" AGENT_IDENTITY_DIR)"
  identity_dir="$(resolve_security_path "${env_file}" "${identity_dir}")"
  current_path="${identity_dir}/current"
  if [ -f "${current_path}" ] && [ ! -L "${current_path}" ]; then
    return 0
  fi

  cleanup_all_in_one_enrollment_bootstrap() {
    set +e
    local cleanup_deadline=$((SECONDS + 30))
    unset token AGENT_ENROLLMENT_TOKEN
    bounded_upgrade_systemctl \
      "${cleanup_deadline}" stop "${bootstrap_unit}" >/dev/null 2>&1 || true
    bounded_upgrade_systemctl \
      "${cleanup_deadline}" reset-failed "${bootstrap_unit}" \
      >/dev/null 2>&1 || true
  }
  trap cleanup_all_in_one_enrollment_bootstrap EXIT

  NODE_ID="$(security_env_value "${env_file}" AGENT_NODE_ID)"
  AGENT_IDENTITY_DIR="${identity_dir}"
  http_port="$(security_env_value "${env_file}" CORE_HTTP_PORT)"
  grpc_port="$(security_env_value "${env_file}" CORE_GRPC_PORT)"
  http_cert="$(resolve_security_path "${env_file}" \
    "$(security_env_value "${env_file}" CORE_HTTP_TLS_CERT_PATH)")"
  server_ca="$(resolve_security_path "${env_file}" \
    "$(security_env_value "${env_file}" CORE_GRPC_TLS_SERVER_CA_PATH)")"
  is_canonical_non_nil_uuid "${NODE_ID}" \
    || fail "all-in-one enrollment requires a canonical non-nil Agent node ID"
  [[ "${http_port}" =~ ^[0-9]+$ ]] \
    && [ "$((10#${http_port}))" -ge 1 ] \
    && [ "$((10#${http_port}))" -le 65535 ] \
    || fail "all-in-one enrollment requires a valid Core HTTP port"
  [[ "${grpc_port}" =~ ^[0-9]+$ ]] \
    && [ "$((10#${grpc_port}))" -ge 1 ] \
    && [ "$((10#${grpc_port}))" -le 65535 ] \
    || fail "all-in-one enrollment requires a valid Core gRPC port"
  [ -n "${identity_dir}" ] && [ -n "${http_cert}" ] && [ -n "${server_ca}" ] \
    || fail "all-in-one enrollment configuration is incomplete"
  validate_x509_ca_certificate_for_service "${server_ca}" \
    && validate_certificate_directly_issued_by_ca_for_service \
      "${http_cert}" "${server_ca}" \
    && validate_certificate_san_name_for_service "${http_cert}" localhost \
    || fail "all-in-one enrollment requires a localhost HTTP certificate issued by the configured server CA"

  security_preflight_env \
    "${env_file}" "${core_bin}" "${INSTALL_DIR}/bin/media-agent" core-only \
    || fail "all-in-one Core-only security preflight failed"

  if ! token="$(run_core_auth_from_installed_env \
    "${env_file}" "${core_bin}" agent create-enrollment \
    --node-id "${NODE_ID}" --token-stdout)"; then
    fail "failed to create the local all-in-one Agent enrollment token"
  fi
  [[ "${token}" =~ ^ssae1[.][A-Za-z0-9_-]{96}[.][A-Za-z0-9_-]{43}$ ]] \
    || fail "media-core returned an invalid Agent enrollment token"
  export -n token 2>/dev/null || true

  if bounded_upgrade_systemctl \
    "${bootstrap_deadline}" is-active --quiet "${bootstrap_unit}"; then
    fail "a stale all-in-one enrollment bootstrap Core is already active"
  fi
  bounded_upgrade_systemctl \
    "${bootstrap_deadline}" reset-failed "${bootstrap_unit}" \
    >/dev/null 2>&1 || true
  bounded_upgrade_command "${bootstrap_deadline}" systemd-run --quiet --collect \
    --unit="${bootstrap_unit%.service}" \
    --property=Type=simple \
    --property="User=${SERVICE_USER}" \
    --property="Group=${SERVICE_GROUP}" \
    --property="WorkingDirectory=${INSTALL_DIR}" \
    --property="EnvironmentFile=${env_file}" \
    --property=NoNewPrivileges=yes \
    --property=PrivateTmp=yes \
    --property=UMask=0077 \
    --property=KillMode=mixed \
    --property=TimeoutStopSec=30s \
    /usr/bin/env \
    STREAMSERVER_ENV=production \
    "STREAMSERVER_UI_DIR=${INSTALL_DIR}/ui" \
    "CORE_HTTP_ADDR=127.0.0.1:${http_port}" \
    "CORE_GRPC_ADDR=127.0.0.1:${grpc_port}" \
    "${core_bin}" \
    || fail "failed to start the temporary all-in-one enrollment Core"

  AGENT_ENROLLMENT_CORE_URL="https://localhost:${http_port}"
  AGENT_ENROLLMENT_SERVER_CA_PATH="${server_ca}"
  local ready=0
  while [ "${SECONDS}" -lt "${bootstrap_deadline}" ]; do
    if ! bounded_upgrade_systemctl \
      "${bootstrap_deadline}" is-active --quiet "${bootstrap_unit}"; then
      break
    fi
    remaining=$((bootstrap_deadline - SECONDS))
    [ "${remaining}" -gt 0 ] || break
    curl_timeout="${remaining}"
    [ "${curl_timeout}" -le 3 ] || curl_timeout=3
    curl_connect_timeout="${curl_timeout}"
    [ "${curl_connect_timeout}" -le 2 ] || curl_connect_timeout=2
    if curl -q --proto '=https' --fail --silent --show-error --noproxy '*' \
      --cacert "${server_ca}" --connect-timeout "${curl_connect_timeout}" \
      --max-time "${curl_timeout}" \
      "${AGENT_ENROLLMENT_CORE_URL}/health/ready" >/dev/null 2>&1; then
      ready=1
      break
    fi
    [ "${SECONDS}" -lt "${bootstrap_deadline}" ] && sleep 1
  done
  [ "${ready}" -eq 1 ] \
    || fail "temporary all-in-one enrollment Core did not become ready"

  AGENT_ENROLLMENT_TOKEN="${token}"
  export -n AGENT_ENROLLMENT_TOKEN 2>/dev/null || true
  run_agent_enrollment_if_needed
  unset token AGENT_ENROLLMENT_TOKEN
  [ -f "${current_path}" ] && [ ! -L "${current_path}" ] \
    || fail "all-in-one enrollment did not install an Agent identity"
)

configure_worker_values() {
  local existing_env_file="${INSTALL_DIR}/.env"
  local default_ip
  local existing_core_hook_secret
  local existing_zlm_api_secret
  local existing_zlm_hook_secret
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
  fi
  existing_core_hook_secret="${HOOK_SHARED_SECRET:-}"
  [ -n "${existing_core_hook_secret}" ] \
    || existing_core_hook_secret="$(env_value_or_default "${existing_env_file}" \
      "HOOK_SHARED_SECRET" "")"
  if ! role_has_core "${INSTALL_ROLE}"; then
    unset HOOK_SHARED_SECRET
  fi
  existing_zlm_api_secret="$(env_value_or_default "${existing_env_file}" \
    "ZLM_API_SECRET" "")"
  existing_zlm_hook_secret="$(env_value_or_default "${existing_env_file}" \
    "ZLM_HOOK_SHARED_SECRET" "")"
  if is_strong_url_safe_secret "${existing_zlm_api_secret}" \
    && [ "${existing_zlm_api_secret}" != "${existing_core_hook_secret}" ] \
    && [ "${existing_zlm_api_secret}" != "${existing_zlm_hook_secret}" ]; then
    ZLM_API_SECRET="${existing_zlm_api_secret}"
  else
    ZLM_API_SECRET="$(generate_distinct_secret \
      "${existing_core_hook_secret}" "${existing_zlm_hook_secret}")"
  fi
  if is_strong_url_safe_secret "${existing_zlm_hook_secret}" \
    && [ "${existing_zlm_hook_secret}" != "${existing_core_hook_secret}" ] \
    && [ "${existing_zlm_hook_secret}" != "${ZLM_API_SECRET}" ]; then
    ZLM_HOOK_SHARED_SECRET="${existing_zlm_hook_secret}"
  else
    ZLM_HOOK_SHARED_SECRET="$(generate_distinct_secret \
      "${existing_core_hook_secret}" "${ZLM_API_SECRET}")"
  fi
  [ "${ZLM_API_SECRET}" != "${ZLM_HOOK_SHARED_SECRET}" ] \
    || fail "ZLM API and Agent hook credentials must be independent"
  assign_local_tcp_port AGENT_HTTP_PORT "${existing_env_file}" \
    "AGENT_HTTP_PORT" "工作节点本地接口端口" "8081"
  assign_local_tcp_port AGENT_MANAGEMENT_PORT "${existing_env_file}" "AGENT_MANAGEMENT_PORT" \
    "工作节点管理接口端口" "8443"
  assign_local_tcp_port AGENT_ZLM_HOOK_PORT "${existing_env_file}" "AGENT_ZLM_HOOK_PORT" \
    "Agent ZLMediaKit hook loopback listener port" "18082"
  configure_zlm_port_values "${existing_env_file}"
  if ! role_has_core "${INSTALL_ROLE}"; then
    CORE_HTTP_SCHEME="https"
  fi
  AGENT_IDENTITY_DIR="$(env_value_or_default "${existing_env_file}" "AGENT_IDENTITY_DIR" \
    "${INSTALL_DIR}/data/agent/identity")"
  default_agent_tls_domain="${CORE_GRPC_HOST}"
  if role_has_core "${INSTALL_ROLE}"; then
    default_agent_tls_domain="${CORE_GRPC_TLS_DOMAIN_NAME}"
  fi
  AGENT_TLS_DOMAIN_NAME="$(prompt_non_empty "Core gRPC TLS 域名或 IP" \
    "$(env_value_or_default "${existing_env_file}" "AGENT_TLS_DOMAIN_NAME" \
      "${default_agent_tls_domain}")")"
  validate_internal_pki_host "${AGENT_TLS_DOMAIN_NAME}" \
    || fail "Core gRPC TLS 域名或 IP 格式无效"
  if [ ! -f "${AGENT_IDENTITY_DIR}/current" ]; then
    if role_has_core "${INSTALL_ROLE}"; then
      AGENT_ENROLLMENT_CORE_URL="https://localhost:${CORE_HTTP_PORT:-8080}"
      AGENT_ENROLLMENT_SERVER_CA_PATH="${CORE_GRPC_TLS_SERVER_CA_PATH}"
    else
      AGENT_ENROLLMENT_CORE_URL="$(prompt_non_empty "Agent enrollment Core HTTPS URL" \
        "$(env_value_or_default "${existing_env_file}" "AGENT_ENROLLMENT_CORE_URL" \
          "https://${CORE_HTTP_HOST}:${CORE_HTTP_PORT:-8080}")")"
      AGENT_ENROLLMENT_SERVER_CA_PATH="$(prompt_non_empty "Core HTTP server CA 路径" \
        "$(env_value_or_default "${existing_env_file}" "AGENT_ENROLLMENT_SERVER_CA_PATH" "")")"
    fi
  else
    AGENT_ENROLLMENT_CORE_URL=""
    AGENT_ENROLLMENT_SERVER_CA_PATH=""
    unset AGENT_ENROLLMENT_TOKEN
  fi
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
  local final_env_file="${INSTALL_DIR}/.env"
  local env_file
  local ffmpeg_variant="cpu"
  begin_atomic_target_write "${final_env_file}"
  env_file="${LAST_INSTALLER_TEMP_FILE}"
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
    write_env_entry "${env_file}" CORE_HTTP_PUBLIC_HOST "${CORE_HTTP_PUBLIC_HOST}"
    write_env_entry "${env_file}" CORE_HTTP_TLS_CERT_PATH "${CORE_HTTP_TLS_CERT_PATH}"
    write_env_entry "${env_file}" CORE_HTTP_TLS_KEY_PATH "${CORE_HTTP_TLS_KEY_PATH}"
    write_env_entry "${env_file}" CORE_GRPC_ADDR "${CORE_GRPC_ADDR}"
    write_env_entry "${env_file}" CORE_GRPC_PORT "${CORE_GRPC_PORT}"
    write_env_entry "${env_file}" CORE_GRPC_TLS_DOMAIN_NAME "${CORE_GRPC_TLS_DOMAIN_NAME}"
    write_env_entry "${env_file}" CORE_GRPC_TLS_CERT_PATH "${CORE_GRPC_TLS_CERT_PATH}"
    write_env_entry "${env_file}" CORE_GRPC_TLS_KEY_PATH "${CORE_GRPC_TLS_KEY_PATH}"
    write_env_entry "${env_file}" CORE_GRPC_TLS_CLIENT_CA_PATH "${CORE_GRPC_TLS_CLIENT_CA_PATH}"
    write_env_entry "${env_file}" CORE_GRPC_TLS_SERVER_CA_PATH "${CORE_GRPC_TLS_SERVER_CA_PATH}"
    write_env_entry "${env_file}" CORE_AGENT_CA_CERT_PATH "${CORE_AGENT_CA_CERT_PATH}"
    write_env_entry "${env_file}" CORE_AGENT_CA_KEY_PATH "${CORE_AGENT_CA_KEY_PATH}"
    write_env_entry "${env_file}" CORE_AGENT_CAPABILITY_JWT_PRIVATE_KEY_PATH \
      "${CORE_AGENT_CAPABILITY_JWT_PRIVATE_KEY_PATH}"
    write_env_entry "${env_file}" CORE_AGENT_CAPABILITY_JWT_PUBLIC_KEY_PATH \
      "${CORE_AGENT_CAPABILITY_JWT_PUBLIC_KEY_PATH}"
    write_env_entry "${env_file}" CORE_AGENT_CAPABILITY_TTL_SEC 60
    write_env_entry "${env_file}" CORE_INSTANCE_ID "${CORE_INSTANCE_ID}"
    write_env_entry "${env_file}" CORE_AGENT_MANAGEMENT_CLIENT_CERT_PATH \
      "${CORE_AGENT_MANAGEMENT_CLIENT_CERT_PATH}"
    write_env_entry "${env_file}" CORE_AGENT_MANAGEMENT_CLIENT_KEY_PATH \
      "${CORE_AGENT_MANAGEMENT_CLIENT_KEY_PATH}"
    write_env_entry "${env_file}" CORE_AGENT_MANAGEMENT_CA_PATH \
      "${CORE_AGENT_MANAGEMENT_CA_PATH}"
    write_env_entry "${env_file}" STREAMSERVER_UI_DIR "${INSTALL_DIR}/ui"
    write_env_entry "${env_file}" HOOK_SHARED_SECRET "${HOOK_SHARED_SECRET}"
    write_env_entry "${env_file}" HOOK_SOURCE_ALLOWLIST "${HOOK_SOURCE_ALLOWLIST}"
    write_env_entry "${env_file}" STORAGE_ALLOWLIST "${STORAGE_ALLOWLIST}"
    write_env_entry "${env_file}" SOURCE_GATEWAY_BASE_URL "${SOURCE_GATEWAY_BASE_URL}"
    write_env_entry "${env_file}" SOURCE_GATEWAY_TLS_INSECURE_SKIP_VERIFY \
      "${SOURCE_GATEWAY_TLS_INSECURE_SKIP_VERIFY}"
    write_env_entry "${env_file}" SOURCE_GATEWAY_PREFETCH_POLL_MS \
      "${SOURCE_GATEWAY_PREFETCH_POLL_MS}"
    write_env_entry "${env_file}" SOURCE_GATEWAY_PREFETCH_TIMEOUT_MS \
      "${SOURCE_GATEWAY_PREFETCH_TIMEOUT_MS}"
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
    write_env_entry "${env_file}" CORE_GRPC_HOST "${CORE_GRPC_HOST}"
    if ! role_has_core "${INSTALL_ROLE}"; then
      write_env_entry "${env_file}" CORE_HTTP_PORT "${CORE_HTTP_PORT:-8080}"
      write_env_entry "${env_file}" CORE_GRPC_PORT "${CORE_GRPC_PORT:-50051}"
    fi
    write_env_entry "${env_file}" CORE_HTTP_SCHEME "${CORE_HTTP_SCHEME}"
    write_env_entry "${env_file}" AGENT_CORE_ENDPOINT "https://${CORE_GRPC_HOST}:${CORE_GRPC_PORT:-50051}"
    write_env_entry "${env_file}" AGENT_TLS_DOMAIN_NAME "${AGENT_TLS_DOMAIN_NAME}"
    write_env_entry "${env_file}" PUBLIC_HOST "${PUBLIC_HOST}"
    write_env_entry "${env_file}" AGENT_STREAM_ADDR "http://${PUBLIC_HOST}:${ZLM_HTTP_PORT}"
    write_env_entry "${env_file}" AGENT_PUBLIC_MEDIA_ADDR "127.0.0.1:${AGENT_HTTP_PORT}"
    write_env_entry "${env_file}" AGENT_PUBLIC_MEDIA_EXPOSE false
    write_env_entry "${env_file}" AGENT_MANAGEMENT_ADDR "0.0.0.0:${AGENT_MANAGEMENT_PORT}"
    write_env_entry "${env_file}" AGENT_MANAGEMENT_PORT "${AGENT_MANAGEMENT_PORT}"
    write_env_entry "${env_file}" AGENT_MANAGEMENT_MAX_CONCURRENCY 4
    write_env_entry "${env_file}" AGENT_MANAGEMENT_CHUNK_IDLE_TIMEOUT_SEC 30
    write_env_entry "${env_file}" AGENT_ZLM_HOOK_ADDR "127.0.0.1:${AGENT_ZLM_HOOK_PORT}"
    write_env_entry "${env_file}" AGENT_ZLM_HOOK_PORT "${AGENT_ZLM_HOOK_PORT}"
    write_env_entry "${env_file}" AGENT_ZLM_HOOK_QUEUE_CAPACITY 64
    write_env_entry "${env_file}" AGENT_ZLM_HOOK_TIMEOUT_SEC 4
    write_env_entry "${env_file}" AGENT_IDENTITY_DIR "${AGENT_IDENTITY_DIR}"
    write_env_entry "${env_file}" AGENT_HTTP_PORT "${AGENT_HTTP_PORT}"
    write_env_entry "${env_file}" ZLM_API_BASE "http://127.0.0.1:${ZLM_HTTP_PORT}"
    write_env_entry "${env_file}" ZLM_API_SECRET "${ZLM_API_SECRET}"
    write_env_entry "${env_file}" ZLM_API_ALLOW_IP_RANGE "::1,127.0.0.1,10.0.0.0-10.255.255.255,172.16.0.0-172.31.255.255,192.168.0.0-192.168.255.255"
    write_env_entry "${env_file}" ZLM_HOOK_SHARED_SECRET "${ZLM_HOOK_SHARED_SECRET}"
    write_env_entry "${env_file}" ZLM_SERVER_ID "${NODE_ID}"
    write_env_entry "${env_file}" ZLM_HOOK_BASE "http://127.0.0.1:${AGENT_ZLM_HOOK_PORT}/internal/zlm-hooks"
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
  finish_atomic_target_write "${env_file}" "${final_env_file}" 600
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

run_streamserver_config_tui_with_handoff_guard() {
  if role_has_core "${INSTALL_ROLE}" \
    && { pending_admin_handoff_exists || delivered_admin_handoff_exists; }; then
    log "管理员密码交付进行中；本次跳过高级配置 TUI，安装成功后可单独运行配置工具并重启服务。"
    return 0
  fi
  run_streamserver_config_tui_if_requested
}

validate_identity_after_optional_tui() {
  local expected_role="$1"
  local expected_instance_name="$2"
  local expected_unit_basename="$3"
  local env_file="${INSTALL_DIR}/.env"
  local observed_role
  local observed_instance_name
  local observed_unit_basename

  require_unique_env_key "${env_file}" INSTALL_ROLE
  require_unique_env_key "${env_file}" INSTANCE_NAME
  require_unique_env_key "${env_file}" SYSTEMD_TARGET
  require_unique_env_key "${env_file}" SYSTEMD_CORE_UNIT
  require_unique_env_key "${env_file}" SYSTEMD_AGENT_UNIT
  require_unique_env_key "${env_file}" SYSTEMD_ZLM_UNIT
  require_unique_env_key "${env_file}" SYSTEMD_POSTGRES_UNIT
  observed_role="$(strict_identity_env_value "${env_file}" INSTALL_ROLE)"
  observed_instance_name="$(strict_identity_env_value "${env_file}" INSTANCE_NAME)"
  [ "${observed_role}" = "${expected_role}" ] \
    || fail "advanced configuration cannot change the installer-selected role"
  [ "${observed_instance_name}" = "${expected_instance_name}" ] \
    || fail "advanced configuration cannot rename an installed native instance"
  observed_unit_basename="$(unit_basename_for_instance "${observed_instance_name}")"
  [ "${observed_unit_basename}" = "${expected_unit_basename}" ] \
    || fail "advanced configuration changed the native unit identity"
  require_upgrade_systemd_identity \
    "${env_file}" SYSTEMD_TARGET "${expected_unit_basename}.target"
  require_upgrade_systemd_identity \
    "${env_file}" SYSTEMD_CORE_UNIT "${expected_unit_basename}-core.service"
  require_upgrade_systemd_identity \
    "${env_file}" SYSTEMD_AGENT_UNIT "${expected_unit_basename}-agent.service"
  require_upgrade_systemd_identity \
    "${env_file}" SYSTEMD_ZLM_UNIT "${expected_unit_basename}-zlm.service"
  require_upgrade_systemd_identity \
    "${env_file}" SYSTEMD_POSTGRES_UNIT "${expected_unit_basename}-postgres.service"
  INSTALL_ROLE="${expected_role}"
  INSTANCE_NAME="${expected_instance_name}"
  UNIT_BASENAME="${expected_unit_basename}"
}

render_template() {
  local source="$1"
  local target="$2"
  local temporary_file
  local ffmpeg_variant="cpu"
  local postgres_unit="" postgres_requires="" core_unit="" core_requires="" zlm_unit=""
  local postgres_pkglib_dir=""
  local gpu_nvidia_pre="" gpu_h264_pre="" gpu_hevc_pre=""
  local admin_handoff_condition=""
  role_is_gpu "${INSTALL_ROLE}" && ffmpeg_variant="gpu"
  if role_has_core "${INSTALL_ROLE}"; then
    core_unit="${UNIT_BASENAME}-core.service"
    admin_handoff_condition="ConditionPathExists=!$(pending_admin_handoff_path)"
  fi
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
  begin_atomic_target_write "${target}"
  temporary_file="${LAST_INSTALLER_TEMP_FILE}"
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
    -e "s|__ADMIN_HANDOFF_CONDITION__|$(sed_escape "${admin_handoff_condition}")|g" \
    -e "s|__ZLM_UNIT__|$(sed_escape "${zlm_unit}")|g" \
    -e "s|__FFMPEG_VARIANT__|$(sed_escape "${ffmpeg_variant}")|g" \
    -e "s|__GPU_NVIDIA_SMI_PRE__|$(sed_escape "${gpu_nvidia_pre}")|g" \
    -e "s|__GPU_H264_PRE__|$(sed_escape "${gpu_h264_pre}")|g" \
    -e "s|__GPU_HEVC_PRE__|$(sed_escape "${gpu_hevc_pre}")|g" \
    "${source}" >"${temporary_file}"
  finish_atomic_target_write "${temporary_file}" "${target}" 644
}

install_systemd_units() {
  local units=()
  local deadline=$((SECONDS + 60))
  local rendered_dir="${INSTALL_DIR}/systemd"
  ensure_control_directory "${rendered_dir}"
  render_template "${PACKAGE_ROOT}/templates/systemd/streamserver.target" "${rendered_dir}/${UNIT_BASENAME}.target"
  copy_file_atomically "${rendered_dir}/${UNIT_BASENAME}.target" \
    "${SYSTEMD_UNIT_ROOT}/${UNIT_BASENAME}.target" 644
  if role_has_core "${INSTALL_ROLE}" && [ "${DATABASE_MODE}" = "bundled" ]; then
    render_template "${PACKAGE_ROOT}/templates/systemd/streamserver-postgres.service" "${rendered_dir}/${UNIT_BASENAME}-postgres.service"
    copy_file_atomically "${rendered_dir}/${UNIT_BASENAME}-postgres.service" \
      "${SYSTEMD_UNIT_ROOT}/${UNIT_BASENAME}-postgres.service" 644
    units+=("${UNIT_BASENAME}-postgres.service")
  fi
  if role_has_core "${INSTALL_ROLE}"; then
    render_template "${PACKAGE_ROOT}/templates/systemd/streamserver-core.service" "${rendered_dir}/${UNIT_BASENAME}-core.service"
    copy_file_atomically "${rendered_dir}/${UNIT_BASENAME}-core.service" \
      "${SYSTEMD_UNIT_ROOT}/${UNIT_BASENAME}-core.service" 644
    units+=("${UNIT_BASENAME}-core.service")
  fi
  if role_has_worker "${INSTALL_ROLE}"; then
    render_template "${PACKAGE_ROOT}/templates/systemd/streamserver-zlm.service" "${rendered_dir}/${UNIT_BASENAME}-zlm.service"
    render_template "${PACKAGE_ROOT}/templates/systemd/streamserver-agent.service" "${rendered_dir}/${UNIT_BASENAME}-agent.service"
    copy_file_atomically "${rendered_dir}/${UNIT_BASENAME}-zlm.service" \
      "${SYSTEMD_UNIT_ROOT}/${UNIT_BASENAME}-zlm.service" 644
    copy_file_atomically "${rendered_dir}/${UNIT_BASENAME}-agent.service" \
      "${SYSTEMD_UNIT_ROOT}/${UNIT_BASENAME}-agent.service" 644
    units+=("${UNIT_BASENAME}-zlm.service" "${UNIT_BASENAME}-agent.service")
  fi
  bounded_upgrade_systemctl "${deadline}" daemon-reload
  if [ "${UPGRADE}" -eq 1 ]; then
    restore_upgrade_unit_enablement "${deadline}" \
      || fail "failed to restore captured native unit enablement after upgrade"
  else
    bounded_upgrade_systemctl "${deadline}" \
      enable "${UNIT_BASENAME}.target" "${units[@]}" >/dev/null
  fi
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
  local temporary_pwfile
  assert_managed_data_paths_safe
  assert_postgres_password_file_safe
  ensure_managed_data_directory "${data_dir}"
  chown -R -h "${SERVICE_USER}:${SERVICE_GROUP}" "${data_dir}" "${INSTALL_DIR}/data/postgres-run"
  if [ ! -f "${data_dir}/PG_VERSION" ]; then
    begin_atomic_target_write "${pwfile}"
    temporary_pwfile="${LAST_INSTALLER_TEMP_FILE}"
    printf '%s\n' "${POSTGRES_PASSWORD}" >"${temporary_pwfile}"
    finish_atomic_target_write "${temporary_pwfile}" "${pwfile}" 600 \
      "${SERVICE_USER}:${SERVICE_GROUP}"
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

run_native_service_command() {
  local -a clean_env=(
    env -i
    "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
    "HOME=${INSTALL_DIR}/data"
  )
  if [ "$(id -u)" -eq 0 ] \
    && [ "${EMULATED_SECURITY_METADATA:-0}" -ne 1 ]; then
    runuser -u "${SERVICE_USER}" -- "${clean_env[@]}" "$@"
  else
    "$@"
  fi
}

bounded_native_service_command() {
  local deadline="$1"
  shift
  local -a clean_env=(
    env -i
    "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
    "HOME=${INSTALL_DIR}/data"
  )
  if [ "$(id -u)" -eq 0 ] \
    && [ "${EMULATED_SECURITY_METADATA:-0}" -ne 1 ]; then
    bounded_upgrade_command "${deadline}" \
      runuser -u "${SERVICE_USER}" -- "${clean_env[@]}" "$@"
  else
    bounded_upgrade_command "${deadline}" "$@"
  fi
}

wait_for_postgres() {
  [ "${DATABASE_MODE}" = "bundled" ] || return 0
  local deadline=$((SECONDS + 60))
  while [ "${SECONDS}" -lt "${deadline}" ]; do
    if bounded_native_service_command "${deadline}" \
      "${INSTALL_DIR}/bin/pg_isready" \
      -h 127.0.0.1 -p "${POSTGRES_PORT}" -U "${POSTGRES_USER}" \
      >/dev/null 2>&1; then
      return 0
    fi
    [ "${SECONDS}" -lt "${deadline}" ] || break
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

load_core_auth_environment() {
  local env_file="$1"
  local auth_mode
  local database_url
  local jwt_private
  local jwt_public
  local jwt_external
  local access_ttl
  local refresh_ttl
  local security_key
  local security_value

  assert_security_env_keys_unique "${env_file}" \
    || fail "auth probe environment contains duplicate security keys"
  auth_mode="$(security_env_value "${env_file}" AUTH_MODE)"
  database_url="$(security_env_value "${env_file}" DATABASE_URL)"
  jwt_private="$(security_env_value "${env_file}" AUTH_JWT_PRIVATE_KEY_PATH)"
  jwt_public="$(security_env_value "${env_file}" AUTH_JWT_PUBLIC_KEY_PATH)"
  jwt_external="$(security_env_value "${env_file}" JWT_PUBLIC_KEY)"
  access_ttl="$(security_env_value "${env_file}" AUTH_ACCESS_TOKEN_TTL)"
  refresh_ttl="$(security_env_value "${env_file}" AUTH_REFRESH_TOKEN_TTL)"
  [ -n "${database_url}" ] \
    || fail "auth probe requires DATABASE_URL"

  CORE_AUTH_ENV=(
    "STREAMSERVER_ENV=production"
    "DATABASE_URL=${database_url}"
    "AUTH_MODE=${auth_mode}"
  )
  for security_key in \
    CORE_HTTP_ADDR CORE_HTTP_PUBLIC_HOST \
    CORE_HTTP_TLS_CERT_PATH CORE_HTTP_TLS_KEY_PATH \
    CORE_GRPC_ADDR CORE_GRPC_TLS_DOMAIN_NAME \
    CORE_GRPC_TLS_CERT_PATH CORE_GRPC_TLS_KEY_PATH \
    CORE_GRPC_TLS_CLIENT_CA_PATH CORE_GRPC_TLS_SERVER_CA_PATH \
    CORE_AGENT_CA_CERT_PATH CORE_AGENT_CA_KEY_PATH \
    CORE_AGENT_CAPABILITY_JWT_PRIVATE_KEY_PATH \
    CORE_AGENT_CAPABILITY_JWT_PUBLIC_KEY_PATH CORE_AGENT_CAPABILITY_TTL_SEC \
    CORE_INSTANCE_ID CORE_AGENT_MANAGEMENT_CLIENT_CERT_PATH \
    CORE_AGENT_MANAGEMENT_CLIENT_KEY_PATH CORE_AGENT_MANAGEMENT_CA_PATH; do
    security_value="$(security_env_value "${env_file}" "${security_key}")"
    [ -n "${security_value}" ] || continue
    case "${security_key}" in
      *_PATH) security_value="$(resolve_security_path "${env_file}" "${security_value}")" ;;
    esac
    CORE_AUTH_ENV+=("${security_key}=${security_value}")
  done
  case "${auth_mode}" in
    local_password)
      jwt_private="$(resolve_security_path "${env_file}" "${jwt_private}")"
      jwt_public="$(resolve_security_path "${env_file}" "${jwt_public}")"
      [ -n "${jwt_private}" ] && [ -n "${jwt_public}" ] \
        || fail "local auth probe requires JWT key paths"
      CORE_AUTH_ENV+=(
        "AUTH_JWT_PRIVATE_KEY_PATH=${jwt_private}"
        "AUTH_JWT_PUBLIC_KEY_PATH=${jwt_public}"
      )
      ;;
    external_jwt)
      [ -n "${jwt_external}" ] \
        || fail "external auth probe requires JWT_PUBLIC_KEY"
      CORE_AUTH_ENV+=("JWT_PUBLIC_KEY=${jwt_external}")
      ;;
    *) fail "auth probe requires a supported AUTH_MODE" ;;
  esac
  [ -z "${access_ttl}" ] || CORE_AUTH_ENV+=("AUTH_ACCESS_TOKEN_TTL=${access_ttl}")
  [ -z "${refresh_ttl}" ] || CORE_AUTH_ENV+=("AUTH_REFRESH_TOKEN_TTL=${refresh_ttl}")
}

run_core_auth_from_installed_env() {
  local env_file="$1"
  local core_bin="$2"
  shift 2
  load_core_auth_environment "${env_file}"
  (
    unset ADMIN_PASSWORD
    if [ "$(id -u)" -eq 0 ] && [ "${EMULATED_SECURITY_METADATA:-0}" -ne 1 ]; then
      runuser -u "${SERVICE_USER}" -- env -i \
        PATH=/usr/sbin:/usr/bin:/sbin:/bin LANG=C.UTF-8 \
        "${CORE_AUTH_ENV[@]}" "${core_bin}" "$@"
    else
      env -i PATH=/usr/sbin:/usr/bin:/sbin:/bin LANG=C.UTF-8 \
        "${CORE_AUTH_ENV[@]}" "${core_bin}" "$@"
    fi
  )
}

run_core_auth_with_password_from_installed_env() {
  local env_file="$1"
  local core_bin="$2"
  local bootstrap_password="$3"
  export -n bootstrap_password 2>/dev/null || true
  shift 3
  load_core_auth_environment "${env_file}"
  printf '%s' "${bootstrap_password}" | (
    unset ADMIN_PASSWORD
    if [ "$(id -u)" -eq 0 ] && [ "${EMULATED_SECURITY_METADATA:-0}" -ne 1 ]; then
      runuser -u "${SERVICE_USER}" -- env -i \
        PATH=/usr/sbin:/usr/bin:/sbin:/bin LANG=C.UTF-8 \
        "${CORE_AUTH_ENV[@]}" "${core_bin}" "$@"
    else
      env -i PATH=/usr/sbin:/usr/bin:/sbin:/bin LANG=C.UTF-8 \
        "${CORE_AUTH_ENV[@]}" "${core_bin}" "$@"
    fi
  )
}

prepare_pending_admin_password_handoff() {
  local env_file="$1"
  local core_bin="$2"
  local expected_version
  local handoff_id
  local state
  if delivered_admin_handoff_exists; then
    ADMIN_USERNAME="$(read_admin_handoff_username "$(delivered_admin_handoff_path)")"
    ADMIN_HANDOFF_DELIVERED_READY=1
    log "检测到已交付的一次性管理员密码，继续完成安装: ${ADMIN_USERNAME}"
    return 0
  fi
  pending_admin_handoff_exists || return 0
  [ "${INTERACTIVE_INSTALL:-0}" -eq 1 ] \
    || fail "pending administrator password delivery requires an interactive terminal"
  ADMIN_USERNAME="$(read_pending_admin_handoff_username)"
  handoff_id="$(read_admin_handoff_id "$(pending_admin_handoff_path)")"
  ADMIN_BOOTSTRAP_REQUIRED=1
  if [ "${INITIAL_ADMIN_PASSWORD_READY:-0}" -eq 1 ] && [ -n "${ADMIN_PASSWORD:-}" ]; then
    return 0
  fi
  state="$(run_core_auth_from_installed_env \
    "${env_file}" "${core_bin}" auth bootstrap-status --username "${ADMIN_USERNAME}" \
    --handoff-id "${handoff_id}")" \
    || fail "无法读取待交付管理员状态"
  case "${state}" in
    complete)
      log "管理员已完成改密，不重置长期密码: ${ADMIN_USERNAME}"
      acknowledge_admin_handoff_delivery
      ADMIN_HANDOFF_DELIVERED_READY=1
      cleanup_admin_password
      return 0
      ;;
    conflict)
      fail "待交付管理员与现有账号状态冲突，拒绝自动重置"
      ;;
    missing:*)
      expected_version="${state#missing:}"
      [ "${expected_version}" = "0" ] \
        || fail "media-core returned an invalid missing handoff version"
      ;;
    pending-password-change:*)
      expected_version="${state#pending-password-change:}"
      [[ "${expected_version}" =~ ^[1-9][0-9]*$ ]] \
        || fail "media-core returned an invalid pending handoff version"
      ;;
    *) fail "media-core 返回了未知的管理员交付状态" ;;
  esac

  if [ -n "${ADMIN_PASSWORD:-}" ]; then
    :
  else
    unset ADMIN_PASSWORD
    ADMIN_PASSWORD="$(generate_one_time_admin_password)"
  fi
  run_core_auth_with_password_from_installed_env \
    "${env_file}" "${core_bin}" "${ADMIN_PASSWORD}" \
    auth recover-bootstrap-admin --username "${ADMIN_USERNAME}" \
    --handoff-id "${handoff_id}" --expected-version "${expected_version}" --password-stdin \
    >/dev/null \
    || fail "待交付管理员原子创建或恢复失败；未覆盖已完成改密的账号"
  INITIAL_ADMIN_PASSWORD_READY=1
}

emit_initial_admin_credentials() {
  local username="$1"
  local password="$2"
  printf '\n首次登录管理员: %s\n一次性初始密码: %s\n请登录后立即修改；此密码不会保存，也不会再次显示。\n\n' \
    "${username}" "${password}" >/dev/tty
}

show_initial_admin_credentials_if_needed() {
  local emit_status=0
  if [ "${INTERACTIVE_INSTALL:-0}" -eq 1 ] \
    && [ "${ADMIN_BOOTSTRAP_REQUIRED:-0}" -eq 1 ] \
    && [ "${INITIAL_ADMIN_PASSWORD_READY:-0}" -eq 1 ] \
    && [ -n "${ADMIN_PASSWORD:-}" ]; then
    pending_admin_handoff_exists \
      || fail "管理员密码可交付但缺少 durable pending 标记"
    emit_initial_admin_credentials "${ADMIN_USERNAME}" "${ADMIN_PASSWORD}" || emit_status=$?
    if [ "${emit_status}" -eq 0 ]; then
      acknowledge_admin_handoff_delivery
      ADMIN_HANDOFF_DELIVERED_READY=1
    fi
  fi
  cleanup_admin_password
  [ "${emit_status}" -eq 0 ] || fail "无法向交互终端显示一次性管理员初始密码"
}

finalize_admin_handoff_after_install_success() {
  if [ "${ADMIN_HANDOFF_DELIVERED_READY:-0}" -eq 1 ]; then
    clear_delivered_admin_handoff_marker
    ADMIN_HANDOFF_DELIVERED_READY=0
  fi
}

write_streamserverctl() {
  local ctl="${INSTALL_DIR}/bin/streamserverctl"
  local temporary_file
  begin_atomic_target_write "${ctl}"
  temporary_file="${LAST_INSTALLER_TEMP_FILE}"
  cat >"${temporary_file}" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
INSTALL_DIR="$(cd "$(dirname "$0")/.." && pwd)"
ENV_FILE="${INSTALL_DIR}/.env"

config_fail() {
  printf '[FAILED] invalid streamserverctl configuration: %s\n' "$*" >&2
  exit 1
}

read_config_scalar() {
  local key="$1"
  local raw
  local value
  [ -f "${ENV_FILE}" ] || config_fail "missing ${ENV_FILE}"
  raw="$(awk -v key="${key}" '
    index($0, key "=") == 1 {
      count += 1
      value = substr($0, length(key) + 2)
    }
    END {
      if (count != 1) exit 1
      print value
    }
  ' "${ENV_FILE}")" || config_fail "${key} must appear exactly once"
  [ "${#raw}" -ge 2 ] \
    && [ "${raw:0:1}" = "'" ] \
    && [ "${raw: -1}" = "'" ] \
    || config_fail "${key} must use the installer single-quoted scalar format"
  value="${raw:1:${#raw}-2}"
  [[ "${value}" != *"'"* ]] \
    || config_fail "${key} contains unsupported quote syntax"
  printf '%s' "${value}"
}

read_config_port() {
  local key="$1"
  local value
  value="$(read_config_scalar "${key}")"
  [[ "${value}" =~ ^[0-9]+$ ]] \
    || config_fail "${key} must be a decimal TCP port"
  [ "$((10#${value}))" -ge 1 ] && [ "$((10#${value}))" -le 65535 ] \
    || config_fail "${key} is outside the TCP port range"
  printf '%s' "${value}"
}

require_config_value() {
  local key="$1"
  local expected="$2"
  local actual
  actual="$(read_config_scalar "${key}")"
  [ "${actual}" = "${expected}" ] \
    || config_fail "${key} does not match the persisted instance identity"
}

load_streamserverctl_config() {
  local unit_basename
  INSTALL_ROLE="$(read_config_scalar INSTALL_ROLE)"
  INSTANCE_NAME="$(read_config_scalar INSTANCE_NAME)"
  case "${INSTALL_ROLE}" in
    control-plane|worker-host-cpu|worker-host-gpu|all-in-one-host-cpu|all-in-one-host-gpu) ;;
    *) config_fail "unsupported INSTALL_ROLE" ;;
  esac
  case "${INSTANCE_NAME}" in
    ''|-*|*[!A-Za-z0-9_.@-]*) config_fail "invalid INSTANCE_NAME" ;;
  esac
  case "${INSTANCE_NAME}" in
    ss-*) unit_basename="${INSTANCE_NAME}" ;;
    *) unit_basename="ss-${INSTANCE_NAME}" ;;
  esac
  require_config_value SYSTEMD_TARGET "${unit_basename}.target"
  require_config_value SYSTEMD_CORE_UNIT "${unit_basename}-core.service"
  require_config_value SYSTEMD_AGENT_UNIT "${unit_basename}-agent.service"
  require_config_value SYSTEMD_ZLM_UNIT "${unit_basename}-zlm.service"
  require_config_value SYSTEMD_POSTGRES_UNIT "${unit_basename}-postgres.service"
  SYSTEMD_TARGET="${unit_basename}.target"
  SYSTEMD_CORE_UNIT="${unit_basename}-core.service"
  SYSTEMD_AGENT_UNIT="${unit_basename}-agent.service"
  SYSTEMD_ZLM_UNIT="${unit_basename}-zlm.service"
  SYSTEMD_POSTGRES_UNIT="${unit_basename}-postgres.service"

  CORE_HTTP_PORT=""
  CORE_HTTP_TLS_CERT_PATH=""
  AGENT_HTTP_PORT=""
  ZLM_HTTP_PORT=""
  HOOK_SHARED_SECRET=""
  ZLM_API_SECRET=""
  case "${INSTALL_ROLE}" in
    control-plane|all-in-one-host-cpu|all-in-one-host-gpu)
      HOOK_SHARED_SECRET="$(read_config_scalar HOOK_SHARED_SECRET)"
      [[ "${HOOK_SHARED_SECRET}" =~ ^[A-Za-z0-9._~-]+$ ]] \
        && [ "${#HOOK_SHARED_SECRET}" -ge 16 ] \
        && [ "${#HOOK_SHARED_SECRET}" -le 256 ] \
        || config_fail "HOOK_SHARED_SECRET is not a safe Core hook token"
      ;;
  esac
  case "${INSTALL_ROLE}" in
    worker-host-cpu|worker-host-gpu|all-in-one-host-cpu|all-in-one-host-gpu)
      ZLM_API_SECRET="$(read_config_scalar ZLM_API_SECRET)"
      [[ "${ZLM_API_SECRET}" =~ ^[A-Za-z0-9._~-]+$ ]] \
        && [ "${#ZLM_API_SECRET}" -ge 32 ] \
        && [ "${#ZLM_API_SECRET}" -le 256 ] \
        || config_fail "ZLM_API_SECRET is not a safe local API token"
      ;;
  esac
  case "${INSTALL_ROLE}" in
    control-plane)
      CORE_HTTP_PORT="$(read_config_port CORE_HTTP_PORT)"
      CORE_HTTP_TLS_CERT_PATH="$(read_config_scalar CORE_HTTP_TLS_CERT_PATH)"
      ;;
    worker-host-cpu|worker-host-gpu)
      AGENT_HTTP_PORT="$(read_config_port AGENT_HTTP_PORT)"
      ZLM_HTTP_PORT="$(read_config_port ZLM_HTTP_PORT)"
      ;;
    all-in-one-host-cpu|all-in-one-host-gpu)
      CORE_HTTP_PORT="$(read_config_port CORE_HTTP_PORT)"
      CORE_HTTP_TLS_CERT_PATH="$(read_config_scalar CORE_HTTP_TLS_CERT_PATH)"
      AGENT_HTTP_PORT="$(read_config_port AGENT_HTTP_PORT)"
      ZLM_HTTP_PORT="$(read_config_port ZLM_HTTP_PORT)"
      ;;
  esac
}

load_streamserverctl_config

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
  local curl_local_args=(
    -q --fail --silent --show-error --noproxy '*'
    --connect-timeout 2 --max-time 4 --proto '=http,https'
  )
  local expect_core=0
  local expect_agent=0
  local expect_zlm=0
  local expected=0
  local failures=0

  zlm_api_health() {
    local response compact_response code_occurrences
    response="$(printf \
      'url = "http://127.0.0.1:%s/index/api/getStatistic?secret=%s"\n' \
      "${ZLM_HTTP_PORT}" "${ZLM_API_SECRET}" \
      | curl "${curl_local_args[@]}" --max-filesize 4096 \
        --config - 2>/dev/null)" || return 1
    [ "${#response}" -le 4096 ] || return 1
    compact_response="$(printf '%s' "${response}" | tr -d '[:space:]')"
    [[ "${compact_response}" == \{*\} ]] || return 1
    code_occurrences="$(printf '%s' "${compact_response}" \
      | grep -o '"code":' | wc -l | tr -d '[:space:]')"
    [ "${code_occurrences}" = 1 ] \
      && [[ "${compact_response}" =~ \"code\":0([,}]) ]]
  }

  case "${INSTALL_ROLE}" in
    control-plane) expect_core=1 ;;
    worker-host-cpu|worker-host-gpu) expect_agent=1; expect_zlm=1 ;;
    all-in-one-host-cpu|all-in-one-host-gpu) expect_core=1; expect_agent=1; expect_zlm=1 ;;
  esac
  if [ -n "${CORE_HTTP_TLS_CERT_PATH:-}" ]; then
    core_scheme="https"
    # The configured server certificate can be CA-signed; there is no separate HTTP CA setting.
    # This probe is loopback-only and verifies readiness, while clients still validate normally.
    core_curl_tls=(-k)
  fi
  if [ "${expect_core}" -eq 1 ]; then
    expected=$((expected + 1))
    if [ -z "${CORE_HTTP_PORT:-}" ] || [ -z "${SYSTEMD_CORE_UNIT:-}" ]; then
      echo "[FAILED] media-core health configuration is incomplete" >&2
      failures=$((failures + 1))
    elif ! systemctl list-unit-files "${SYSTEMD_CORE_UNIT}" >/dev/null 2>&1; then
      echo "[FAILED] media-core systemd unit is missing" >&2
      failures=$((failures + 1))
    elif curl "${curl_local_args[@]}" "${core_curl_tls[@]}" \
      "${core_scheme}://127.0.0.1:${CORE_HTTP_PORT}/health/ready" >/dev/null; then
      echo "[OK] media-core"
    else
      echo "[FAILED] media-core readiness probe failed" >&2
      failures=$((failures + 1))
    fi
  fi
  if [ "${expect_agent}" -eq 1 ]; then
    expected=$((expected + 1))
    if [ -z "${AGENT_HTTP_PORT:-}" ] || [ -z "${SYSTEMD_AGENT_UNIT:-}" ]; then
      echo "[FAILED] media-agent health configuration is incomplete" >&2
      failures=$((failures + 1))
    elif ! systemctl list-unit-files "${SYSTEMD_AGENT_UNIT}" >/dev/null 2>&1; then
      echo "[FAILED] media-agent systemd unit is missing" >&2
      failures=$((failures + 1))
    elif curl "${curl_local_args[@]}" \
      "http://127.0.0.1:${AGENT_HTTP_PORT}/health/ready" >/dev/null; then
      echo "[OK] media-agent"
    else
      echo "[FAILED] media-agent readiness probe failed" >&2
      failures=$((failures + 1))
    fi
  fi
  if [ "${expect_zlm}" -eq 1 ]; then
    expected=$((expected + 1))
    if [ -z "${ZLM_HTTP_PORT:-}" ] || [ -z "${SYSTEMD_ZLM_UNIT:-}" ]; then
      echo "[FAILED] zlmediakit health configuration is incomplete" >&2
      failures=$((failures + 1))
    elif ! systemctl list-unit-files "${SYSTEMD_ZLM_UNIT}" >/dev/null 2>&1; then
      echo "[FAILED] zlmediakit systemd unit is missing" >&2
      failures=$((failures + 1))
    elif zlm_api_health; then
      echo "[OK] zlmediakit"
    else
      echo "[FAILED] zlmediakit readiness probe failed" >&2
      failures=$((failures + 1))
    fi
  fi
  if [ "${expected}" -eq 0 ]; then
    echo "[FAILED] no health probes are defined for INSTALL_ROLE=${INSTALL_ROLE}" >&2
    return 1
  fi
  [ "${failures}" -eq 0 ]
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
  finish_atomic_target_write "${temporary_file}" "${ctl}" 755
}

assert_control_path_not_symlink() {
  local path="$1"
  [ ! -L "${path}" ] \
    || fail "native installation control path must not be a symbolic link: ${path}"
}

control_tree_directory_chain_is_safe_status() {
  local root="$1"
  local directory="$2"
  local ownership_policy="${3:-strict}"
  local current
  local mode
  current="${directory}"
  while true; do
    case "${current}" in
      "${root}"|"${root}"/*) ;;
      *) return 1 ;;
    esac
    [ -d "${current}" ] && [ ! -L "${current}" ] || return 1
    mode="$(stat -c '%a' -- "${current}" 2>/dev/null)" || return 1
    (( (8#${mode} & 8#022) == 0 )) || return 1
    if [ "${ownership_policy}" = strict ] \
      && [ "$(id -u)" -eq 0 ] \
      && [ "${EMULATED_SECURITY_METADATA:-0}" -ne 1 ]; then
      [ "$(stat -c '%u' -- "${current}" 2>/dev/null)" = 0 ] || return 1
    fi
    [ "${current}" = "${root}" ] && break
    current="$(dirname "${current}")"
  done
}

control_tree_symlink_is_confined_status() {
  local root="$1"
  local link_path="$2"
  local ownership_policy="${3:-strict}"
  local root_normalized
  local root_resolved
  local link_normalized
  local link_parent
  local link_target=""
  local target_lexical
  local mode
  root_normalized="$(realpath -ms -- "${root}" 2>/dev/null)" || return 1
  root_resolved="$(realpath -e -- "${root}" 2>/dev/null)" || return 1
  [ "${root_normalized}" = "${root_resolved}" ] || return 1
  link_normalized="$(realpath -ms -- "${link_path}" 2>/dev/null)" || return 1
  case "${link_normalized}" in
    "${root_resolved}"/*) ;;
    *) return 1 ;;
  esac
  [ -L "${link_path}" ] || return 1
  IFS= read -r -d '' link_target < <(readlink -z -- "${link_path}" 2>/dev/null) \
    || return 1
  [ -n "${link_target}" ] \
    && [[ "${link_target}" != /* ]] \
    && [[ "${link_target}" != *$'\n'* ]] \
    && [[ "${link_target}" != *$'\r'* ]] || return 1
  link_parent="$(dirname "${link_normalized}")"
  control_tree_directory_chain_is_safe_status \
    "${root_resolved}" "${link_parent}" "${ownership_policy}" \
    || return 1
  target_lexical="$(realpath -ms -- "${link_parent}/${link_target}" 2>/dev/null)" \
    || return 1
  case "${target_lexical}" in
    "${root_resolved}"/*) ;;
    *) return 1 ;;
  esac
  # Native package links deliberately use a strict one-hop policy.  Rejecting
  # every symbolic component in the target path avoids silently trusting an
  # absolute, service-owned, or mutable intermediate link.
  admin_handoff_no_symlink_boundary_status "${target_lexical}" || return 1
  control_tree_directory_chain_is_safe_status \
    "${root_resolved}" "$(dirname "${target_lexical}")" \
    "${ownership_policy}" || return 1
  [ ! -L "${target_lexical}" ] && [ -f "${target_lexical}" ] || return 1
  [ "$(stat -c '%h' -- "${target_lexical}" 2>/dev/null)" = 1 ] || return 1
  mode="$(stat -c '%a' -- "${target_lexical}" 2>/dev/null)" || return 1
  (( (8#${mode} & 8#022) == 0 )) || return 1
  if [ "${ownership_policy}" = strict ] \
    && [ "$(id -u)" -eq 0 ] \
    && [ "${EMULATED_SECURITY_METADATA:-0}" -ne 1 ]; then
    [ "$(stat -c '%u' -- "${link_path}" 2>/dev/null)" = 0 ] \
      && [ "$(stat -c '%u' -- "${target_lexical}" 2>/dev/null)" = 0 ] || return 1
  fi
}

assert_control_tree_safe() {
  local root="$1"
  local ownership_policy="${2:-strict}"
  local inventory
  local tmp_root
  local entry_type
  local entry_path
  case "${ownership_policy}" in strict|structural) ;; *)
    fail "native control tree ownership policy is invalid" ;;
  esac
  [ -e "${root}" ] || return 0
  [ ! -L "${root}" ] && [ -d "${root}" ] \
    || fail "native control tree root must be a real directory: ${root}"
  admin_handoff_no_symlink_boundary_status "${root}" \
    || fail "native control tree root path contains a symbolic link: ${root}"
  tmp_root="$(secure_installer_tmp_root)"
  inventory="$(mktemp "${tmp_root%/}/streamserver-control-tree.XXXXXX")" \
    || fail "cannot create native control tree inventory"
  chmod 600 "${inventory}"
  if ! find -P "${root}" -mindepth 1 -printf '%y\0%p\0' >"${inventory}"; then
    rm -f -- "${inventory}" >/dev/null 2>&1 || true
    fail "cannot enumerate native control tree: ${root}"
  fi
  while IFS= read -r -d '' entry_type; do
    if ! IFS= read -r -d '' entry_path; then
      rm -f -- "${inventory}" >/dev/null 2>&1 || true
      fail "native control tree inventory is truncated: ${root}"
    fi
    case "${entry_type}" in
      d) ;;
      f)
        if [ "$(stat -c '%h' -- "${entry_path}" 2>/dev/null)" != 1 ]; then
          rm -f -- "${inventory}" >/dev/null 2>&1 || true
          fail "native control tree contains a multiply-linked regular file: ${entry_path}"
        fi
        ;;
      l)
        if ! control_tree_symlink_is_confined_status \
          "${root}" "${entry_path}" "${ownership_policy}"; then
          rm -f -- "${inventory}" >/dev/null 2>&1 || true
          fail "native control tree contains an unsafe symbolic link: ${entry_path}"
        fi
        ;;
      *)
        rm -f -- "${inventory}" >/dev/null 2>&1 || true
        fail "native control tree contains an unsupported special entry: ${entry_path}"
        ;;
    esac
  done <"${inventory}"
  rm -f -- "${inventory}" \
    || fail "cannot remove native control tree inventory"
}

certificate_tree_has_unsafe_entries_status() {
  find "$1" -mindepth 1 ! -type d ! -type f -print -quit | grep -q .
}

assert_certificate_tree_safe() {
  local cert_root="${INSTALL_DIR}/certs"
  [ -e "${cert_root}" ] || return 0
  assert_control_path_not_symlink "${cert_root}"
  if certificate_tree_has_unsafe_entries_status "${cert_root}"; then
    fail "native certificate tree must contain only regular files and directories"
  fi
}

is_root_only_internal_ca_key() {
  case "$1" in
    "${INSTALL_DIR}/certs/internal/control-plane-server-ca-key.pem"|\
    "${INSTALL_DIR}/certs/internal/management-client-ca-key.pem") return 0 ;;
    *) return 1 ;;
  esac
}

seal_certificate_tree() {
  local cert_root="${INSTALL_DIR}/certs"
  local path
  local temporary_file
  local -a cert_dirs=()
  local -a cert_files=()
  [ -d "${cert_root}" ] || return 0
  assert_certificate_tree_safe
  mapfile -d '' -t cert_dirs < <(find "${cert_root}" -type d -print0)
  mapfile -d '' -t cert_files < <(find "${cert_root}" -type f -print0)
  for path in "${cert_dirs[@]}"; do
    chown -h root:"${SERVICE_GROUP}" "${path}"
    chmod 750 "${path}"
  done
  for path in "${cert_files[@]}"; do
    begin_atomic_target_write "${path}"
    temporary_file="${LAST_INSTALLER_TEMP_FILE}"
    cp -- "${path}" "${temporary_file}"
    if is_root_only_internal_ca_key "${path}"; then
      finish_atomic_target_write "${temporary_file}" "${path}" 600 root:root
    else
      finish_atomic_target_write "${temporary_file}" "${path}" 640 \
        "root:${SERVICE_GROUP}"
    fi
  done
}

seal_upgrade_control_file_inode() {
  local path="$1"
  local mode
  local temporary_file
  [ ! -L "${path}" ] && [ -f "${path}" ] \
    || fail "upgrade control file must be a regular non-symbolic file: ${path}"
  mode="$(stat -c '%a' -- "${path}")" \
    || fail "cannot inspect upgrade control file mode: ${path}"
  if (( (8#${mode} & 8#111) != 0 )); then
    mode=755
  else
    mode=644
  fi
  begin_atomic_target_write "${path}"
  temporary_file="${LAST_INSTALLER_TEMP_FILE}"
  cp -- "${path}" "${temporary_file}"
  finish_atomic_target_write "${temporary_file}" "${path}" "${mode}" root:root
}

seal_upgrade_control_directory_files() {
  local directory="$1"
  local path
  [ -e "${directory}" ] || return 0
  [ ! -L "${directory}" ] && [ -d "${directory}" ] \
    || fail "upgrade control directory is unsafe: ${directory}"
  for path in "${directory}"/*; do
    [ -e "${path}" ] || [ -L "${path}" ] || continue
    [ ! -L "${path}" ] && [ -f "${path}" ] \
      || fail "upgrade control directory contains a non-regular entry: ${path}"
    seal_upgrade_control_file_inode "${path}"
  done
}

harden_install_root_before_copy() {
  local item
  if [ "${UPGRADE}" -eq 1 ]; then
    case "${UPGRADE_TRANSACTION_STATE}" in
      presealed|armed) ;;
      *) fail "native upgrade hardening requires a durable rollback barrier" ;;
    esac
  fi
  assert_control_path_not_symlink "${INSTALL_DIR}"
  if [ ! -e "${INSTALL_DIR}" ]; then
    install -d -o root -g root -m 0755 -- "${INSTALL_DIR}"
  fi
  [ -d "${INSTALL_DIR}" ] \
    || fail "native installation path must be a directory"
  chown root:root "${INSTALL_DIR}"
  chmod 755 "${INSTALL_DIR}"

  for item in bin runtime ui zlm docs certs systemd .installer-backups; do
    [ -e "${INSTALL_DIR}/${item}" ] || continue
    assert_control_tree_safe "${INSTALL_DIR}/${item}" structural
    chown -R -h root:root "${INSTALL_DIR}/${item}"
    chmod -R go-w "${INSTALL_DIR}/${item}"
    assert_control_tree_safe "${INSTALL_DIR}/${item}"
  done
  for item in uninstall.sh .env; do
    [ -e "${INSTALL_DIR}/${item}" ] || continue
    assert_control_path_not_symlink "${INSTALL_DIR}/${item}"
    chown root:root "${INSTALL_DIR}/${item}"
    chmod go-w "${INSTALL_DIR}/${item}"
  done
  # Permission changes do not revoke a legacy service's already-open writable
  # descriptor. Publish fresh root-owned inodes for every executable control
  # entry that may later be invoked by root or copied into systemd.
  seal_upgrade_control_directory_files "${INSTALL_DIR}/bin"
  seal_upgrade_control_directory_files "${INSTALL_DIR}/systemd"
  if [ -f "${INSTALL_DIR}/zlm/render-config.sh" ]; then
    seal_upgrade_control_file_inode "${INSTALL_DIR}/zlm/render-config.sh"
  fi
  if [ -f "${INSTALL_DIR}/zlm/config.ini.template" ]; then
    seal_upgrade_control_file_inode "${INSTALL_DIR}/zlm/config.ini.template"
  fi
  if [ -f "${INSTALL_DIR}/uninstall.sh" ]; then
    seal_upgrade_control_file_inode "${INSTALL_DIR}/uninstall.sh"
  fi
  seal_certificate_tree
}

fix_permissions() {
  local item
  assert_managed_data_paths_safe
  assert_postgres_password_file_safe
  assert_control_path_not_symlink "${INSTALL_DIR}"
  chown root:root "${INSTALL_DIR}"
  chmod 755 "${INSTALL_DIR}"
  for item in bin runtime ui zlm docs systemd .installer-backups; do
    [ -e "${INSTALL_DIR}/${item}" ] || continue
    assert_control_tree_safe "${INSTALL_DIR}/${item}" structural
    chown -R -h root:root "${INSTALL_DIR}/${item}"
    chmod -R go-w "${INSTALL_DIR}/${item}"
    assert_control_tree_safe "${INSTALL_DIR}/${item}"
  done
  if [ -e "${INSTALL_DIR}/uninstall.sh" ]; then
    assert_control_path_not_symlink "${INSTALL_DIR}/uninstall.sh"
    chown root:root "${INSTALL_DIR}/uninstall.sh"
    chmod 755 "${INSTALL_DIR}/uninstall.sh"
  fi
  seal_certificate_tree
  if [ -e "${INSTALL_DIR}/.env" ]; then
    assert_control_path_not_symlink "${INSTALL_DIR}/.env"
    chown root:"${SERVICE_GROUP}" "${INSTALL_DIR}/.env"
    chmod 640 "${INSTALL_DIR}/.env"
  fi
  for item in data data/agent data/media data/media/work data/media/logs data/postgres data/postgres-run data/zlm data/zlm/www data/zlm/www/record data/zlm/www/snap; do
    [ -e "${INSTALL_DIR}/${item}" ] && chown -h "${SERVICE_USER}:${SERVICE_GROUP}" "${INSTALL_DIR}/${item}"
  done
  if [ -d "${INSTALL_DIR}/data/agent" ]; then
    chmod 700 "${INSTALL_DIR}/data/agent"
  fi
  fix_output_permissions
}

non_database_units_for_role() {
  case "${INSTALL_ROLE}" in
    control-plane)
      printf '%s\n' "${UNIT_BASENAME}-core.service"
      ;;
    worker-host-cpu|worker-host-gpu)
      printf '%s\n%s\n' \
        "${UNIT_BASENAME}-zlm.service" \
        "${UNIT_BASENAME}-agent.service"
      ;;
    all-in-one-host-cpu|all-in-one-host-gpu)
      printf '%s\n%s\n%s\n' \
        "${UNIT_BASENAME}-core.service" \
        "${UNIT_BASENAME}-zlm.service" \
        "${UNIT_BASENAME}-agent.service"
      ;;
    *) fail "cannot determine native services for unsupported role ${INSTALL_ROLE}" ;;
  esac
}

upgrade_units_for_role() {
  non_database_units_for_role
  if role_has_core "${INSTALL_ROLE}" \
    && [ "${TRUSTED_POSTGRES_UNIT_COUNT:-0}" -eq 1 ]; then
    printf '%s\n' "${UNIT_BASENAME}-postgres.service"
  fi
}

upgrade_rollback_units() {
  upgrade_units_for_role
  printf '%s\n' "${UNIT_BASENAME}.target"
}

upgrade_unit_was_active() {
  local expected="$1"
  local unit
  for unit in "${UPGRADE_ACTIVE_UNITS[@]}"; do
    [ "${unit}" = "${expected}" ] && return 0
  done
  return 1
}

upgrade_active_main_pid_for_unit() {
  local expected="$1"
  local index
  for index in "${!UPGRADE_ACTIVE_UNITS[@]}"; do
    if [ "${UPGRADE_ACTIVE_UNITS[${index}]}" = "${expected}" ]; then
      printf '%s' "${UPGRADE_ACTIVE_MAIN_PIDS[${index}]}"
      return 0
    fi
  done
  return 1
}

persist_upgrade_service_state() {
  local state_dir="${UPGRADE_TRANSACTION_DIR}/service-state"
  local unit
  local state
  local pid
  [ "${UPGRADE_SERVICE_STATE_CAPTURED:-0}" -eq 1 ] || return 1
  admin_handoff_assert_secure_directory "${state_dir}" || return 1
  if [ "${UPGRADE_TARGET_WAS_ACTIVE}" -eq 1 ]; then
    state=active
  else
    state=inactive
  fi
  (umask 077 && printf 'STATE=%s\nPID=0\n' "${state}" \
    >"${state_dir}/target.state") || return 1
  chmod 600 "${state_dir}/target.state" || return 1
  while IFS= read -r unit; do
    if upgrade_unit_was_active "${unit}"; then
      state=active
      pid="$(upgrade_active_main_pid_for_unit "${unit}")" || return 1
    else
      state=inactive
      pid=0
    fi
    (umask 077 && printf 'STATE=%s\nPID=%s\n' "${state}" "${pid}" \
      >"${state_dir}/${unit}.state") || return 1
    chmod 600 "${state_dir}/${unit}.state" || return 1
  done < <(upgrade_units_for_role)
}

load_persisted_upgrade_service_state() {
  local state_dir="${UPGRADE_TRANSACTION_DIR}/service-state"
  local unit
  local state_file
  local state
  local pid
  local target_state
  local expected_files=1
  local observed_files
  admin_handoff_assert_secure_directory "${state_dir}" || return 1
  state_file="${state_dir}/target.state"
  admin_handoff_assert_secure_file "${state_file}" 600 || return 1
  [ "$(wc -l <"${state_file}" | tr -d '[:space:]')" = 2 ] || return 1
  target_state="$(upgrade_install_root_metadata_value "${state_file}" STATE)" \
    || return 1
  [ "$(upgrade_install_root_metadata_value "${state_file}" PID)" = 0 ] \
    || return 1
  case "${target_state}" in
    active) UPGRADE_TARGET_WAS_ACTIVE=1 ;;
    inactive) UPGRADE_TARGET_WAS_ACTIVE=0 ;;
    *) return 1 ;;
  esac
  UPGRADE_ACTIVE_UNITS=()
  UPGRADE_ACTIVE_MAIN_PIDS=()
  while IFS= read -r unit; do
    expected_files=$((expected_files + 1))
    state_file="${state_dir}/${unit}.state"
    admin_handoff_assert_secure_file "${state_file}" 600 || return 1
    [ "$(wc -l <"${state_file}" | tr -d '[:space:]')" = 2 ] || return 1
    state="$(upgrade_install_root_metadata_value "${state_file}" STATE)" \
      && pid="$(upgrade_install_root_metadata_value "${state_file}" PID)" \
      || return 1
    case "${state}" in
      active)
        [[ "${pid}" =~ ^[1-9][0-9]*$ ]] || return 1
        UPGRADE_ACTIVE_UNITS+=("${unit}")
        UPGRADE_ACTIVE_MAIN_PIDS+=("${pid}")
        ;;
      inactive) [ "${pid}" = 0 ] || return 1 ;;
      *) return 1 ;;
    esac
  done < <(upgrade_units_for_role)
  observed_files="$(find -P "${state_dir}" -mindepth 1 -maxdepth 1 \
    -printf '.' | wc -c | tr -d '[:space:]')" \
    || return 1
  [ "${observed_files}" = "${expected_files}" ] || return 1
  UPGRADE_SERVICE_STATE_CAPTURED=1
}

verify_captured_upgrade_service_state_unchanged() {
  local deadline=$((SECONDS + 60))
  local target_unit="${UNIT_BASENAME}.target"
  local unit
  local observed_state
  local observed_pid
  local expected_pid
  local -a desired_units=()
  [ "${UPGRADE_SERVICE_STATE_CAPTURED:-0}" -eq 1 ] || return 1
  mapfile -t desired_units < <(upgrade_units_for_role)
  wait_for_upgrade_units_steady "${deadline}" "${desired_units[@]}" "${target_unit}"
  observed_state="$(bounded_upgrade_systemctl "${deadline}" \
    show --property ActiveState --value "${target_unit}")" || return 1
  if [ "${UPGRADE_TARGET_WAS_ACTIVE}" -eq 1 ]; then
    [ "${observed_state}" = active ] || return 1
  else
    [ "${observed_state}" = inactive ] || return 1
  fi
  for unit in "${desired_units[@]}"; do
    observed_state="$(bounded_upgrade_systemctl "${deadline}" \
      show --property ActiveState --value "${unit}")" || return 1
    if upgrade_unit_was_active "${unit}"; then
      [ "${observed_state}" = active ] || return 1
      expected_pid="$(upgrade_active_main_pid_for_unit "${unit}")" || return 1
      observed_pid="$(bounded_upgrade_systemctl "${deadline}" \
        show --property MainPID --value "${unit}")" || return 1
      [ "${observed_pid}" = "${expected_pid}" ] || return 1
    else
      [ "${observed_state}" = inactive ] || return 1
    fi
  done
}

restore_captured_upgrade_service_state() {
  local deadline="${1:-$((SECONDS + 60))}"
  local unit
  local observed_state
  local restore_failed=0
  local target_unit="${UNIT_BASENAME}.target"
  local -a desired_units=()
  local -a inactive_units=()
  [ "${UPGRADE_RESTORE_ON_FAILURE:-0}" -eq 1 ] \
    || return 0
  mapfile -t desired_units < <(upgrade_units_for_role)
  for unit in "${desired_units[@]}"; do
    if ! upgrade_unit_was_active "${unit}"; then
      inactive_units+=("${unit}")
    fi
  done

  # Restore component state before the aggregate target. Starting a regular
  # target first would pull every Wanted unit, briefly executing components
  # that were intentionally inactive at the transaction baseline.
  if [ "${#UPGRADE_ACTIVE_UNITS[@]}" -gt 0 ]; then
    bounded_upgrade_systemctl "${deadline}" start "${UPGRADE_ACTIVE_UNITS[@]}" || restore_failed=1
  fi
  if [ "${#inactive_units[@]}" -gt 0 ]; then
    bounded_upgrade_systemctl "${deadline}" stop "${inactive_units[@]}" || restore_failed=1
  fi
  if [ "${UPGRADE_TARGET_WAS_ACTIVE}" -eq 1 ]; then
    bounded_upgrade_systemctl "${deadline}" start \
      --job-mode=ignore-dependencies "${target_unit}" || restore_failed=1
  else
    bounded_upgrade_systemctl "${deadline}" stop "${target_unit}" || restore_failed=1
  fi
  observed_state="$(bounded_upgrade_systemctl \
    "${deadline}" show --property ActiveState --value "${target_unit}")" \
    || restore_failed=1
  if [ "${UPGRADE_TARGET_WAS_ACTIVE}" -eq 1 ]; then
    [ "${observed_state}" = active ] || restore_failed=1
  else
    [ "${observed_state}" = inactive ] || restore_failed=1
  fi
  for unit in "${desired_units[@]}"; do
    observed_state="$(bounded_upgrade_systemctl \
      "${deadline}" show --property ActiveState --value "${unit}")" \
      || restore_failed=1
    if upgrade_unit_was_active "${unit}"; then
      [ "${observed_state}" = active ] || restore_failed=1
    else
      [ "${observed_state}" = inactive ] || restore_failed=1
    fi
  done
  [ "${restore_failed}" -eq 0 ]
}

verify_restored_upgrade_readiness() {
  local deadline="${1:-$((SECONDS + 60))}"
  probe_upgrade_active_components_readiness 1 "${deadline}"
}

upgrade_restore_target_is_allowed() {
  local target="$1"
  local state_dir
  state_dir="$(admin_handoff_state_dir 2>/dev/null)" || return 1
  case "${target}" in
    "${INSTALL_DIR}/.env"|\
    "${INSTALL_DIR}/bin"|\
    "${INSTALL_DIR}/ui"|\
    "${INSTALL_DIR}/runtime"|\
    "${INSTALL_DIR}/zlm"|\
    "${INSTALL_DIR}/docs"|\
    "${INSTALL_DIR}/certs"|\
    "${INSTALL_DIR}/systemd"|\
    "${INSTALL_DIR}/uninstall.sh"|\
    "${INSTALL_DIR}/.installer-backups"|\
    "${SYSTEMD_UNIT_ROOT}/${UNIT_BASENAME}.target"|\
    "${SYSTEMD_UNIT_ROOT}/${UNIT_BASENAME}-postgres.service"|\
    "${SYSTEMD_UNIT_ROOT}/${UNIT_BASENAME}-core.service"|\
    "${SYSTEMD_UNIT_ROOT}/${UNIT_BASENAME}-zlm.service"|\
    "${SYSTEMD_UNIT_ROOT}/${UNIT_BASENAME}-agent.service"|\
    "${state_dir}/admin-handoff.pending"|\
    "${state_dir}/${ADMIN_HANDOFF_DELIVERED_NAME}") return 0 ;;
    *) return 1 ;;
  esac
}

remove_upgrade_restore_target() {
  local target="$1"
  upgrade_restore_target_is_allowed "${target}" || {
    printf '[streamserver-native-install] ERROR: refused unsafe upgrade rollback target\n' >&2
    return 1
  }
  if [ -L "${target}" ] || [ -f "${target}" ]; then
    rm -f -- "${target}"
    return $?
  fi
  if [ -d "${target}" ]; then
    if ! (assert_control_tree_safe "${target}" structural) >/dev/null 2>&1; then
      printf '[streamserver-native-install] ERROR: refused unsafe upgrade rollback tree\n' >&2
      return 1
    fi
    rm -rf -- "${target}"
    return $?
  fi
  if [ -e "${target}" ]; then
    printf '[streamserver-native-install] ERROR: refused special file during upgrade rollback\n' >&2
    return 1
  fi
}

validate_upgrade_transaction_entry_snapshot() {
  local snapshot="$1"
  local state_file="$2"
  local state
  [ ! -L "${state_file}" ] && [ -f "${state_file}" ] || return 1
  [ "$(wc -l <"${state_file}" | tr -d '[:space:]')" = 1 ] || return 1
  state="$(<"${state_file}")"
  case "${state}" in
    absent)
      [ ! -e "${snapshot}" ] && [ ! -L "${snapshot}" ]
      ;;
    file)
      [ ! -L "${snapshot}" ] && [ -f "${snapshot}" ] \
        && upgrade_entry_fingerprint "${snapshot}" >/dev/null
      ;;
    directory)
      [ ! -L "${snapshot}" ] && [ -d "${snapshot}" ] \
        && (assert_control_tree_safe "${snapshot}" structural) >/dev/null 2>&1 \
        && upgrade_entry_fingerprint "${snapshot}" >/dev/null
      ;;
    *) return 1 ;;
  esac
}

validate_upgrade_install_root_metadata_state() {
  local state_file="${UPGRADE_TRANSACTION_DIR}/install-root.state"
  local uid
  local gid
  local mode
  local mtime
  local device
  local inode
  [ ! -L "${state_file}" ] && [ -f "${state_file}" ] \
    && [ "$(stat -c '%a' -- "${state_file}")" = 600 ] \
    && [ "$(wc -l <"${state_file}" | tr -d '[:space:]')" = 6 ] || return 1
  uid="$(upgrade_install_root_metadata_value "${state_file}" UID)" \
    && gid="$(upgrade_install_root_metadata_value "${state_file}" GID)" \
    && mode="$(upgrade_install_root_metadata_value "${state_file}" MODE)" \
    && mtime="$(upgrade_install_root_metadata_value "${state_file}" MTIME)" \
    && device="$(upgrade_install_root_metadata_value "${state_file}" DEVICE)" \
    && inode="$(upgrade_install_root_metadata_value "${state_file}" INODE)" \
    || return 1
  [[ "${uid}" =~ ^[0-9]+$ ]] \
    && [[ "${gid}" =~ ^[0-9]+$ ]] \
    && [[ "${mode}" =~ ^[0-7]{3,4}$ ]] \
    && [ -n "${mtime}" ] \
    && [[ "${mtime}" != *$'\n'* ]] \
    && [[ "${mtime}" != *$'\r'* ]] \
    && [[ "${device}" =~ ^[0-9]+$ ]] \
    && [[ "${inode}" =~ ^[0-9]+$ ]] || return 1
  [ ! -L "${INSTALL_DIR}" ] && [ -d "${INSTALL_DIR}" ] \
    && [ "$(stat -c '%d:%i' -- "${INSTALL_DIR}")" = "${device}:${inode}" ]
}

load_upgrade_recovery_topology_from_snapshot() {
  local kind
  local state_file
  local observed
  local expected
  local postgres_present=0
  [ "$(read_upgrade_transaction_snapshot_kind)" = full ] || return 1
  for kind in core agent zlm postgres; do
    state_file="${UPGRADE_TRANSACTION_DIR}/unit-state/${UNIT_BASENAME}-${kind}.service.state"
    [ ! -L "${state_file}" ] && [ -f "${state_file}" ] \
      && [ "$(stat -c '%a:%h' -- "${state_file}")" = 600:1 ] \
      && [ "$(wc -l <"${state_file}" | tr -d '[:space:]')" = 1 ] \
      || return 1
    observed="$(<"${state_file}")"
    case "${kind}" in
      core)
        if role_has_core "${INSTALL_ROLE}"; then expected=file; else expected=absent; fi
        ;;
      agent|zlm)
        if role_has_worker "${INSTALL_ROLE}"; then expected=file; else expected=absent; fi
        ;;
      postgres)
        case "${observed}" in
          file)
            role_has_core "${INSTALL_ROLE}" || return 1
            postgres_present=1
            ;;
          absent) ;;
          *) return 1 ;;
        esac
        continue
        ;;
    esac
    [ "${observed}" = "${expected}" ] || return 1
  done
  TRUSTED_UNIT_BASENAME="${UNIT_BASENAME}"
  if role_has_core "${INSTALL_ROLE}"; then TRUSTED_CORE_UNIT_COUNT=1; else TRUSTED_CORE_UNIT_COUNT=0; fi
  if role_has_worker "${INSTALL_ROLE}"; then
    TRUSTED_AGENT_UNIT_COUNT=1
    TRUSTED_ZLM_UNIT_COUNT=1
  else
    TRUSTED_AGENT_UNIT_COUNT=0
    TRUSTED_ZLM_UNIT_COUNT=0
  fi
  TRUSTED_POSTGRES_UNIT_COUNT="${postgres_present}"
  if [ "${postgres_present}" -eq 1 ]; then
    DATABASE_MODE=bundled
  else
    DATABASE_MODE=external
  fi
  validate_role_against_trusted_units "${INSTALL_ROLE}"
}

validate_upgrade_transaction_snapshot_for_restore() {
  local item
  local unit
  local marker
  local state_dir
  local enablement_file
  local enablement
  (assert_control_tree_safe "${UPGRADE_TRANSACTION_DIR}" structural) >/dev/null 2>&1 \
    || return 1
  load_upgrade_recovery_topology_from_snapshot || return 1
  validate_upgrade_install_root_metadata_state || return 1
  while IFS= read -r item; do
    validate_upgrade_transaction_entry_snapshot \
      "${UPGRADE_TRANSACTION_DIR}/install/${item}" \
      "${UPGRADE_TRANSACTION_DIR}/install-state/${item}.state" || return 1
  done < <(upgrade_transaction_install_items)
  while IFS= read -r unit; do
    validate_upgrade_transaction_entry_snapshot \
      "${UPGRADE_TRANSACTION_DIR}/units/${unit}" \
      "${UPGRADE_TRANSACTION_DIR}/unit-state/${unit}.state" || return 1
    enablement_file="${UPGRADE_TRANSACTION_DIR}/enablement/${unit}.state"
    [ ! -L "${enablement_file}" ] && [ -f "${enablement_file}" ] \
      && [ "$(wc -l <"${enablement_file}" | tr -d '[:space:]')" = 1 ] \
      || return 1
    enablement="$(<"${enablement_file}")"
    case "${enablement}" in
      enabled|enabled-runtime|disabled|masked|masked-runtime|static|indirect|generated|transient|linked|linked-runtime|alias|not-found) ;;
      *) return 1 ;;
    esac
  done < <(upgrade_transaction_unit_names)
  state_dir="$(admin_handoff_state_dir)" || return 1
  for marker in admin-handoff.pending "${ADMIN_HANDOFF_DELIVERED_NAME}"; do
    validate_upgrade_transaction_entry_snapshot \
      "${UPGRADE_TRANSACTION_DIR}/handoff/${marker}" \
      "${UPGRADE_TRANSACTION_DIR}/handoff-state/${marker}.state" || return 1
  done
  if [ -e "${UPGRADE_TRANSACTION_DIR}/service-state/target.state" ] \
    || [ -L "${UPGRADE_TRANSACTION_DIR}/service-state/target.state" ]; then
    load_persisted_upgrade_service_state || return 1
  fi
  [ "$(read_upgrade_transaction_snapshot_kind)" = full ] || return 1
  [ -n "${state_dir}" ]
}

cleanup_upgrade_restore_staging_residue() {
  local target_parent="$1"
  local residue
  [[ "${UPGRADE_TRANSACTION_ID}" =~ ^[0-9]+-[0-9]+-[0-9]+$ ]] || return 1
  for residue in \
    "${target_parent}/.streamserver-restore.${UPGRADE_TRANSACTION_ID}."*; do
    [ -e "${residue}" ] || [ -L "${residue}" ] || continue
    [ ! -L "${residue}" ] && [ -d "${residue}" ] \
      && [ "$(stat -c '%a' -- "${residue}")" = 700 ] || return 1
    if [ "$(id -u)" -eq 0 ] \
      && [ "${EMULATED_SECURITY_METADATA:-0}" -ne 1 ]; then
      [ "$(stat -c '%u' -- "${residue}")" = 0 ] || return 1
    fi
    (assert_control_tree_safe "${residue}" structural) >/dev/null 2>&1 \
      || return 1
    rm -rf -- "${residue}" || return 1
  done
}

restore_upgrade_transaction_entry() {
  local snapshot="$1"
  local state_file="$2"
  local target="$3"
  local state
  local target_parent
  local staging_dir=""
  local staging_entry=""
  local snapshot_fingerprint=""
  local restored_fingerprint=""
  validate_upgrade_transaction_entry_snapshot "${snapshot}" "${state_file}" \
    || return 1
  state="$(<"${state_file}")"
  case "${state}" in
    absent|file|directory) ;;
    *) return 1 ;;
  esac
  if [ "${state}" != absent ]; then
    target_parent="$(dirname "${target}")"
    [ ! -L "${target_parent}" ] && [ -d "${target_parent}" ] || return 1
    cleanup_upgrade_restore_staging_residue "${target_parent}" || return 1
    staging_dir="$(mktemp -d \
      "${target_parent}/.streamserver-restore.${UPGRADE_TRANSACTION_ID}.XXXXXX")" \
      || return 1
    chmod 700 "${staging_dir}" || {
      rm -rf -- "${staging_dir}" >/dev/null 2>&1 || true
      return 1
    }
    staging_entry="${staging_dir}/entry"
    snapshot_fingerprint="$(upgrade_entry_fingerprint "${snapshot}")" || {
      rm -rf -- "${staging_dir}" >/dev/null 2>&1 || true
      return 1
    }
    if ! copy_upgrade_transaction_entry "${snapshot}" "${staging_entry}"; then
      rm -rf -- "${staging_dir}" >/dev/null 2>&1 || true
      return 1
    fi
    case "${state}" in
      file)
        [ ! -L "${staging_entry}" ] && [ -f "${staging_entry}" ] || {
          rm -rf -- "${staging_dir}" >/dev/null 2>&1 || true
          return 1
        }
        ;;
      directory)
        [ ! -L "${staging_entry}" ] && [ -d "${staging_entry}" ] \
          && (assert_control_tree_safe "${staging_entry}" structural) >/dev/null 2>&1 || {
          rm -rf -- "${staging_dir}" >/dev/null 2>&1 || true
          return 1
        }
        ;;
    esac
    restored_fingerprint="$(upgrade_entry_fingerprint "${staging_entry}")" || {
      rm -rf -- "${staging_dir}" >/dev/null 2>&1 || true
      return 1
    }
    [ "${snapshot_fingerprint}" = "${restored_fingerprint}" ] || {
      rm -rf -- "${staging_dir}" >/dev/null 2>&1 || true
      return 1
    }
  fi
  if [ "${state}" = file ]; then
    upgrade_restore_target_is_allowed "${target}" || {
      rm -rf -- "${staging_dir}" >/dev/null 2>&1 || true
      return 1
    }
    [ ! -d "${target}" ] || {
      rm -rf -- "${staging_dir}" >/dev/null 2>&1 || true
      return 1
    }
    mv -fT -- "${staging_entry}" "${target}" || {
      rm -rf -- "${staging_dir}" >/dev/null 2>&1 || true
      return 1
    }
  else
    remove_upgrade_restore_target "${target}" || {
      [ -z "${staging_dir}" ] || rm -rf -- "${staging_dir}" >/dev/null 2>&1 || true
      return 1
    }
  fi
  if [ "${state}" != absent ]; then
    if [ "${state}" = directory ]; then
      mv -- "${staging_entry}" "${target}" || {
        rm -rf -- "${staging_dir}" >/dev/null 2>&1 || true
        return 1
      }
    fi
    rmdir -- "${staging_dir}" || return 1
    case "${state}" in
      file) [ ! -L "${target}" ] && [ -f "${target}" ] || return 1 ;;
      directory)
        [ ! -L "${target}" ] && [ -d "${target}" ] \
          && (assert_control_tree_safe "${target}" structural) >/dev/null 2>&1 || return 1
        ;;
    esac
    restored_fingerprint="$(upgrade_entry_fingerprint "${target}")" || return 1
    [ "${snapshot_fingerprint}" = "${restored_fingerprint}" ] || return 1
  fi
  if [ -e "${target}" ] || [ -L "${target}" ]; then
    sync -f "${target}" || return 1
  fi
  sync -f "$(dirname "${target}")"
}

restore_upgrade_install_tree() {
  local item
  local failed=0
  while IFS= read -r item; do
    validate_upgrade_transaction_entry_snapshot \
      "${UPGRADE_TRANSACTION_DIR}/install/${item}" \
      "${UPGRADE_TRANSACTION_DIR}/install-state/${item}.state" || return 1
  done < <(upgrade_transaction_install_items)
  while IFS= read -r item; do
    restore_upgrade_transaction_entry \
      "${UPGRADE_TRANSACTION_DIR}/install/${item}" \
      "${UPGRADE_TRANSACTION_DIR}/install-state/${item}.state" \
      "${INSTALL_DIR}/${item}" || failed=1
  done < <(upgrade_transaction_install_items)
  [ "${failed}" -eq 0 ]
}

restore_upgrade_prequiesce_state() {
  local failed=0
  local deadline=$((SECONDS + 60))
  if [ "${UPGRADE_PREFLIGHT_POSTGRES_STARTED:-0}" -eq 1 ]; then
    bounded_upgrade_systemctl "${deadline}" stop "${UNIT_BASENAME}-postgres.service" || failed=1
    UPGRADE_PREFLIGHT_POSTGRES_STARTED=0
  fi
  [ "${failed}" -eq 0 ] || return 1
  restore_upgrade_transaction_entry \
    "${UPGRADE_TRANSACTION_DIR}/install/.env" \
    "${UPGRADE_TRANSACTION_DIR}/install-state/.env.state" \
    "${INSTALL_DIR}/.env" || failed=1
  restore_upgrade_handoff_markers || failed=1
  restore_upgrade_install_root_metadata || failed=1
  [ "${failed}" -eq 0 ]
}

restore_upgrade_external_units() {
  local unit
  local failed=0
  while IFS= read -r unit; do
    validate_upgrade_transaction_entry_snapshot \
      "${UPGRADE_TRANSACTION_DIR}/units/${unit}" \
      "${UPGRADE_TRANSACTION_DIR}/unit-state/${unit}.state" || return 1
  done < <(upgrade_transaction_unit_names)
  while IFS= read -r unit; do
    restore_upgrade_transaction_entry \
      "${UPGRADE_TRANSACTION_DIR}/units/${unit}" \
      "${UPGRADE_TRANSACTION_DIR}/unit-state/${unit}.state" \
      "${SYSTEMD_UNIT_ROOT}/${unit}" || failed=1
  done < <(upgrade_transaction_unit_names)
  [ "${failed}" -eq 0 ]
}

restore_upgrade_handoff_markers() {
  local marker
  local state_dir
  local failed=0
  state_dir="$(admin_handoff_state_dir)" || return 1
  for marker in admin-handoff.pending "${ADMIN_HANDOFF_DELIVERED_NAME}"; do
    validate_upgrade_transaction_entry_snapshot \
      "${UPGRADE_TRANSACTION_DIR}/handoff/${marker}" \
      "${UPGRADE_TRANSACTION_DIR}/handoff-state/${marker}.state" || return 1
  done
  for marker in admin-handoff.pending "${ADMIN_HANDOFF_DELIVERED_NAME}"; do
    restore_upgrade_transaction_entry \
      "${UPGRADE_TRANSACTION_DIR}/handoff/${marker}" \
      "${UPGRADE_TRANSACTION_DIR}/handoff-state/${marker}.state" \
      "${state_dir}/${marker}" || failed=1
  done
  [ "${failed}" -eq 0 ]
}

apply_upgrade_unit_enablement() {
  local unit="$1"
  local desired="$2"
  local deadline="${3:-$((SECONDS + 30))}"
  case "${desired}" in
    enabled)
      bounded_upgrade_systemctl "${deadline}" unmask "${unit}" >/dev/null 2>&1 || return 1
      bounded_upgrade_systemctl "${deadline}" enable "${unit}" >/dev/null 2>&1 || return 1
      ;;
    enabled-runtime)
      bounded_upgrade_systemctl "${deadline}" unmask --runtime "${unit}" >/dev/null 2>&1 || return 1
      bounded_upgrade_systemctl "${deadline}" enable --runtime "${unit}" >/dev/null 2>&1 || return 1
      ;;
    disabled)
      bounded_upgrade_systemctl "${deadline}" unmask "${unit}" >/dev/null 2>&1 || return 1
      bounded_upgrade_systemctl "${deadline}" disable "${unit}" >/dev/null 2>&1 || return 1
      ;;
    masked)
      bounded_upgrade_systemctl "${deadline}" disable "${unit}" >/dev/null 2>&1 || true
      bounded_upgrade_systemctl "${deadline}" mask "${unit}" >/dev/null 2>&1 || return 1
      ;;
    masked-runtime)
      bounded_upgrade_systemctl "${deadline}" disable --runtime "${unit}" >/dev/null 2>&1 || true
      bounded_upgrade_systemctl "${deadline}" mask --runtime "${unit}" >/dev/null 2>&1 || return 1
      ;;
    linked)
      bounded_upgrade_systemctl "${deadline}" unmask "${unit}" >/dev/null 2>&1 || return 1
      bounded_upgrade_systemctl "${deadline}" link "${SYSTEMD_UNIT_ROOT}/${unit}" >/dev/null 2>&1 || return 1
      ;;
    linked-runtime)
      bounded_upgrade_systemctl "${deadline}" unmask --runtime "${unit}" >/dev/null 2>&1 || return 1
      bounded_upgrade_systemctl "${deadline}" link --runtime "${SYSTEMD_UNIT_ROOT}/${unit}" >/dev/null 2>&1 || return 1
      ;;
    static|indirect|generated|transient|alias|not-found) ;;
    *) return 1 ;;
  esac
}

restore_upgrade_unit_enablement() {
  local deadline="${1:-$((SECONDS + 60))}"
  local unit
  local desired
  local observed
  local state_file
  local failed=0
  while IFS= read -r unit; do
    state_file="${UPGRADE_TRANSACTION_DIR}/enablement/${unit}.state"
    [ ! -L "${state_file}" ] && [ -f "${state_file}" ] \
      && [ "$(wc -l <"${state_file}" | tr -d '[:space:]')" = 1 ] || {
      failed=1
      continue
    }
    desired="$(<"${state_file}")"
    apply_upgrade_unit_enablement "${unit}" "${desired}" "${deadline}" || {
      failed=1
      continue
    }
    observed="$(capture_upgrade_unit_enablement "${unit}" "${deadline}")" || {
      failed=1
      continue
    }
    [ "${observed}" = "${desired}" ] || failed=1
  done < <(upgrade_transaction_unit_names)
  [ "${failed}" -eq 0 ]
}

remove_upgrade_transaction_snapshot() {
  local state_dir
  state_dir="$(admin_handoff_state_dir)" || return 1
  case "${UPGRADE_TRANSACTION_DIR}" in
    "${state_dir}/upgrade-transaction"|\
    "${state_dir}/upgrade-transaction.building."*|\
    "${state_dir}/upgrade-transaction.committed."*|\
    "${state_dir}/upgrade-transaction.restored."*) ;;
    *) return 1 ;;
  esac
  [ ! -L "${UPGRADE_TRANSACTION_DIR}" ] \
    && [ -d "${UPGRADE_TRANSACTION_DIR}" ] || return 1
  if ! (assert_control_tree_safe "${UPGRADE_TRANSACTION_DIR}" structural) >/dev/null 2>&1; then
    return 1
  fi
  rm -rf -- "${UPGRADE_TRANSACTION_DIR}" || return 1
  sync -f "${state_dir}"
}

finalize_upgrade_transaction_terminal() {
  local terminal_phase="$1"
  local state_dir
  local transaction_id
  local tombstone
  case "${terminal_phase}" in
    committed|restored) ;;
    *) return 1 ;;
  esac
  state_dir="$(admin_handoff_state_dir)" || return 1
  write_upgrade_transaction_phase "${terminal_phase}" || return 1

  # From this point onward the on-disk decision is terminal. Keep the complete
  # tombstone until the persistent reboot fence has been cleared (and, after a
  # reboot, normal enablement has been replayed). Only then may GC discard the
  # service/enablement evidence.
  UPGRADE_TRANSACTION_STATE="${terminal_phase}"
  transaction_id="${UPGRADE_TRANSACTION_ID}"
  [[ "${transaction_id}" =~ ^[0-9]+-[0-9]+-[0-9]+$ ]] || return 1
  tombstone="${state_dir}/upgrade-transaction.${terminal_phase}.${transaction_id}"
  if [ ! -e "${tombstone}" ] && [ ! -L "${tombstone}" ] \
    && mv -- "${UPGRADE_TRANSACTION_DIR}" "${tombstone}"; then
    UPGRADE_TRANSACTION_DIR="${tombstone}"
    UPGRADE_TRANSACTION_PHASE_FILE="${tombstone}/phase"
    if ! sync -f "${state_dir}"; then
      printf '[streamserver-native-install] WARNING: terminal upgrade transaction tombstone publication could not be fsynced\n' >&2
    fi
  else
    printf '[streamserver-native-install] WARNING: terminal upgrade transaction snapshot could not be renamed for garbage collection\n' >&2
    return 0
  fi
  return 0
}

replay_native_enablement_after_fenced_reboot() {
  local deadline=$((SECONDS + 60))
  local unit
  local state_file
  local enablement
  local active_state
  local main_pid
  local target_unit="${UNIT_BASENAME}.target"
  local -a desired_units=()
  state_file="${UPGRADE_TRANSACTION_DIR}/enablement/${target_unit}.state"
  [ ! -L "${state_file}" ] && [ -f "${state_file}" ] \
    && [ "$(wc -l <"${state_file}" | tr -d '[:space:]')" = 1 ] \
    || return 1
  enablement="$(<"${state_file}")"
  case "${enablement}" in
    enabled)
      # Replay the normal boot root. The target's Wants/Requires graph decides
      # which persistently enabled components start; do not directly start
      # every component merely because it reports enabled.
      bounded_upgrade_systemctl "${deadline}" start "${target_unit}" \
      || return 1
      ;;
    enabled-runtime|disabled|masked|masked-runtime|static|indirect|generated|transient|linked|linked-runtime|alias|not-found) ;;
    *) return 1 ;;
  esac
  active_state="$(bounded_upgrade_systemctl "${deadline}" \
    show --property ActiveState --value "${target_unit}")" || return 1
  if [ "${enablement}" = enabled ]; then
    [ "${active_state}" = active ] || return 1
  else
    [ "${active_state}" = inactive ] || return 1
  fi
  UPGRADE_ACTIVE_UNITS=()
  UPGRADE_ACTIVE_MAIN_PIDS=()
  mapfile -t desired_units < <(upgrade_units_for_role)
  for unit in "${desired_units[@]}"; do
    active_state="$(bounded_upgrade_systemctl "${deadline}" \
      show --property ActiveState --value "${unit}")" || return 1
    if [ "${active_state}" = active ]; then
      main_pid="$(bounded_upgrade_systemctl "${deadline}" \
        show --property MainPID --value "${unit}")" || return 1
      [[ "${main_pid}" =~ ^[1-9][0-9]*$ ]] || return 1
      UPGRADE_ACTIVE_UNITS+=("${unit}")
      UPGRADE_ACTIVE_MAIN_PIDS+=("${main_pid}")
    elif [ "${active_state}" != inactive ]; then
      return 1
    fi
  done
  probe_upgrade_active_components_readiness 0 "${deadline}" \
    || return 1
  log "replayed native systemd enablement after a fenced upgrade reboot"
}

complete_terminal_upgrade_transaction() {
  local terminal_phase
  local transaction_id="${UPGRADE_TRANSACTION_ID}"
  local tombstone="${UPGRADE_TRANSACTION_DIR}"
  local full_snapshot=0
  local fence_marker
  local snapshot_kind
  terminal_phase="$(read_upgrade_transaction_phase)" || return 1
  case "${terminal_phase}" in committed|restored) ;; *) return 1 ;; esac
  [[ "${transaction_id}" =~ ^[0-9]+-[0-9]+-[0-9]+$ ]] || return 1
  fence_marker="$(upgrade_boot_fence_marker_path)"
  if [ -e "${fence_marker}" ] || [ -L "${fence_marker}" ]; then
    validate_upgrade_boot_fence_files \
      "${fence_marker}" "$(upgrade_boot_fence_lease_path)"
    [ "$(<"${fence_marker}")" = "${transaction_id}" ] \
      || fail "terminal native upgrade boot fence transaction ID mismatch"
  fi
  snapshot_kind="$(read_upgrade_transaction_snapshot_kind)" || return 1
  case "${snapshot_kind}" in
    full)
      full_snapshot=1
      validate_upgrade_transaction_snapshot_for_restore || return 1
      ;;
    minimal)
      validate_upgrade_transaction_entry_snapshot \
        "${UPGRADE_TRANSACTION_DIR}/install/.env" \
        "${UPGRADE_TRANSACTION_DIR}/install-state/.env.state" || return 1
      validate_upgrade_install_root_metadata_state || return 1
      ;;
    *) return 1 ;;
  esac
  if [ "${full_snapshot}" -eq 1 ] && upgrade_transaction_crossed_reboot; then
    clear_upgrade_boot_fence || return 1
    replay_native_enablement_after_fenced_reboot || return 1
  else
    clear_upgrade_boot_fence || return 1
  fi
  if ! garbage_collect_upgrade_transaction_tree \
    "${tombstone}" "${terminal_phase}" "${transaction_id}"; then
    printf '[streamserver-native-install] WARNING: terminal upgrade transaction was retained for later garbage collection\n' >&2
    return 0
  fi
  UPGRADE_TRANSACTION_STATE="${terminal_phase}"
  return 0
}

restore_upgrade_transaction() {
  local deadline
  local unit
  local failed=0
  local disk_phase
  local -a all_units=()
  disk_phase="$(read_upgrade_transaction_phase 2>/dev/null)" \
    || disk_phase="${UPGRADE_TRANSACTION_STATE}"
  [ "${disk_phase}" = armed ] || return 1
  if ! (assert_install_transaction_lock_held) >/dev/null 2>&1; then
    printf '[streamserver-native-install] ERROR: native installer lock validation failed during rollback\n' >&2
    return 1
  fi
  validate_upgrade_transaction_snapshot_for_restore || {
    printf '[streamserver-native-install] ERROR: native upgrade snapshot validation failed before rollback mutation\n' >&2
    return 1
  }
  if [ "${UPGRADE_RESTORE_ON_FAILURE:-0}" -eq 1 ]; then
    mapfile -t all_units < <(upgrade_rollback_units)
    deadline=$((SECONDS + 60))
    bounded_upgrade_systemctl "${deadline}" stop "${all_units[@]}" \
      >/dev/null 2>&1 || return 1
  fi
  if [ "${UPGRADE_RESTORE_ON_FAILURE:-0}" -eq 1 ]; then
    restore_upgrade_install_tree || failed=1
    restore_upgrade_install_root_metadata || failed=1
    restore_upgrade_handoff_markers || failed=1
    restore_upgrade_external_units || failed=1
    [ "${failed}" -eq 0 ] || return 1
    deadline=$((SECONDS + 60))
    bounded_upgrade_systemctl "${deadline}" daemon-reload || return 1
    restore_upgrade_unit_enablement "${deadline}" || return 1
    deadline=$((SECONDS + 60))
    restore_captured_upgrade_service_state "${deadline}" || return 1
    deadline=$((SECONDS + 60))
    verify_restored_upgrade_readiness "${deadline}" || return 1
  else
    restore_upgrade_prequiesce_state || failed=1
  fi
  if [ "${failed}" -eq 0 ]; then
    finalize_upgrade_transaction_terminal restored || return 1
    complete_terminal_upgrade_transaction || return 1
    UPGRADE_RESTORE_ON_FAILURE=0
    return 0
  fi
  return 1
}

commit_upgrade_transaction() {
  [ "${UPGRADE}" -eq 1 ] || return 0
  [ "${UPGRADE_TRANSACTION_STATE}" = armed ] \
    || fail "native upgrade transaction is not armed at commit"
  assert_install_transaction_lock_held
  finalize_upgrade_transaction_terminal committed \
    || fail "cannot commit native upgrade transaction"
  complete_terminal_upgrade_transaction \
    || fail "committed native upgrade could not complete its protective fence handoff"
  UPGRADE_RESTORE_ON_FAILURE=0
  trap cleanup_installer_ephemeral_state EXIT
}

cleanup_installer_state() {
  local exit_status=$?
  local rollback_failed=0
  local disk_phase=""
  trap '' HUP INT TERM
  trap - EXIT
  set +e
  disk_phase="$(read_upgrade_transaction_phase 2>/dev/null)" || true
  if [ -z "${disk_phase}" ]; then
    disk_phase="$(read_upgrade_terminal_decision 2>/dev/null)" || true
  fi
  [ -n "${disk_phase}" ] || disk_phase="${UPGRADE_TRANSACTION_STATE:-none}"
  if [ "${disk_phase}" = committed ] || [ "${disk_phase}" = restored ]; then
    # The durable terminal phase is authoritative even when a signal or a
    # garbage-collection error races with in-memory state updates.
    :
  elif [ "${disk_phase}" = building ]; then
    if [ -n "${UPGRADE_TRANSACTION_DIR}" ] \
      && [ -d "${UPGRADE_TRANSACTION_DIR}" ] \
      && [ ! -L "${UPGRADE_TRANSACTION_DIR}" ]; then
      remove_upgrade_transaction_snapshot || rollback_failed=1
    fi
    UPGRADE_TRANSACTION_STATE=none
  elif [ "${disk_phase}" = presealed ]; then
    if restore_upgrade_preseal_guard; then
      log "restored the native upgrade preseal baseline after installer failure"
    else
      rollback_failed=1
    fi
  elif [ "${disk_phase}" = armed ]; then
    if [ "${exit_status}" -eq 0 ]; then
      printf '[streamserver-native-install] ERROR: installer exited with an uncommitted native upgrade transaction\n' >&2
      exit_status=1
    fi
    if restore_upgrade_transaction; then
      log "restored the complete pre-upgrade native transaction after installer failure"
    else
      rollback_failed=1
    fi
  elif [ "${exit_status}" -ne 0 ] \
    && [ "${UPGRADE_RESTORE_ON_FAILURE:-0}" -eq 1 ]; then
    if restore_captured_upgrade_service_state; then
      log "restored the pre-upgrade native service state after installer failure"
    else
      rollback_failed=1
    fi
  fi
  if [ "${rollback_failed}" -ne 0 ]; then
    printf '[streamserver-native-install] ERROR: native upgrade rollback failed; the root-only transaction snapshot was retained\n' >&2
  fi
  cleanup_upgrade_boot_fence_lease_only || true
  cleanup_installer_ephemeral_state
  exit "${exit_status}"
}

wait_for_upgrade_units_steady() {
  local deadline="$1"
  shift
  local unit
  local active_state
  local all_steady
  local -a units=("$@")
  while [ "${SECONDS}" -lt "${deadline}" ]; do
    all_steady=1
    for unit in "${units[@]}"; do
      active_state="$(bounded_upgrade_systemctl \
        "${deadline}" show --property ActiveState --value "${unit}")" \
        || fail "cannot read native unit state before upgrade: ${unit}"
      case "${active_state}" in
        active|inactive) ;;
        *) all_steady=0 ;;
      esac
    done
    [ "${all_steady}" -eq 0 ] || return 0
    sleep 1
  done
  fail "native units did not reach a steady active/inactive state before upgrade"
}

capture_upgrade_service_state() {
  local deadline=$((SECONDS + 60))
  local unit
  local main_pid
  local active_state
  local target_unit="${UNIT_BASENAME}.target"
  local -a desired_units=()
  [ "${UPGRADE}" -eq 1 ] || return 0

  UPGRADE_TARGET_WAS_ACTIVE=0
  UPGRADE_SERVICES_QUIESCED=0
  UPGRADE_SERVICE_STATE_CAPTURED=0
  UPGRADE_ACTIVE_UNITS=()
  UPGRADE_ACTIVE_MAIN_PIDS=()
  mapfile -t desired_units < <(upgrade_units_for_role)

  wait_for_upgrade_units_steady "${deadline}" "${desired_units[@]}" "${target_unit}"
  active_state="$(bounded_upgrade_systemctl "${deadline}" show --property ActiveState --value "${target_unit}")" || fail "cannot capture native target state before upgrade"
  case "${active_state}" in
    active) UPGRADE_TARGET_WAS_ACTIVE=1 ;;
    inactive) ;;
    *) fail "native target changed state while preparing the upgrade: ${active_state}" ;;
  esac
  for unit in "${desired_units[@]}"; do
    active_state="$(bounded_upgrade_systemctl "${deadline}" show --property ActiveState --value "${unit}")" || fail "cannot capture native service state before upgrade: ${unit}"
    case "${active_state}" in
      active)
        main_pid="$(bounded_upgrade_systemctl "${deadline}" show --property MainPID --value "${unit}")" || fail "cannot read the active service PID before upgrade: ${unit}"
        [[ "${main_pid}" =~ ^[1-9][0-9]*$ ]] || fail "active service has no verifiable MainPID before upgrade: ${unit}"
        UPGRADE_ACTIVE_UNITS+=("${unit}")
        UPGRADE_ACTIVE_MAIN_PIDS+=("${main_pid}")
        ;;
      inactive) ;;
      *) fail "native service changed state while preparing the upgrade: ${unit} (${active_state})" ;;
    esac
  done

  UPGRADE_SERVICE_STATE_CAPTURED=1
  trap cleanup_installer_state EXIT
}

ensure_upgrade_preflight_database_available() {
  local postgres_unit="${UNIT_BASENAME}-postgres.service"
  role_has_core "${INSTALL_ROLE}" || return 0
  [ "${TRUSTED_POSTGRES_UNIT_COUNT:-0}" -eq 1 ] || return 0
  upgrade_unit_was_active "${postgres_unit}" && return 0
  UPGRADE_PREFLIGHT_POSTGRES_STARTED=1
  bounded_upgrade_systemctl "$((SECONDS + 60))" start "${postgres_unit}" || fail "cannot start bundled PostgreSQL for upgrade security preflight"
  wait_for_postgres
  log "temporarily started bundled PostgreSQL for upgrade security preflight"
}

verify_quiesced_upgrade_control_sources_stable() {
  local item
  local unit
  local marker
  local state_dir
  [ "${UPGRADE_TRANSACTION_STATE:-none}" = armed ] || return 0
  state_dir="$(admin_handoff_state_dir)"
  while IFS= read -r item; do
    [ "${item}" != .env ] || continue
    verify_upgrade_transaction_entry_matches_source \
      "${INSTALL_DIR}/${item}" \
      "${UPGRADE_TRANSACTION_DIR}/install/${item}" \
      "${UPGRADE_TRANSACTION_DIR}/install-state/${item}.state" content \
      || return 1
  done < <(upgrade_transaction_install_items)
  while IFS= read -r unit; do
    verify_upgrade_transaction_entry_matches_source \
      "${SYSTEMD_UNIT_ROOT}/${unit}" \
      "${UPGRADE_TRANSACTION_DIR}/units/${unit}" \
      "${UPGRADE_TRANSACTION_DIR}/unit-state/${unit}.state" \
      || return 1
  done < <(upgrade_transaction_unit_names)
  for marker in admin-handoff.pending "${ADMIN_HANDOFF_DELIVERED_NAME}"; do
    verify_upgrade_transaction_entry_matches_source \
      "${state_dir}/${marker}" \
      "${UPGRADE_TRANSACTION_DIR}/handoff/${marker}" \
      "${UPGRADE_TRANSACTION_DIR}/handoff-state/${marker}.state" \
      || return 1
  done
}

quiesce_captured_upgrade_services() {
  local deadline=$((SECONDS + 60))
  local handoff_core_bin="${1:-}"
  local preflight_agent_bin="${2:-}"
  local unit
  local main_pid
  local active_state
  local target_unit="${UNIT_BASENAME}.target"
  local -a application_units=()
  local -a database_units=()
  mapfile -t application_units < <(non_database_units_for_role)
  if role_has_core "${INSTALL_ROLE}" && [ "${TRUSTED_POSTGRES_UNIT_COUNT:-0}" -eq 1 ]; then
    database_units=("${UNIT_BASENAME}-postgres.service")
  fi

  UPGRADE_RESTORE_ON_FAILURE=1
  bounded_upgrade_systemctl "${deadline}" stop "${application_units[@]}" "${target_unit}" || fail "failed to quiesce native application services before upgrade"
  for unit in "${application_units[@]}" "${target_unit}"; do
    active_state="$(bounded_upgrade_systemctl "${deadline}" show --property ActiveState --value "${unit}")" || fail "cannot verify quiesced native application unit state: ${unit}"
    case "${active_state}" in
      inactive|failed) ;;
      *) fail "native application unit did not reach a quiescent state during upgrade: ${unit} (${active_state})" ;;
    esac
  done
  for unit in "${application_units[@]}"; do
    main_pid="$(bounded_upgrade_systemctl "${deadline}" show --property MainPID --value "${unit}")" || fail "cannot verify quiesced native application PID: ${unit}"
    [ "${main_pid}" = "0" ] || fail "native application service retained a MainPID during upgrade: ${unit} (${main_pid})"
  done
  if role_has_core "${INSTALL_ROLE}" && [ -n "${handoff_core_bin}" ]; then
    prepare_pending_admin_password_handoff "${INSTALL_DIR}/.env" "${handoff_core_bin}"
  fi
  if [ -n "${handoff_core_bin}" ] || [ -n "${preflight_agent_bin}" ]; then
    security_preflight_env "${INSTALL_DIR}/.env" "${handoff_core_bin}" "${preflight_agent_bin}" full || fail "post-handoff production security preflight failed"
  fi

  if [ "${#database_units[@]}" -gt 0 ]; then
    deadline=$((SECONDS + 60))
    bounded_upgrade_systemctl "${deadline}" stop "${database_units[@]}" || fail "failed to quiesce bundled PostgreSQL after administrator handoff"
    for unit in "${database_units[@]}"; do
      active_state="$(bounded_upgrade_systemctl "${deadline}" show --property ActiveState --value "${unit}")" || fail "cannot verify quiesced bundled PostgreSQL state"
      case "${active_state}" in
        inactive|failed) ;;
        *) fail "bundled PostgreSQL did not reach a quiescent state during upgrade: ${active_state}" ;;
      esac
      main_pid="$(bounded_upgrade_systemctl "${deadline}" show --property MainPID --value "${unit}")" || fail "cannot verify quiesced bundled PostgreSQL PID"
      [ "${main_pid}" = "0" ] || fail "bundled PostgreSQL retained a MainPID during upgrade: ${main_pid}"
    done
  fi
  verify_quiesced_upgrade_control_sources_stable \
    || fail "native control files changed before every legacy service was quiesced"
  UPGRADE_PREFLIGHT_POSTGRES_STARTED=0
  UPGRADE_SERVICES_QUIESCED=1
}

capture_and_quiesce_upgrade_services() {
  capture_upgrade_service_state
  quiesce_captured_upgrade_services "${1:-}" "${2:-}"
}

prepare_upgrade_security_gate() {
  local state_dir
  acquire_install_transaction_lock
  state_dir="$(admin_handoff_state_dir)"
  resume_upgrade_boot_fence_for_recovery
  if [ -e "${state_dir}/upgrade-transaction" ] \
    || [ -L "${state_dir}/upgrade-transaction" ] \
    || compgen -G "${state_dir}/upgrade-transaction.building.*" >/dev/null \
    || compgen -G "${state_dir}/upgrade-transaction.committed.*" >/dev/null \
    || compgen -G "${state_dir}/upgrade-transaction.restored.*" >/dev/null \
    || compgen -G "${state_dir}/upgrade-transaction.gc.*" >/dev/null \
    || compgen -G "${state_dir}/upgrade-transaction.terminal.*" >/dev/null; then
    garbage_collect_resolved_upgrade_transactions
  fi
  if [ "${UPGRADE_BOOT_FENCE_ACTIVE}" -eq 1 ]; then
    fail "native upgrade boot fence has no matching recoverable transaction"
  fi
  # Recovery above intentionally relies only on CLI + durable snapshot
  # identity. Validate the now-restored live systemd topology before arming a
  # new transaction.
  prepare_upgrade_cli_identity
  begin_upgrade_preseal_guard
  seal_legacy_upgrade_environment
  validate_sealed_upgrade_environment_identity
  prepare_upgrade_database_configuration
  capture_upgrade_service_state
  begin_upgrade_transaction
  prepare_package_security_probe_binaries
  migrate_legacy_zlm_api_endpoint
  ensure_upgrade_preflight_database_available
  security_preflight_env "${INSTALL_DIR}/.env" "${SECURITY_PROBE_CORE_BIN}" "${SECURITY_PROBE_AGENT_BIN}" upgrade-gate \
    || fail "upgrade blocked until auth/admin and TLS gaps are migrated"
  quiesce_captured_upgrade_services "${SECURITY_PROBE_CORE_BIN}" "${SECURITY_PROBE_AGENT_BIN}"
  cleanup_security_probe_binaries \
    || fail "failed to clean package security probe binaries"
}

probe_upgrade_readiness() {
  "${INSTALL_DIR}/bin/streamserverctl" health >/dev/null 2>&1
}

upgrade_readiness_config_value() {
  local key="$1"
  local value
  [ "$(env_key_occurrence_count "${INSTALL_DIR}/.env" "${key}")" = 1 ] \
    || return 1
  value="$(existing_env_value "${INSTALL_DIR}/.env" "${key}")" || return 1
  [[ "${value}" != *$'\n'* ]] && [[ "${value}" != *$'\r'* ]] || return 1
  printf '%s' "${value}"
}

upgrade_readiness_port_status() {
  local value="$1"
  [[ "${value}" =~ ^[0-9]+$ ]] \
    && [ "$((10#${value}))" -ge 1 ] \
    && [ "$((10#${value}))" -le 65535 ]
}

prepare_upgrade_readiness_configuration() {
  local allow_legacy_zlm="$1"
  local unit
  local needs_core=0
  local needs_agent=0
  local needs_zlm=0
  local needs_postgres=0
  UPGRADE_READINESS_CORE_PORT=""
  UPGRADE_READINESS_CORE_TLS_CERT=""
  UPGRADE_READINESS_AGENT_PORT=""
  UPGRADE_READINESS_ZLM_PORT=""
  UPGRADE_READINESS_ZLM_SECRET=""
  UPGRADE_READINESS_POSTGRES_PORT=""
  UPGRADE_READINESS_POSTGRES_USER=""
  UPGRADE_READINESS_POSTGRES_DB=""
  for unit in "${UPGRADE_ACTIVE_UNITS[@]}"; do
    case "${unit}" in
      "${UNIT_BASENAME}-core.service") needs_core=1 ;;
      "${UNIT_BASENAME}-agent.service") needs_agent=1 ;;
      "${UNIT_BASENAME}-zlm.service") needs_zlm=1 ;;
      "${UNIT_BASENAME}-postgres.service") needs_postgres=1 ;;
      *) return 1 ;;
    esac
  done
  if [ "${needs_core}" -eq 1 ]; then
    UPGRADE_READINESS_CORE_PORT="$(upgrade_readiness_config_value CORE_HTTP_PORT)" \
      || return 1
    upgrade_readiness_port_status "${UPGRADE_READINESS_CORE_PORT}" || return 1
    UPGRADE_READINESS_CORE_TLS_CERT="$(upgrade_readiness_config_value \
      CORE_HTTP_TLS_CERT_PATH)" || return 1
  fi
  if [ "${needs_agent}" -eq 1 ]; then
    UPGRADE_READINESS_AGENT_PORT="$(upgrade_readiness_config_value AGENT_HTTP_PORT)" \
      || return 1
    upgrade_readiness_port_status "${UPGRADE_READINESS_AGENT_PORT}" || return 1
  fi
  if [ "${needs_zlm}" -eq 1 ]; then
    UPGRADE_READINESS_ZLM_PORT="$(upgrade_readiness_config_value ZLM_HTTP_PORT)" \
      || return 1
    upgrade_readiness_port_status "${UPGRADE_READINESS_ZLM_PORT}" || return 1
    if [ "$(env_key_occurrence_count "${INSTALL_DIR}/.env" ZLM_API_SECRET)" = 1 ]; then
      UPGRADE_READINESS_ZLM_SECRET="$(upgrade_readiness_config_value ZLM_API_SECRET)" \
        || return 1
      is_strong_url_safe_secret "${UPGRADE_READINESS_ZLM_SECRET}" || return 1
    elif [ "${allow_legacy_zlm}" -eq 1 ] \
      && [ "$(env_key_occurrence_count "${INSTALL_DIR}/.env" HOOK_SHARED_SECRET)" = 1 ]; then
      UPGRADE_READINESS_ZLM_SECRET="$(upgrade_readiness_config_value HOOK_SHARED_SECRET)" \
        || return 1
      [[ "${UPGRADE_READINESS_ZLM_SECRET}" =~ ^[A-Za-z0-9._~-]+$ ]] \
        && [ "${#UPGRADE_READINESS_ZLM_SECRET}" -ge 16 ] \
        && [ "${#UPGRADE_READINESS_ZLM_SECRET}" -le 256 ] || return 1
    else
      return 1
    fi
  fi
  if [ "${needs_postgres}" -eq 1 ]; then
    UPGRADE_READINESS_POSTGRES_PORT="$(upgrade_readiness_config_value POSTGRES_PORT)" \
      || return 1
    upgrade_readiness_port_status "${UPGRADE_READINESS_POSTGRES_PORT}" || return 1
    UPGRADE_READINESS_POSTGRES_USER="$(upgrade_readiness_config_value POSTGRES_USER)" \
      || return 1
    UPGRADE_READINESS_POSTGRES_DB="$(upgrade_readiness_config_value POSTGRES_DB)" \
      || return 1
    [ -n "${UPGRADE_READINESS_POSTGRES_USER}" ] \
      && [ -n "${UPGRADE_READINESS_POSTGRES_DB}" ] \
      && [ -x "${INSTALL_DIR}/bin/pg_isready" ] || return 1
  fi
}

probe_upgrade_component_readiness_once() {
  local unit="$1"
  local timeout_sec="$2"
  local response
  local compact_response
  local code_occurrences
  local -a curl_args=(
    -q --noproxy '*' --proto '=http,https'
    --connect-timeout 1 --max-time "${timeout_sec}"
    --fail --silent
  )
  case "${unit}" in
    "${UNIT_BASENAME}-core.service")
      if [ -n "${UPGRADE_READINESS_CORE_TLS_CERT}" ]; then
        curl "${curl_args[@]}" --output /dev/null -k \
          "https://127.0.0.1:${UPGRADE_READINESS_CORE_PORT}/health/ready" \
          2>/dev/null
      else
        curl "${curl_args[@]}" --output /dev/null \
          "http://127.0.0.1:${UPGRADE_READINESS_CORE_PORT}/health/ready" \
          2>/dev/null
      fi
      ;;
    "${UNIT_BASENAME}-agent.service")
      curl "${curl_args[@]}" --output /dev/null \
        "http://127.0.0.1:${UPGRADE_READINESS_AGENT_PORT}/health/ready" \
        2>/dev/null
      ;;
    "${UNIT_BASENAME}-zlm.service")
      response="$(printf \
        'url = "http://127.0.0.1:%s/index/api/getStatistic?secret=%s"\n' \
        "${UPGRADE_READINESS_ZLM_PORT}" "${UPGRADE_READINESS_ZLM_SECRET}" \
        | curl "${curl_args[@]}" --max-filesize 4096 --config - 2>/dev/null)" \
        || return 1
      [ "${#response}" -le 4096 ] || return 1
      compact_response="$(printf '%s' "${response}" | tr -d '[:space:]')"
      [[ "${compact_response}" == \{*\} ]] || return 1
      code_occurrences="$(printf '%s' "${compact_response}" \
        | grep -o '"code":' | wc -l | tr -d '[:space:]')"
      [ "${code_occurrences}" = 1 ] || return 1
      [[ "${compact_response}" =~ \"code\":0([,}]) ]]
      ;;
    "${UNIT_BASENAME}-postgres.service")
      run_native_service_command timeout --signal=KILL "${timeout_sec}s" \
        "${INSTALL_DIR}/bin/pg_isready" \
        -h 127.0.0.1 -p "${UPGRADE_READINESS_POSTGRES_PORT}" \
        -U "${UPGRADE_READINESS_POSTGRES_USER}" \
        -d "${UPGRADE_READINESS_POSTGRES_DB}" -t "${timeout_sec}" \
        >/dev/null 2>&1
      ;;
    *) return 1 ;;
  esac
}

probe_upgrade_active_components_readiness() {
  local allow_legacy_zlm="${1:-0}"
  local deadline="${2:-$((SECONDS + 60))}"
  local remaining
  local probe_timeout
  local unit
  local active_state
  local all_ready
  prepare_upgrade_readiness_configuration "${allow_legacy_zlm}" || {
    printf '[streamserver-native-install] ERROR: active component readiness configuration is invalid\n' >&2
    return 1
  }
  [ "${#UPGRADE_ACTIVE_UNITS[@]}" -gt 0 ] || return 0
  while [ "${SECONDS}" -lt "${deadline}" ]; do
    all_ready=1
    for unit in "${UPGRADE_ACTIVE_UNITS[@]}"; do
      remaining=$((deadline - SECONDS))
      [ "${remaining}" -gt 0 ] || {
        all_ready=0
        continue
      }
      active_state="$(bounded_upgrade_systemctl \
        "${deadline}" show --property ActiveState --value "${unit}")" \
        || active_state=""
      [ "${active_state}" = active ] || all_ready=0
      remaining=$((deadline - SECONDS))
      [ "${remaining}" -gt 0 ] || {
        all_ready=0
        continue
      }
      probe_timeout="${remaining}"
      [ "${probe_timeout}" -le 4 ] || probe_timeout=4
      if ! probe_upgrade_component_readiness_once \
        "${unit}" "${probe_timeout}"; then
        all_ready=0
      fi
    done
    if [ "${all_ready}" -eq 1 ]; then
      for unit in "${UPGRADE_ACTIVE_UNITS[@]}"; do
        active_state="$(bounded_upgrade_systemctl \
          "${deadline}" show --property ActiveState --value "${unit}")" \
          || active_state=""
        [ "${active_state}" = active ] || all_ready=0
      done
      [ "${all_ready}" -eq 0 ] || return 0
    fi
    [ "${SECONDS}" -lt "${deadline}" ] || break
    sleep 1
  done
  printf '[streamserver-native-install] ERROR: previously-active component readiness timed out\n' >&2
  return 1
}

verify_upgrade_services_ready() {
  local deadline="${1:-$((SECONDS + 60))}"
  local unit
  local new_main_pid
  local observed_state
  local index
  local -a desired_units=()
  mapfile -t desired_units < <(upgrade_units_for_role)

  observed_state="$(bounded_upgrade_systemctl "${deadline}" \
    show --property ActiveState --value "${UNIT_BASENAME}.target")" \
    || fail "cannot verify upgraded native target state"
  if [ "${UPGRADE_TARGET_WAS_ACTIVE}" -eq 1 ]; then
    [ "${observed_state}" = active ] \
      || fail "upgraded native target did not return to its active state"
  else
    [ "${observed_state}" = inactive ] \
      || fail "upgraded native target was not restored to inactive"
  fi
  for unit in "${desired_units[@]}"; do
    observed_state="$(bounded_upgrade_systemctl "${deadline}" \
      show --property ActiveState --value "${unit}")" \
      || fail "cannot verify upgraded native service state: ${unit}"
    if upgrade_unit_was_active "${unit}"; then
      [ "${observed_state}" = active ] \
        || fail "upgraded native service did not return to its active state: ${unit}"
    else
      [ "${observed_state}" = inactive ] \
        || fail "upgraded native service was not restored to inactive: ${unit}"
    fi
  done
  for index in "${!UPGRADE_ACTIVE_UNITS[@]}"; do
    unit="${UPGRADE_ACTIVE_UNITS[${index}]}"
    new_main_pid="$(bounded_upgrade_systemctl "${deadline}" \
      show --property MainPID --value "${unit}")" \
      || fail "cannot read upgraded service PID: ${unit}"
    [[ "${new_main_pid}" =~ ^[1-9][0-9]*$ ]] \
      || fail "upgraded service has no verifiable MainPID: ${unit}"
    [ "${new_main_pid}" != "${UPGRADE_ACTIVE_MAIN_PIDS[${index}]}" ] \
      || fail "upgraded service is still running the pre-upgrade process: ${unit}"
  done

  probe_upgrade_active_components_readiness 0 "${deadline}" \
    || fail "upgraded native services did not become ready"
}

start_services_if_requested() {
  local deadline=$((SECONDS + 60))
  if [ "${START_AFTER_INSTALL}" -ne 1 ]; then
    if [ "${UPGRADE}" -eq 1 ] && [ "${UPGRADE_SERVICES_QUIESCED}" -eq 1 ]; then
      log "upgrade completed with native services stopped because --no-start was selected."
    fi
    return 0
  fi
  if [ "${UPGRADE}" -eq 1 ]; then
    [ "${UPGRADE_SERVICES_QUIESCED}" -eq 1 ] \
      || fail "upgrade service state was not captured and quiesced"
    restore_captured_upgrade_service_state "${deadline}" \
      || fail "failed to restore the pre-upgrade native service state"
    verify_upgrade_services_ready "${deadline}"
  else
    bounded_upgrade_systemctl "${deadline}" start "${UNIT_BASENAME}.target" \
      || fail "failed to start the native target before the systemd deadline"
  fi
  log "已启动 native 服务。"
  log "状态: ${INSTALL_DIR}/bin/streamserverctl status"
  log "健康检查: ${INSTALL_DIR}/bin/streamserverctl health"
}

prepare_production_security_state() {
  local deadline=$((SECONDS + 60))
  if role_has_core "${INSTALL_ROLE}"; then
    if [ "${DATABASE_MODE}" = "bundled" ]; then
      bounded_upgrade_systemctl \
        "${deadline}" start "${UNIT_BASENAME}-postgres.service" \
        || fail "failed to start bundled PostgreSQL before the systemd deadline"
      wait_for_postgres
      ensure_database_exists
    fi
    prepare_pending_admin_password_handoff \
      "${INSTALL_DIR}/.env" "${INSTALL_DIR}/bin/media-core"
  fi

  if role_is_all_in_one "${INSTALL_ROLE}"; then
    if ! bootstrap_all_in_one_agent_identity_if_needed; then
      if [ "${DATABASE_MODE}" = bundled ]; then
        bounded_upgrade_systemctl \
          "${deadline}" stop "${UNIT_BASENAME}-postgres.service" \
          >/dev/null 2>&1 || true
      fi
      fail "all-in-one Agent enrollment bootstrap failed"
    fi
  elif role_has_worker "${INSTALL_ROLE}"; then
    run_agent_enrollment_if_needed
  fi

  # Enrollment publishes service-owned identity files. Reassert all native
  # control/data boundaries before the final full Core+Agent preflight.
  fix_permissions

  if ! security_preflight_env "${INSTALL_DIR}/.env"; then
    if role_has_core "${INSTALL_ROLE}" && [ "${DATABASE_MODE}" = "bundled" ]; then
      bounded_upgrade_systemctl \
        "${deadline}" stop "${UNIT_BASENAME}-postgres.service" \
        >/dev/null 2>&1 || true
    fi
    fail "production security preflight failed; no Core/Agent service was started"
  fi

  if [ "${START_AFTER_INSTALL}" -eq 0 ] \
    && role_has_core "${INSTALL_ROLE}" \
    && [ "${DATABASE_MODE}" = "bundled" ]; then
    bounded_upgrade_systemctl \
      "${deadline}" stop "${UNIT_BASENAME}-postgres.service" \
      || fail "failed to stop bundled PostgreSQL before the systemd deadline"
  fi
}

confirm_start_after_install() {
  [ "${START_AFTER_INSTALL}" -eq 1 ] || return 0
  if ! prompt_yes_no "是否立即启动 native 服务？" "Y"; then
    START_AFTER_INSTALL=0
    log "已选择暂不启动服务。后续可执行: ${INSTALL_DIR}/bin/streamserverctl start"
  fi
}

validate_install_root_for_lock_planning() {
  local parent
  normalize_install_dir_for_transaction
  parent="$(dirname "${INSTALL_DIR}")"
  [ -d "${parent}" ] && [ ! -L "${parent}" ] \
    || fail "native installation root parent must be a real directory"
  admin_handoff_assert_no_symlink_boundary "${parent}"
  admin_handoff_assert_secure_root_ancestors "${parent}"
  if [ -e "${INSTALL_DIR}" ] || [ -L "${INSTALL_DIR}" ]; then
    [ -d "${INSTALL_DIR}" ] && [ ! -L "${INSTALL_DIR}" ] \
      || fail "native installation root must be a real directory"
  fi
}

perform_locked_install_mutation() {
  local installer_selected_role
  local installer_selected_instance_name
  local installer_selected_unit_basename
  if [ "${UPGRADE}" -eq 1 ]; then
    prepare_upgrade_security_gate
  else
    prepare_install_root_for_transaction false
    acquire_install_transaction_lock
  fi
  confirm_existing_install_target
  ensure_service_user
  harden_install_root_before_copy
  configure_database
  prepare_layout
  if role_has_core "${INSTALL_ROLE}"; then
    acquire_admin_handoff_lock
  fi
  copy_package_assets
  configure_core_values
  if role_has_worker "${INSTALL_ROLE}"; then
    configure_worker_values
  fi
  write_env_file
  write_streamserverctl
  install_uninstaller
  fix_permissions
  installer_selected_role="${INSTALL_ROLE}"
  installer_selected_instance_name="${INSTANCE_NAME}"
  installer_selected_unit_basename="${UNIT_BASENAME}"
  run_streamserver_config_tui_with_handoff_guard
  validate_identity_after_optional_tui \
    "${installer_selected_role}" \
    "${installer_selected_instance_name}" \
    "${installer_selected_unit_basename}"
  # The TUI publishes a new inode atomically; re-assert its service-readable
  # ownership after validating the immutable installer identity.
  fix_permissions
  initialize_postgres_if_needed
  install_systemd_units
  confirm_start_after_install
  prepare_production_security_state
  show_initial_admin_credentials_if_needed
  start_services_if_requested
  finalize_admin_handoff_after_install_success
  commit_upgrade_transaction
  log "安装完成: ${INSTALL_DIR}"
  log "卸载: ${INSTALL_DIR}/uninstall.sh"
}

run_locked_install_stage_from_plan_fd() {
  local plan_fd="$1"
  load_locked_install_plan_from_fd "${plan_fd}" \
    || fail "invalid or incomplete native installer lock plan"
  INSTALL_TRANSACTION_EXTERNAL_LOCKS=1
  derive_external_installer_lock_paths
  ensure_root_for_install
  assert_external_installer_flocks_held
  load_manifest
  ensure_prerequisites
  verify_package_checksums
  assert_locked_package_identity \
    "${LOCKED_PACKAGE_EXPECTED_CHECKSUM_SHA256}" \
    "${LOCKED_PACKAGE_EXPECTED_TREE_FINGERPRINT}"
  assert_no_docker_assets
  validate_role_supported "${INSTALL_ROLE}"
  if [ "${UPGRADE}" -eq 1 ]; then
    prepare_install_root_for_transaction true
    prepare_upgrade_cli_lock_identity
  else
    assert_fresh_instance_namespace_available
  fi
  perform_locked_install_mutation
}

main() {
  local persisted_instance_name
  if [ "${1:-}" = --_locked-install-stage ]; then
    [ "$#" -eq 2 ] || fail "invalid internal native installer stage invocation"
    run_locked_install_stage_from_plan_fd "$2"
    return
  fi
  if [ "${1:-}" = --_locked-readonly-check-stage ]; then
    [ "$#" -eq 5 ] || fail "invalid internal readonly diagnostic stage invocation"
    run_locked_readonly_check_stage "$2" "$3" "$4" "$5"
    return
  fi
  parse_args "$@"
  if [ -t 0 ] && [ -r /dev/tty ] && [ -w /dev/tty ]; then
    INTERACTIVE_INSTALL=1
  fi
  if [ "${CHECK_ONLY}" -eq 1 ] && [ "${SECURITY_PREFLIGHT}" -eq 1 ]; then
    fail "--check-only and --security-preflight cannot be used together"
  fi
  load_manifest
  ensure_prerequisites
  verify_package_checksums
  assert_no_docker_assets
  if [ "${SECURITY_PREFLIGHT}" -eq 1 ]; then
    [ -n "${INSTALL_DIR}" ] || fail "--security-preflight requires --install-dir"
    ensure_root_for_install
    prepare_install_root_for_transaction true
    persisted_instance_name="$(existing_env_value "${INSTALL_DIR}/.env" INSTANCE_NAME)"
    [ -n "${persisted_instance_name}" ] \
      && [ "$(sanitize_instance_name "${persisted_instance_name}")" = "${persisted_instance_name}" ] \
      || fail "installed INSTANCE_NAME is invalid"
    INSTANCE_NAME="${persisted_instance_name}"
    UNIT_BASENAME="$(unit_basename_for_instance "${INSTANCE_NAME}")"
    run_readonly_check_with_external_flocks security-preflight
    exit 0
  fi
  if [ "${CHECK_ONLY}" -eq 1 ]; then
    if [ -n "${INSTALL_DIR}" ]; then
      ensure_root_for_install
      prepare_install_root_for_transaction true
      persisted_instance_name="$(existing_env_value "${INSTALL_DIR}/.env" INSTANCE_NAME)"
      [ -n "${persisted_instance_name}" ] \
        && [ "$(sanitize_instance_name "${persisted_instance_name}")" = "${persisted_instance_name}" ] \
        || fail "installed INSTANCE_NAME is invalid"
      INSTANCE_NAME="${persisted_instance_name}"
      UNIT_BASENAME="$(unit_basename_for_instance "${INSTANCE_NAME}")"
      run_readonly_check_with_external_flocks check-only
    fi
    log "check-only 通过。"
    exit 0
  fi
  ensure_root_for_install
  if [ "${UPGRADE}" -eq 1 ]; then
    [ -n "${INSTALL_DIR}" ] || fail "--upgrade requires --install-dir"
    prepare_install_root_for_transaction true
    prepare_upgrade_cli_lock_identity
  else
    select_role
    collect_basic_inputs
    validate_install_root_for_lock_planning
  fi
  run_install_with_external_flocks
}

main "$@"
