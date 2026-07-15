#!/usr/bin/env bash
# Globals in this contract are intentionally consumed by functions sourced
# from install.sh; ShellCheck cannot follow that dynamic call boundary.
# shellcheck disable=SC2034
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
INSTALLER="${REPO_ROOT}/packaging/native/install.sh"
UNINSTALLER="${REPO_ROOT}/packaging/native/uninstall.sh"
VERIFY_SCRIPT="${REPO_ROOT}/scripts/verify-native-bundle-on-target.sh"
CONFIG_TUI="${REPO_ROOT}/crates/streamserver-config/src/main.rs"
NATIVE_WORKFLOW="${REPO_ROOT}/.github/workflows/server-native-bundles.yml"
SYSTEMD_TARGET_TEMPLATE="${REPO_ROOT}/packaging/native/templates/systemd/streamserver.target"
SYSTEMD_CORE_TEMPLATE="${REPO_ROOT}/packaging/native/templates/systemd/streamserver-core.service"
SYSTEMD_AGENT_TEMPLATE="${REPO_ROOT}/packaging/native/templates/systemd/streamserver-agent.service"
SYSTEMD_ZLM_TEMPLATE="${REPO_ROOT}/packaging/native/templates/systemd/streamserver-zlm.service"
ZLM_RENDERER="${REPO_ROOT}/packaging/native/templates/common/zlm.render-config.sh"
TMP_DIR="$(mktemp -d)"
trap 'rm -rf "${TMP_DIR}"' EXIT

FUNCTIONS_FILE="${TMP_DIR}/install-functions.sh"
sed '/^main "\$@"$/d' "${INSTALLER}" >"${FUNCTIONS_FILE}"
# shellcheck disable=SC1090
source "${FUNCTIONS_FILE}"
# Transaction tests replace systemctl with shell functions. Preserve the
# production timeout call boundary while dispatching to those test doubles.
timeout() {
  while [ "$#" -gt 0 ] && [[ "$1" == --* ]]; do shift; done
  [ "$#" -gt 0 ] && shift
  "$@"
}
set -x
trap 'cleanup_admin_password; rm -rf "${TMP_DIR}"' EXIT
ADMIN_HANDOFF_STATE_ROOT="${TMP_DIR}/installer-state"

# The installer accepts standard two-component locations, including its
# /home/streamserver default and /opt/streamserver-<instance>. Purge must accept
# those exact native identities while still rejecting a forged unit mapping.
(
  set +x
  UNINSTALL_FUNCTIONS_FILE="${TMP_DIR}/uninstall-functions.sh"
  sed '/^main "\$@"$/d' "${UNINSTALLER}" >"${UNINSTALL_FUNCTIONS_FILE}"
  # shellcheck disable=SC1090
  source "${UNINSTALL_FUNCTIONS_FILE}"
  INSTALL_DIR="${TMP_DIR}/native-uninstall-depth-two"
  mkdir -p "${INSTALL_DIR}"
  : >"${INSTALL_DIR}/.env"
  DEPLOY_MODE=native
  INSTALL_ROLE=all-in-one-host-cpu
  INSTANCE_NAME=r1-aio
  SYSTEMD_TARGET=ss-r1-aio.target
  SYSTEMD_CORE_UNIT=ss-r1-aio-core.service
  SYSTEMD_AGENT_UNIT=ss-r1-aio-agent.service
  SYSTEMD_ZLM_UNIT=ss-r1-aio-zlm.service
  SYSTEMD_POSTGRES_UNIT=ss-r1-aio-postgres.service
  component_count() { printf '%s\n' 2; }
  assert_trusted_install_control_boundary() { :; }
  assert_safe_install_dir_for_purge

  SYSTEMD_AGENT_UNIT=unrelated.service
  set +e
  FORGED_UNINSTALL_OUTPUT="$(
    (assert_safe_install_dir_for_purge) 2>&1
  )"
  FORGED_UNINSTALL_STATUS=$?
  set -e
  [ "${FORGED_UNINSTALL_STATUS}" -ne 0 ]
  printf '%s' "${FORGED_UNINSTALL_OUTPUT}" | grep -Fq \
    'native instance systemd identity does not match INSTANCE_NAME'
)

set +e
ADMIN_PASSWORD=parent-export-marker bash -c \
  'source "$1"; env | grep -q "^ADMIN_PASSWORD="' \
  native-password-env-probe "${FUNCTIONS_FILE}"
EARLY_PASSWORD_EXPORT_STATUS=$?
set -e
[ "${EARLY_PASSWORD_EXPORT_STATUS}" -ne 0 ] || {
  echo 'installer did not clear an inherited exported ADMIN_PASSWORD before subprocesses' >&2
  exit 1
}

set +e
AGENT_ENROLLMENT_TOKEN=parent-export-marker bash -c \
  'source "$1"; env | grep -q "^AGENT_ENROLLMENT_TOKEN="' \
  native-enrollment-env-probe "${FUNCTIONS_FILE}"
EARLY_ENROLLMENT_EXPORT_STATUS=$?
set -e
[ "${EARLY_ENROLLMENT_EXPORT_STATUS}" -ne 0 ] || {
  echo 'installer did not clear an inherited exported Agent enrollment token' >&2
  exit 1
}

set +e
bash -c 'set -a; export SHELLOPTS; exec bash -c '\''source "$1"; ADMIN_PASSWORD=presence-only; env | grep -q "^ADMIN_PASSWORD="'\'' inherited-allexport "$1"' \
  inherited-allexport-launcher "${FUNCTIONS_FILE}"
ALLEXPORT_PASSWORD_STATUS=$?
set -e
[ "${ALLEXPORT_PASSWORD_STATUS}" -ne 0 ] || {
  echo 'installer did not disable inherited allexport before sensitive assignments' >&2
  exit 1
}

assert_contains() {
  local haystack="$1"
  local needle="$2"
  printf '%s' "${haystack}" | grep -Fq -- "${needle}" || {
    printf 'expected output to contain %s\nactual output:\n%s\n' "${needle}" "${haystack}" >&2
    exit 1
  }
}

if grep -Fq 'mktemp "${TMPDIR:-/tmp}/streamserver-' "${INSTALLER}"; then
  echo 'privileged installer security inventories still trust caller TMPDIR' >&2
  exit 1
fi
(
  set +x
  adversarial_tmp="${TMP_DIR}/caller-controlled-tmp"
  mkdir -p "${adversarial_tmp}"
  chmod 777 "${adversarial_tmp}"
  TMPDIR="${adversarial_tmp}"
  EMULATED_SECURITY_METADATA=0
  selected_tmp_root="$(secure_installer_tmp_root)"
  if [ "${EUID}" -eq 0 ]; then
    [ "${selected_tmp_root}" = /run/streamserver-native-installer ]
    case "${selected_tmp_root}" in
      "${adversarial_tmp}"|"${adversarial_tmp}"/*)
        echo 'root installer selected a caller-controlled temporary directory' >&2
        exit 1
        ;;
    esac
    [ "$(stat -c '%u:%g:%a' -- "${selected_tmp_root}")" = 0:0:700 ]
  else
    [ "${selected_tmp_root}" = "$(realpath -e -- "${adversarial_tmp}")" ]
  fi
)

grep -Fq 'write_env_entry "${env_file}" AGENT_PUBLIC_MEDIA_ADDR' "${INSTALLER}"
grep -Fq \
  '[[ "${token}" =~ ^ssae1[.][A-Za-z0-9_-]{96}[.][A-Za-z0-9_-]{43}$ ]]' \
  "${INSTALLER}" || {
  echo 'all-in-one enrollment does not enforce the canonical 146-byte token wire' >&2
  exit 1
}
grep -Fq 'write_env_entry "${env_file}" AGENT_MANAGEMENT_ADDR' "${INSTALLER}"
grep -Fq 'write_env_entry "${env_file}" AGENT_ZLM_HOOK_ADDR' "${INSTALLER}"
grep -Fq 'write_env_entry "${env_file}" AGENT_ZLM_HOOK_PORT "${AGENT_ZLM_HOOK_PORT}"' \
  "${INSTALLER}"
grep -Fq \
  'assign_local_tcp_port AGENT_ZLM_HOOK_PORT "${existing_env_file}" "AGENT_ZLM_HOOK_PORT"' \
  "${INSTALLER}"
grep -Fq 'write_env_entry "${env_file}" AGENT_MANAGEMENT_PORT "${AGENT_MANAGEMENT_PORT}"' \
  "${INSTALLER}"
grep -Fq \
  'assign_local_tcp_port AGENT_MANAGEMENT_PORT "${existing_env_file}" "AGENT_MANAGEMENT_PORT"' \
  "${INSTALLER}"
grep -Fq \
  'write_env_entry "${env_file}" ZLM_API_BASE "http://127.0.0.1:${ZLM_HTTP_PORT}"' \
  "${INSTALLER}"
grep -Fq \
  'write_env_entry "${env_file}" ZLM_HOOK_BASE "http://127.0.0.1:${AGENT_ZLM_HOOK_PORT}/internal/zlm-hooks"' \
  "${INSTALLER}"
grep -Fq \
  'write_env_entry "${env_file}" ZLM_API_ALLOW_IP_RANGE "::1,127.0.0.1,10.0.0.0-10.255.255.255,172.16.0.0-172.31.255.255,192.168.0.0-192.168.255.255"' \
  "${INSTALLER}"
if grep -Fq \
  'write_env_entry "${env_file}" ZLM_API_ALLOW_IP_RANGE "::1,127.0.0.1"' \
  "${INSTALLER}"; then
  echo 'native installer would break remote media by making the shared ZLM HTTP listener loopback-only' >&2
  exit 1
fi
grep -Fq 'export ZLM_API_SECRET=verify-api-secret-0123456789abcdef' "${VERIFY_SCRIPT}"
grep -Fq 'export ZLM_HOOK_SHARED_SECRET=verify-hook-secret-0123456789abcde' \
  "${VERIFY_SCRIPT}"
grep -Fq \
  "export ZLM_API_ALLOW_IP_RANGE='::1,127.0.0.1,10.0.0.0-10.255.255.255,172.16.0.0-172.31.255.255,192.168.0.0-192.168.255.255'" \
  "${VERIFY_SCRIPT}"
if grep -Fq 'ZLM_HOOK_BASE "${CORE_HTTP_SCHEME}' "${INSTALLER}"; then
  echo 'native installer still sends ZLM hooks directly to Core' >&2
  exit 1
fi
for zlm_hook_name in \
  on_publish on_rtp_server_timeout on_record_mp4 on_record_ts on_record_hls \
  on_stream_none_reader on_stream_not_found on_server_keepalive on_server_started; do
  grep -Fq \
    "${zlm_hook_name}=__HOOK_BASE__/${zlm_hook_name}?secret=__HOOK_SHARED_SECRET__" \
    "${REPO_ROOT}/packaging/native/templates/common/zlm.config.ini.template" || {
    printf 'native ZLM template is missing fixed hook %s\n' "${zlm_hook_name}" >&2
    exit 1
  }
done
grep -Fqx 'UMask=0077' "${SYSTEMD_ZLM_TEMPLATE}"
grep -Fq -- '--log-dir __INSTALL_DIR__/data/media/logs' \
  "${SYSTEMD_ZLM_TEMPLATE}"
if grep -Fq 'ensure_control_directory "${INSTALL_DIR}/runtime/zlm/lib/log"' \
  "${INSTALLER}"; then
  echo 'native installer still directs ZLM logs into the immutable runtime tree' >&2
  exit 1
fi
grep -Fqx 'umask 077' "${ZLM_RENDERER}"
grep -Fq 'mktemp "${output_file}.tmp.XXXXXX"' "${ZLM_RENDERER}"
grep -Fq 'chmod 600 "${temporary_file}"' "${ZLM_RENDERER}"
grep -Fq 'sync "${temporary_file}"' "${ZLM_RENDERER}"
grep -Fq 'mv -fT -- "${temporary_file}" "${output_file}"' "${ZLM_RENDERER}"
grep -Fq 'sync "${output_dir}"' "${ZLM_RENDERER}"
if grep -Eq 'sed .*(__ZLM_API_SECRET__|__HOOK_SHARED_SECRET__)|escape_sed_replacement.*(ZLM_API_SECRET|ZLM_HOOK_SHARED_SECRET)' \
  "${ZLM_RENDERER}"; then
  echo 'ZLM renderer still exposes a secret through sed argv' >&2
  exit 1
fi

(
  set +x
  render_root="${TMP_DIR}/zlm-render-contract"
  output_file="${render_root}/config.ini"
  fake_bin="${render_root}/fake-bin"
  sync_compat_bin="${render_root}/sync-compat-bin"
  mkdir -p "${render_root}/www/record" "${render_root}/www/snap" "${fake_bin}" "${sync_compat_bin}"
  durable_sync="$(command -v sync)"
  case "$(uname -s)" in
    MINGW*|MSYS*)
      cat >"${sync_compat_bin}/sync" <<'EOF'
#!/usr/bin/env bash
# Git for Windows exposes sync(1), but its file/directory fsync emulation can
# return EACCES for otherwise writable test fixtures. Linux VM acceptance
# exercises the real durability path; this host-only contract shim preserves
# the renderer call ordering and failure-injection assertions.
exit 0
EOF
      chmod 755 "${sync_compat_bin}/sync"
      durable_sync="${sync_compat_bin}/sync"
      ;;
  esac
  export ZLM_API_SECRET='contract-api-secret-0123456789abcdef'
  export ZLM_HOOK_SHARED_SECRET='contract-hook-secret-0123456789abcdef'
  export ZLM_SERVER_ID='0190d8d4-31d2-7b23-b27e-8b9b28a2ed99'
  export ZLM_HOOK_BASE='http://127.0.0.1:18082/internal/zlm-hooks'
  export ZLM_API_ALLOW_IP_RANGE='::1,127.0.0.1,10.0.0.0-10.255.255.255,172.16.0.0-172.31.255.255,192.168.0.0-192.168.255.255'
  export ZLM_HTTP_PORT=18080 ZLM_HTTPS_PORT=18443
  export ZLM_RTMP_PORT=19350 ZLM_RTMPS_PORT=19351
  export ZLM_RTSP_PORT=18554 ZLM_RTSPS_PORT=18322
  export ZLM_RTP_PROXY_PORT=11000 ZLM_RTP_PROXY_PORT_RANGE=30000-30599
  export ZLM_RTC_SIGNALING_PORT=18081 ZLM_RTC_SIGNALING_SSL_PORT=18083
  export ZLM_RTC_ICE_PORT=18084 ZLM_RTC_ICE_TCP_PORT=18085
  export ZLM_RTC_PORT=18086 ZLM_RTC_TCP_PORT=18087 ZLM_RTC_PORT_RANGE=30600-30799
  export ZLM_SRT_PORT=19000 ZLM_SHELL_PORT=19001 ZLM_ONVIF_PORT=19002
  export ZLM_WWW_ROOT="${render_root}/www"
  export ZLM_RECORD_ROOT="${render_root}/www/record"
  export ZLM_SNAP_ROOT="${render_root}/www/snap"
  export ZLM_DEFAULT_PEM="${render_root}/default.pem"
  export AGENT_MP4_RECORD_SEGMENT_SEC=7200

  PATH="$(dirname "${durable_sync}"):${PATH}" "${ZLM_RENDERER}" \
    "${REPO_ROOT}/packaging/native/templates/common/zlm.config.ini.template" \
    "${output_file}"
  case "$(uname -s)" in
    MINGW*|MSYS*) : ;;
    *) [ "$(stat -c %a "${output_file}")" = 600 ] ;;
  esac
  grep -Fq "secret=${ZLM_API_SECRET}" "${output_file}"
  grep -Fq "?secret=${ZLM_HOOK_SHARED_SECRET}" "${output_file}"

  printf '%s\n' '#!/usr/bin/env bash' 'exit 73' >"${fake_bin}/sync"
  chmod 755 "${fake_bin}/sync"
  printf '%s\n' 'known-good-config' >"${output_file}"
  chmod 600 "${output_file}"
  set +e
  PATH="${fake_bin}:${PATH}" "${ZLM_RENDERER}" \
    "${REPO_ROOT}/packaging/native/templates/common/zlm.config.ini.template" \
    "${output_file}" >/dev/null 2>&1
  render_failure_status=$?
  set -e
  [ "${render_failure_status}" -ne 0 ]
  [ "$(cat "${output_file}")" = 'known-good-config' ]
  ! compgen -G "${output_file}.tmp.*" >/dev/null
  ! compgen -G "${output_file}.previous.*" >/dev/null

  sync_counter_file="${render_root}/sync-counter"
  cat >"${fake_bin}/sync" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
count=0
[ ! -f "${SYNC_COUNTER_FILE}" ] || count="$(<"${SYNC_COUNTER_FILE}")"
count=$((count + 1))
printf '%s\n' "${count}" >"${SYNC_COUNTER_FILE}"
[ "${count}" -ne 3 ] || exit 73
exec "${REAL_SYNC}" "$@"
EOF
  chmod 755 "${fake_bin}/sync"
  rm -f -- "${sync_counter_file}"
  printf '%s\n' 'known-good-config' >"${output_file}"
  chmod 600 "${output_file}"
  set +e
  REAL_SYNC="${durable_sync}" SYNC_COUNTER_FILE="${sync_counter_file}" \
    PATH="${fake_bin}:${PATH}" "${ZLM_RENDERER}" \
    "${REPO_ROOT}/packaging/native/templates/common/zlm.config.ini.template" \
    "${output_file}" >/dev/null 2>&1
  post_rename_failure_status=$?
  set -e
  [ "${post_rename_failure_status}" -ne 0 ]
  [ "$(cat "${output_file}")" = 'known-good-config' ]
  [ "$(<"${sync_counter_file}")" -eq 4 ]
  ! compgen -G "${output_file}.tmp.*" >/dev/null
  ! compgen -G "${output_file}.previous.*" >/dev/null
)
agent_hook_timeout="$(awk -F= '
  {
    key=$1
    gsub(/[[:space:]\r]/, "", key)
    if (key == "zlm_hook_timeout_sec") {
      value=$2
      gsub(/[[:space:]\r]/, "", value)
      print value
      exit
    }
  }
' "${REPO_ROOT}/config/base.toml")"
zlm_hook_timeout="$(awk -F= '
  {
    line=$0
    sub(/\r$/, "", line)
    if (line == "[hook]") {
      in_hook=1
      next
    }
    if (line ~ /^\[/) {
      in_hook=0
    }
    key=$1
    gsub(/[[:space:]\r]/, "", key)
    if (in_hook && key == "timeoutSec") {
      value=$2
      gsub(/[[:space:]\r]/, "", value)
      print value
      exit
    }
  }
' "${REPO_ROOT}/packaging/native/templates/common/zlm.config.ini.template")"
[[ "${agent_hook_timeout}" =~ ^[0-9]+$ ]] \
  && [[ "${zlm_hook_timeout}" =~ ^[0-9]+$ ]] \
  && [ "${agent_hook_timeout}" -gt 0 ] \
  && [ "${agent_hook_timeout}" -lt "${zlm_hook_timeout}" ] || {
  echo 'Agent hook relay timeout must be positive and strictly below ZLM hook.timeoutSec' >&2
  exit 1
}
if grep -Fq 'write_env_entry "${env_file}" ZLM_API_HOST' "${INSTALLER}"; then
  echo 'native installer still emits the legacy ZLM_API_HOST field' >&2
  exit 1
fi
if grep -Fq 'ZLM_API_BASE "http://${PUBLIC_HOST}' "${INSTALLER}"; then
  echo 'native installer still exposes the Agent-to-ZLM control path through PUBLIC_HOST' >&2
  exit 1
fi
if grep -Fq 'write_env_entry "${env_file}" AGENT_HTTP_ADDR' "${INSTALLER}"; then
  echo 'installer still emits legacy AGENT_HTTP_ADDR listener configuration' >&2
  exit 1
fi
if grep -Fq 'worker 角色必须提供与 control-plane 一致的 Hook/API 密钥' "${INSTALLER}" \
  || grep -Fq 'write_env_entry "${env_file}" ZLM_API_SECRET "${HOOK_SHARED_SECRET' "${INSTALLER}"; then
  echo 'worker still shares the Core hook credential with the local ZLM API' >&2
  exit 1
fi
grep -Fq 'ZLM_API_SECRET="$(read_config_scalar ZLM_API_SECRET)"' "${INSTALLER}"
if grep -Fq 'getStatistic?secret=${HOOK_SHARED_SECRET' "${INSTALLER}"; then
  echo 'ZLM readiness still uses the Core hook credential' >&2
  exit 1
fi
if grep -Fq 'write_env_entry "${env_file}" CORE_INSECURE_DEV' "${INSTALLER}"; then
  echo 'fresh native environments still emit legacy CORE_INSECURE_DEV' >&2
  exit 1
fi

(
  RESERVED_LOCAL_TCP_PORTS=""
  prompt_non_empty() { printf '%s' "$2"; }
  describe_tcp_port_usage() { :; }
  assign_local_tcp_port FIRST_PORT "${TMP_DIR}/no-existing-port-env" \
    FIRST_PORT 'first local listener' 8443
  assign_local_tcp_port SECOND_PORT "${TMP_DIR}/no-existing-port-env" \
    SECOND_PORT 'second local listener' 8443 2>/dev/null
  [ "${FIRST_PORT}" = 8443 ]
  [ "${SECOND_PORT}" = 8444 ]
  [ "${RESERVED_LOCAL_TCP_PORTS}" = '8443 8444' ] || {
    echo 'same installer session did not retain both selected local TCP ports' >&2
    exit 1
  }
)

grep -Fqx \
  'ExecStart=/usr/bin/env STREAMSERVER_ENV=production STREAMSERVER_UI_DIR=__INSTALL_DIR__/ui __INSTALL_DIR__/bin/media-core' \
  "${SYSTEMD_CORE_TEMPLATE}" || {
  echo 'steady Core unit does not force production mode and the trusted UI root at exec time' >&2
  exit 1
}
grep -Fqx \
  'ExecStart=/usr/bin/env STREAMSERVER_ENV=production __INSTALL_DIR__/bin/media-agent' \
  "${SYSTEMD_AGENT_TEMPLATE}" || {
  echo 'steady Agent unit does not force production mode at exec time' >&2
  exit 1
}
if grep -Fq 'Environment=STREAMSERVER_ENV=' \
  "${SYSTEMD_CORE_TEMPLATE}" "${SYSTEMD_AGENT_TEMPLATE}"; then
  echo 'steady units still rely on Environment= values that EnvironmentFile can override' >&2
  exit 1
fi

for core_identity_key in \
  CORE_GRPC_TLS_SERVER_CA_PATH \
  CORE_AGENT_CA_CERT_PATH \
  CORE_AGENT_CA_KEY_PATH \
  CORE_AGENT_CAPABILITY_JWT_PRIVATE_KEY_PATH \
  CORE_AGENT_CAPABILITY_JWT_PUBLIC_KEY_PATH \
  CORE_INSTANCE_ID \
  CORE_AGENT_MANAGEMENT_CLIENT_CERT_PATH \
  CORE_AGENT_MANAGEMENT_CLIENT_KEY_PATH \
  CORE_AGENT_MANAGEMENT_CA_PATH; do
  grep -Fq "write_env_entry \"\${env_file}\" ${core_identity_key}" "${INSTALLER}" || {
    echo "installer does not persist required Core internal PKI setting: ${core_identity_key}" >&2
    exit 1
  }
done
grep -Fq 'ensure_core_internal_pki' "${INSTALLER}" || {
  echo 'installer does not provision the three-root Core internal PKI' >&2
  exit 1
}
grep -Fq 'run_agent_enrollment_if_needed' "${INSTALLER}" || {
  echo 'installer does not gate worker startup on one-time Agent enrollment' >&2
  exit 1
}

for valid_internal_pki_host in \
  localhost media-core.example.test a-b.example.test \
  0.0.0.0 127.0.0.1 255.255.255.255 \
  ::1 2001:db8::1 2001:db8:0:1:2:3:4:5 ::ffff:192.0.2.10; do
  validate_internal_pki_host "${valid_internal_pki_host}" || {
    echo "valid internal PKI host was rejected: ${valid_internal_pki_host}" >&2
    exit 1
  }
done
LONG_DNS_LABEL="$(printf 'a%.0s' {1..64})"
LONG_DNS_NAME="$(printf 'a%.0s' {1..63}).$(printf 'b%.0s' {1..63}).$(printf 'c%.0s' {1..63}).$(printf 'd%.0s' {1..62})"
for invalid_internal_pki_host in \
  '' '-bad.example' 'bad-.example' 'good.-bad.example' 'good-.bad.example' \
  '.example' 'example.' 'a..b' 'bad_name.example' '*.example' '999.1.1.1' '1.2.3' \
  ':::' '1:2:3:4:5:6:7:8:9' '1::2::3' \
  "${LONG_DNS_LABEL}.example" "${LONG_DNS_NAME}"; do
  if validate_internal_pki_host "${invalid_internal_pki_host}"; then
    echo "invalid internal PKI host was accepted: ${invalid_internal_pki_host}" >&2
    exit 1
  fi
done

(
  INSTALL_DIR="${TMP_DIR}/unsafe-data-root-install"
  OUTSIDE_DATA_TARGET="${TMP_DIR}/unsafe-data-root-target"
  mkdir -p "${INSTALL_DIR}" "${OUTSIDE_DATA_TARGET}"
  ln -s "${OUTSIDE_DATA_TARGET}" "${INSTALL_DIR}/data"
  path_is_symbolic_link_status() { [ "$1" = "${INSTALL_DIR}/data" ]; }
  set +e
  (prepare_layout) >/dev/null 2>&1
  UNSAFE_DATA_ROOT_STATUS=$?
  set -e
  if find "${OUTSIDE_DATA_TARGET}" -mindepth 1 -print -quit | grep -q .; then
    echo 'prepare_layout followed the managed data symlink outside the installation' >&2
    exit 1
  fi
  [ "${UNSAFE_DATA_ROOT_STATUS}" -ne 0 ] || {
    echo 'prepare_layout accepted a managed data symlink' >&2
    exit 1
  }
)

for unsafe_relative_path in data/agent data/postgres data/zlm/www/output; do
  unsafe_case_name="${unsafe_relative_path//\//-}"
  (
    INSTALL_DIR="${TMP_DIR}/unsafe-${unsafe_case_name}-install"
    OUTSIDE_DATA_TARGET="${TMP_DIR}/unsafe-${unsafe_case_name}-target"
    mkdir -p \
      "$(dirname "${INSTALL_DIR}/${unsafe_relative_path}")" \
      "${OUTSIDE_DATA_TARGET}"
    ln -s "${OUTSIDE_DATA_TARGET}" "${INSTALL_DIR}/${unsafe_relative_path}"
    path_is_symbolic_link_status() {
      [ "$1" = "${INSTALL_DIR}/${unsafe_relative_path}" ]
    }
    set +e
    (assert_managed_data_paths_safe) >/dev/null 2>&1
    UNSAFE_MANAGED_PATH_STATUS=$?
    set -e
    [ "${UNSAFE_MANAGED_PATH_STATUS}" -ne 0 ] || {
      echo "managed data symlink unexpectedly passed: ${unsafe_relative_path}" >&2
      exit 1
    }
  )
done

(
  INSTALL_DIR="${TMP_DIR}/worker-identity-parent-layout"
  SERVICE_USER=streamserver
  SERVICE_GROUP=streamserver
  PERMISSION_LOG="${TMP_DIR}/worker-identity-parent-layout.permissions"
  posix_metadata_available=1
  case "$(uname -s)" in
    MINGW*|MSYS*)
      posix_metadata_available=0
      install() {
        [ "${1:-}" = -d ] || { command install "$@"; return; }
        mkdir -p -- "${!#}"
      }
      chmod() { :; }
      ;;
  esac
  mkdir -p "${INSTALL_DIR}"
  prepare_layout
  [ -d "${INSTALL_DIR}/data/agent" ]
  [ ! -L "${INSTALL_DIR}/data/agent" ]
  : >"${PERMISSION_LOG}"
  chown() { printf '%s\n' "$*" >>"${PERMISSION_LOG}"; }
  fix_permissions
  if [ "${posix_metadata_available}" -eq 1 ]; then
    [ "$(stat -c '%a' "${INSTALL_DIR}/data/agent")" = 700 ]
  fi
  grep -Fqx -- \
    "-h ${SERVICE_USER}:${SERVICE_GROUP} ${INSTALL_DIR}/data/agent" \
    "${PERMISSION_LOG}"
)

for guarded_operation in prepare_layout fix_permissions initialize_postgres_if_needed; do
  (
    INSTALL_DIR="${TMP_DIR}/guarded-${guarded_operation}-install"
    OUTSIDE_DATA_TARGET="${TMP_DIR}/guarded-${guarded_operation}-outside"
    ROOT_MUTATIONS="${TMP_DIR}/guarded-${guarded_operation}.mutations"
    SERVICE_USER=streamserver
    SERVICE_GROUP=streamserver
    DATABASE_MODE=bundled
    : >"${ROOT_MUTATIONS}"
    mkdir -p "${INSTALL_DIR}/data" "${OUTSIDE_DATA_TARGET}"
    ln -s "${OUTSIDE_DATA_TARGET}" "${INSTALL_DIR}/data/postgres"
    path_is_symbolic_link_status() {
      [ "$1" = "${INSTALL_DIR}/data/postgres" ]
    }
    chown() { printf 'chown %s\n' "$*" >>"${ROOT_MUTATIONS}"; }
    set +e
    ("${guarded_operation}") >/dev/null 2>&1
    GUARDED_OPERATION_STATUS=$?
    set -e
    [ "${GUARDED_OPERATION_STATUS}" -ne 0 ] || {
      echo "${guarded_operation} accepted a managed data symlink" >&2
      exit 1
    }
    [ ! -s "${ROOT_MUTATIONS}" ] || {
      echo "${guarded_operation} performed root mutation after a data symlink" >&2
      exit 1
    }
    [ -z "$(find "${OUTSIDE_DATA_TARGET}" -mindepth 1 -print -quit)" ]
  )
done

(
  INSTALL_DIR="${TMP_DIR}/unsafe-data-nondirectory-install"
  mkdir -p "${INSTALL_DIR}/data"
  : >"${INSTALL_DIR}/data/postgres-run"
  set +e
  (assert_managed_data_paths_safe) >/dev/null 2>&1
  UNSAFE_DATA_NON_DIRECTORY_STATUS=$?
  set -e
  [ "${UNSAFE_DATA_NON_DIRECTORY_STATUS}" -ne 0 ] || {
    echo 'managed data non-directory unexpectedly passed' >&2
    exit 1
  }
)

(
  INSTALL_DIR="${TMP_DIR}/unsafe-certificate-tree-install"
  mkdir -p "${INSTALL_DIR}/certs/auth"
  certificate_tree_has_unsafe_entries_status() { return 0; }
  set +e
  (assert_certificate_tree_safe) >/dev/null 2>&1
  UNSAFE_CERTIFICATE_TREE_STATUS=$?
  set -e
  [ "${UNSAFE_CERTIFICATE_TREE_STATUS}" -ne 0 ] || {
    echo 'unsafe certificate tree unexpectedly passed' >&2
    exit 1
  }
)

write_upgrade_identity_env() {
  local install_dir="$1"
  local unit_root="${install_dir}/systemd-unit-root"
  mkdir -p "${install_dir}" "${unit_root}"
  printf '%s\n' \
    'INSTALL_ROLE=control-plane' \
    'INSTANCE_NAME=contract-identity' \
    'SYSTEMD_TARGET=ss-contract-identity.target' \
    'SYSTEMD_CORE_UNIT=ss-contract-identity-core.service' \
    'SYSTEMD_AGENT_UNIT=ss-contract-identity-agent.service' \
    'SYSTEMD_ZLM_UNIT=ss-contract-identity-zlm.service' \
    'SYSTEMD_POSTGRES_UNIT=ss-contract-identity-postgres.service' \
    >"${install_dir}/.env"
  printf '%s\n' \
    '[Service]' \
    "WorkingDirectory=${install_dir}" \
    "EnvironmentFile=${install_dir}/.env" \
    "ExecStart=/usr/bin/env STREAMSERVER_ENV=production STREAMSERVER_UI_DIR=${install_dir}/ui ${install_dir}/bin/media-core" \
    '[Install]' \
    'WantedBy=ss-contract-identity.target' \
    >"${unit_root}/ss-contract-identity-core.service"
  printf '%s\n' \
    '[Unit]' \
    'Description=trusted identity target' \
    '[Install]' \
    'WantedBy=multi-user.target' \
    >"${unit_root}/ss-contract-identity.target"
}

convert_core_upgrade_identity_to_legacy_unit() {
  local install_dir="$1"
  local unit_file="${install_dir}/systemd-unit-root/ss-contract-identity-core.service"
  sed -i \
    "s|ExecStart=/usr/bin/env STREAMSERVER_ENV=production STREAMSERVER_UI_DIR=${install_dir}/ui ${install_dir}/bin/media-core|ExecStart=${install_dir}/bin/media-core|" \
    "${unit_file}"
  sed -i \
    "/^EnvironmentFile=/a Environment=STREAMSERVER_ENV=production\nEnvironment=STREAMSERVER_UI_DIR=${install_dir}/ui" \
    "${unit_file}"
}

write_worker_upgrade_identity() {
  local install_dir="$1"
  local role="$2"
  local include_gpu_preflight="$3"
  local unit_root="${install_dir}/systemd-unit-root"
  mkdir -p "${install_dir}" "${unit_root}"
  printf '%s\n' \
    "INSTALL_ROLE=${role}" \
    'INSTANCE_NAME=contract-worker' \
    'SYSTEMD_TARGET=ss-contract-worker.target' \
    'SYSTEMD_CORE_UNIT=ss-contract-worker-core.service' \
    'SYSTEMD_AGENT_UNIT=ss-contract-worker-agent.service' \
    'SYSTEMD_ZLM_UNIT=ss-contract-worker-zlm.service' \
    'SYSTEMD_POSTGRES_UNIT=ss-contract-worker-postgres.service' \
    >"${install_dir}/.env"
  printf '%s\n' '[Install]' 'WantedBy=multi-user.target' \
    >"${unit_root}/ss-contract-worker.target"
  {
    printf '%s\n' \
      '[Service]' \
      "WorkingDirectory=${install_dir}" \
      "EnvironmentFile=${install_dir}/.env"
    if [ "${include_gpu_preflight}" = 1 ]; then
      printf '%s\n' \
        'ExecStartPre=/usr/bin/nvidia-smi' \
        'ExecStartPre=/bin/sh -c h264_nvenc' \
        'ExecStartPre=/bin/sh -c hevc_nvenc'
    fi
    printf '%s\n' \
      "ExecStart=/usr/bin/env STREAMSERVER_ENV=production ${install_dir}/bin/media-agent" \
      '[Install]' \
      'WantedBy=ss-contract-worker.target'
  } >"${unit_root}/ss-contract-worker-agent.service"
  printf '%s\n' \
    '[Service]' \
    "WorkingDirectory=${install_dir}/runtime/zlm" \
    "EnvironmentFile=${install_dir}/.env" \
    "ExecStart=${install_dir}/bin/zlm-mediaserver -s default.pem" \
    '[Install]' \
    'WantedBy=ss-contract-worker.target' \
    >"${unit_root}/ss-contract-worker-zlm.service"
}

convert_worker_upgrade_identity_to_legacy_unit() {
  local install_dir="$1"
  local unit_file="${install_dir}/systemd-unit-root/ss-contract-worker-agent.service"
  sed -i \
    "s|ExecStart=/usr/bin/env STREAMSERVER_ENV=production ${install_dir}/bin/media-agent|ExecStart=${install_dir}/bin/media-agent|" \
    "${unit_file}"
  sed -i \
    '/^EnvironmentFile=/a Environment=STREAMSERVER_ENV=production' \
    "${unit_file}"
}

run_worker_cli_identity_check() {
  local install_dir="$1"
  local role="$2"
  (
    INSTALL_DIR="${install_dir}"
    INSTALL_ROLE="${role}"
    INSTANCE_NAME=contract-worker
    INSTALL_ROLE_WAS_EXPLICIT=1
    INSTANCE_NAME_WAS_EXPLICIT=1
    BUNDLE_WORKER_SUPPORT=true
    BUNDLE_GPU_SUPPORT=true
    SYSTEMD_UNIT_ROOT="${install_dir}/systemd-unit-root"
    stat() {
      if [ "${1:-}" = -c ]; then
        case "${2:-}" in
          %u) printf '%s\n' 0; return 0 ;;
          %a) printf '%s\n' 755; return 0 ;;
        esac
      fi
      command stat "$@"
    }
    prepare_upgrade_cli_identity
  )
}

run_upgrade_identity_main() {
  local install_dir="$1"
  local effects_file="$2"
  shift 2
  (
    SYSTEMD_UNIT_ROOT="${install_dir}/systemd-unit-root"
    load_manifest() {
      MEDIA_CORE_BINARY_PATH=bin/media-core
      BUNDLE_WORKER_SUPPORT=true
      BUNDLE_GPU_SUPPORT=true
    }
    ensure_prerequisites() { :; }
    verify_package_checksums() { :; }
    assert_no_docker_assets() { :; }
    ensure_root_for_install() { :; }
    stat() {
      if [ "${1:-}" = -c ]; then
        case "${2:-}" in
          %u) printf '%s\n' 0; return 0 ;;
          %a) printf '%s\n' 755; return 0 ;;
        esac
      fi
      command stat "$@"
    }
    prepare_upgrade_security_gate() {
      prepare_upgrade_cli_identity
      validate_sealed_upgrade_environment_identity
      printf '%s\n' 'stop ss-contract-identity-core.service' >>"${effects_file}"
      exit 0
    }
    run_install_with_external_flocks() {
      prepare_upgrade_security_gate "${install_dir}/bin/media-core"
    }
    main --upgrade --install-dir "${install_dir}" "$@"
  )
}

UPGRADE_IDENTITY_DIR="${TMP_DIR}/upgrade-identity"
UPGRADE_IDENTITY_EFFECTS="${TMP_DIR}/upgrade-identity.effects"
write_upgrade_identity_env "${UPGRADE_IDENTITY_DIR}"

for mismatch_args in \
  '--role worker-host-cpu --instance-name contract-identity' \
  '--role control-plane --instance-name contract-other'; do
  : >"${UPGRADE_IDENTITY_EFFECTS}"
  read -r -a mismatch_argv <<<"${mismatch_args}"
  set +e
  run_upgrade_identity_main \
    "${UPGRADE_IDENTITY_DIR}" "${UPGRADE_IDENTITY_EFFECTS}" \
    "${mismatch_argv[@]}" >/dev/null 2>&1
  UPGRADE_IDENTITY_STATUS=$?
  set -e
  [ "${UPGRADE_IDENTITY_STATUS}" -ne 0 ] || {
    echo "mismatched upgrade identity unexpectedly passed: ${mismatch_args}" >&2
    exit 1
  }
  [ ! -s "${UPGRADE_IDENTITY_EFFECTS}" ] || {
    echo "mismatched upgrade identity touched the old Core: ${mismatch_args}" >&2
    exit 1
  }
done

: >"${UPGRADE_IDENTITY_EFFECTS}"
run_upgrade_identity_main \
  "${UPGRADE_IDENTITY_DIR}" "${UPGRADE_IDENTITY_EFFECTS}" \
  --role control-plane --instance-name contract-identity
[ "$(cat "${UPGRADE_IDENTITY_EFFECTS}")" = 'stop ss-contract-identity-core.service' ]

LEGACY_UNIT_IDENTITY_DIR="${TMP_DIR}/legacy-upgrade-unit-identity"
LEGACY_UNIT_IDENTITY_EFFECTS="${TMP_DIR}/legacy-upgrade-unit-identity.effects"
write_upgrade_identity_env "${LEGACY_UNIT_IDENTITY_DIR}"
convert_core_upgrade_identity_to_legacy_unit "${LEGACY_UNIT_IDENTITY_DIR}"
: >"${LEGACY_UNIT_IDENTITY_EFFECTS}"
run_upgrade_identity_main \
  "${LEGACY_UNIT_IDENTITY_DIR}" "${LEGACY_UNIT_IDENTITY_EFFECTS}" \
  --role control-plane --instance-name contract-identity
[ "$(cat "${LEGACY_UNIT_IDENTITY_EFFECTS}")" = 'stop ss-contract-identity-core.service' ]

LEGACY_UNIT_ARGS_DIR="${TMP_DIR}/legacy-upgrade-unit-args"
LEGACY_UNIT_ARGS_EFFECTS="${TMP_DIR}/legacy-upgrade-unit-args.effects"
write_upgrade_identity_env "${LEGACY_UNIT_ARGS_DIR}"
convert_core_upgrade_identity_to_legacy_unit "${LEGACY_UNIT_ARGS_DIR}"
sed -i \
  "s|ExecStart=${LEGACY_UNIT_ARGS_DIR}/bin/media-core|ExecStart=${LEGACY_UNIT_ARGS_DIR}/bin/media-core --insecure-dev|" \
  "${LEGACY_UNIT_ARGS_DIR}/systemd-unit-root/ss-contract-identity-core.service"
: >"${LEGACY_UNIT_ARGS_EFFECTS}"
set +e
run_upgrade_identity_main \
  "${LEGACY_UNIT_ARGS_DIR}" "${LEGACY_UNIT_ARGS_EFFECTS}" \
  --role control-plane --instance-name contract-identity \
  >/dev/null 2>&1
LEGACY_UNIT_ARGS_STATUS=$?
set -e
[ "${LEGACY_UNIT_ARGS_STATUS}" -ne 0 ]
[ ! -s "${LEGACY_UNIT_ARGS_EFFECTS}" ]

MIXED_UNIT_ENV_DIR="${TMP_DIR}/mixed-upgrade-unit-environment"
MIXED_UNIT_ENV_EFFECTS="${TMP_DIR}/mixed-upgrade-unit-environment.effects"
write_upgrade_identity_env "${MIXED_UNIT_ENV_DIR}"
sed -i \
  '/^EnvironmentFile=/a Environment=STREAMSERVER_ENV=production' \
  "${MIXED_UNIT_ENV_DIR}/systemd-unit-root/ss-contract-identity-core.service"
: >"${MIXED_UNIT_ENV_EFFECTS}"
set +e
run_upgrade_identity_main \
  "${MIXED_UNIT_ENV_DIR}" "${MIXED_UNIT_ENV_EFFECTS}" \
  --role control-plane --instance-name contract-identity \
  >/dev/null 2>&1
MIXED_UNIT_ENV_STATUS=$?
set -e
[ "${MIXED_UNIT_ENV_STATUS}" -ne 0 ]
[ ! -s "${MIXED_UNIT_ENV_EFFECTS}" ]

: >"${UPGRADE_IDENTITY_EFFECTS}"
set +e
run_upgrade_identity_main \
  "${UPGRADE_IDENTITY_DIR}" "${UPGRADE_IDENTITY_EFFECTS}" \
  >/dev/null 2>&1
MISSING_EXPLICIT_IDENTITY_STATUS=$?
set -e
[ "${MISSING_EXPLICIT_IDENTITY_STATUS}" -ne 0 ]
[ ! -s "${UPGRADE_IDENTITY_EFFECTS}" ]

UNIT_MISMATCH_DIR="${TMP_DIR}/upgrade-unit-mismatch"
UNIT_MISMATCH_EFFECTS="${TMP_DIR}/upgrade-unit-mismatch.effects"
write_upgrade_identity_env "${UNIT_MISMATCH_DIR}"
sed -i \
  "s|ExecStart=/usr/bin/env STREAMSERVER_ENV=production STREAMSERVER_UI_DIR=${UNIT_MISMATCH_DIR}/ui ${UNIT_MISMATCH_DIR}/bin/media-core|ExecStart=/usr/bin/env STREAMSERVER_ENV=production ${UNIT_MISMATCH_DIR}/bin/media-agent|" \
  "${UNIT_MISMATCH_DIR}/systemd-unit-root/ss-contract-identity-core.service"
: >"${UNIT_MISMATCH_EFFECTS}"
set +e
run_upgrade_identity_main \
  "${UNIT_MISMATCH_DIR}" "${UNIT_MISMATCH_EFFECTS}" \
  --role control-plane --instance-name contract-identity \
  >/dev/null 2>&1
UNIT_MISMATCH_STATUS=$?
set -e
[ "${UNIT_MISMATCH_STATUS}" -ne 0 ]
[ ! -s "${UNIT_MISMATCH_EFFECTS}" ]

GPU_TOPOLOGY_DIR="${TMP_DIR}/upgrade-gpu-topology"
write_worker_upgrade_identity "${GPU_TOPOLOGY_DIR}" worker-host-gpu 0
set +e
run_worker_cli_identity_check "${GPU_TOPOLOGY_DIR}" worker-host-gpu >/dev/null 2>&1
GPU_TOPOLOGY_STATUS=$?
set -e
[ "${GPU_TOPOLOGY_STATUS}" -ne 0 ] || {
  echo 'GPU role passed without root-managed GPU unit preflight' >&2
  exit 1
}

CPU_TOPOLOGY_DIR="${TMP_DIR}/upgrade-cpu-topology"
write_worker_upgrade_identity "${CPU_TOPOLOGY_DIR}" worker-host-cpu 1
set +e
run_worker_cli_identity_check "${CPU_TOPOLOGY_DIR}" worker-host-cpu >/dev/null 2>&1
CPU_TOPOLOGY_STATUS=$?
set -e
[ "${CPU_TOPOLOGY_STATUS}" -ne 0 ] || {
  echo 'CPU role passed with root-managed GPU unit preflight' >&2
  exit 1
}

GPU_TOPOLOGY_VALID_DIR="${TMP_DIR}/upgrade-gpu-topology-valid"
write_worker_upgrade_identity "${GPU_TOPOLOGY_VALID_DIR}" worker-host-gpu 1
run_worker_cli_identity_check \
  "${GPU_TOPOLOGY_VALID_DIR}" worker-host-gpu >/dev/null

LEGACY_CPU_UNIT_DIR="${TMP_DIR}/upgrade-legacy-cpu-unit"
write_worker_upgrade_identity "${LEGACY_CPU_UNIT_DIR}" worker-host-cpu 0
convert_worker_upgrade_identity_to_legacy_unit "${LEGACY_CPU_UNIT_DIR}"
run_worker_cli_identity_check \
  "${LEGACY_CPU_UNIT_DIR}" worker-host-cpu >/dev/null

LEGACY_GPU_UNIT_DIR="${TMP_DIR}/upgrade-legacy-gpu-unit"
write_worker_upgrade_identity "${LEGACY_GPU_UNIT_DIR}" worker-host-gpu 1
convert_worker_upgrade_identity_to_legacy_unit "${LEGACY_GPU_UNIT_DIR}"
run_worker_cli_identity_check \
  "${LEGACY_GPU_UNIT_DIR}" worker-host-gpu >/dev/null

LEGACY_AGENT_ARGS_DIR="${TMP_DIR}/upgrade-legacy-agent-args"
write_worker_upgrade_identity "${LEGACY_AGENT_ARGS_DIR}" worker-host-cpu 0
convert_worker_upgrade_identity_to_legacy_unit "${LEGACY_AGENT_ARGS_DIR}"
sed -i \
  "s|ExecStart=${LEGACY_AGENT_ARGS_DIR}/bin/media-agent|ExecStart=${LEGACY_AGENT_ARGS_DIR}/bin/media-agent --insecure-dev|" \
  "${LEGACY_AGENT_ARGS_DIR}/systemd-unit-root/ss-contract-worker-agent.service"
set +e
run_worker_cli_identity_check \
  "${LEGACY_AGENT_ARGS_DIR}" worker-host-cpu >/dev/null 2>&1
LEGACY_AGENT_ARGS_STATUS=$?
set -e
[ "${LEGACY_AGENT_ARGS_STATUS}" -ne 0 ]

MIXED_AGENT_ENV_DIR="${TMP_DIR}/upgrade-mixed-agent-environment"
write_worker_upgrade_identity "${MIXED_AGENT_ENV_DIR}" worker-host-cpu 0
sed -i \
  '/^EnvironmentFile=/a Environment=STREAMSERVER_ENV=production' \
  "${MIXED_AGENT_ENV_DIR}/systemd-unit-root/ss-contract-worker-agent.service"
set +e
run_worker_cli_identity_check \
  "${MIXED_AGENT_ENV_DIR}" worker-host-cpu >/dev/null 2>&1
MIXED_AGENT_ENV_STATUS=$?
set -e
[ "${MIXED_AGENT_ENV_STATUS}" -ne 0 ]

UNTRUSTED_IDENTITY_DIR="${TMP_DIR}/upgrade-untrusted-env-identity"
UNTRUSTED_IDENTITY_EFFECTS="${TMP_DIR}/upgrade-untrusted-env-identity.effects"
write_upgrade_identity_env "${UNTRUSTED_IDENTITY_DIR}"
sed -i \
  -e 's/INSTALL_ROLE=control-plane/INSTALL_ROLE=worker-host-cpu/' \
  -e 's/contract-identity/forged-identity/g' \
  "${UNTRUSTED_IDENTITY_DIR}/.env"
: >"${UNTRUSTED_IDENTITY_EFFECTS}"
set +e
run_upgrade_identity_main \
  "${UNTRUSTED_IDENTITY_DIR}" "${UNTRUSTED_IDENTITY_EFFECTS}" \
  --role control-plane --instance-name contract-identity \
  >/dev/null 2>&1
UNTRUSTED_IDENTITY_STATUS=$?
set -e
[ "${UNTRUSTED_IDENTITY_STATUS}" -ne 0 ] || {
  echo 'service-writable environment identity overrode root-managed systemd identity' >&2
  exit 1
}
[ ! -s "${UNTRUSTED_IDENTITY_EFFECTS}" ] || {
  echo 'untrusted environment identity touched the old Core' >&2
  exit 1
}

DUPLICATE_IDENTITY_DIR="${TMP_DIR}/upgrade-duplicate-env-identity"
DUPLICATE_IDENTITY_EFFECTS="${TMP_DIR}/upgrade-duplicate-env-identity.effects"
write_upgrade_identity_env "${DUPLICATE_IDENTITY_DIR}"
printf '%s\n' '  INSTANCE_NAME=contract-identity' >>"${DUPLICATE_IDENTITY_DIR}/.env"
: >"${DUPLICATE_IDENTITY_EFFECTS}"
set +e
run_upgrade_identity_main \
  "${DUPLICATE_IDENTITY_DIR}" "${DUPLICATE_IDENTITY_EFFECTS}" \
  --role control-plane --instance-name contract-identity \
  >/dev/null 2>&1
DUPLICATE_IDENTITY_STATUS=$?
set -e
[ "${DUPLICATE_IDENTITY_STATUS}" -ne 0 ]
[ ! -s "${DUPLICATE_IDENTITY_EFFECTS}" ]

(
  INSTALL_DIR="${TMP_DIR}/post-tui-identity"
  write_upgrade_identity_env "${INSTALL_DIR}"
  sed -i 's/INSTANCE_NAME=contract-identity/INSTANCE_NAME=renamed-by-tui/' \
    "${INSTALL_DIR}/.env"
  set +e
  (validate_identity_after_optional_tui \
    control-plane contract-identity ss-contract-identity) >/dev/null 2>&1
  POST_TUI_IDENTITY_STATUS=$?
  set -e
  [ "${POST_TUI_IDENTITY_STATUS}" -ne 0 ] || {
    echo 'post-TUI identity mutation unexpectedly passed' >&2
    exit 1
  }
)

(
  UNIT_BASENAME="ss-role-contract"
  INSTALL_ROLE="control-plane"
  [ "$(non_database_units_for_role)" = "ss-role-contract-core.service" ]
  INSTALL_ROLE="worker-host-cpu"
  [ "$(non_database_units_for_role)" = $'ss-role-contract-zlm.service\nss-role-contract-agent.service' ]
  INSTALL_ROLE="all-in-one-host-cpu"
  [ "$(non_database_units_for_role)" = $'ss-role-contract-core.service\nss-role-contract-zlm.service\nss-role-contract-agent.service' ]
)

(
  INSTALL_DIR="${TMP_DIR}/streamserverctl-health"
  mkdir -p "${INSTALL_DIR}/bin"
  : >"${INSTALL_DIR}/.env"
  write_env_entry "${INSTALL_DIR}/.env" INSTALL_ROLE control-plane
  write_env_entry "${INSTALL_DIR}/.env" INSTANCE_NAME health
  write_env_entry "${INSTALL_DIR}/.env" CORE_HTTP_PORT 8080
  write_env_entry "${INSTALL_DIR}/.env" CORE_HTTP_TLS_CERT_PATH ''
  write_env_entry "${INSTALL_DIR}/.env" SYSTEMD_TARGET ss-health.target
  write_env_entry "${INSTALL_DIR}/.env" SYSTEMD_CORE_UNIT ss-health-core.service
  write_env_entry "${INSTALL_DIR}/.env" SYSTEMD_POSTGRES_UNIT ss-health-postgres.service
  write_env_entry "${INSTALL_DIR}/.env" SYSTEMD_AGENT_UNIT ss-health-agent.service
  write_env_entry "${INSTALL_DIR}/.env" SYSTEMD_ZLM_UNIT ss-health-zlm.service
  write_env_entry \
    "${INSTALL_DIR}/.env" HOOK_SHARED_SECRET 0123456789abcdef0123456789abcdef
  write_streamserverctl
  systemctl() { return 0; }
  curl() {
    [ "${1:-}" = -q ] || return 90
    case " $* " in *" --noproxy * "*) ;; *) return 91 ;; esac
    case " $* " in *" --connect-timeout 2 "*) ;; *) return 92 ;; esac
    case " $* " in *" --max-time 4 "*) ;; *) return 93 ;; esac
    case " $* " in *" --proto =http,https "*) ;; *) return 94 ;; esac
    [ "${HEALTH_CURL_FAIL:-0}" -eq 0 ]
  }
  export -f systemctl curl

  set +e
  HEALTH_CURL_FAIL=1 "${INSTALL_DIR}/bin/streamserverctl" health >/dev/null 2>&1
  HEALTH_FAILURE_STATUS=$?
  HEALTH_CURL_FAIL=1 probe_upgrade_readiness
  UPGRADE_PROBE_FAILURE_STATUS=$?
  set -e
  [ "${HEALTH_FAILURE_STATUS}" -ne 0 ]
  [ "${UPGRADE_PROBE_FAILURE_STATUS}" -ne 0 ]
  HEALTH_CURL_FAIL=0 "${INSTALL_DIR}/bin/streamserverctl" health >/dev/null
  HEALTH_CURL_FAIL=0 probe_upgrade_readiness
)

# Worker readiness must not require the Core hook credential and must feed the
# local ZLM API secret over curl config stdin, never through process argv.
(
  INSTALL_DIR="${TMP_DIR}/streamserverctl-worker-health"
  ZLM_READINESS_CALLS="${TMP_DIR}/streamserverctl-worker-health.calls"
  ZLM_READINESS_SECRET=abcdef0123456789abcdef0123456789
  mkdir -p "${INSTALL_DIR}/bin"
  : >"${INSTALL_DIR}/.env"
  : >"${ZLM_READINESS_CALLS}"
  write_env_entry "${INSTALL_DIR}/.env" INSTALL_ROLE worker-host-cpu
  write_env_entry "${INSTALL_DIR}/.env" INSTANCE_NAME worker-health
  write_env_entry "${INSTALL_DIR}/.env" AGENT_HTTP_PORT 18081
  write_env_entry "${INSTALL_DIR}/.env" ZLM_HTTP_PORT 18080
  write_env_entry "${INSTALL_DIR}/.env" ZLM_API_SECRET "${ZLM_READINESS_SECRET}"
  write_env_entry "${INSTALL_DIR}/.env" SYSTEMD_TARGET ss-worker-health.target
  write_env_entry "${INSTALL_DIR}/.env" SYSTEMD_CORE_UNIT ss-worker-health-core.service
  write_env_entry "${INSTALL_DIR}/.env" SYSTEMD_POSTGRES_UNIT ss-worker-health-postgres.service
  write_env_entry "${INSTALL_DIR}/.env" SYSTEMD_AGENT_UNIT ss-worker-health-agent.service
  write_env_entry "${INSTALL_DIR}/.env" SYSTEMD_ZLM_UNIT ss-worker-health-zlm.service
  write_streamserverctl
  systemctl() { return 0; }
  curl() {
    local argument config_payload
    [ "${1:-}" = -q ] || return 89
    case " $* " in *" --noproxy * "*) ;; *) return 90 ;; esac
    case " $* " in *" --connect-timeout 2 "*) ;; *) return 91 ;; esac
    case " $* " in *" --max-time 4 "*) ;; *) return 92 ;; esac
    case " $* " in *" --proto =http,https "*) ;; *) return 93 ;; esac
    for argument in "$@"; do
      [[ "${argument}" != *"${ZLM_READINESS_SECRET}"* ]] || return 91
    done
    if [ "${*: -2}" = '--config -' ]; then
      config_payload="$(cat)"
      [ "${config_payload}" = \
        "url = \"http://127.0.0.1:18080/index/api/getStatistic?secret=${ZLM_READINESS_SECRET}\"" ] \
        || return 92
      printf '%s\n' zlm-config-stdin >>"${ZLM_READINESS_CALLS}"
      printf '%s\n' '{"code":0,"data":{}}'
    else
      printf '%s\n' agent-no-secret >>"${ZLM_READINESS_CALLS}"
    fi
  }
  export ZLM_READINESS_CALLS ZLM_READINESS_SECRET
  export -f systemctl curl

  fake_home="${TMP_DIR}/streamserverctl-fake-home"
  mkdir -p "${fake_home}"
  printf '%s\n' 'proxy = http://127.0.0.1:9' >"${fake_home}/.curlrc"
  HOME="${fake_home}" HTTP_PROXY=http://127.0.0.1:9 \
    HTTPS_PROXY=http://127.0.0.1:9 \
    "${INSTALL_DIR}/bin/streamserverctl" health >/dev/null
  [ "$(sort "${ZLM_READINESS_CALLS}" | tr '\n' ' ')" = \
    'agent-no-secret zlm-config-stdin ' ]
  [ "$(env_key_occurrence_count "${INSTALL_DIR}/.env" HOOK_SHARED_SECRET)" -eq 0 ]
)

# Upgrade readiness probes exactly the components that were active before
# quiesce. Local curl probes ignore curlrc/proxies, use hard timeouts, and keep
# the ZLM secret exclusively in config stdin; PostgreSQL uses pg_isready.
(
  set +x
  INSTALL_DIR="${TMP_DIR}/upgrade-component-readiness"
  UNIT_BASENAME=ss-component-readiness
  UPGRADE_ACTIVE_UNITS=(
    ss-component-readiness-core.service
    ss-component-readiness-agent.service
    ss-component-readiness-zlm.service
    ss-component-readiness-postgres.service
  )
  READINESS_CALLS="${TMP_DIR}/upgrade-component-readiness.calls"
  READINESS_SECRET=abcdef0123456789abcdef0123456789
  mkdir -p "${INSTALL_DIR}/bin"
  : >"${INSTALL_DIR}/.env"
  : >"${READINESS_CALLS}"
  for entry in \
    'CORE_HTTP_PORT 18443' \
    'CORE_HTTP_TLS_CERT_PATH certs/http.pem' \
    'AGENT_HTTP_PORT 18081' \
    'ZLM_HTTP_PORT 18080' \
    "ZLM_API_SECRET ${READINESS_SECRET}" \
    'POSTGRES_PORT 55432' \
    'POSTGRES_USER streamserver' \
    'POSTGRES_DB streamserver'; do
    read -r key value <<<"${entry}"
    write_env_entry "${INSTALL_DIR}/.env" "${key}" "${value}"
  done
  printf '%s\n' \
    '#!/usr/bin/env bash' \
    'set -euo pipefail' \
    '[ "$#" -eq 10 ]' \
    '[ "$1" = -h ] && [ "$2" = 127.0.0.1 ]' \
    '[ "$3" = -p ] && [ "$4" = 55432 ]' \
    '[ "$5" = -U ] && [ "$6" = streamserver ]' \
    '[ "$7" = -d ] && [ "$8" = streamserver ]' \
    '[ "$9" = -t ] && [[ "${10}" =~ ^[1-9][0-9]*$ ]]' \
    'printf "%s\n" postgres >>"${READINESS_CALLS}"' \
    >"${INSTALL_DIR}/bin/pg_isready"
  chmod 755 "${INSTALL_DIR}/bin/pg_isready"
  curl() {
    local argument payload url="${!#}"
    [ "${1:-}" = -q ] || return 81
    case " $* " in *" --noproxy * "*) ;; *) return 82 ;; esac
    case " $* " in *" --connect-timeout "*" --max-time "*) ;; *) return 83 ;; esac
    case " $* " in *" --proto =http,https "*) ;; *) return 84 ;; esac
    for argument in "$@"; do
      [[ "${argument}" != *"${READINESS_SECRET}"* ]] || return 85
    done
    if [ "${*: -2}" = '--config -' ]; then
      payload="$(cat)"
      [ "${payload}" = \
        "url = \"http://127.0.0.1:18080/index/api/getStatistic?secret=${READINESS_SECRET}\"" ] \
        || return 86
      printf '%s\n' zlm >>"${READINESS_CALLS}"
      printf '%s\n' '{"code":0,"data":{}}'
    elif [ "${url}" = https://127.0.0.1:18443/health/ready ]; then
      case " $* " in *" -k "*) ;; *) return 87 ;; esac
      printf '%s\n' core >>"${READINESS_CALLS}"
    elif [ "${url}" = http://127.0.0.1:18081/health/ready ]; then
      printf '%s\n' agent >>"${READINESS_CALLS}"
    else
      return 88
    fi
  }
  systemctl() {
    case "$*" in
      *'show --property ActiveState --value'*) printf '%s\n' active ;;
      *) return 0 ;;
    esac
  }
  export READINESS_CALLS READINESS_SECRET
  export -f curl
  fake_home="${TMP_DIR}/upgrade-component-readiness-home"
  mkdir -p "${fake_home}"
  printf '%s\n' 'proxy = http://127.0.0.1:9' >"${fake_home}/.curlrc"
  prepare_upgrade_readiness_configuration 0
  for readiness_unit in "${UPGRADE_ACTIVE_UNITS[@]}"; do
    probe_upgrade_component_readiness_once "${readiness_unit}" 4 || {
      printf 'single component readiness probe failed: %s\n' \
        "${readiness_unit}" >&2
      exit 1
    }
  done
  : >"${READINESS_CALLS}"
  HOME="${fake_home}" HTTP_PROXY=http://127.0.0.1:9 \
    HTTPS_PROXY=http://127.0.0.1:9 \
    probe_upgrade_active_components_readiness || {
      printf 'component readiness calls before failure:\n%s\n' \
        "$(cat "${READINESS_CALLS}")" >&2
      exit 1
    }
  [ "$(sort "${READINESS_CALLS}" | tr '\n' ' ')" = \
    'agent core postgres zlm ' ]

  : >"${READINESS_CALLS}"
  UPGRADE_ACTIVE_UNITS=(ss-component-readiness-core.service)
  write_env_entry "${INSTALL_DIR}/.env" CORE_HTTP_PORT 65535
  set +e
  probe_upgrade_active_components_readiness >/dev/null 2>&1
  duplicate_readiness_status=$?
  set -e
  [ "${duplicate_readiness_status}" -ne 0 ]
  [ ! -s "${READINESS_CALLS}" ]
)

# ZLMediaKit reports authentication failures in a JSON code while retaining
# HTTP 200. Readiness must require exactly one numeric code=0 response.
(
  set +x
  UNIT_BASENAME=ss-zlm-json-readiness
  UPGRADE_READINESS_ZLM_PORT=18080
  UPGRADE_READINESS_ZLM_SECRET=abcdef0123456789abcdef0123456789
  curl() {
    cat >/dev/null
    printf '%s\n' "${ZLM_JSON_RESPONSE}"
  }
  ZLM_JSON_RESPONSE='{"code":-100,"msg":"secret error"}'
  set +e
  probe_upgrade_component_readiness_once \
    ss-zlm-json-readiness-zlm.service 2 >/dev/null 2>&1
  bad_zlm_json_status=$?
  set -e
  [ "${bad_zlm_json_status}" -ne 0 ]
  ZLM_JSON_RESPONSE='{"code":0,"data":{}}'
  probe_upgrade_component_readiness_once \
    ss-zlm-json-readiness-zlm.service 2 >/dev/null
  ZLM_JSON_RESPONSE='{"code":0,"nested":{"code":0}}'
  set +e
  probe_upgrade_component_readiness_once \
    ss-zlm-json-readiness-zlm.service 2 >/dev/null 2>&1
  duplicate_zlm_code_status=$?
  set -e
  [ "${duplicate_zlm_code_status}" -ne 0 ]
)

# streamserverctl is invoked by root during upgrade readiness. It must parse
# the service environment as data and fail closed instead of sourcing it.
(
  INSTALL_DIR="${TMP_DIR}/streamserverctl-malicious-env"
  SENTINEL="${TMP_DIR}/streamserverctl-root-command-ran"
  mkdir -p "${INSTALL_DIR}/bin"
  write_streamserverctl
  : >"${INSTALL_DIR}/.env"
  write_env_entry "${INSTALL_DIR}/.env" INSTALL_ROLE control-plane
  write_env_entry "${INSTALL_DIR}/.env" INSTANCE_NAME contract-malicious
  write_env_entry "${INSTALL_DIR}/.env" CORE_HTTP_PORT 8080
  write_env_entry "${INSTALL_DIR}/.env" CORE_HTTP_TLS_CERT_PATH ''
  write_env_entry \
    "${INSTALL_DIR}/.env" SYSTEMD_TARGET ss-contract-malicious.target
  write_env_entry \
    "${INSTALL_DIR}/.env" SYSTEMD_CORE_UNIT ss-contract-malicious-core.service
  write_env_entry \
    "${INSTALL_DIR}/.env" SYSTEMD_POSTGRES_UNIT ss-contract-malicious-postgres.service
  write_env_entry \
    "${INSTALL_DIR}/.env" SYSTEMD_AGENT_UNIT ss-contract-malicious-agent.service
  write_env_entry \
    "${INSTALL_DIR}/.env" SYSTEMD_ZLM_UNIT ss-contract-malicious-zlm.service
  write_env_entry \
    "${INSTALL_DIR}/.env" HOOK_SHARED_SECRET "\$(touch ${SENTINEL})"
  systemctl() { return 0; }
  curl() { return 0; }
  export -f systemctl curl

  set +e
  "${INSTALL_DIR}/bin/streamserverctl" health >/dev/null 2>&1
  MALICIOUS_CTL_STATUS=$?
  set -e
  [ ! -e "${SENTINEL}" ] || {
    echo 'streamserverctl executed shell syntax from the service environment' >&2
    exit 1
  }
  [ "${MALICIOUS_CTL_STATUS}" -ne 0 ] || {
    echo 'streamserverctl accepted a shell-expression hook secret' >&2
    exit 1
  }
  if grep -Fq '. "${INSTALL_DIR}/.env"' "${INSTALL_DIR}/bin/streamserverctl"; then
    echo 'streamserverctl still sources the service environment' >&2
    exit 1
  fi
)

(
  SAFE_ENV_FILE="${TMP_DIR}/safe-env-writer.env"
  : >"${SAFE_ENV_FILE}"
  write_env_entry \
    "${SAFE_ENV_FILE}" HOOK_SHARED_SECRET '$(touch must-remain-literal)'
  grep -Fq \
    "HOOK_SHARED_SECRET='\$(touch must-remain-literal)'" \
    "${SAFE_ENV_FILE}"
)

(
  INSTALL_DIR="${TMP_DIR}/atomic-env-replacement"
  INSTALL_ROLE=unknown-contract-role
  INSTANCE_NAME=atomic-env
  UNIT_BASENAME=ss-atomic-env
  mkdir -p "${INSTALL_DIR}"
  printf '%s\n' old-env-inode >"${INSTALL_DIR}/.env"
  exec 8>>"${INSTALL_DIR}/.env"
  write_env_file
  printf '%s\n' stale-env-fd-write >&8
  exec 8>&-
  ! grep -Fq stale-env-fd-write "${INSTALL_DIR}/.env"
  grep -Fq "INSTANCE_NAME='atomic-env'" "${INSTALL_DIR}/.env"
)

# The all-in-one role passes through both the Core and worker writers. Its
# published EnvironmentFile must still be a strict scalar map because
# streamserverctl rejects duplicate keys instead of silently choosing one.
(
  set +u
  INSTALL_DIR="${TMP_DIR}/all-in-one-unique-env"
  INSTALL_ROLE=all-in-one-host-cpu
  INSTANCE_NAME=contract-aio-env
  UNIT_BASENAME=ss-contract-aio-env
  EMULATED_SECURITY_METADATA=1
  CORE_HTTP_PORT=18443
  CORE_GRPC_PORT=15051
  CORE_HTTP_TLS_CERT_PATH=''
  HOOK_SHARED_SECRET=0123456789abcdef0123456789abcdef
  ZLM_API_SECRET=abcdef0123456789abcdef0123456789
  ZLM_HOOK_SHARED_SECRET=fedcba9876543210fedcba9876543210
  AGENT_HTTP_PORT=18081
  ZLM_HTTP_PORT=18080
  mkdir -p "${INSTALL_DIR}/bin"

  write_env_file
  set -u

  DUPLICATE_AIO_KEYS="$(awk -F= '
    /^[A-Z0-9_]+=/ { counts[$1] += 1 }
    END {
      for (key in counts) {
        if (counts[key] != 1) print key "=" counts[key]
      }
    }
  ' "${INSTALL_DIR}/.env" | sort)"
  [ -z "${DUPLICATE_AIO_KEYS}" ] || {
    printf 'all-in-one environment contains duplicate scalar keys:\n%s\n' \
      "${DUPLICATE_AIO_KEYS}" >&2
    exit 1
  }
  for unique_key in CORE_HTTP_PORT CORE_GRPC_PORT HOOK_SHARED_SECRET; do
    [ "$(env_key_occurrence_count "${INSTALL_DIR}/.env" "${unique_key}")" -eq 1 ]
  done

  write_streamserverctl
  systemctl() { return 0; }
  export -f systemctl
  "${INSTALL_DIR}/bin/streamserverctl" status >/dev/null
)

(
  INSTALL_DIR="${TMP_DIR}/sealed-upgrade-env-replacement"
  mkdir -p "${INSTALL_DIR}"
  printf '%s\n' 'INSTALL_ROLE=control-plane' >"${INSTALL_DIR}/.env"
  exec 4>>"${INSTALL_DIR}/.env"
  chown() { :; }
  seal_legacy_upgrade_environment
  printf '%s\n' stale-sealed-env-fd-write >&4
  exec 4>&-
  ! grep -Fq stale-sealed-env-fd-write "${INSTALL_DIR}/.env"
  grep -Fq INSTALL_ROLE=control-plane "${INSTALL_DIR}/.env"
)

(
  INSTALL_DIR="${TMP_DIR}/atomic-ctl-replacement"
  mkdir -p "${INSTALL_DIR}/bin"
  printf '%s\n' old-ctl-inode >"${INSTALL_DIR}/bin/streamserverctl"
  exec 9>>"${INSTALL_DIR}/bin/streamserverctl"
  write_streamserverctl
  printf '%s\n' stale-ctl-fd-write >&9
  exec 9>&-
  ! grep -Fq stale-ctl-fd-write "${INSTALL_DIR}/bin/streamserverctl"
  "${REAL_BASH:-bash}" -n "${INSTALL_DIR}/bin/streamserverctl"
)

(
  INSTALL_DIR="${TMP_DIR}/atomic-wrapper-replacement"
  mkdir -p "${INSTALL_DIR}/bin" "${INSTALL_DIR}/runtime/lib"
  printf '%s\n' '#!/usr/bin/env sh' 'exit 0' >"${INSTALL_DIR}/runtime/tool"
  chmod 755 "${INSTALL_DIR}/runtime/tool"
  printf '%s\n' old-wrapper-inode >"${INSTALL_DIR}/bin/tool"
  exec 7>>"${INSTALL_DIR}/bin/tool"
  write_runtime_wrapper \
    "${INSTALL_DIR}/bin/tool" \
    "${INSTALL_DIR}/runtime/tool" \
    "${INSTALL_DIR}/runtime/lib"
  printf '%s\n' stale-wrapper-fd-write >&7
  exec 7>&-
  ! grep -Fq stale-wrapper-fd-write "${INSTALL_DIR}/bin/tool"
  "${REAL_BASH:-bash}" -n "${INSTALL_DIR}/bin/tool"
)

(
  INSTALL_DIR="${TMP_DIR}/atomic-render-replacement"
  INSTALL_ROLE=unknown-contract-role
  INSTANCE_NAME=atomic-render
  UNIT_BASENAME=ss-atomic-render
  DATABASE_MODE=external
  SERVICE_USER=streamserver
  SERVICE_GROUP=streamserver
  mkdir -p "${INSTALL_DIR}/systemd"
  RENDER_SOURCE="${TMP_DIR}/atomic-render.template"
  RENDER_TARGET="${INSTALL_DIR}/systemd/atomic.service"
  printf '%s\n' 'WorkingDirectory=__INSTALL_DIR__' >"${RENDER_SOURCE}"
  printf '%s\n' old-render-inode >"${RENDER_TARGET}"
  exec 6>>"${RENDER_TARGET}"
  render_template "${RENDER_SOURCE}" "${RENDER_TARGET}"
  printf '%s\n' stale-render-fd-write >&6
  exec 6>&-
  ! grep -Fq stale-render-fd-write "${RENDER_TARGET}"
  grep -Fq "WorkingDirectory=${INSTALL_DIR}" "${RENDER_TARGET}"
)

(
  INSTALL_DIR="${TMP_DIR}/atomic-certificate-replacement"
  SERVICE_GROUP=streamserver
  mkdir -p "${INSTALL_DIR}/certs/auth"
  chown() { :; }
  CERT_STATE="${INSTALL_DIR}/certs/auth/state.pem"
  printf '%s\n' preserved-certificate-data >"${CERT_STATE}"
  exec 5>>"${CERT_STATE}"
  seal_certificate_tree
  printf '%s\n' stale-certificate-fd-write >&5
  exec 5>&-
  ! grep -Fq stale-certificate-fd-write "${CERT_STATE}"
  grep -Fq preserved-certificate-data "${CERT_STATE}"
)

(
  INSTALL_DIR="${TMP_DIR}/root-only-internal-ca-keys"
  SERVICE_GROUP=streamserver
  CHOWN_LOG="${TMP_DIR}/root-only-internal-ca-keys.chown"
  CONTROL_CA_KEY="${INSTALL_DIR}/certs/internal/control-plane-server-ca-key.pem"
  MANAGEMENT_CA_KEY="${INSTALL_DIR}/certs/internal/management-client-ca-key.pem"
  SERVICE_KEY="${INSTALL_DIR}/certs/internal/core-grpc-server-key.pem"
  mkdir -p "${INSTALL_DIR}/certs/internal"
  printf '%s\n' control-ca-secret >"${CONTROL_CA_KEY}"
  printf '%s\n' management-ca-secret >"${MANAGEMENT_CA_KEY}"
  printf '%s\n' service-key >"${SERVICE_KEY}"
  chmod 600 "${CONTROL_CA_KEY}" "${MANAGEMENT_CA_KEY}" "${SERVICE_KEY}"
  : >"${CHOWN_LOG}"
  finish_atomic_target_write() {
    local temporary_file="$1"
    local target="$2"
    local mode="$3"
    local owner_group="$4"
    printf '%s %s %s\n' "${mode}" "${owner_group}" "${target}" >>"${CHOWN_LOG}"
    chmod "${mode}" "${temporary_file}"
    mv -f -- "${temporary_file}" "${target}"
  }
  chown() { :; }

  seal_certificate_tree

  case "$(uname -s)" in
    MINGW*|MSYS*) : ;;
    *)
      [ "$(stat -c '%a' "${CONTROL_CA_KEY}")" = 600 ]
      [ "$(stat -c '%a' "${MANAGEMENT_CA_KEY}")" = 600 ]
      [ "$(stat -c '%a' "${SERVICE_KEY}")" = 640 ]
      ;;
  esac
  grep -Fqx "600 root:root ${CONTROL_CA_KEY}" "${CHOWN_LOG}"
  grep -Fqx "600 root:root ${MANAGEMENT_CA_KEY}" "${CHOWN_LOG}"
  grep -Fqx "640 root:${SERVICE_GROUP} ${SERVICE_KEY}" "${CHOWN_LOG}"
)

(
  INSTALL_DIR="${TMP_DIR}/control-target-symlink"
  OUTSIDE_CONTROL_TARGET="${TMP_DIR}/outside-control-target"
  mkdir -p "${INSTALL_DIR}/bin"
  printf '%s\n' outside-must-not-change >"${OUTSIDE_CONTROL_TARGET}"
  ln -s "${OUTSIDE_CONTROL_TARGET}" "${INSTALL_DIR}/bin/streamserverctl"
  if [ -L "${INSTALL_DIR}/bin/streamserverctl" ]; then
    set +e
    (write_streamserverctl) >/dev/null 2>&1
    CONTROL_SYMLINK_STATUS=$?
    set -e
    [ "${CONTROL_SYMLINK_STATUS}" -ne 0 ]
    [ "$(cat "${OUTSIDE_CONTROL_TARGET}")" = outside-must-not-change ]
  fi
)

# Pending handoff recovery runs before legacy ownership is hardened. It must
# pass only parsed auth values to the package binary and never source old env.
(
  LEGACY_AUTH_ENV="${TMP_DIR}/legacy-service-writable.env"
  LEGACY_AUTH_SENTINEL="${TMP_DIR}/legacy-auth-root-command-ran"
  LEGACY_AUTH_CORE="${TMP_DIR}/legacy-auth-core"
  printf '%s\n' \
    '#!/usr/bin/env bash' \
    'set -euo pipefail' \
    '[ "${STREAMSERVER_ENV:-}" = production ]' \
    '[ "${DATABASE_URL:-}" = postgresql://127.0.0.1/streamserver ]' \
    '[ "${AUTH_MODE:-}" = local_password ]' \
    '! env | grep -q "^UNPERSISTED_CORE_SETTING="' >"${LEGACY_AUTH_CORE}"
  chmod 755 "${LEGACY_AUTH_CORE}"
  printf '%s\n' \
    'DATABASE_URL=postgresql://127.0.0.1/streamserver' \
    'AUTH_MODE=local_password' \
    'AUTH_JWT_PRIVATE_KEY_PATH=certs/auth/private.pem' \
    'AUTH_JWT_PUBLIC_KEY_PATH=certs/auth/public.pem' \
    "HOOK_SHARED_SECRET=\$(touch ${LEGACY_AUTH_SENTINEL})" >"${LEGACY_AUTH_ENV}"
  export UNPERSISTED_CORE_SETTING=parent-only-value

  set +e
  run_core_auth_from_installed_env \
    "${LEGACY_AUTH_ENV}" "${LEGACY_AUTH_CORE}" auth check-config
  LEGACY_AUTH_STATUS=$?
  set -e
  [ ! -e "${LEGACY_AUTH_SENTINEL}" ] || {
    echo 'pending handoff auth probe executed shell syntax from the legacy environment' >&2
    exit 1
  }
  [ "${LEGACY_AUTH_STATUS}" -eq 0 ]

  for duplicate_auth_key in AUTH_MODE DATABASE_URL; do
    DUPLICATE_AUTH_ENV="${TMP_DIR}/legacy-auth-duplicate-${duplicate_auth_key}.env"
    cp "${LEGACY_AUTH_ENV}" "${DUPLICATE_AUTH_ENV}"
    printf '%s=%s\n' "${duplicate_auth_key}" forged >>"${DUPLICATE_AUTH_ENV}"
    set +e
    DUPLICATE_AUTH_OUTPUT="$(run_core_auth_from_installed_env \
      "${DUPLICATE_AUTH_ENV}" "${LEGACY_AUTH_CORE}" auth check-config 2>&1)"
    DUPLICATE_AUTH_STATUS=$?
    set -e
    [ "${DUPLICATE_AUTH_STATUS}" -ne 0 ] || {
      echo "duplicate ${duplicate_auth_key} unexpectedly reached the Core auth probe" >&2
      exit 1
    }
    assert_contains "${DUPLICATE_AUTH_OUTPUT}" \
      "[INVALID] configuration: ${duplicate_auth_key} must appear at most once"
  done
)

# An upgrade records the exact running set, quiesces it only after preflight,
# and restores that exact set. Starting an already-active target is
# intentionally modeled as a no-op here.
(
  UPGRADE=1
  START_AFTER_INSTALL=1
  INSTALL_ROLE="control-plane"
  INSTALL_DIR="${TMP_DIR}/active-upgrade"
  INSTANCE_NAME="contract-upgrade"
  UNIT_BASENAME="ss-contract-upgrade"
  TRUSTED_POSTGRES_UNIT_COUNT=1
  UPGRADE_TARGET_WAS_ACTIVE=0
  UPGRADE_ACTIVE_UNITS=()
  UPGRADE_ACTIVE_MAIN_PIDS=()
  TARGET_ACTIVE=1
  CORE_ACTIVE=1
  POSTGRES_ACTIVE=1
  ACTIVATION_PHASE=0
  READINESS_CHECKED=0
  SYSTEMCTL_CALLS="${TMP_DIR}/active-upgrade-systemctl.calls"
  mkdir -p "${INSTALL_DIR}/bin"
  : >"${SYSTEMCTL_CALLS}"

  systemctl() {
    printf '%s\n' "$*" >>"${SYSTEMCTL_CALLS}"
    case "$1" in
      is-active)
        case "${!#}" in
          "${UNIT_BASENAME}.target") [ "${TARGET_ACTIVE}" -eq 1 ] ;;
          "${UNIT_BASENAME}-core.service") [ "${CORE_ACTIVE}" -eq 1 ] ;;
          "${UNIT_BASENAME}-postgres.service") [ "${POSTGRES_ACTIVE}" -eq 1 ] ;;
          *) return 3 ;;
        esac
        ;;
      show)
        case "$*" in
          *'--property ActiveState'*"${UNIT_BASENAME}.target")
            [ "${TARGET_ACTIVE}" -eq 1 ] && printf '%s\n' active || printf '%s\n' inactive
            ;;
          *'--property ActiveState'*"${UNIT_BASENAME}-core.service")
            [ "${CORE_ACTIVE}" -eq 1 ] && printf '%s\n' active || printf '%s\n' inactive
            ;;
          *'--property ActiveState'*"${UNIT_BASENAME}-postgres.service")
            [ "${POSTGRES_ACTIVE}" -eq 1 ] && printf '%s\n' active || printf '%s\n' inactive
            ;;
          *'--property MainPID'*"${UNIT_BASENAME}-core.service")
            if [ "${CORE_ACTIVE}" -eq 0 ]; then
              printf '%s\n' 0
            elif [ "${ACTIVATION_PHASE}" -eq 0 ]; then
              printf '%s\n' 111
            else
              printf '%s\n' 222
            fi
            ;;
          *'--property MainPID'*"${UNIT_BASENAME}-postgres.service")
            if [ "${POSTGRES_ACTIVE}" -eq 0 ]; then
              printf '%s\n' 0
            elif [ "${ACTIVATION_PHASE}" -eq 0 ]; then
              printf '%s\n' 311
            else
              printf '%s\n' 322
            fi
            ;;
          *) return 1 ;;
        esac
        ;;
      stop)
        shift
        for unit in "$@"; do
          case "${unit}" in
            "${UNIT_BASENAME}.target") TARGET_ACTIVE=0 ;;
            "${UNIT_BASENAME}-core.service") CORE_ACTIVE=0 ;;
            "${UNIT_BASENAME}-postgres.service") POSTGRES_ACTIVE=0 ;;
          esac
        done
        ;;
      start)
        shift
        ACTIVATION_PHASE=1
        for unit in "$@"; do
          case "${unit}" in
            "${UNIT_BASENAME}.target") TARGET_ACTIVE=1 ;;
            "${UNIT_BASENAME}-core.service") CORE_ACTIVE=1 ;;
            "${UNIT_BASENAME}-postgres.service") POSTGRES_ACTIVE=1 ;;
          esac
        done
        ;;
      *) return 0 ;;
    esac
  }
  probe_upgrade_active_components_readiness() {
    READINESS_CHECKED=1
  }

  capture_and_quiesce_upgrade_services
  [ "${TARGET_ACTIVE}" -eq 0 ]
  [ "${CORE_ACTIVE}" -eq 0 ]
  [ "${POSTGRES_ACTIVE}" -eq 0 ]
  [ "${UPGRADE_TARGET_WAS_ACTIVE}" -eq 1 ]
  [ "${UPGRADE_ACTIVE_UNITS[*]}" = \
    "${UNIT_BASENAME}-core.service ${UNIT_BASENAME}-postgres.service" ]
  [ "${UPGRADE_ACTIVE_MAIN_PIDS[*]}" = "111 311" ]

  start_services_if_requested
  [ "${TARGET_ACTIVE}" -eq 1 ]
  [ "${CORE_ACTIVE}" -eq 1 ]
  [ "${POSTGRES_ACTIVE}" -eq 1 ]
  [ "${READINESS_CHECKED}" -eq 1 ]
  grep -Fq \
    "stop ${UNIT_BASENAME}-core.service ${UNIT_BASENAME}.target" \
    "${SYSTEMCTL_CALLS}"
  grep -Fq \
    "stop ${UNIT_BASENAME}-postgres.service" \
    "${SYSTEMCTL_CALLS}"
  grep -Fq \
    "start --job-mode=ignore-dependencies ${UNIT_BASENAME}.target" \
    "${SYSTEMCTL_CALLS}"
  grep -Fq \
    "start ${UNIT_BASENAME}-core.service ${UNIT_BASENAME}-postgres.service" \
    "${SYSTEMCTL_CALLS}"
)

# An originally inactive bundled PostgreSQL may be started only for the
# read-only upgrade gate.  A pre-quiesce failure must stop it again without
# touching the application units.
(
  set +x
  INSTALL_ROLE=control-plane
  UNIT_BASENAME=ss-preflight-db
  TRUSTED_POSTGRES_UNIT_COUNT=1
  UPGRADE_ACTIVE_UNITS=()
  UPGRADE_PREFLIGHT_POSTGRES_STARTED=0
  PREFLIGHT_DB_ACTIVE=0
  PREFLIGHT_DB_CALLS="${TMP_DIR}/preflight-db.calls"
  : >"${PREFLIGHT_DB_CALLS}"
  bounded_upgrade_systemctl() {
    shift
    printf '%s\n' "$*" >>"${PREFLIGHT_DB_CALLS}"
    case "$1" in
      start) PREFLIGHT_DB_ACTIVE=1 ;;
      stop) PREFLIGHT_DB_ACTIVE=0 ;;
      *) return 64 ;;
    esac
  }
  wait_for_postgres() { [ "${PREFLIGHT_DB_ACTIVE}" -eq 1 ]; }
  restore_upgrade_transaction_entry() { :; }
  restore_upgrade_install_tree() { :; }
  restore_upgrade_handoff_markers() { :; }
  restore_upgrade_install_root_metadata() { :; }

  ensure_upgrade_preflight_database_available
  [ "${PREFLIGHT_DB_ACTIVE}" -eq 1 ]
  [ "${UPGRADE_PREFLIGHT_POSTGRES_STARTED}" -eq 1 ]
  restore_upgrade_prequiesce_state
  [ "${PREFLIGHT_DB_ACTIVE}" -eq 0 ]
  [ "${UPGRADE_PREFLIGHT_POSTGRES_STARTED}" -eq 0 ]
  [ "$(tr '\n' ' ' <"${PREFLIGHT_DB_CALLS}")" = \
    "start ${UNIT_BASENAME}-postgres.service stop ${UNIT_BASENAME}-postgres.service " ]
  if grep -Eq '(core|agent|zlm)[.]service' "${PREFLIGHT_DB_CALLS}"; then
    echo 'preflight database restoration disturbed an application service' >&2
    exit 1
  fi
)

# Exact restore activates the aggregate target without dependencies, so a
# component that was inactive at baseline must never execute even briefly.
(
  UPGRADE=1
  START_AFTER_INSTALL=1
  INSTALL_ROLE=all-in-one-host-cpu
  UNIT_BASENAME=ss-partial-upgrade
  TRUSTED_POSTGRES_UNIT_COUNT=1
  UPGRADE_TARGET_WAS_ACTIVE=0
  UPGRADE_SERVICES_QUIESCED=0
  UPGRADE_RESTORE_ON_FAILURE=0
  UPGRADE_ACTIVE_UNITS=()
  UPGRADE_ACTIVE_MAIN_PIDS=()
  TARGET_ACTIVE=1
  CORE_ACTIVE=1
  ZLM_ACTIVE=0
  AGENT_ACTIVE=0
  POSTGRES_ACTIVE=0
  ACTIVATION_PHASE=0
  READINESS_CHECKED=0
  PARTIAL_READINESS_MARKER="${TMP_DIR}/partial-upgrade-readiness.checked"
  SYSTEMCTL_CALLS="${TMP_DIR}/partial-upgrade-systemctl.calls"
  : >"${SYSTEMCTL_CALLS}"
  systemctl() {
    printf '%s\n' "$*" >>"${SYSTEMCTL_CALLS}"
    case "$1" in
      is-active)
        case "${!#}" in
          "${UNIT_BASENAME}.target") [ "${TARGET_ACTIVE}" -eq 1 ] ;;
          "${UNIT_BASENAME}-core.service") [ "${CORE_ACTIVE}" -eq 1 ] ;;
          "${UNIT_BASENAME}-zlm.service") [ "${ZLM_ACTIVE}" -eq 1 ] ;;
          "${UNIT_BASENAME}-agent.service") [ "${AGENT_ACTIVE}" -eq 1 ] ;;
          "${UNIT_BASENAME}-postgres.service") [ "${POSTGRES_ACTIVE}" -eq 1 ] ;;
          *) return 3 ;;
        esac
        ;;
      show)
        case "$*" in
          *'--property ActiveState'*"${UNIT_BASENAME}.target")
            [ "${TARGET_ACTIVE}" -eq 1 ] && echo active || echo inactive ;;
          *'--property ActiveState'*"${UNIT_BASENAME}-core.service")
            [ "${CORE_ACTIVE}" -eq 1 ] && echo active || echo inactive ;;
          *'--property ActiveState'*"${UNIT_BASENAME}-zlm.service")
            [ "${ZLM_ACTIVE}" -eq 1 ] && echo active || echo inactive ;;
          *'--property ActiveState'*"${UNIT_BASENAME}-agent.service")
            [ "${AGENT_ACTIVE}" -eq 1 ] && echo active || echo inactive ;;
          *'--property ActiveState'*"${UNIT_BASENAME}-postgres.service")
            [ "${POSTGRES_ACTIVE}" -eq 1 ] && echo active || echo inactive ;;
          *'--property MainPID'*"${UNIT_BASENAME}-core.service")
            if [ "${CORE_ACTIVE}" -eq 0 ]; then echo 0; elif [ "${ACTIVATION_PHASE}" -eq 0 ]; then echo 411; else echo 422; fi
            ;;
          *'--property MainPID'*) echo 0 ;;
          *) return 1 ;;
        esac
        ;;
      stop)
        shift
        for unit in "$@"; do
          case "${unit}" in
            "${UNIT_BASENAME}.target") TARGET_ACTIVE=0 ;;
            "${UNIT_BASENAME}-core.service") CORE_ACTIVE=0 ;;
            "${UNIT_BASENAME}-zlm.service") ZLM_ACTIVE=0 ;;
            "${UNIT_BASENAME}-agent.service") AGENT_ACTIVE=0 ;;
            "${UNIT_BASENAME}-postgres.service") POSTGRES_ACTIVE=0 ;;
          esac
        done
        ;;
      start)
        shift
        ignore_dependencies=0
        if [ "${1:-}" = --job-mode=ignore-dependencies ]; then
          ignore_dependencies=1
          shift
        fi
        ACTIVATION_PHASE=1
        for unit in "$@"; do
          case "${unit}" in
            "${UNIT_BASENAME}.target")
              TARGET_ACTIVE=1
              if [ "${ignore_dependencies}" -eq 0 ]; then
                CORE_ACTIVE=1
                ZLM_ACTIVE=1
                AGENT_ACTIVE=1
                POSTGRES_ACTIVE=1
              fi
              ;;
            "${UNIT_BASENAME}-core.service") CORE_ACTIVE=1 ;;
            "${UNIT_BASENAME}-zlm.service") ZLM_ACTIVE=1 ;;
            "${UNIT_BASENAME}-agent.service") AGENT_ACTIVE=1 ;;
            "${UNIT_BASENAME}-postgres.service") POSTGRES_ACTIVE=1 ;;
          esac
        done
        ;;
      *) return 0 ;;
    esac
  }
  probe_upgrade_active_components_readiness() {
    READINESS_CHECKED=1
    : >"${PARTIAL_READINESS_MARKER}"
    return 0
  }

  capture_and_quiesce_upgrade_services
  start_services_if_requested
  [ "${TARGET_ACTIVE}" -eq 1 ]
  [ "${CORE_ACTIVE}" -eq 1 ]
  [ "${ZLM_ACTIVE}" -eq 0 ]
  [ "${AGENT_ACTIVE}" -eq 0 ]
  [ "${POSTGRES_ACTIVE}" -eq 0 ]
  [ "${READINESS_CHECKED}" -eq 1 ]
  [ -e "${PARTIAL_READINESS_MARKER}" ]
  grep -Fq \
    "stop ${UNIT_BASENAME}-core.service ${UNIT_BASENAME}-zlm.service ${UNIT_BASENAME}-agent.service ${UNIT_BASENAME}.target" \
    "${SYSTEMCTL_CALLS}"
  grep -Fq \
    "stop ${UNIT_BASENAME}-postgres.service" \
    "${SYSTEMCTL_CALLS}"
)

# A partial topology is not exempt from readiness. If its one previously
# active Core is unhealthy, the new tree must be rejected before commit.
(
  INSTALL_ROLE=all-in-one-host-cpu
  UNIT_BASENAME=ss-partial-bad-core
  TRUSTED_POSTGRES_UNIT_COUNT=1
  UPGRADE_TARGET_WAS_ACTIVE=1
  UPGRADE_ACTIVE_UNITS=(ss-partial-bad-core-core.service)
  UPGRADE_ACTIVE_MAIN_PIDS=(411)
  systemctl() {
    case "$1" in
      is-active)
        case "${!#}" in
          ss-partial-bad-core.target|ss-partial-bad-core-core.service) return 0 ;;
          *) return 3 ;;
        esac
        ;;
      show)
        case "$*" in
          *'--property MainPID'*ss-partial-bad-core-core.service) printf '%s\n' 422 ;;
          *) return 1 ;;
        esac
        ;;
      *) return 0 ;;
    esac
  }
  probe_upgrade_active_components_readiness() { return 1; }
  set +e
  (verify_upgrade_services_ready) >/dev/null 2>&1
  partial_bad_core_status=$?
  set -e
  [ "${partial_bad_core_status}" -ne 0 ] || {
    echo 'partial upgrade accepted an unhealthy previously-active Core' >&2
    exit 1
  }
)

# A later failure while starting the captured active set must not cause the
# dependency-free target activation to execute originally inactive units.
(
  UPGRADE_RESTORE_ON_FAILURE=1
  UPGRADE_TARGET_WAS_ACTIVE=1
  INSTALL_ROLE=all-in-one-host-cpu
  UNIT_BASENAME=ss-restore-start-failure
  UPGRADE_ACTIVE_UNITS=("${UNIT_BASENAME}-core.service")
  TARGET_ACTIVE=0
  CORE_ACTIVE=0
  ZLM_ACTIVE=0
  AGENT_ACTIVE=0
  SYSTEMCTL_CALLS="${TMP_DIR}/restore-start-failure-systemctl.calls"
  : >"${SYSTEMCTL_CALLS}"

  systemctl() {
    printf '%s\n' "$*" >>"${SYSTEMCTL_CALLS}"
    case "$1" in
      start)
        shift
        if [ "${1:-}" = --job-mode=ignore-dependencies ]; then
          shift
        fi
        for unit in "$@"; do
          case "${unit}" in
            "${UNIT_BASENAME}.target")
              TARGET_ACTIVE=1
              ;;
            "${UNIT_BASENAME}-core.service") return 42 ;;
          esac
        done
        ;;
      stop)
        shift
        for unit in "$@"; do
          case "${unit}" in
            "${UNIT_BASENAME}.target") TARGET_ACTIVE=0 ;;
            "${UNIT_BASENAME}-core.service") CORE_ACTIVE=0 ;;
            "${UNIT_BASENAME}-zlm.service") ZLM_ACTIVE=0 ;;
            "${UNIT_BASENAME}-agent.service") AGENT_ACTIVE=0 ;;
          esac
        done
        ;;
      is-active)
        case "${!#}" in
          "${UNIT_BASENAME}.target") [ "${TARGET_ACTIVE}" -eq 1 ] ;;
          "${UNIT_BASENAME}-core.service") [ "${CORE_ACTIVE}" -eq 1 ] ;;
          "${UNIT_BASENAME}-zlm.service") [ "${ZLM_ACTIVE}" -eq 1 ] ;;
          "${UNIT_BASENAME}-agent.service") [ "${AGENT_ACTIVE}" -eq 1 ] ;;
          *) return 3 ;;
        esac
        ;;
      *) return 0 ;;
    esac
  }

  set +e
  restore_captured_upgrade_service_state
  RESTORE_START_FAILURE_STATUS=$?
  set -e
  [ "${RESTORE_START_FAILURE_STATUS}" -ne 0 ]
  [ "${TARGET_ACTIVE}" -eq 1 ]
  [ "${CORE_ACTIVE}" -eq 0 ]
  [ "${ZLM_ACTIVE}" -eq 0 ]
  [ "${AGENT_ACTIVE}" -eq 0 ]
  grep -Fq \
    "stop ${UNIT_BASENAME}-zlm.service ${UNIT_BASENAME}-agent.service" \
    "${SYSTEMCTL_CALLS}"
)

# A successful systemctl return is not sufficient proof of exact restoration:
# restart policies or concurrent operators can leave an originally inactive
# target or service active after the corrective stop.
(
  UPGRADE_RESTORE_ON_FAILURE=1
  UPGRADE_TARGET_WAS_ACTIVE=0
  INSTALL_ROLE=control-plane
  UNIT_BASENAME=ss-restore-state-verification
  UPGRADE_ACTIVE_UNITS=()
  TARGET_ACTIVE=1
  CORE_ACTIVE=1

  systemctl() {
    case "$1" in
      stop) return 0 ;;
      is-active)
        case "${!#}" in
          "${UNIT_BASENAME}.target") [ "${TARGET_ACTIVE}" -eq 1 ] ;;
          "${UNIT_BASENAME}-core.service") [ "${CORE_ACTIVE}" -eq 1 ] ;;
          *) return 3 ;;
        esac
        ;;
      *) return 0 ;;
    esac
  }

  set +e
  restore_captured_upgrade_service_state
  RESTORE_STATE_VERIFICATION_STATUS=$?
  set -e
  [ "${RESTORE_STATE_VERIFICATION_STATUS}" -ne 0 ]
)

# A unit stuck in an auto-restart/activation transition has no stable state to
# restore. The upgrade must time out before issuing any stop operation.
(
  UPGRADE=1
  INSTALL_ROLE="worker-host-cpu"
  UNIT_BASENAME="ss-auto-restart"
  UPGRADE_TARGET_WAS_ACTIVE=0
  UPGRADE_SERVICES_QUIESCED=0
  UPGRADE_ACTIVE_UNITS=()
  UPGRADE_ACTIVE_MAIN_PIDS=()
  ZLM_STATE=activating
  AGENT_STATE=inactive
  TARGET_STATE=active
  SYSTEMCTL_CALLS="${TMP_DIR}/auto-restart-upgrade-systemctl.calls"
  : >"${SYSTEMCTL_CALLS}"
  sleep() { :; }

  systemctl() {
    printf '%s\n' "$*" >>"${SYSTEMCTL_CALLS}"
    case "$1" in
      is-active)
        case "${!#}" in
          "${UNIT_BASENAME}.target") [ "${TARGET_STATE}" = active ] ;;
          "${UNIT_BASENAME}-zlm.service") [ "${ZLM_STATE}" = active ] ;;
          "${UNIT_BASENAME}-agent.service") [ "${AGENT_STATE}" = active ] ;;
          *) return 3 ;;
        esac
        ;;
      stop)
        shift
        for unit in "$@"; do
          case "${unit}" in
            "${UNIT_BASENAME}.target") TARGET_STATE=inactive ;;
            "${UNIT_BASENAME}-zlm.service") ZLM_STATE=inactive ;;
            "${UNIT_BASENAME}-agent.service") AGENT_STATE=inactive ;;
          esac
        done
        ;;
      show)
        case "$*" in
          *'--property ActiveState'*"${UNIT_BASENAME}.target") printf '%s\n' "${TARGET_STATE}" ;;
          *'--property ActiveState'*"${UNIT_BASENAME}-zlm.service") printf '%s\n' "${ZLM_STATE}" ;;
          *'--property ActiveState'*"${UNIT_BASENAME}-agent.service") printf '%s\n' "${AGENT_STATE}" ;;
          *'--property MainPID'*) printf '%s\n' 0 ;;
          *) return 1 ;;
        esac
        ;;
      *) return 0 ;;
    esac
  }

  set +e
  (capture_and_quiesce_upgrade_services) >/dev/null 2>&1
  AUTO_RESTART_CAPTURE_STATUS=$?
  set -e
  [ "${AUTO_RESTART_CAPTURE_STATUS}" -ne 0 ] || {
    echo 'upgrade captured a transient unit state instead of failing closed' >&2
    exit 1
  }
  if grep -Eq '^stop( |$)' "${SYSTEMCTL_CALLS}"; then
    echo 'upgrade quiesced services before all unit states became steady' >&2
    exit 1
  fi
  [ "${ZLM_STATE}" = activating ]
  [ "${AGENT_STATE}" = inactive ]
  [ "${TARGET_STATE}" = active ]
  [ "${UPGRADE_SERVICES_QUIESCED}" -eq 0 ]
)

(
  UPGRADE=1
  START_AFTER_INSTALL=0
  INSTALL_ROLE="control-plane"
  INSTALL_DIR="${TMP_DIR}/no-start-upgrade"
  INSTANCE_NAME="contract-no-start"
  UNIT_BASENAME="ss-contract-no-start"
  UPGRADE_TARGET_WAS_ACTIVE=0
  UPGRADE_SERVICES_QUIESCED=0
  UPGRADE_ACTIVE_UNITS=()
  UPGRADE_ACTIVE_MAIN_PIDS=()
  TARGET_ACTIVE=1
  CORE_ACTIVE=1
  SYSTEMCTL_CALLS="${TMP_DIR}/no-start-upgrade-systemctl.calls"
  mkdir -p "${INSTALL_DIR}/bin"
  : >"${SYSTEMCTL_CALLS}"
  systemctl() {
    printf '%s\n' "$*" >>"${SYSTEMCTL_CALLS}"
    case "$1" in
      is-active)
        case "${!#}" in
          "${UNIT_BASENAME}.target") [ "${TARGET_ACTIVE}" -eq 1 ] ;;
          "${UNIT_BASENAME}-core.service") [ "${CORE_ACTIVE}" -eq 1 ] ;;
          *) return 3 ;;
        esac
        ;;
      show)
        case "$*" in
          *'--property ActiveState'*"${UNIT_BASENAME}.target")
            [ "${TARGET_ACTIVE}" -eq 1 ] && printf '%s\n' active || printf '%s\n' inactive
            ;;
          *'--property ActiveState'*"${UNIT_BASENAME}-core.service")
            [ "${CORE_ACTIVE}" -eq 1 ] && printf '%s\n' active || printf '%s\n' inactive
            ;;
          *'--property MainPID'*"${UNIT_BASENAME}-core.service")
            [ "${CORE_ACTIVE}" -eq 1 ] && printf '%s\n' 333 || printf '%s\n' 0
            ;;
          *) return 1 ;;
        esac
        ;;
      stop)
        shift
        for unit in "$@"; do
          case "${unit}" in
            "${UNIT_BASENAME}.target") TARGET_ACTIVE=0 ;;
            "${UNIT_BASENAME}-core.service") CORE_ACTIVE=0 ;;
          esac
        done
        ;;
      start) return 97 ;;
      *) return 0 ;;
    esac
  }

  capture_and_quiesce_upgrade_services
  start_services_if_requested
  [ "${TARGET_ACTIVE}" -eq 0 ]
  [ "${CORE_ACTIVE}" -eq 0 ]
  if grep -Eq '^start( |$)' "${SYSTEMCTL_CALLS}"; then
    echo '--no-start upgrade unexpectedly started a native unit' >&2
    exit 1
  fi
)

# Replacing unit files during an upgrade must preserve the complete captured
# enablement topology. In particular, --no-start must not silently enable a
# previously disabled target or service.
(
  set +x
  UPGRADE=1
  START_AFTER_INSTALL=0
  INSTALL_ROLE=control-plane
  DATABASE_MODE=external
  INSTANCE_NAME=contract-enable-success
  UNIT_BASENAME=ss-contract-enable-success
  INSTALL_DIR="${TMP_DIR}/upgrade-enable-success-install"
  SYSTEMD_UNIT_ROOT="${TMP_DIR}/upgrade-enable-success-units"
  ENABLEMENT_RESTORED_MARKER="${TMP_DIR}/upgrade-enable-success-restored"
  SYSTEMCTL_CALLS="${TMP_DIR}/upgrade-enable-success-systemctl.calls"
  mkdir -p "${INSTALL_DIR}/systemd" "${SYSTEMD_UNIT_ROOT}"
  : >"${SYSTEMCTL_CALLS}"
  ensure_control_directory() { mkdir -p "$1"; }
  render_template() { printf '%s\n' rendered >"$2"; }
  copy_file_atomically() { cp -- "$1" "$2"; }
  restore_upgrade_unit_enablement() {
    : >"${ENABLEMENT_RESTORED_MARKER}"
  }
  systemctl() {
    printf '%s\n' "$*" >>"${SYSTEMCTL_CALLS}"
    return 0
  }

  install_systemd_units
  [ -e "${ENABLEMENT_RESTORED_MARKER}" ] || {
    echo 'successful upgrade did not restore captured unit enablement' >&2
    exit 1
  }
  if grep -Eq '^enable( |$)' "${SYSTEMCTL_CALLS}"; then
    echo 'successful upgrade enabled units before restoring captured topology' >&2
    exit 1
  fi
)

# Success is announced only after the durable transaction commit. Otherwise a
# late commit/fsync failure can print a false "installation complete" result.
mutation_function_body="$(sed -n \
  '/^perform_locked_install_mutation() {$/,/^}$/p' "${INSTALLER}")"
commit_line="$(printf '%s\n' "${mutation_function_body}" \
  | grep -n '^[[:space:]]*commit_upgrade_transaction$' | cut -d: -f1)"
success_line="$(printf '%s\n' "${mutation_function_body}" \
  | grep -n '安装完成:' | cut -d: -f1)"
[[ "${commit_line}" =~ ^[0-9]+$ ]] && [[ "${success_line}" =~ ^[0-9]+$ ]]
[ "${commit_line}" -lt "${success_line}" ] || {
  echo 'installer announces success before the upgrade transaction commits' >&2
  exit 1
}

# Upgrade database topology is immutable. It is derived from the trusted
# postgres unit, never from an installer prompt or package capabilities.
(
  INSTALL_ROLE=control-plane
  UPGRADE=1
  INSTALL_DIR="${TMP_DIR}/upgrade-external-database-mode"
  PACKAGE_ROOT="${TMP_DIR}/upgrade-external-database-package"
  TRUSTED_POSTGRES_UNIT_COUNT=0
  DATABASE_MODE=""
  DATABASE_URL_INPUT=""
  BUNDLE_POSTGRES_RUNTIME=true
  POSTGRES_RUNTIME_PATH=runtime/postgres
  mkdir -p "${INSTALL_DIR}" "${PACKAGE_ROOT}/runtime/postgres"
  : >"${INSTALL_DIR}/.env"
  write_env_entry "${INSTALL_DIR}/.env" DATABASE_URL \
    postgresql://old-external.example/streamserver
  write_env_entry "${INSTALL_DIR}/.env" POSTGRES_DB streamserver
  write_env_entry "${INSTALL_DIR}/.env" POSTGRES_USER external_user
  write_env_entry "${INSTALL_DIR}/.env" POSTGRES_PASSWORD external_password
  write_env_entry "${INSTALL_DIR}/.env" POSTGRES_PORT 5432
  prompt() { echo 'external upgrade unexpectedly prompted' >&2; return 91; }
  prompt_non_empty() { echo 'external upgrade unexpectedly prompted' >&2; return 91; }
  prompt_yes_no() { echo 'external upgrade unexpectedly prompted' >&2; return 91; }

  prepare_upgrade_database_configuration
  [ "${DATABASE_MODE}" = external ]
  [ "${DATABASE_URL}" = postgresql://old-external.example/streamserver ]
  configure_database
  [ "${DATABASE_MODE}" = external ]
  [ "${DATABASE_URL}" = postgresql://old-external.example/streamserver ]

  DATABASE_MODE=external
  DATABASE_URL_INPUT=postgresql://new-external.example/streamserver
  prepare_upgrade_database_configuration
  [ "${DATABASE_URL}" = postgresql://new-external.example/streamserver ]
)

(
  INSTALL_ROLE=control-plane
  UPGRADE=1
  INSTALL_DIR="${TMP_DIR}/upgrade-bundled-database-mode"
  PACKAGE_ROOT="${TMP_DIR}/upgrade-bundled-database-package"
  TRUSTED_POSTGRES_UNIT_COUNT=1
  DATABASE_MODE=""
  DATABASE_URL_INPUT=""
  BUNDLE_POSTGRES_RUNTIME=true
  POSTGRES_RUNTIME_PATH=runtime/postgres
  mkdir -p "${INSTALL_DIR}" "${PACKAGE_ROOT}/runtime/postgres"
  : >"${INSTALL_DIR}/.env"
  write_env_entry "${INSTALL_DIR}/.env" DATABASE_URL \
    postgresql://bundled_user:bundled_password@127.0.0.1:55432/streamserver
  write_env_entry "${INSTALL_DIR}/.env" POSTGRES_DB streamserver
  write_env_entry "${INSTALL_DIR}/.env" POSTGRES_USER bundled_user
  write_env_entry "${INSTALL_DIR}/.env" POSTGRES_PASSWORD bundled_password
  write_env_entry "${INSTALL_DIR}/.env" POSTGRES_PORT 55432
  prompt() { echo 'bundled upgrade unexpectedly prompted' >&2; return 91; }
  prompt_non_empty() { echo 'bundled upgrade unexpectedly prompted' >&2; return 91; }
  prompt_yes_no() { echo 'bundled upgrade unexpectedly prompted' >&2; return 91; }

  prepare_upgrade_database_configuration
  [ "${DATABASE_MODE}" = bundled ]
  [ "${POSTGRES_PORT}" = 55432 ]
  configure_database
  [ "${DATABASE_MODE}" = bundled ]

  DATABASE_MODE=external
  DATABASE_URL_INPUT=postgresql://forbidden-switch.example/streamserver
  set +e
  (prepare_upgrade_database_configuration) >/dev/null 2>&1
  bundled_to_external_status=$?
  set -e
  [ "${bundled_to_external_status}" -ne 0 ] || {
    echo 'bundled upgrade accepted a switch to external PostgreSQL' >&2
    exit 1
  }

  DATABASE_MODE=""
  DATABASE_URL_INPUT=""
  BUNDLE_POSTGRES_RUNTIME=false
  set +e
  (prepare_upgrade_database_configuration) >/dev/null 2>&1
  missing_bundled_runtime_status=$?
  set -e
  [ "${missing_bundled_runtime_status}" -ne 0 ] || {
    echo 'bundled upgrade accepted a package without PostgreSQL runtime' >&2
    exit 1
  }
)

# Runtime trees may contain package-authenticated relative symlinks such as
# PostgreSQL's versioned sample configuration.  Only root-confined links to a
# regular, non-writable target through non-writable parents are acceptable.
(
  set +x
  umask 0022
  EMULATED_SECURITY_METADATA=1
  confined_root="${TMP_DIR}/confined-runtime-tree"
  link_parent="${confined_root}/share/postgresql/18"
  mkdir -p "${link_parent}"
  printf '%s\n' sample >"${confined_root}/share/postgresql/postgresql.conf.sample"
  ln -s ../postgresql.conf.sample \
    "${link_parent}/postgresql.conf.sample"
  assert_control_tree_safe "${confined_root}"

  rm -f "${link_parent}/postgresql.conf.sample"
  for unsafe_target in \
    /etc/passwd \
    ../../../../outside-runtime-tree \
    missing.conf \
    ../..; do
    ln -s "${unsafe_target}" "${link_parent}/postgresql.conf.sample"
    set +e
    (assert_control_tree_safe "${confined_root}") >/dev/null 2>&1
    unsafe_link_status=$?
    set -e
    [ "${unsafe_link_status}" -ne 0 ]
    rm -f "${link_parent}/postgresql.conf.sample"
  done

  ln -s loop-b "${confined_root}/loop-a"
  ln -s loop-a "${confined_root}/loop-b"
  set +e
  (assert_control_tree_safe "${confined_root}") >/dev/null 2>&1
  loop_link_status=$?
  set -e
  [ "${loop_link_status}" -ne 0 ]
  rm -f "${confined_root}/loop-a" "${confined_root}/loop-b"

  ln -s ../postgresql.conf.sample \
    "${link_parent}/postgresql.conf.sample"
  chmod 0777 "${link_parent}"
  set +e
  (assert_control_tree_safe "${confined_root}") >/dev/null 2>&1
  writable_parent_status=$?
  set -e
  [ "${writable_parent_status}" -ne 0 ]
  chmod 0755 "${link_parent}"
  rm -f "${link_parent}/postgresql.conf.sample"

  ln -s share/postgresql/postgresql.conf.sample "${confined_root}/link-hop"
  ln -s link-hop "${confined_root}/link-chain"
  set +e
  (assert_control_tree_safe "${confined_root}") >/dev/null 2>&1
  intermediate_link_status=$?
  set -e
  [ "${intermediate_link_status}" -ne 0 ]
  rm -f "${confined_root}/link-hop" "${confined_root}/link-chain"

  ln -s ../postgresql.conf.sample \
    "${link_parent}/postgresql.conf.sample"
  chmod 0777 "${confined_root}/share/postgresql"
  set +e
  (assert_control_tree_safe "${confined_root}") >/dev/null 2>&1
  writable_target_parent_status=$?
  set -e
  [ "${writable_target_parent_status}" -ne 0 ]
  chmod 0755 "${confined_root}/share/postgresql"
  chmod 0666 "${confined_root}/share/postgresql/postgresql.conf.sample"
  set +e
  (assert_control_tree_safe "${confined_root}") >/dev/null 2>&1
  writable_target_status=$?
  set -e
  [ "${writable_target_status}" -ne 0 ]
  chmod 0644 "${confined_root}/share/postgresql/postgresql.conf.sample"
  rm -f "${link_parent}/postgresql.conf.sample"

  mkfifo "${confined_root}/unexpected-fifo"
  set +e
  (assert_control_tree_safe "${confined_root}") >/dev/null 2>&1
  special_entry_status=$?
  set -e
  [ "${special_entry_status}" -ne 0 ]
)

(
  set +x
  EMULATED_SECURITY_METADATA=1
  find_failure_root="${TMP_DIR}/control-tree-find-failure"
  mkdir -p "${find_failure_root}"
  find() { return 73; }
  set +e
  (assert_control_tree_safe "${find_failure_root}") >/dev/null 2>&1
  find_failure_status=$?
  set -e
  [ "${find_failure_status}" -ne 0 ]
)

# Directory st_size reflects filesystem allocation rather than tree content.
# A directory that once held many entries can remain larger than a fresh cp -a
# snapshot even when every path, byte, and meaningful metadata field matches.
(
  set +x
  source_tree="${TMP_DIR}/directory-size-fingerprint-source"
  snapshot_tree="${TMP_DIR}/directory-size-fingerprint-snapshot"
  mkdir -p "${source_tree}/empty-cache"
  for index in $(seq 1 1024); do
    : >"${source_tree}/empty-cache/entry-${index}"
  done
  rm -f "${source_tree}/empty-cache/"entry-*
  touch -t 202401010000.00 "${source_tree}" "${source_tree}/empty-cache"
  cp -a "${source_tree}" "${snapshot_tree}"
  [ "$(stat -c '%s' "${source_tree}/empty-cache")" != \
    "$(stat -c '%s' "${snapshot_tree}/empty-cache")" ]
  [ "$(upgrade_entry_fingerprint "${source_tree}")" = \
    "$(upgrade_entry_fingerprint "${snapshot_tree}")" ]
)

# Package trees are fully staged and audited before an existing runtime is
# removed.  Both an unsafe source and an unsafe post-copy result must leave the
# previously installed target byte-for-byte and metadata-for-metadata intact.
(
  set +x
  umask 0022
  EMULATED_SECURITY_METADATA=1
  INSTALL_DIR="${TMP_DIR}/verified-install-tree"
  source_tree="${TMP_DIR}/verified-install-tree-source"
  target_tree="${INSTALL_DIR}/runtime/postgres"
  mkdir -p "${source_tree}/share/postgresql/18" "${target_tree}"
  printf '%s\n' sample >"${source_tree}/share/postgresql/postgresql.conf.sample"
  ln -s ../postgresql.conf.sample \
    "${source_tree}/share/postgresql/18/postgresql.conf.sample"
  printf '%s\n' old-runtime >"${target_tree}/sentinel"
  install_tree "${source_tree}" "${target_tree}"
  [ "$(readlink "${target_tree}/share/postgresql/18/postgresql.conf.sample")" = \
    ../postgresql.conf.sample ]
  assert_control_tree_safe "${target_tree}"

  target_before="$(upgrade_entry_fingerprint "${target_tree}")"
  rm -f "${source_tree}/share/postgresql/18/postgresql.conf.sample"
  ln -s /etc/passwd "${source_tree}/share/postgresql/18/postgresql.conf.sample"
  set +e
  (install_tree "${source_tree}" "${target_tree}") >/dev/null 2>&1
  unsafe_source_status=$?
  set -e
  [ "${unsafe_source_status}" -ne 0 ]
  [ "$(upgrade_entry_fingerprint "${target_tree}")" = "${target_before}" ]

  rm -f "${source_tree}/share/postgresql/18/postgresql.conf.sample"
  ln -s ../postgresql.conf.sample \
    "${source_tree}/share/postgresql/18/postgresql.conf.sample"
  cp() {
    local destination
    command cp "$@" || return
    destination="${!#}"
    case "$*" in
      *"${source_tree}"*)
        rm -f -- "${destination%/}/share/postgresql/18/postgresql.conf.sample"
        ln -s /etc/passwd \
          "${destination%/}/share/postgresql/18/postgresql.conf.sample"
        ;;
    esac
  }
  set +e
  (install_tree "${source_tree}" "${target_tree}") >/dev/null 2>&1
  unsafe_copy_status=$?
  set -e
  [ "${unsafe_copy_status}" -ne 0 ]
  [ "$(upgrade_entry_fingerprint "${target_tree}")" = "${target_before}" ]
  [ "$(find "${INSTALL_DIR}/runtime" -maxdepth 1 \
    -name 'postgres.installing.*' -print -quit | wc -l)" -eq 0 ]
)

# Rollback validates and stages every snapshot before removing the live target.
# Malformed state, missing data, unsafe links, and a corrupted staging copy all
# preserve the live sentinel.  A valid PostgreSQL link survives restoration.
(
  set +x
  umask 0022
  EMULATED_SECURITY_METADATA=1
  UPGRADE_TRANSACTION_ID=123-456-701
  INSTALL_DIR="${TMP_DIR}/verified-restore-order"
  ADMIN_HANDOFF_STATE_ROOT="${TMP_DIR}/verified-restore-state"
  target_tree="${INSTALL_DIR}/runtime"
  snapshot_tree="${TMP_DIR}/verified-restore-snapshot"
  state_file="${TMP_DIR}/verified-restore.state"
  mkdir -p "${target_tree}" "${snapshot_tree}/share/postgresql/18" \
    "${ADMIN_HANDOFF_STATE_ROOT}"
  chmod 700 "${ADMIN_HANDOFF_STATE_ROOT}"
  printf '%s\n' live-sentinel >"${target_tree}/sentinel"
  live_before="$(upgrade_entry_fingerprint "${target_tree}")"

  printf '%s\n' unknown >"${state_file}"
  set +e
  restore_upgrade_transaction_entry \
    "${snapshot_tree}" "${state_file}" "${target_tree}" >/dev/null 2>&1
  unknown_state_status=$?
  set -e
  [ "${unknown_state_status}" -ne 0 ]
  [ "$(upgrade_entry_fingerprint "${target_tree}")" = "${live_before}" ]

  printf '%s\n' file >"${state_file}"
  rm -rf "${snapshot_tree}"
  set +e
  restore_upgrade_transaction_entry \
    "${snapshot_tree}" "${state_file}" "${target_tree}" >/dev/null 2>&1
  missing_snapshot_status=$?
  set -e
  [ "${missing_snapshot_status}" -ne 0 ]
  [ "$(upgrade_entry_fingerprint "${target_tree}")" = "${live_before}" ]

  ln -s /etc/passwd "${snapshot_tree}"
  set +e
  restore_upgrade_transaction_entry \
    "${snapshot_tree}" "${state_file}" "${target_tree}" >/dev/null 2>&1
  linked_file_snapshot_status=$?
  set -e
  [ "${linked_file_snapshot_status}" -ne 0 ]
  [ "$(upgrade_entry_fingerprint "${target_tree}")" = "${live_before}" ]
  rm -f "${snapshot_tree}"

  mkdir -p "${snapshot_tree}/share/postgresql/18"
  ln -s /etc/passwd "${snapshot_tree}/share/postgresql/18/postgresql.conf.sample"
  printf '%s\n' directory >"${state_file}"
  set +e
  restore_upgrade_transaction_entry \
    "${snapshot_tree}" "${state_file}" "${target_tree}" >/dev/null 2>&1
  unsafe_directory_snapshot_status=$?
  set -e
  [ "${unsafe_directory_snapshot_status}" -ne 0 ]
  [ "$(upgrade_entry_fingerprint "${target_tree}")" = "${live_before}" ]

  rm -f "${snapshot_tree}/share/postgresql/18/postgresql.conf.sample"
  printf '%s\n' sample >"${snapshot_tree}/share/postgresql/postgresql.conf.sample"
  ln -s ../postgresql.conf.sample \
    "${snapshot_tree}/share/postgresql/18/postgresql.conf.sample"
  restore_upgrade_transaction_entry \
    "${snapshot_tree}" "${state_file}" "${target_tree}"
  [ "$(readlink "${target_tree}/share/postgresql/18/postgresql.conf.sample")" = \
    ../postgresql.conf.sample ]
  assert_control_tree_safe "${target_tree}"

  rm -rf "${target_tree}"
  mkdir -p "${target_tree}"
  printf '%s\n' live-sentinel >"${target_tree}/sentinel"
  live_before="$(upgrade_entry_fingerprint "${target_tree}")"
  copy_upgrade_transaction_entry() {
    command cp -a --no-dereference --reflink=auto -- "$1" "$2" || return
    case "$2" in
      */.streamserver-restore.*/entry)
        ln -s /etc/passwd "$2/post-copy-escape"
        ;;
    esac
  }
  set +e
  restore_upgrade_transaction_entry \
    "${snapshot_tree}" "${state_file}" "${target_tree}" >/dev/null 2>&1
  unsafe_restore_copy_status=$?
  set -e
  [ "${unsafe_restore_copy_status}" -ne 0 ]
  [ "$(upgrade_entry_fingerprint "${target_tree}")" = "${live_before}" ]
  [ "$(find "${INSTALL_DIR}" -maxdepth 1 \
    -name '.streamserver-restore.*' -print -quit | wc -l)" -eq 0 ]
)

# Collection restore validates every snapshot entry before replacing the first
# live entry.  A corrupt later item cannot leave an earlier item rolled back.
(
  set +x
  umask 0022
  EMULATED_SECURITY_METADATA=1
  INSTALL_DIR="${TMP_DIR}/restore-collection-prevalidation"
  UPGRADE_TRANSACTION_DIR="${TMP_DIR}/restore-collection-transaction"
  mkdir -p "${INSTALL_DIR}/bin" "${INSTALL_DIR}/runtime" \
    "${UPGRADE_TRANSACTION_DIR}/install/bin" \
    "${UPGRADE_TRANSACTION_DIR}/install/runtime" \
    "${UPGRADE_TRANSACTION_DIR}/install-state"
  printf '%s\n' live-bin >"${INSTALL_DIR}/bin/sentinel"
  printf '%s\n' live-runtime >"${INSTALL_DIR}/runtime/sentinel"
  printf '%s\n' old-bin >"${UPGRADE_TRANSACTION_DIR}/install/bin/old"
  ln -s /etc/passwd "${UPGRADE_TRANSACTION_DIR}/install/runtime/escape"
  printf '%s\n' directory >"${UPGRADE_TRANSACTION_DIR}/install-state/bin.state"
  printf '%s\n' directory >"${UPGRADE_TRANSACTION_DIR}/install-state/runtime.state"
  bin_before="$(upgrade_entry_fingerprint "${INSTALL_DIR}/bin")"
  runtime_before="$(upgrade_entry_fingerprint "${INSTALL_DIR}/runtime")"
  upgrade_transaction_install_items() { printf '%s\n' bin runtime; }
  set +e
  restore_upgrade_install_tree >/dev/null 2>&1
  collection_restore_status=$?
  set -e
  [ "${collection_restore_status}" -ne 0 ]
  [ "$(upgrade_entry_fingerprint "${INSTALL_DIR}/bin")" = "${bin_before}" ]
  [ "$(upgrade_entry_fingerprint "${INSTALL_DIR}/runtime")" = "${runtime_before}" ]
)

# If a copy or metadata restore fails after prevalidation, rollback retains the
# snapshot and never reloads systemd or starts a mixed control tree.
(
  set +x
  UPGRADE_TRANSACTION_STATE=armed
  UPGRADE_RESTORE_ON_FAILURE=1
  ROLLBACK_CALLS="${TMP_DIR}/rollback-no-mixed-start.calls"
  FINALIZE_MARKER="${TMP_DIR}/rollback-no-mixed-start.finalized"
  : >"${ROLLBACK_CALLS}"
  read_upgrade_transaction_phase() { printf '%s' armed; }
  assert_install_transaction_lock_held() { :; }
  validate_upgrade_transaction_snapshot_for_restore() { :; }
  upgrade_rollback_units() { printf '%s\n' ss-mixed-core.service ss-mixed.target; }
  bounded_upgrade_systemctl() {
    shift
    printf '%s\n' "$*" >>"${ROLLBACK_CALLS}"
  }
  restore_upgrade_install_tree() { return 1; }
  restore_upgrade_install_root_metadata() { :; }
  restore_upgrade_handoff_markers() { :; }
  restore_upgrade_external_units() { :; }
  finalize_upgrade_transaction_terminal() { : >"${FINALIZE_MARKER}"; }
  set +e
  restore_upgrade_transaction >/dev/null 2>&1
  mixed_restore_status=$?
  set -e
  [ "${mixed_restore_status}" -ne 0 ]
  grep -Fq 'stop ss-mixed-core.service ss-mixed.target' "${ROLLBACK_CALLS}"
  if grep -Eq 'daemon-reload|(^| )start( |$)' "${ROLLBACK_CALLS}"; then
    echo 'failed rollback reloaded or started a mixed control tree' >&2
    exit 1
  fi
  [ ! -e "${FINALIZE_MARKER}" ]
)

# Snapshot validation and filesystem restoration can legitimately take longer
# than any systemd phase budget. Each later stop/reload/start/readiness phase
# therefore receives a fresh absolute deadline after the preceding disk work.
(
  set +x
  UPGRADE_TRANSACTION_STATE=armed
  UPGRADE_RESTORE_ON_FAILURE=1
  INSTALL_ROLE=control-plane
  UNIT_BASENAME=ss-slow-rollback-deadline
  TRUSTED_POSTGRES_UNIT_COUNT=0
  UPGRADE_ACTIVE_UNITS=(ss-slow-rollback-deadline-core.service)
  ROLLBACK_DEADLINES="${TMP_DIR}/slow-rollback-deadline.calls"
  : >"${ROLLBACK_DEADLINES}"
  read_upgrade_transaction_phase() { printf '%s' armed; }
  assert_install_transaction_lock_held() { :; }
  validate_upgrade_transaction_snapshot_for_restore() {
    SECONDS=$((SECONDS + 61))
  }
  upgrade_rollback_units() {
    printf '%s\n' ss-slow-rollback-deadline-core.service ss-slow-rollback-deadline.target
  }
  bounded_upgrade_systemctl() {
    local deadline="$1"
    shift
    printf 'systemctl %s %s %s\n' "$1" "${deadline}" "${SECONDS}" \
      >>"${ROLLBACK_DEADLINES}"
    [ "${deadline}" -gt "${SECONDS}" ]
  }
  restore_upgrade_install_tree() { SECONDS=$((SECONDS + 61)); }
  restore_upgrade_install_root_metadata() { :; }
  restore_upgrade_handoff_markers() { :; }
  restore_upgrade_external_units() { :; }
  restore_upgrade_unit_enablement() {
    printf 'enablement %s %s\n' "$1" "${SECONDS}" >>"${ROLLBACK_DEADLINES}"
    [ "$1" -gt "${SECONDS}" ] || return 1
    SECONDS=$((SECONDS + 61))
  }
  restore_captured_upgrade_service_state() {
    printf 'service-state %s %s\n' "$1" "${SECONDS}" >>"${ROLLBACK_DEADLINES}"
    [ "$1" -gt "${SECONDS}" ] || return 1
    SECONDS=$((SECONDS + 61))
  }
  verify_restored_upgrade_readiness() {
    printf 'readiness %s %s\n' "$1" "${SECONDS}" >>"${ROLLBACK_DEADLINES}"
    [ "$1" -gt "${SECONDS}" ]
  }
  finalize_upgrade_transaction_terminal() { :; }
  complete_terminal_upgrade_transaction() { :; }
  SECONDS=0
  trap 'echo "slow rollback disk work consumed a later systemd phase deadline" >&2' ERR
  restore_upgrade_transaction
  trap - ERR
  grep -Eq '^systemctl stop [0-9]+ 61$' "${ROLLBACK_DEADLINES}"
  grep -Eq '^systemctl daemon-reload [0-9]+ 122$' "${ROLLBACK_DEADLINES}"
  grep -Eq '^enablement [0-9]+ 122$' "${ROLLBACK_DEADLINES}"
  grep -Eq '^service-state [0-9]+ 183$' "${ROLLBACK_DEADLINES}"
  grep -Eq '^readiness [0-9]+ 244$' "${ROLLBACK_DEADLINES}"
)

# The final permission pass validates every nested control entry before the
# first recursive ownership or mode mutation.
(
  set +x
  EMULATED_SECURITY_METADATA=1
  INSTALL_DIR="${TMP_DIR}/fix-permissions-full-tree"
  SERVICE_USER=streamserver-contract
  SERVICE_GROUP=streamserver-contract
  CHOWN_LOG="${TMP_DIR}/fix-permissions-full-tree.chown"
  mkdir -p "${INSTALL_DIR}/runtime/nested"
  ln -s /etc/passwd "${INSTALL_DIR}/runtime/nested/escape"
  : >"${CHOWN_LOG}"
  chown() { printf '%s\n' "$*" >>"${CHOWN_LOG}"; }
  set +e
  (fix_permissions) >/dev/null 2>&1
  unsafe_permission_tree_status=$?
  set -e
  [ "${unsafe_permission_tree_status}" -ne 0 ]
  if grep -Eq '(^| )-R( |$)' "${CHOWN_LOG}"; then
    echo 'fix_permissions recursively mutated an unsafe control tree' >&2
    exit 1
  fi
)

(
  INSTALL_ROLE="all-in-one-host-cpu"
  INSTALL_DIR="${TMP_DIR}/upgrade-call-order"
  CALL_ORDER_FILE="${TMP_DIR}/upgrade-call-order.calls"
  mkdir -p "${INSTALL_DIR}"
  : >"${CALL_ORDER_FILE}"
  acquire_install_transaction_lock() { printf '%s\n' lock >>"${CALL_ORDER_FILE}"; }
  resume_upgrade_boot_fence_for_recovery() { printf '%s\n' resume-fence >>"${CALL_ORDER_FILE}"; }
  prepare_upgrade_cli_identity() { printf '%s\n' live-identity >>"${CALL_ORDER_FILE}"; }
  begin_upgrade_preseal_guard() { printf '%s\n' preseal >>"${CALL_ORDER_FILE}"; }
  begin_upgrade_transaction() { printf '%s\n' arm-transaction >>"${CALL_ORDER_FILE}"; }
  capture_upgrade_service_state() { printf '%s\n' capture-state >>"${CALL_ORDER_FILE}"; }
  ensure_upgrade_preflight_database_available() { printf '%s\n' database-ready >>"${CALL_ORDER_FILE}"; }
  quiesce_captured_upgrade_services() {
    printf '%s\n' quiesce >>"${CALL_ORDER_FILE}"
    prepare_pending_admin_password_handoff
    security_preflight_env
  }
  seal_legacy_upgrade_environment() { printf '%s\n' seal-env >>"${CALL_ORDER_FILE}"; }
  validate_sealed_upgrade_environment_identity() { printf '%s\n' validate-env >>"${CALL_ORDER_FILE}"; }
  prepare_upgrade_database_configuration() { printf '%s\n' database >>"${CALL_ORDER_FILE}"; }
  harden_install_root_before_copy() { printf '%s\n' harden >>"${CALL_ORDER_FILE}"; }
  prepare_package_security_probe_binaries() {
    SECURITY_PROBE_CORE_BIN=/run/contract-media-core
    SECURITY_PROBE_AGENT_BIN=/run/contract-media-agent
    printf '%s\n' stage-probes >>"${CALL_ORDER_FILE}"
  }
  cleanup_security_probe_binaries() {
    SECURITY_PROBE_CORE_BIN=""
    SECURITY_PROBE_AGENT_BIN=""
    printf '%s\n' cleanup-probes >>"${CALL_ORDER_FILE}"
  }
  migrate_legacy_zlm_api_endpoint() { printf '%s\n' migrate-zlm >>"${CALL_ORDER_FILE}"; }
  prepare_pending_admin_password_handoff() { printf '%s\n' handoff >>"${CALL_ORDER_FILE}"; }
  security_preflight_env() { printf '%s\n' preflight >>"${CALL_ORDER_FILE}"; }

  prepare_upgrade_security_gate "${TMP_DIR}/package-media-core"
  [ "$(tr '\n' ' ' <"${CALL_ORDER_FILE}")" = \
    'lock resume-fence live-identity preseal seal-env validate-env database capture-state arm-transaction stage-probes migrate-zlm database-ready preflight quiesce handoff preflight cleanup-probes ' ]
)

# A terminal fence cleanup can be interrupted after deleting the marker and
# only some drop-ins. A normal CLI re-entry must recognize and finish this
# exact root-controlled residue instead of being permanently blocked by .d.
(
  set +x
  SYSTEMD_UNIT_ROOT="${TMP_DIR}/orphan-fence-units"
  INSTALL_DIR="${TMP_DIR}/orphan-fence-install"
  orphan_marker="${TMP_DIR}/orphan-fence.marker"
  orphan_lease="${TMP_DIR}/orphan-fence.lease"
  orphan_calls="${TMP_DIR}/orphan-fence.calls"
  mkdir -p "${INSTALL_DIR}" \
    "${SYSTEMD_UNIT_ROOT}/ss-orphan-one.service.d" \
    "${SYSTEMD_UNIT_ROOT}/ss-orphan-two.target.d"
  chmod 755 "${SYSTEMD_UNIT_ROOT}" "${SYSTEMD_UNIT_ROOT}"/*.d
  upgrade_transaction_unit_names() {
    printf '%s\n' ss-orphan-one.service ss-orphan-two.target
  }
  upgrade_boot_fence_marker_path() { printf '%s' "${orphan_marker}"; }
  upgrade_boot_fence_lease_path() { printf '%s' "${orphan_lease}"; }
  assert_install_transaction_lock_held() { :; }
  trusted_systemd_path_status() {
    [ "$2" = directory ] && [ ! -L "$1" ] && [ -d "$1" ]
  }
  admin_handoff_assert_secure_file() {
    [ ! -L "$1" ] && [ -f "$1" ] && [ "$(stat -c '%a' "$1")" = "$2" ]
  }
  bounded_upgrade_systemctl() { printf '%s\n' "$*" >>"${orphan_calls}"; }
  render_upgrade_boot_fence_dropin \
    ss-orphan-one.service "${orphan_marker}" "${orphan_lease}" \
    >"${SYSTEMD_UNIT_ROOT}/ss-orphan-one.service.d/90-streamserver-upgrade-fence.conf"
  chmod 644 \
    "${SYSTEMD_UNIT_ROOT}/ss-orphan-one.service.d/90-streamserver-upgrade-fence.conf"
  printf '%s\n' '999999 1' >"${orphan_lease}"
  chmod 600 "${orphan_lease}"
  resume_upgrade_boot_fence_for_recovery
  [ ! -e "${SYSTEMD_UNIT_ROOT}/ss-orphan-one.service.d" ]
  [ ! -e "${SYSTEMD_UNIT_ROOT}/ss-orphan-two.target.d" ]
  [ ! -e "${orphan_lease}" ]
  grep -Fq 'daemon-reload' "${orphan_calls}"
)

# A killed installer can leave the same-boot /run bypass behind while the
# durable marker and drop-ins are still armed.  Once the replacement process
# owns both installer locks it must delete that stale bypass before any
# transaction identity check.  A later rejected recovery must therefore leave
# every service fenced off.
(
  set +x
  SYSTEMD_UNIT_ROOT="${TMP_DIR}/stale-fence-units"
  INSTALL_DIR="${TMP_DIR}/stale-fence-install"
  stale_marker="${TMP_DIR}/stale-fence.marker"
  stale_lease="${TMP_DIR}/stale-fence.lease"
  mkdir -p "${INSTALL_DIR}" \
    "${SYSTEMD_UNIT_ROOT}/ss-stale-one.service.d" \
    "${SYSTEMD_UNIT_ROOT}/ss-stale-two.target.d"
  chmod 755 "${SYSTEMD_UNIT_ROOT}" "${SYSTEMD_UNIT_ROOT}"/*.d
  upgrade_transaction_unit_names() {
    printf '%s\n' ss-stale-one.service ss-stale-two.target
  }
  upgrade_boot_fence_marker_path() { printf '%s' "${stale_marker}"; }
  upgrade_boot_fence_lease_path() { printf '%s' "${stale_lease}"; }
  assert_install_transaction_lock_held() { :; }
  ensure_upgrade_boot_fence_guard() { :; }
  validate_upgrade_boot_fence_guard() { :; }
  trusted_systemd_path_status() {
    [ "$2" = directory ] && [ ! -L "$1" ] && [ -d "$1" ]
  }
  admin_handoff_assert_secure_file() {
    [ ! -L "$1" ] && [ -f "$1" ] && [ "$(stat -c '%a' "$1")" = "$2" ]
  }
  printf '%s\n' '123-456-700' >"${stale_marker}"
  printf '%s\n' '999999 1' >"${stale_lease}"
  chmod 600 "${stale_marker}" "${stale_lease}"
  for stale_unit in ss-stale-one.service ss-stale-two.target; do
    render_upgrade_boot_fence_dropin \
      "${stale_unit}" "${stale_marker}" "${stale_lease}" \
      >"${SYSTEMD_UNIT_ROOT}/${stale_unit}.d/90-streamserver-upgrade-fence.conf"
    chmod 644 \
      "${SYSTEMD_UNIT_ROOT}/${stale_unit}.d/90-streamserver-upgrade-fence.conf"
  done

  UPGRADE_TRANSACTION_ID='123-456-701'
  resume_upgrade_boot_fence_for_recovery
  [ ! -e "${stale_lease}" ]
  [ "${UPGRADE_BOOT_FENCE_ACTIVE}" -eq 1 ]
  set +e
  (activate_upgrade_boot_fence_lease_for_recovery) >/dev/null 2>&1
  stale_recovery_status=$?
  set -e
  [ "${stale_recovery_status}" -ne 0 ]
  [ ! -e "${stale_lease}" ]
  [ -f "${stale_marker}" ]
  for stale_unit in ss-stale-one.service ss-stale-two.target; do
    stale_dropin="${SYSTEMD_UNIT_ROOT}/${stale_unit}.d/90-streamserver-upgrade-fence.conf"
    [ -f "${stale_dropin}" ]
    case "${stale_unit}" in
      *.service)
        grep -Fq '[Service]' "${stale_dropin}"
        grep -Eq \
          '^ExecCondition=\+/usr/local/libexec/streamserver-native-installer/upgrade-fence-guard-[0-9a-f]{64} check ' \
          "${stale_dropin}"
        ;;
      *.target)
        if grep -Fq 'ExecCondition=' "${stale_dropin}"; then
          echo 'target fence contains an invalid ExecCondition directive' >&2
          exit 1
        fi
        grep -Fq 'Requires=streamserver-native-upgrade-watchdog-' "${stale_dropin}"
        ;;
    esac
  done
)

# The bypass is an actively held kernel flock, not the mere existence of a
# /run file. SIGKILL releases it automatically on the same boot, so a service
# restart cannot run through the durable fence while waiting for recovery.
(
  set +x
  lease_root="${TMP_DIR}/process-lifetime-fence"
  process_lease="${lease_root}/upgrade-contract.lease"
  lease_ready="${TMP_DIR}/process-lifetime-fence.ready"
  lease_child_file="${TMP_DIR}/process-lifetime-fence.child"
  process_marker="${TMP_DIR}/process-lifetime-fence.marker"
  process_guard="${TMP_DIR}/process-lifetime-fence.guard"
  mkdir -p "${lease_root}"
  chmod 700 "${lease_root}"
  printf '%s\n' '123-456-702' >"${process_marker}"
  upgrade_boot_fence_guard_content >"${process_guard}"
  chmod 700 "${process_guard}"
  upgrade_boot_fence_lease_path() { printf '%s' "${process_lease}"; }
  admin_handoff_assert_secure_directory() {
    [ ! -L "$1" ] && [ -d "$1" ] && [ "$(stat -c '%a' "$1")" = 700 ]
  }
  chown() { :; }
  (
    UPGRADE_BOOT_FENCE_LEASE=""
    UPGRADE_BOOT_FENCE_LEASE_FD=""
    create_upgrade_boot_fence_lease "${process_lease}"
    sleep 30 &
    lease_child=$!
    printf '%s\n' "${lease_child}" >"${lease_child_file}"
    : >"${lease_ready}"
    wait "${lease_child}"
  ) &
  lease_owner=$!
  for _ in $(seq 1 100); do
    [ -e "${lease_ready}" ] && break
    sleep 0.01
  done
  [ -e "${lease_ready}" ]
  lease_child="$(<"${lease_child_file}")"
  "${process_guard}" check "${process_marker}" "${process_lease}"
  if flock -n "${process_lease}" /bin/true; then
    kill -KILL "${lease_owner}" >/dev/null 2>&1 || true
    wait "${lease_owner}" >/dev/null 2>&1 || true
    echo 'live installer did not hold the native upgrade bypass flock' >&2
    exit 1
  fi
  kill -KILL "${lease_owner}"
  set +e
  wait "${lease_owner}" >/dev/null 2>&1
  killed_lease_status=$?
  set -e
  [ "${killed_lease_status}" -ne 0 ]
  kill -0 "${lease_child}"
  if flock -n "${process_lease}" /bin/true; then
    echo 'orphaned child did not inherit the native upgrade lease descriptor' >&2
    exit 1
  fi
  set +e
  "${process_guard}" check "${process_marker}" "${process_lease}"
  dead_owner_guard_status=$?
  set -e
  [ "${dead_owner_guard_status}" -ne 0 ]
  kill -KILL "${lease_child}"
  for _ in $(seq 1 100); do
    kill -0 "${lease_child}" >/dev/null 2>&1 || break
    sleep 0.01
  done
  flock -n "${process_lease}" /bin/true
  current_start="$(awk '{print $22}' "/proc/${BASHPID}/stat")"
  printf '%s %s\n' "${BASHPID}" "${current_start}" >"${process_lease}"
  chmod 600 "${process_lease}"
  set +e
  "${process_guard}" check "${process_marker}" "${process_lease}"
  unlocked_guard_status=$?
  set -e
  [ "${unlocked_guard_status}" -ne 0 ]
  rm -f -- "${process_marker}" "${process_lease}"
  "${process_guard}" check "${process_marker}" "${process_lease}"
  rm -f -- \
    "${process_lease}" "${lease_ready}" "${lease_child_file}" \
    "${process_marker}" "${process_guard}"
)

# Exercise the real migration function, not a call-order stub. A malformed
# legacy worker environment must fail before preflight and before any systemd
# observation or quiesce operation can occur.
(
  INSTALL_ROLE=worker-host-cpu
  INSTALL_DIR="${TMP_DIR}/upgrade-real-migration-failure"
  EMULATED_SECURITY_METADATA=1
  SYSTEMCTL_CALLS="${TMP_DIR}/upgrade-real-migration-failure-systemctl.calls"
  PREFLIGHT_MARKER="${TMP_DIR}/upgrade-real-migration-failure-preflight"
  mkdir -p "${INSTALL_DIR}"
  : >"${SYSTEMCTL_CALLS}"
  printf '%s\n' \
    'INSTALL_ROLE=worker-host-cpu' \
    'ZLM_API_HOST=legacy.example' \
    'ZLM_API_BASE=http://legacy.example:18080' >"${INSTALL_DIR}/.env"

  acquire_install_transaction_lock() { :; }
  resume_upgrade_boot_fence_for_recovery() { :; }
  prepare_upgrade_cli_identity() { :; }
  begin_upgrade_preseal_guard() { :; }
  begin_upgrade_transaction() { :; }
  seal_legacy_upgrade_environment() { :; }
  validate_sealed_upgrade_environment_identity() { :; }
  prepare_upgrade_database_configuration() { :; }
  harden_install_root_before_copy() { :; }
  prepare_package_security_probe_binaries() {
    SECURITY_PROBE_CORE_BIN=/run/contract-media-core
    SECURITY_PROBE_AGENT_BIN=/run/contract-media-agent
  }
  security_preflight_env() {
    : >"${PREFLIGHT_MARKER}"
    return 0
  }
  systemctl() {
    printf '%s\n' "$*" >>"${SYSTEMCTL_CALLS}"
    return 0
  }

  set +e
  REAL_MIGRATION_FAILURE_OUTPUT="$(prepare_upgrade_security_gate \
    "${TMP_DIR}/package-media-core" 2>&1)"
  REAL_MIGRATION_FAILURE_STATUS=$?
  set -e
  [ "${REAL_MIGRATION_FAILURE_STATUS}" -ne 0 ]
  assert_contains "${REAL_MIGRATION_FAILURE_OUTPUT}" \
    'upgrade requires ZLM_HTTP_PORT to appear exactly once before ZLM endpoint migration'
  [ ! -e "${PREFLIGHT_MARKER}" ]
  [ ! -s "${SYSTEMCTL_CALLS}" ] || {
    printf 'real migration failure touched systemd before the gate closed:\n%s\n' \
      "$(cat "${SYSTEMCTL_CALLS}")" >&2
    exit 1
  }
)

# A failed preflight must leave a healthy old deployment running. The targeted
# ZLM endpoint migration happens before it, but quiescing still must not start.
(
  INSTALL_ROLE=control-plane
  INSTALL_DIR="${TMP_DIR}/upgrade-preflight-failure"
  CALL_ORDER_FILE="${TMP_DIR}/upgrade-preflight-failure.calls"
  mkdir -p "${INSTALL_DIR}"
  : >"${CALL_ORDER_FILE}"
  acquire_install_transaction_lock() { printf '%s\n' lock >>"${CALL_ORDER_FILE}"; }
  resume_upgrade_boot_fence_for_recovery() { printf '%s\n' resume-fence >>"${CALL_ORDER_FILE}"; }
  prepare_upgrade_cli_identity() { printf '%s\n' live-identity >>"${CALL_ORDER_FILE}"; }
  begin_upgrade_preseal_guard() { printf '%s\n' preseal >>"${CALL_ORDER_FILE}"; }
  begin_upgrade_transaction() { printf '%s\n' arm-transaction >>"${CALL_ORDER_FILE}"; }
  seal_legacy_upgrade_environment() { printf '%s\n' seal-env >>"${CALL_ORDER_FILE}"; }
  validate_sealed_upgrade_environment_identity() { printf '%s\n' validate-env >>"${CALL_ORDER_FILE}"; }
  prepare_upgrade_database_configuration() { printf '%s\n' database >>"${CALL_ORDER_FILE}"; }
  harden_install_root_before_copy() { printf '%s\n' harden >>"${CALL_ORDER_FILE}"; }
  prepare_package_security_probe_binaries() {
    SECURITY_PROBE_CORE_BIN=/run/contract-media-core
    SECURITY_PROBE_AGENT_BIN=/run/contract-media-agent
    printf '%s\n' stage-probes >>"${CALL_ORDER_FILE}"
  }
  migrate_legacy_zlm_api_endpoint() { printf '%s\n' migrate-zlm >>"${CALL_ORDER_FILE}"; }
  capture_upgrade_service_state() { printf '%s\n' capture-state >>"${CALL_ORDER_FILE}"; }
  ensure_upgrade_preflight_database_available() { printf '%s\n' database-ready >>"${CALL_ORDER_FILE}"; }
  security_preflight_env() { printf '%s\n' preflight >>"${CALL_ORDER_FILE}"; return 1; }
  quiesce_captured_upgrade_services() { printf '%s\n' quiesce >>"${CALL_ORDER_FILE}"; }
  prepare_pending_admin_password_handoff() { printf '%s\n' handoff >>"${CALL_ORDER_FILE}"; }

  set +e
  (prepare_upgrade_security_gate "${TMP_DIR}/package-media-core") >/dev/null 2>&1
  FAILED_UPGRADE_PREFLIGHT_STATUS=$?
  set -e
  [ "${FAILED_UPGRADE_PREFLIGHT_STATUS}" -ne 0 ]
  [ "$(tr '\n' ' ' <"${CALL_ORDER_FILE}")" = \
    'lock resume-fence live-identity preseal seal-env validate-env database capture-state arm-transaction stage-probes migrate-zlm database-ready preflight ' ]
)

# Once quiescing has started, any later installer failure restores the captured
# state before the original non-zero exit is returned.
(
  UPGRADE_RESTORE_ON_FAILURE=1
  RESTORE_MARKER="${TMP_DIR}/upgrade-exit-restore.calls"
  : >"${RESTORE_MARKER}"
  restore_captured_upgrade_service_state() {
    printf '%s\n' restore >>"${RESTORE_MARKER}"
  }
  cleanup_admin_password() {
    printf '%s\n' cleanup >>"${RESTORE_MARKER}"
  }
  set +e
  (trap cleanup_installer_state EXIT; false)
  RESTORE_EXIT_STATUS=$?
  set -e
  [ "${RESTORE_EXIT_STATUS}" -ne 0 ]
  [ "$(tr '\n' ' ' <"${RESTORE_MARKER}")" = 'restore cleanup ' ]
)

# The transaction snapshot is armed before the migration/preflight gate. A
# successful legacy migration followed by a failed preflight must restore the
# exact old environment without observing, stopping, or restarting services.
(
  set +x
  INSTALL_DIR="${TMP_DIR}/sealed-control-inode"
  mkdir -p "${INSTALL_DIR}/bin"
  printf '%s\n' trusted-control >"${INSTALL_DIR}/bin/streamserverctl"
  chmod 755 "${INSTALL_DIR}/bin/streamserverctl"
  old_control_inode="$(stat -c '%d:%i' "${INSTALL_DIR}/bin/streamserverctl")"
  exec 8>>"${INSTALL_DIR}/bin/streamserverctl"
  chown() { :; }
  seal_certificate_tree() { :; }
  harden_install_root_before_copy
  printf '%s\n' stale-fd-mutation >&8
  exec 8>&-
  [ "$(stat -c '%d:%i' "${INSTALL_DIR}/bin/streamserverctl")" != \
    "${old_control_inode}" ]
  if grep -Fq stale-fd-mutation "${INSTALL_DIR}/bin/streamserverctl"; then
    echo 'legacy writable FD can still mutate the sealed control baseline' >&2
    exit 1
  fi
)

(
  set +x
  UPGRADE=1
  INSTALL_ROLE=worker-host-cpu
  INSTANCE_NAME=contract-preflight-rollback
  UNIT_BASENAME=ss-contract-preflight-rollback
  INSTALL_DIR="${TMP_DIR}/upgrade-preflight-byte-rollback"
  EMULATED_SECURITY_METADATA=1
  ADMIN_HANDOFF_STATE_ROOT="${TMP_DIR}/upgrade-preflight-byte-state"
  SYSTEMD_UNIT_ROOT="${TMP_DIR}/upgrade-preflight-byte-units"
  SYSTEMCTL_CALLS="${TMP_DIR}/upgrade-preflight-byte-systemctl.calls"
  mkdir -p "${INSTALL_DIR}/bin" "${SYSTEMD_UNIT_ROOT}" \
    "${ADMIN_HANDOFF_STATE_ROOT}"
  chmod 700 "${ADMIN_HANDOFF_STATE_ROOT}"
  : >"${SYSTEMCTL_CALLS}"
  printf '%s\n' '[Unit]' \
    >"${SYSTEMD_UNIT_ROOT}/${UNIT_BASENAME}.target"
  printf '%s\n' '[Service]' \
    >"${SYSTEMD_UNIT_ROOT}/${UNIT_BASENAME}-agent.service"
  printf '%s\n' '[Service]' \
    >"${SYSTEMD_UNIT_ROOT}/${UNIT_BASENAME}-zlm.service"
  chmod 644 "${SYSTEMD_UNIT_ROOT}/${UNIT_BASENAME}.target" \
    "${SYSTEMD_UNIT_ROOT}/${UNIT_BASENAME}-agent.service" \
    "${SYSTEMD_UNIT_ROOT}/${UNIT_BASENAME}-zlm.service"
  printf '%s\n' \
    'INSTALL_ROLE=worker-host-cpu' \
    'INSTANCE_NAME=contract-preflight-rollback' \
    'ZLM_HTTP_PORT=18080' \
    'ZLM_API_HOST=legacy.internal' \
    'ZLM_API_SECRET=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa' \
    'HOOK_SHARED_SECRET=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb' \
    >"${INSTALL_DIR}/.env"
  chmod 0640 "${INSTALL_DIR}/.env"
  touch -m -d '2025-01-02 03:04:05 UTC' "${INSTALL_DIR}/.env"
  preflight_env_mode="$(stat -c '%a' "${INSTALL_DIR}/.env")"
  preflight_env_mtime="$(stat -c '%y' "${INSTALL_DIR}/.env")"
  cp -a -- "${INSTALL_DIR}/.env" "${INSTALL_DIR}/.env.expected"
  printf '%s\n' old-running-binary >"${INSTALL_DIR}/bin/media-agent"
  exec 7>>"${INSTALL_DIR}/bin/media-agent"

  acquire_install_transaction_lock() { :; }
  chown() { :; }
  resume_upgrade_boot_fence_for_recovery() { :; }
  prepare_upgrade_cli_identity() { :; }
  assert_install_transaction_lock_held() { :; }
  ensure_admin_handoff_state_dir() {
    mkdir -p "$(admin_handoff_state_dir)"
    chmod 700 "${ADMIN_HANDOFF_STATE_ROOT}" "$(admin_handoff_state_dir)"
  }
  admin_handoff_assert_secure_directory() {
    [ ! -L "$1" ] && [ -d "$1" ] && [ "$(stat -c '%a' "$1")" = 700 ]
  }
  validate_sealed_upgrade_environment_identity() { :; }
  prepare_upgrade_database_configuration() { :; }
  harden_install_root_before_copy() { :; }
  prepare_package_security_probe_binaries() {
    SECURITY_PROBE_CORE_BIN=/run/contract-media-core
    SECURITY_PROBE_AGENT_BIN=/run/contract-media-agent
  }
  capture_upgrade_service_state() { :; }
  ensure_upgrade_preflight_database_available() { :; }
  prepare_pending_admin_password_handoff() { :; }
  quiesce_captured_upgrade_services() {
    echo 'preflight failure unexpectedly reached quiesce' >&2
    return 97
  }
  describe_tcp_port_usage() { :; }
  security_preflight_env() { return 1; }
  systemctl() {
    printf '%s\n' "$*" >>"${SYSTEMCTL_CALLS}"
    case "$1" in
      is-enabled) printf '%s\n' disabled; return 1 ;;
      *) return 0 ;;
    esac
  }
  ensure_admin_handoff_state_dir

  set +e
  preflight_rollback_output="$(prepare_upgrade_security_gate \
    "${TMP_DIR}/package-media-core" 2>&1)"
  preflight_rollback_status=$?
  set -e
  [ "${preflight_rollback_status}" -ne 0 ]
  cmp -s "${INSTALL_DIR}/.env.expected" "${INSTALL_DIR}/.env" || {
    printf 'preflight failure did not restore the exact pre-migration environment:\n%s\n' \
      "${preflight_rollback_output}" >&2
    exit 1
  }
  [ "$(stat -c '%a' "${INSTALL_DIR}/.env")" = "${preflight_env_mode}" ] \
    && [ "$(stat -c '%y' "${INSTALL_DIR}/.env")" = "${preflight_env_mtime}" ] || {
    echo 'preflight failure did not restore environment metadata' >&2
    exit 1
  }
  exec 7>&-
  [ "$(cat "${INSTALL_DIR}/bin/media-agent")" = old-running-binary ] || {
    echo 'pre-quiesce rollback did not restore the original binary bytes' >&2
    exit 1
  }
  if grep -E '^(stop|start|restart) ' "${SYSTEMCTL_CALLS}" \
    | grep -Fq "${UNIT_BASENAME}"; then
    printf 'preflight rollback disturbed the running deployment:\n%s\n' \
      "$(cat "${SYSTEMCTL_CALLS}")" >&2
    exit 1
  fi
)

# A process death after the durable preseal barrier is recovered by the next
# locked invocation without touching services.  Environment and installation
# root metadata retain nanosecond timestamp precision.
(
  set +x
  UPGRADE=1
  INSTALL_ROLE=worker-host-cpu
  INSTANCE_NAME=contract-preseal-recovery
  UNIT_BASENAME=ss-contract-preseal-recovery
  INSTALL_DIR="${TMP_DIR}/preseal-recovery-install"
  ADMIN_HANDOFF_STATE_ROOT="${TMP_DIR}/preseal-recovery-state"
  SYSTEMD_UNIT_ROOT="${TMP_DIR}/preseal-recovery-units"
  EMULATED_SECURITY_METADATA=1
  mkdir -p "${INSTALL_DIR}" "${ADMIN_HANDOFF_STATE_ROOT}" \
    "${SYSTEMD_UNIT_ROOT}"
  chmod 700 "${ADMIN_HANDOFF_STATE_ROOT}"
  printf '%s\n' \
    'INSTALL_ROLE=worker-host-cpu' \
    'INSTANCE_NAME=contract-preseal-recovery' \
    'SYSTEMD_TARGET=ss-contract-preseal-recovery.target' \
    'SYSTEMD_CORE_UNIT=ss-contract-preseal-recovery-core.service' \
    'SYSTEMD_AGENT_UNIT=ss-contract-preseal-recovery-agent.service' \
    'SYSTEMD_ZLM_UNIT=ss-contract-preseal-recovery-zlm.service' \
    'SYSTEMD_POSTGRES_UNIT=ss-contract-preseal-recovery-postgres.service' \
    'ORIGINAL_SENTINEL=original-environment' \
    >"${INSTALL_DIR}/.env"
  chmod 0640 "${INSTALL_DIR}/.env"
  touch -m -d '2025-03-04 05:06:07.123456789 UTC' "${INSTALL_DIR}/.env"
  chmod 0751 "${INSTALL_DIR}"
  touch -m -d '2025-03-04 05:06:08.987654321 UTC' "${INSTALL_DIR}"
  cp -a -- "${INSTALL_DIR}/.env" "${TMP_DIR}/preseal-recovery.env.expected"
  expected_root_mode="$(stat -c '%a' "${INSTALL_DIR}")"
  expected_root_mtime="$(stat -c '%y' "${INSTALL_DIR}")"
  assert_install_transaction_lock_held() { :; }
  ensure_admin_handoff_state_dir() {
    mkdir -p "$(admin_handoff_state_dir)"
    chmod 700 "${ADMIN_HANDOFF_STATE_ROOT}" "$(admin_handoff_state_dir)"
  }
  admin_handoff_secure_directory_status() {
    [ ! -L "$1" ] && [ -d "$1" ] && [ "$(stat -c '%a' "$1")" = 700 ]
  }
  admin_handoff_secure_file_status() {
    [ ! -L "$1" ] && [ -f "$1" ] && [ "$(stat -c '%a' "$1")" = "$2" ]
  }
  bounded_upgrade_systemctl() { :; }
  chown() { :; }
  ensure_admin_handoff_state_dir

  begin_upgrade_preseal_guard
  interrupted_transaction="${UPGRADE_TRANSACTION_DIR}"
  [ "$(<"${interrupted_transaction}/snapshot-kind")" = minimal ]
  grep -Eq \
    '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$' \
    "${interrupted_transaction}/boot-id"
  seal_legacy_upgrade_environment
  [ "$(stat -c '%a' "${INSTALL_DIR}/.env")" = 600 ]
  [ "$(stat -c '%a' "${INSTALL_DIR}")" = 755 ]
  trap - EXIT
  UPGRADE_TRANSACTION_STATE=none
  UPGRADE_TRANSACTION_DIR=""
  UPGRADE_TRANSACTION_ID=""
  UPGRADE_TRANSACTION_PHASE_FILE=""

  begin_upgrade_preseal_guard
  [ ! -e "${interrupted_transaction}" ] || {
    echo 'preseal recovery retained the interrupted transaction' >&2
    exit 1
  }
  cmp -s "${TMP_DIR}/preseal-recovery.env.expected" "${INSTALL_DIR}/.env" || {
    echo 'preseal recovery changed the original environment bytes' >&2
    exit 1
  }
  [ "$(stat -c '%a' "${INSTALL_DIR}")" = "${expected_root_mode}" ] || {
    echo 'preseal recovery changed the installation root mode' >&2
    exit 1
  }
  [ "$(stat -c '%y' "${INSTALL_DIR}")" = "${expected_root_mtime}" ] || {
    printf 'preseal recovery changed root mtime: expected %s, got %s\n' \
      "${expected_root_mtime}" "$(stat -c '%y' "${INSTALL_DIR}")" >&2
    exit 1
  }
  restore_upgrade_preseal_guard
  trap - EXIT
)

# A concurrent legacy writer during the full snapshot cannot produce an armed
# rollback image.  The source-before/source-after/snapshot fingerprints must
# all agree before the phase can advance.
(
  set +x
  UPGRADE=1
  INSTALL_ROLE=worker-host-cpu
  INSTANCE_NAME=contract-snapshot-race
  UNIT_BASENAME=ss-contract-snapshot-race
  INSTALL_DIR="${TMP_DIR}/snapshot-race-install"
  ADMIN_HANDOFF_STATE_ROOT="${TMP_DIR}/snapshot-race-state"
  SYSTEMD_UNIT_ROOT="${TMP_DIR}/snapshot-race-units"
  mkdir -p "${INSTALL_DIR}/runtime" "${ADMIN_HANDOFF_STATE_ROOT}" \
    "${SYSTEMD_UNIT_ROOT}"
  chmod 700 "${ADMIN_HANDOFF_STATE_ROOT}"
  printf '%s\n' old-env >"${INSTALL_DIR}/.env"
  printf '%s\n' stable-runtime >"${INSTALL_DIR}/runtime/asset"
  assert_install_transaction_lock_held() { :; }
  ensure_admin_handoff_state_dir() {
    mkdir -p "$(admin_handoff_state_dir)"
    chmod 700 "${ADMIN_HANDOFF_STATE_ROOT}" "$(admin_handoff_state_dir)"
  }
  admin_handoff_secure_directory_status() {
    [ ! -L "$1" ] && [ -d "$1" ] && [ "$(stat -c '%a' "$1")" = 700 ]
  }
  admin_handoff_secure_file_status() {
    [ ! -L "$1" ] && [ -f "$1" ] && [ "$(stat -c '%a' "$1")" = "$2" ]
  }
  upgrade_transaction_install_items() { printf '%s\n' .env runtime; }
  upgrade_transaction_unit_names() { :; }
  bounded_upgrade_systemctl() { :; }
  ensure_admin_handoff_state_dir
  begin_upgrade_preseal_guard
  race_transaction="${UPGRADE_TRANSACTION_DIR}"
  copy_upgrade_transaction_entry() {
    command cp -a --no-dereference --reflink=auto -- "$1" "$2" || return
    if [ "$1" = "${INSTALL_DIR}/runtime" ]; then
      printf '%s\n' concurrent-change >>"${INSTALL_DIR}/runtime/asset"
    fi
  }
  set +e
  (begin_upgrade_transaction) >/dev/null 2>&1
  snapshot_race_status=$?
  set -e
  [ "${snapshot_race_status}" -ne 0 ]
  [ ! -e "$(admin_handoff_state_dir)/upgrade-transaction" ]
  [ "$(cat "${race_transaction}/phase")" = presealed ]
  grep -Fq concurrent-change "${INSTALL_DIR}/runtime/asset"
  restore_upgrade_preseal_guard
  [ ! -e "${race_transaction}" ]
  trap - EXIT
  UPGRADE_TRANSACTION_STATE=none
)

# A large install tree may take longer than the systemd query budget to
# snapshot. Unit enablement receives a fresh deadline after that snapshot.
(
  set +x
  UPGRADE=1
  UPGRADE_TRANSACTION_STATE=none
  UPGRADE_SERVICE_STATE_CAPTURED=0
  UPGRADE_RESTORE_ON_FAILURE=0
  INSTALL_ROLE=control-plane
  INSTANCE_NAME=contract-slow-snapshot-deadline
  UNIT_BASENAME=ss-contract-slow-snapshot-deadline
  INSTALL_DIR="${TMP_DIR}/slow-snapshot-deadline-install"
  ADMIN_HANDOFF_STATE_ROOT="${TMP_DIR}/slow-snapshot-deadline-state"
  SYSTEMD_UNIT_ROOT="${TMP_DIR}/slow-snapshot-deadline-units"
  CAPTURE_DEADLINE_FILE="${TMP_DIR}/slow-snapshot-deadline.capture"
  EMULATED_SECURITY_METADATA=1
  mkdir -p "${INSTALL_DIR}/runtime" "${ADMIN_HANDOFF_STATE_ROOT}" \
    "${SYSTEMD_UNIT_ROOT}"
  chmod 700 "${ADMIN_HANDOFF_STATE_ROOT}"
  printf '%s\n' old-env >"${INSTALL_DIR}/.env"
  printf '%s\n' stable-runtime >"${INSTALL_DIR}/runtime/asset"
  printf '%s\n' '[Unit]' 'Description=slow snapshot deadline fixture' \
    >"${SYSTEMD_UNIT_ROOT}/${UNIT_BASENAME}.target"
  assert_install_transaction_lock_held() { :; }
  ensure_admin_handoff_state_dir() {
    mkdir -p "$(admin_handoff_state_dir)"
    chmod 700 "${ADMIN_HANDOFF_STATE_ROOT}" "$(admin_handoff_state_dir)"
  }
  admin_handoff_secure_directory_status() {
    [ ! -L "$1" ] && [ -d "$1" ] && [ "$(stat -c '%a' "$1")" = 700 ]
  }
  admin_handoff_secure_file_status() {
    [ ! -L "$1" ] && [ -f "$1" ] && [ "$(stat -c '%a' "$1")" = "$2" ]
  }
  upgrade_transaction_install_items() { printf '%s\n' .env runtime; }
  upgrade_transaction_unit_names() {
    printf '%s\n' "${UNIT_BASENAME}.target"
  }
  eval "$(declare -f snapshot_upgrade_transaction_entry \
    | sed '1s/snapshot_upgrade_transaction_entry/snapshot_upgrade_transaction_entry_without_delay/')"
  snapshot_upgrade_transaction_entry() {
    snapshot_upgrade_transaction_entry_without_delay "$@"
    if [ "$1" = "${INSTALL_DIR}/runtime" ]; then
      SECONDS=$((SECONDS + 61))
    fi
  }
  capture_upgrade_unit_enablement() {
    local deadline="$2"
    printf '%s %s\n' "${deadline}" "${SECONDS}" >"${CAPTURE_DEADLINE_FILE}"
    [ "${deadline}" -gt "${SECONDS}" ] || return 124
    printf '%s' enabled
  }
  ensure_admin_handoff_state_dir
  SECONDS=0
  trap 'echo "slow install snapshot consumed the later systemd capture deadline" >&2' ERR
  begin_upgrade_preseal_guard
  begin_upgrade_transaction
  trap - ERR
  [ "${UPGRADE_TRANSACTION_STATE}" = armed ]
  read -r captured_deadline captured_at <"${CAPTURE_DEADLINE_FILE}"
  [ "${captured_deadline}" -gt "${captured_at}" ] || {
    echo 'unit enablement did not receive a fresh post-snapshot deadline' >&2
    exit 1
  }
  trap - EXIT
  rm -rf -- "${ADMIN_HANDOFF_STATE_ROOT}"
  UPGRADE_TRANSACTION_STATE=none
)

# Every snapshot data/metadata sync and file enumeration failure is fatal.
# An intermediate failure may never be hidden by a later successful command
# and may never advance the transaction to armed.
for durability_fault in sync find; do
  (
    set +x
    UPGRADE=1
    INSTALL_ROLE=control-plane
    INSTANCE_NAME=contract-snapshot-durability
    UNIT_BASENAME=ss-contract-snapshot-durability
    INSTALL_DIR="${TMP_DIR}/snapshot-durability-${durability_fault}"
    ADMIN_HANDOFF_STATE_ROOT="${TMP_DIR}/snapshot-durability-state-${durability_fault}"
    SYSTEMD_UNIT_ROOT="${TMP_DIR}/snapshot-durability-units-${durability_fault}"
    ARMED_MARKER="${TMP_DIR}/snapshot-durability-armed-${durability_fault}"
    mkdir -p "${INSTALL_DIR}" "${SYSTEMD_UNIT_ROOT}" \
      "${ADMIN_HANDOFF_STATE_ROOT}"
    chmod 700 "${ADMIN_HANDOFF_STATE_ROOT}"
    printf '%s\n' old-env >"${INSTALL_DIR}/.env"
    assert_install_transaction_lock_held() { :; }
    ensure_admin_handoff_state_dir() {
      mkdir -p "$(admin_handoff_state_dir)"
      chmod 700 "${ADMIN_HANDOFF_STATE_ROOT}" "$(admin_handoff_state_dir)"
    }
    admin_handoff_assert_secure_file() {
      [ ! -L "$1" ] && [ -f "$1" ]
    }
    systemctl() { printf '%s\n' not-found; return 1; }
    ensure_admin_handoff_state_dir
    cleanup_installer_state() { :; }
    case "${durability_fault}" in
      sync)
        sync_calls=0
        sync() {
          sync_calls=$((sync_calls + 1))
          [ "${sync_calls}" -ne 2 ] || return 73
          command sync "$@"
        }
        ;;
      find)
        find() {
          command find "$@"
          return 74
        }
        ;;
    esac
    set +e
    (
      begin_upgrade_transaction
      [ "${UPGRADE_TRANSACTION_STATE}" != armed ] || : >"${ARMED_MARKER}"
    ) >/dev/null 2>&1
    durability_status=$?
    set -e
    [ "${durability_status}" -ne 0 ]
    [ ! -e "${ARMED_MARKER}" ]
  )
done

# The transaction snapshot needs a private umask while it is being built, but
# that process-wide setting must not leak into subsequently copied runtime
# directories. A leaked 0077 makes the service WorkingDirectory root-only and
# causes a real systemd CHDIR failure after the old services are quiesced.
(
  set +x
  UPGRADE=1
  INSTALL_ROLE=control-plane
  INSTANCE_NAME=contract-transaction-umask
  UNIT_BASENAME=ss-contract-transaction-umask
  INSTALL_DIR="${TMP_DIR}/transaction-umask-install"
  ADMIN_HANDOFF_STATE_ROOT="${TMP_DIR}/transaction-umask-state"
  SYSTEMD_UNIT_ROOT="${TMP_DIR}/transaction-umask-units"
  mkdir -p "${INSTALL_DIR}" "${ADMIN_HANDOFF_STATE_ROOT}" \
    "${SYSTEMD_UNIT_ROOT}"
  chmod 700 "${ADMIN_HANDOFF_STATE_ROOT}"
  assert_install_transaction_lock_held() { :; }
  ensure_admin_handoff_state_dir() {
    mkdir -p "$(admin_handoff_state_dir)"
    chmod 700 "${ADMIN_HANDOFF_STATE_ROOT}" "$(admin_handoff_state_dir)"
  }
  upgrade_transaction_install_items() { :; }
  upgrade_transaction_unit_names() { :; }
  ensure_admin_handoff_state_dir
  umask 0022
  caller_umask="$(umask)"
  begin_upgrade_transaction
  trap - EXIT
  transaction_umask="$(umask)"
  UPGRADE_TRANSACTION_STATE=none
  rm -rf -- "$(admin_handoff_state_dir)"
  [ "${transaction_umask}" = "${caller_umask}" ] || {
    printf 'upgrade transaction leaked umask %s (expected %s)\n' \
      "${transaction_umask}" "${caller_umask}" >&2
    exit 1
  }
)

# Control/runtime trees are not secret material and must remain traversable by
# their systemd service users even when root invokes the installer with a
# restrictive caller umask. Secret certificate directories are explicitly
# requested as 0750 and are sealed again after generation.
(
  set +x
  EMULATED_SECURITY_METADATA=1
  INSTALL_DIR="${TMP_DIR}/control-directory-root-equality"
  mkdir -p "${INSTALL_DIR}"
  chmod 700 "${INSTALL_DIR}"

  # install_tree() legitimately asks to prepare INSTALL_DIR itself when a
  # top-level target such as ui/ or docs/ is published. Equality with the
  # trusted root is inside the boundary; only siblings and ancestors escape.
  ensure_control_directory "${INSTALL_DIR}"
  [ "$(stat -c '%a' "${INSTALL_DIR}")" = 755 ]

  set +e
  (ensure_control_directory "${INSTALL_DIR}-sibling") >/dev/null 2>&1
  sibling_status=$?
  set -e
  [ "${sibling_status}" -ne 0 ]
)

(
  set +x
  INSTALL_DIR="${TMP_DIR}/restrictive-caller-umask-install"
  source_tree="${TMP_DIR}/restrictive-caller-umask-source"
  mkdir -p "${INSTALL_DIR}" "${source_tree}/nested"
  printf '%s\n' '#!/usr/bin/env sh' 'exit 0' >"${source_tree}/nested/tool"
  chmod 755 "${source_tree}/nested/tool"
  chmod 700 "${source_tree}" "${source_tree}/nested"
  umask 0077
  ensure_control_directory "${INSTALL_DIR}/bin"
  ensure_control_directory "${INSTALL_DIR}/certs/auth" 750
  install_tree "${source_tree}" "${INSTALL_DIR}/runtime/zlm"
  [ "$(stat -c '%a' "${INSTALL_DIR}/bin")" = 755 ]
  [ "$(stat -c '%a' "${INSTALL_DIR}/certs")" = 750 ]
  [ "$(stat -c '%a' "${INSTALL_DIR}/certs/auth")" = 750 ]
  [ "$(stat -c '%a' "${INSTALL_DIR}/runtime/zlm")" = 755 ]
  [ "$(stat -c '%a' "${INSTALL_DIR}/runtime/zlm/nested")" = 755 ]
  [ "$(stat -c '%a' "${INSTALL_DIR}/runtime/zlm/nested/tool")" = 755 ]
)

# A control-tree hard link can otherwise make recursive chmod/chown mutate an
# inode outside the installation transaction. Reject it before any hardening.
(
  set +x
  hardlink_tree="${TMP_DIR}/control-hardlink-tree"
  hardlink_outside="${TMP_DIR}/control-hardlink-outside"
  mkdir -p "${hardlink_tree}"
  printf '%s\n' outside-baseline >"${hardlink_outside}"
  chmod 640 "${hardlink_outside}"
  outside_before="$(stat -c '%u:%g:%a:%s' "${hardlink_outside}")"
  ln "${hardlink_outside}" "${hardlink_tree}/linked"
  set +e
  (assert_control_tree_safe "${hardlink_tree}" structural) >/dev/null 2>&1
  hardlink_status=$?
  set -e
  [ "${hardlink_status}" -ne 0 ]
  [ "$(stat -c '%u:%g:%a:%s' "${hardlink_outside}")" = "${outside_before}" ]
  [ "$(cat "${hardlink_outside}")" = outside-baseline ]
)

# A fresh local-password install fingerprints the JWT public key as the
# service account before the final permission-sealing pass. Every certificate
# directory component therefore has to receive the service group immediately,
# while remaining root-owned and non-writable by that group. Stub chown so this
# contract is enforceable in the unprivileged CI job as well as by the real
# root install smoke.
(
  set +x
  INSTALL_DIR="${TMP_DIR}/service-readable-auth-directory"
  SERVICE_GROUP=streamserver-contract
  EMULATED_SECURITY_METADATA=0
  CHOWN_LOG="${TMP_DIR}/service-readable-auth-directory.chown"
  mkdir -p "${INSTALL_DIR}"
  id() {
    [ "${1:-}" = '-u' ] && { printf '%s\n' 0; return 0; }
    command id "$@"
  }
  chown() {
    printf '%s\n' "$*" >>"${CHOWN_LOG}"
  }
  ensure_control_directory \
    "${INSTALL_DIR}/certs/auth" 750 "root:${SERVICE_GROUP}"
  [ "$(stat -c '%a' "${INSTALL_DIR}/certs")" = 750 ]
  [ "$(stat -c '%a' "${INSTALL_DIR}/certs/auth")" = 750 ]
  [ "$(grep -Fxc -- '-h root:streamserver-contract '"${INSTALL_DIR}"'/certs' \
      "${CHOWN_LOG}")" -eq 1 ]
  [ "$(grep -Fxc -- '-h root:streamserver-contract '"${INSTALL_DIR}"'/certs/auth' \
      "${CHOWN_LOG}")" -eq 1 ]
)

# A service-readability failure must be reported through the installer's error
# contract instead of being converted by `set -e` into a silent exit.
(
  set +x
  INSTALL_DIR="${TMP_DIR}/unreadable-handoff-key"
  mkdir -p "${INSTALL_DIR}"
  ensure_admin_handoff_state_dir() { :; }
  delivered_admin_handoff_path() { printf '%s' "${INSTALL_DIR}/delivered"; }
  pending_admin_handoff_path() { printf '%s' "${INSTALL_DIR}/pending"; }
  generate_admin_handoff_id() {
    printf '%s' '0190d8d4-31d2-7b23-b27e-8b9b28a2ed11'
  }
  admin_handoff_public_key_fingerprint() { return 1; }
  set +e
  UNREADABLE_KEY_OUTPUT="$(write_pending_admin_handoff_marker admin 2>&1)"
  UNREADABLE_KEY_STATUS=$?
  set -e
  [ "${UNREADABLE_KEY_STATUS}" -ne 0 ]
  assert_contains "${UNREADABLE_KEY_OUTPUT}" \
    'administrator handoff JWT public key is not readable by the service account'
)

# Once the durable phase is committed, cleanup is best effort. A successful
# snapshot removal followed by parent-fsync failure must never re-enter
# rollback or claim that the removed snapshot was retained.
(
  set +x
  UPGRADE=1
  INSTALL_ROLE=control-plane
  INSTANCE_NAME=contract-terminal-commit
  UNIT_BASENAME=ss-contract-terminal-commit
  INSTALL_DIR="${TMP_DIR}/terminal-commit-install"
  ADMIN_HANDOFF_STATE_ROOT="${TMP_DIR}/terminal-commit-state"
  SYSTEMD_UNIT_ROOT="${TMP_DIR}/terminal-commit-units"
  RESTORE_AFTER_COMMIT_MARKER="${TMP_DIR}/terminal-commit-restored"
  GC_REMOVED_MARKER="${TMP_DIR}/terminal-commit-gc-removed"
  mkdir -p "${INSTALL_DIR}" "${SYSTEMD_UNIT_ROOT}" \
    "${ADMIN_HANDOFF_STATE_ROOT}"
  chmod 700 "${ADMIN_HANDOFF_STATE_ROOT}"
  printf '%s\n' old-env >"${INSTALL_DIR}/.env"
  printf '%s\n' '[Unit]' \
    >"${SYSTEMD_UNIT_ROOT}/${UNIT_BASENAME}.target"
  printf '%s\n' '[Service]' \
    >"${SYSTEMD_UNIT_ROOT}/${UNIT_BASENAME}-core.service"
  chmod 644 "${SYSTEMD_UNIT_ROOT}/${UNIT_BASENAME}.target" \
    "${SYSTEMD_UNIT_ROOT}/${UNIT_BASENAME}-core.service"
  assert_install_transaction_lock_held() { :; }
  ensure_admin_handoff_state_dir() {
    mkdir -p "$(admin_handoff_state_dir)"
    chmod 700 "${ADMIN_HANDOFF_STATE_ROOT}" "$(admin_handoff_state_dir)"
  }
  admin_handoff_assert_secure_file() {
    [ ! -L "$1" ] && [ -f "$1" ]
  }
  admin_handoff_assert_secure_directory() {
    [ ! -L "$1" ] && [ -d "$1" ] && [ "$(stat -c '%a' "$1")" = 700 ]
  }
  systemctl() { printf '%s\n' not-found; return 1; }
  ensure_admin_handoff_state_dir
  begin_upgrade_transaction
  restore_upgrade_transaction() {
    : >"${RESTORE_AFTER_COMMIT_MARKER}"
    return 1
  }
  clear_upgrade_boot_fence() { :; }
  rm() {
    command rm "$@" || return
    case " $* " in *upgrade-transaction*) : >"${GC_REMOVED_MARKER}" ;; esac
  }
  sync() {
    if [ -e "${GC_REMOVED_MARKER}" ] \
      && [ "${*: -1}" = "$(admin_handoff_state_dir)" ]; then
      return 75
    fi
    command sync "$@"
  }
  set +e
  (commit_upgrade_transaction) >/dev/null 2>&1
  terminal_commit_status=$?
  set -e
  [ "${terminal_commit_status}" -eq 0 ]
  [ ! -e "${RESTORE_AFTER_COMMIT_MARKER}" ]
)

# Rollback masks repeated termination signals until restore reaches a durable
# terminal phase; the original failure status remains authoritative.
(
  set +x
  ROLLBACK_SIGNAL_MARKER="${TMP_DIR}/rollback-signal-complete"
  set +e
  (
    UPGRADE_TRANSACTION_STATE=armed
    restore_upgrade_transaction() {
      kill -TERM "${BASHPID}"
      kill -TERM "${BASHPID}"
      : >"${ROLLBACK_SIGNAL_MARKER}"
      UPGRADE_TRANSACTION_STATE=restored
      return 0
    }
    cleanup_admin_password() { :; }
    trap cleanup_installer_state EXIT
    false
  ) >/dev/null 2>&1
  rollback_signal_status=$?
  set -e
  [ "${rollback_signal_status}" -eq 1 ]
  [ -e "${ROLLBACK_SIGNAL_MARKER}" ]
)

# A durable terminal transaction left at the fixed name (or a prior terminal
# tombstone/decision marker) is garbage, not an unresolved armed rollback.
# The next locked upgrade must collect it safely; armed/unknown/unsafe entries
# remain fail-closed.
declare -F garbage_collect_resolved_upgrade_transactions >/dev/null || {
  echo 'installer is missing durable terminal transaction recovery/GC' >&2
  exit 1
}
for recovered_phase in committed restored; do
  for recovered_shape in fixed tombstone; do
  (
    set +x
    UPGRADE=1
    INSTALL_ROLE=control-plane
    INSTANCE_NAME="contract-terminal-gc-${recovered_phase}-${recovered_shape}"
    UNIT_BASENAME="ss-contract-terminal-gc-${recovered_phase}-${recovered_shape}"
    INSTALL_DIR="${TMP_DIR}/terminal-gc-${recovered_phase}-${recovered_shape}-install"
    ADMIN_HANDOFF_STATE_ROOT="${TMP_DIR}/terminal-gc-${recovered_phase}-${recovered_shape}-state"
    SYSTEMD_UNIT_ROOT="${TMP_DIR}/terminal-gc-${recovered_phase}-${recovered_shape}-units"
    EMULATED_SECURITY_METADATA=1
    mkdir -p "${INSTALL_DIR}" "${ADMIN_HANDOFF_STATE_ROOT}" \
      "${SYSTEMD_UNIT_ROOT}"
    chmod 700 "${ADMIN_HANDOFF_STATE_ROOT}"
    printf '%s\n' \
      "INSTALL_ROLE=${INSTALL_ROLE}" \
      "INSTANCE_NAME=${INSTANCE_NAME}" \
      "SYSTEMD_TARGET=${UNIT_BASENAME}.target" \
      "SYSTEMD_CORE_UNIT=${UNIT_BASENAME}-core.service" \
      "SYSTEMD_AGENT_UNIT=${UNIT_BASENAME}-agent.service" \
      "SYSTEMD_ZLM_UNIT=${UNIT_BASENAME}-zlm.service" \
      "SYSTEMD_POSTGRES_UNIT=${UNIT_BASENAME}-postgres.service" \
      >"${INSTALL_DIR}/.env"
    ensure_admin_handoff_state_dir() {
      mkdir -p "$(admin_handoff_state_dir)"
      chmod 700 "${ADMIN_HANDOFF_STATE_ROOT}" "$(admin_handoff_state_dir)"
    }
    admin_handoff_secure_directory_status() {
      [ ! -L "$1" ] && [ -d "$1" ]
    }
    admin_handoff_secure_file_status() {
      [ ! -L "$1" ] && [ -f "$1" ] && [ "$(stat -c '%a' "$1")" = "$2" ]
    }
    assert_install_transaction_lock_held() { :; }
    bounded_upgrade_systemctl() { :; }
    chown() { :; }
    ensure_admin_handoff_state_dir

    begin_upgrade_preseal_guard
    first_transaction="${UPGRADE_TRANSACTION_DIR}"
    finalize_upgrade_transaction_terminal "${recovered_phase}"
    terminal_transaction="${UPGRADE_TRANSACTION_DIR}"
    if [ "${recovered_shape}" = fixed ]; then
      fixed_terminal="$(admin_handoff_state_dir)/upgrade-transaction"
      mv -- "${terminal_transaction}" "${fixed_terminal}"
      terminal_transaction="${fixed_terminal}"
    fi
    trap - EXIT
    UPGRADE_TRANSACTION_STATE=none
    UPGRADE_TRANSACTION_DIR=""
    UPGRADE_TRANSACTION_ID=""
    UPGRADE_TRANSACTION_PHASE_FILE=""

    garbage_collect_resolved_upgrade_transactions
    [ ! -e "${first_transaction}" ]
    [ ! -e "${terminal_transaction}" ]
    [ "${UPGRADE_TRANSACTION_STATE}" = none ]
    [ -z "${UPGRADE_TRANSACTION_DIR}" ]
    [ -z "${UPGRADE_TRANSACTION_ID}" ]
    [ -z "${UPGRADE_TRANSACTION_PHASE_FILE}" ]

    # Recovery is part of the same locked CLI invocation.  Completing the old
    # terminal decision must not poison the next transaction's in-memory state.
    begin_upgrade_preseal_guard
    [ "${UPGRADE_TRANSACTION_STATE}" = presealed ]
    [ -d "${UPGRADE_TRANSACTION_DIR}" ]
    restore_upgrade_preseal_guard
    trap - EXIT
  )
  done
done

# Partial rm of a terminal tree is quarantined behind a durable decision. The
# next scanner deletes that quarantine without requiring phase/id files that
# the interrupted rm may already have removed.
(
  set +x
  UPGRADE=1
  INSTALL_DIR="${TMP_DIR}/terminal-partial-gc-install"
  ADMIN_HANDOFF_STATE_ROOT="${TMP_DIR}/terminal-partial-gc-state"
  mkdir -p "${INSTALL_DIR}" "${ADMIN_HANDOFF_STATE_ROOT}"
  chmod 700 "${ADMIN_HANDOFF_STATE_ROOT}"
  ensure_admin_handoff_state_dir() {
    mkdir -p "$(admin_handoff_state_dir)"
    chmod 700 "${ADMIN_HANDOFF_STATE_ROOT}" "$(admin_handoff_state_dir)"
  }
  admin_handoff_assert_secure_directory() {
    [ ! -L "$1" ] && [ -d "$1" ]
  }
  admin_handoff_assert_secure_file() {
    [ ! -L "$1" ] && [ -f "$1" ] && [ "$(stat -c '%a' "$1")" = "$2" ]
  }
  assert_control_tree_safe() { :; }
  ensure_admin_handoff_state_dir
  terminal_id=123-456-799
  terminal_tomb="$(admin_handoff_state_dir)/upgrade-transaction.committed.${terminal_id}"
  terminal_gc="$(admin_handoff_state_dir)/upgrade-transaction.gc.committed.${terminal_id}"
  terminal_decision="$(admin_handoff_state_dir)/upgrade-transaction.terminal.${terminal_id}"
  mkdir "${terminal_tomb}"
  chmod 700 "${terminal_tomb}"
  printf '%s\n' committed >"${terminal_tomb}/phase"
  printf '%s\n' "${terminal_id}" >"${terminal_tomb}/transaction-id"
  chmod 600 "${terminal_tomb}/phase" "${terminal_tomb}/transaction-id"
  rm() {
    local target="${!#}"
    if [[ "${target}" == *upgrade-transaction.gc.* ]]; then
      command rm -f -- "${target}/phase"
      return 75
    fi
    command rm "$@"
  }
  garbage_collect_upgrade_transaction_tree \
    "${terminal_tomb}" committed "${terminal_id}"
  [ -d "${terminal_gc}" ]
  [ ! -e "${terminal_gc}/phase" ]
  [ -f "${terminal_decision}" ]
  unset -f rm
  garbage_collect_resolved_upgrade_transactions
  [ ! -e "${terminal_gc}" ]
  [ ! -e "${terminal_decision}" ]
)
(
  set +x
  UPGRADE=1
  INSTALL_DIR="${TMP_DIR}/building-armed-gc-install"
  ADMIN_HANDOFF_STATE_ROOT="${TMP_DIR}/building-armed-gc-state"
  mkdir -p "${INSTALL_DIR}" "${ADMIN_HANDOFF_STATE_ROOT}"
  chmod 700 "${ADMIN_HANDOFF_STATE_ROOT}"
  ensure_admin_handoff_state_dir() {
    mkdir -p "$(admin_handoff_state_dir)"
    chmod 700 "${ADMIN_HANDOFF_STATE_ROOT}" "$(admin_handoff_state_dir)"
  }
  admin_handoff_secure_directory_status() {
    [ ! -L "$1" ] && [ -d "$1" ]
  }
  admin_handoff_secure_file_status() {
    [ ! -L "$1" ] && [ -f "$1" ]
  }
  assert_control_tree_safe() { :; }
  ensure_admin_handoff_state_dir
  unpublished_id=123-456-790
  unpublished_dir="$(admin_handoff_state_dir)/upgrade-transaction.building.${unpublished_id}"
  mkdir "${unpublished_dir}"
  chmod 700 "${unpublished_dir}"
  printf '%s\n' armed >"${unpublished_dir}/phase"
  printf '%s\n' "${unpublished_id}" >"${unpublished_dir}/transaction-id"
  chmod 600 "${unpublished_dir}/phase" "${unpublished_dir}/transaction-id"
  set +e
  (garbage_collect_resolved_upgrade_transactions) >/dev/null 2>&1
  unpublished_armed_status=$?
  set -e
  [ "${unpublished_armed_status}" -ne 0 ]
  [ -d "${unpublished_dir}" ]
)
(
  set +x
  UPGRADE=1
  INSTALL_DIR="${TMP_DIR}/armed-gc-install"
  ADMIN_HANDOFF_STATE_ROOT="${TMP_DIR}/armed-gc-state"
  mkdir -p "${INSTALL_DIR}" "${ADMIN_HANDOFF_STATE_ROOT}"
  chmod 700 "${ADMIN_HANDOFF_STATE_ROOT}"
  ensure_admin_handoff_state_dir() {
    mkdir -p "$(admin_handoff_state_dir)"
    chmod 700 "${ADMIN_HANDOFF_STATE_ROOT}" "$(admin_handoff_state_dir)"
  }
  admin_handoff_secure_directory_status() {
    [ ! -L "$1" ] && [ -d "$1" ]
  }
  admin_handoff_secure_file_status() {
    [ ! -L "$1" ] && [ -f "$1" ]
  }
  assert_control_tree_safe() { :; }
  ensure_admin_handoff_state_dir
  armed_fixed="$(admin_handoff_state_dir)/upgrade-transaction"
  mkdir "${armed_fixed}"
  chmod 700 "${armed_fixed}"
  printf '%s\n' armed >"${armed_fixed}/phase"
  printf '%s\n' '123-456-789' >"${armed_fixed}/transaction-id"
  chmod 600 "${armed_fixed}/phase" "${armed_fixed}/transaction-id"
  set +e
  (garbage_collect_resolved_upgrade_transactions) >/dev/null 2>&1
  armed_gc_status=$?
  set -e
  [ "${armed_gc_status}" -ne 0 ]
  [ -d "${armed_fixed}" ]
)

# A valid fixed armed transaction is self-recovering after process loss.  The
# service baseline is loaded from disk, so empty in-memory arrays cannot turn a
# formerly active component into an inactive one.
for recovery_role in control-plane all-in-one-host-cpu; do
  (
    set +x
    INSTALL_ROLE="${recovery_role}"
    UNIT_BASENAME="ss-contract-recovery-topology-${recovery_role}"
    UPGRADE_TRANSACTION_DIR="${TMP_DIR}/recovery-topology-${recovery_role}"
    mkdir -p "${UPGRADE_TRANSACTION_DIR}/unit-state"
    printf '%s\n' full >"${UPGRADE_TRANSACTION_DIR}/snapshot-kind"
    chmod 600 "${UPGRADE_TRANSACTION_DIR}/snapshot-kind"
    for topology_kind in core agent zlm postgres; do
      topology_state=absent
      case "${topology_kind}" in
        core|postgres) topology_state=file ;;
        agent|zlm)
          [ "${recovery_role}" = all-in-one-host-cpu ] && topology_state=file
          ;;
      esac
      printf '%s\n' "${topology_state}" \
        >"${UPGRADE_TRANSACTION_DIR}/unit-state/${UNIT_BASENAME}-${topology_kind}.service.state"
      chmod 600 \
        "${UPGRADE_TRANSACTION_DIR}/unit-state/${UNIT_BASENAME}-${topology_kind}.service.state"
    done
    TRUSTED_POSTGRES_UNIT_COUNT=0
    DATABASE_MODE=""
    load_upgrade_recovery_topology_from_snapshot
    [ "${TRUSTED_POSTGRES_UNIT_COUNT}" -eq 1 ]
    [ "${DATABASE_MODE}" = bundled ]
    printf '%s\n' "$(upgrade_units_for_role)" \
      | grep -Fqx "${UNIT_BASENAME}-postgres.service"
  )
done
(
  set +x
  INSTALL_ROLE=worker-host-cpu
  UNIT_BASENAME=ss-contract-invalid-recovery-topology
  UPGRADE_TRANSACTION_DIR="${TMP_DIR}/invalid-recovery-topology"
  mkdir -p "${UPGRADE_TRANSACTION_DIR}/unit-state"
  printf '%s\n' full >"${UPGRADE_TRANSACTION_DIR}/snapshot-kind"
  chmod 600 "${UPGRADE_TRANSACTION_DIR}/snapshot-kind"
  for topology_kind in core agent zlm postgres; do
    case "${topology_kind}" in agent|zlm|postgres) topology_state=file ;; *) topology_state=absent ;; esac
    printf '%s\n' "${topology_state}" \
      >"${UPGRADE_TRANSACTION_DIR}/unit-state/${UNIT_BASENAME}-${topology_kind}.service.state"
    chmod 600 \
      "${UPGRADE_TRANSACTION_DIR}/unit-state/${UNIT_BASENAME}-${topology_kind}.service.state"
  done
  set +e
  load_upgrade_recovery_topology_from_snapshot >/dev/null 2>&1
  invalid_recovery_topology_status=$?
  set -e
  [ "${invalid_recovery_topology_status}" -ne 0 ]
)

(
  set +x
  UPGRADE=1
  INSTALL_ROLE=control-plane
  INSTANCE_NAME=contract-fixed-armed-recovery
  UNIT_BASENAME=ss-contract-fixed-armed-recovery
  TRUSTED_POSTGRES_UNIT_COUNT=0
  INSTALL_DIR="${TMP_DIR}/fixed-armed-recovery-install"
  ADMIN_HANDOFF_STATE_ROOT="${TMP_DIR}/fixed-armed-recovery-state"
  SYSTEMD_UNIT_ROOT="${TMP_DIR}/fixed-armed-recovery-units"
  RECOVERY_READINESS="${TMP_DIR}/fixed-armed-recovery-readiness"
  mkdir -p "${INSTALL_DIR}/bin" "${ADMIN_HANDOFF_STATE_ROOT}" \
    "${SYSTEMD_UNIT_ROOT}"
  chmod 700 "${ADMIN_HANDOFF_STATE_ROOT}"
  printf '%s\n' \
    'INSTALL_ROLE=control-plane' \
    'INSTANCE_NAME=contract-fixed-armed-recovery' \
    'SYSTEMD_TARGET=ss-contract-fixed-armed-recovery.target' \
    'SYSTEMD_CORE_UNIT=ss-contract-fixed-armed-recovery-core.service' \
    'SYSTEMD_AGENT_UNIT=ss-contract-fixed-armed-recovery-agent.service' \
    'SYSTEMD_ZLM_UNIT=ss-contract-fixed-armed-recovery-zlm.service' \
    'SYSTEMD_POSTGRES_UNIT=ss-contract-fixed-armed-recovery-postgres.service' \
    >"${INSTALL_DIR}/.env"
  cp -a -- "${INSTALL_DIR}/.env" \
    "${TMP_DIR}/fixed-armed-recovery.env.expected"
  printf '%s\n' original-core >"${INSTALL_DIR}/bin/media-core"
  printf '%s\n' original-target \
    >"${SYSTEMD_UNIT_ROOT}/${UNIT_BASENAME}.target"
  printf '%s\n' original-core-unit \
    >"${SYSTEMD_UNIT_ROOT}/${UNIT_BASENAME}-core.service"
  target_active=1
  core_active=1
  core_pid=111
  target_enablement=enabled
  core_enablement=enabled
  assert_install_transaction_lock_held() { :; }
  ensure_admin_handoff_state_dir() {
    mkdir -p "$(admin_handoff_state_dir)"
    chmod 700 "${ADMIN_HANDOFF_STATE_ROOT}" "$(admin_handoff_state_dir)"
  }
  admin_handoff_secure_directory_status() {
    [ ! -L "$1" ] && [ -d "$1" ]
  }
  admin_handoff_secure_file_status() {
    [ ! -L "$1" ] && [ -f "$1" ]
  }
  systemctl() {
    case "$1" in
      show)
        case "$*" in
          *ActiveState*"${UNIT_BASENAME}.target")
            [ "${target_active}" -eq 1 ] && printf '%s\n' active || printf '%s\n' inactive
            ;;
          *ActiveState*"${UNIT_BASENAME}-core.service")
            [ "${core_active}" -eq 1 ] && printf '%s\n' active || printf '%s\n' inactive
            ;;
          *MainPID*"${UNIT_BASENAME}-core.service")
            [ "${core_active}" -eq 1 ] && printf '%s\n' "${core_pid}" || printf '%s\n' 0
            ;;
          *) printf '%s\n' inactive ;;
        esac
        ;;
      is-enabled)
        case "${2:-}" in
          "${UNIT_BASENAME}.target") printf '%s\n' "${target_enablement}" ;;
          "${UNIT_BASENAME}-core.service") printf '%s\n' "${core_enablement}" ;;
          *) printf '%s\n' not-found; return 1 ;;
        esac
        ;;
      stop)
        shift
        for unit in "$@"; do
          case "${unit}" in
            "${UNIT_BASENAME}.target") target_active=0 ;;
            "${UNIT_BASENAME}-core.service") core_active=0; core_pid=0 ;;
          esac
        done
        ;;
      start)
        shift
        for unit in "$@"; do
          case "${unit}" in
            "${UNIT_BASENAME}.target") target_active=1 ;;
            "${UNIT_BASENAME}-core.service") core_active=1; core_pid=222 ;;
          esac
        done
        ;;
      enable)
        shift
        for unit in "$@"; do
          case "${unit}" in
            "${UNIT_BASENAME}.target") target_enablement=enabled ;;
            "${UNIT_BASENAME}-core.service") core_enablement=enabled ;;
          esac
        done
        ;;
      disable|unmask|daemon-reload) return 0 ;;
      *) return 0 ;;
    esac
  }
  probe_upgrade_active_components_readiness() {
    : >"${RECOVERY_READINESS}"
  }
  arm_upgrade_boot_fence() { :; }
  ensure_admin_handoff_state_dir
  capture_upgrade_service_state
  begin_upgrade_preseal_guard
  begin_upgrade_transaction
  armed_transaction="${UPGRADE_TRANSACTION_DIR}"
  [ "$(<"${armed_transaction}/snapshot-kind")" = full ]
  grep -Eq \
    '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$' \
    "${armed_transaction}/boot-id"
  trap - EXIT

  printf '%s\n' partial-env >"${INSTALL_DIR}/.env"
  printf '%s\n' partial-core >"${INSTALL_DIR}/bin/media-core"
  printf '%s\n' partial-unit \
    >"${SYSTEMD_UNIT_ROOT}/${UNIT_BASENAME}-core.service"
  target_active=0
  core_active=0
  core_pid=0
  UPGRADE_TARGET_WAS_ACTIVE=0
  UPGRADE_ACTIVE_UNITS=()
  UPGRADE_ACTIVE_MAIN_PIDS=()
  UPGRADE_SERVICE_STATE_CAPTURED=0
  UPGRADE_TRANSACTION_STATE=none
  UPGRADE_TRANSACTION_DIR=""
  UPGRADE_TRANSACTION_ID=""
  UPGRADE_TRANSACTION_PHASE_FILE=""

  garbage_collect_resolved_upgrade_transactions
  [ ! -e "${armed_transaction}" ]
  cmp -s "${TMP_DIR}/fixed-armed-recovery.env.expected" \
    "${INSTALL_DIR}/.env"
  [ "$(cat "${INSTALL_DIR}/bin/media-core")" = original-core ]
  [ "$(cat "${SYSTEMD_UNIT_ROOT}/${UNIT_BASENAME}-core.service")" = \
    original-core-unit ]
  [ "${target_active}" -eq 1 ]
  [ "${core_active}" -eq 1 ]
  [ "${core_pid}" -eq 222 ]
  [ -e "${RECOVERY_READINESS}" ]
)

# After quiescing, rollback restores every control-plane artifact, external
# systemd unit, enablement bit, administrator handoff marker, and the exact
# active/inactive service set. Entries absent at snapshot time are removed.
(
  set +x
  umask 0022
  UPGRADE=1
  INSTALL_ROLE=control-plane
  INSTANCE_NAME=contract-transaction
  UNIT_BASENAME=ss-contract-transaction
  TRUSTED_POSTGRES_UNIT_COUNT=0
  INSTALL_DIR="${TMP_DIR}/upgrade-transaction-restore"
  ADMIN_HANDOFF_STATE_ROOT="${TMP_DIR}/upgrade-transaction-state"
  SYSTEMD_UNIT_ROOT="${TMP_DIR}/upgrade-transaction-units"
  SYSTEMCTL_CALLS="${TMP_DIR}/upgrade-transaction-systemctl.calls"
  ROLLBACK_READINESS_MARKER="${TMP_DIR}/upgrade-transaction-readiness"
  mkdir -p "${INSTALL_DIR}" "${SYSTEMD_UNIT_ROOT}" \
    "${ADMIN_HANDOFF_STATE_ROOT}"
  chmod 700 "${ADMIN_HANDOFF_STATE_ROOT}"
  : >"${SYSTEMCTL_CALLS}"

  for item in bin ui runtime zlm certs systemd; do
    mkdir -p "${INSTALL_DIR}/${item}"
    printf 'old-%s\n' "${item}" >"${INSTALL_DIR}/${item}/old"
  done
  mkdir -p "${INSTALL_DIR}/runtime/share/postgresql/18"
  printf '%s\n' old-sample \
    >"${INSTALL_DIR}/runtime/share/postgresql/postgresql.conf.sample"
  ln -s ../postgresql.conf.sample \
    "${INSTALL_DIR}/runtime/share/postgresql/18/postgresql.conf.sample"
  printf '%s\n' old-env >"${INSTALL_DIR}/.env"
  printf '%s\n' old-uninstall >"${INSTALL_DIR}/uninstall.sh"
  printf '%s\n' old-target >"${SYSTEMD_UNIT_ROOT}/${UNIT_BASENAME}.target"
  printf '%s\n' old-core \
    >"${SYSTEMD_UNIT_ROOT}/${UNIT_BASENAME}-core.service"
  chmod 0751 "${INSTALL_DIR}"
  touch -m -d '2025-02-03 04:05:06 UTC' "${INSTALL_DIR}"
  transaction_install_mode="$(stat -c '%a' "${INSTALL_DIR}")"
  transaction_install_mtime="$(stat -c '%y' "${INSTALL_DIR}")"

  assert_install_transaction_lock_held() { :; }
  ensure_admin_handoff_state_dir() {
    mkdir -p "$(admin_handoff_state_dir)"
    chmod 700 "${ADMIN_HANDOFF_STATE_ROOT}" "$(admin_handoff_state_dir)"
  }
  admin_handoff_assert_secure_file() {
    [ ! -L "$1" ] && [ -f "$1" ]
  }
  admin_handoff_assert_secure_directory() {
    [ ! -L "$1" ] && [ -d "$1" ] && [ "$(stat -c '%a' "$1")" = 700 ]
  }
  ensure_admin_handoff_state_dir
  printf '%s\n' old-pending >"$(pending_admin_handoff_path)"
  chmod 600 "$(pending_admin_handoff_path)"

  target_active=1
  core_active=1
  enable_target=enabled
  enable_core=enabled
  systemctl() {
    printf '%s\n' "$*" >>"${SYSTEMCTL_CALLS}"
    case "$1" in
      is-enabled)
        case "${2:-}" in
          "${UNIT_BASENAME}.target") printf '%s\n' "${enable_target}" ;;
          "${UNIT_BASENAME}-core.service") printf '%s\n' "${enable_core}" ;;
          *) printf '%s\n' not-found; return 1 ;;
        esac
        ;;
      enable)
        shift
        for unit in "$@"; do
          case "${unit}" in
            "${UNIT_BASENAME}.target") enable_target=enabled ;;
            "${UNIT_BASENAME}-core.service") enable_core=enabled ;;
          esac
        done
        ;;
      disable)
        shift
        for unit in "$@"; do
          case "${unit}" in
            "${UNIT_BASENAME}.target") enable_target=disabled ;;
            "${UNIT_BASENAME}-core.service") enable_core=disabled ;;
          esac
        done
        ;;
      stop)
        shift
        for unit in "$@"; do
          case "${unit}" in
            "${UNIT_BASENAME}.target") target_active=0 ;;
            "${UNIT_BASENAME}-core.service") core_active=0 ;;
            *) return 44 ;;
          esac
        done
        ;;
      start)
        shift
        for unit in "$@"; do
          case "${unit}" in
            "${UNIT_BASENAME}.target") target_active=1 ;;
            "${UNIT_BASENAME}-core.service") core_active=1 ;;
          esac
        done
        ;;
      is-active)
        case "${!#}" in
          "${UNIT_BASENAME}.target") [ "${target_active}" -eq 1 ] ;;
          "${UNIT_BASENAME}-core.service") [ "${core_active}" -eq 1 ] ;;
          *) return 3 ;;
        esac
        ;;
      show)
        case "$*" in
          *'ActiveState'*"${UNIT_BASENAME}.target")
            [ "${target_active}" -eq 1 ] && printf '%s\n' active || printf '%s\n' inactive
            ;;
          *'ActiveState'*"${UNIT_BASENAME}-core.service")
            [ "${core_active}" -eq 1 ] && printf '%s\n' active || printf '%s\n' inactive
            ;;
          *) return 1 ;;
        esac
        ;;
      daemon-reload) ;;
      *) return 0 ;;
    esac
  }
  probe_upgrade_active_components_readiness() {
    : >"${ROLLBACK_READINESS_MARKER}"
    return 0
  }

  set +e
  (
    begin_upgrade_transaction
    UPGRADE_RESTORE_ON_FAILURE=1
    UPGRADE_TARGET_WAS_ACTIVE=1
    UPGRADE_SERVICES_QUIESCED=1
    UPGRADE_ACTIVE_UNITS=("${UNIT_BASENAME}-core.service")
    UPGRADE_ACTIVE_MAIN_PIDS=(111)

    printf '%s\n' new-env >"${INSTALL_DIR}/.env"
    rm -rf -- "${INSTALL_DIR:?}/bin" "${INSTALL_DIR}/ui" \
      "${INSTALL_DIR}/runtime" "${INSTALL_DIR}/zlm" \
      "${INSTALL_DIR}/certs" "${INSTALL_DIR}/systemd"
    for item in bin ui runtime zlm certs systemd docs; do
      mkdir -p "${INSTALL_DIR}/${item}"
      printf 'new-%s\n' "${item}" >"${INSTALL_DIR}/${item}/new"
    done
    printf '%s\n' new-uninstall >"${INSTALL_DIR}/uninstall.sh"
    printf '%s\n' new-target >"${SYSTEMD_UNIT_ROOT}/${UNIT_BASENAME}.target"
    printf '%s\n' new-core \
      >"${SYSTEMD_UNIT_ROOT}/${UNIT_BASENAME}-core.service"
    printf '%s\n' new-agent \
      >"${SYSTEMD_UNIT_ROOT}/${UNIT_BASENAME}-agent.service"
    printf '%s\n' new-pending >"$(pending_admin_handoff_path)"
    printf '%s\n' new-delivered >"$(delivered_admin_handoff_path)"
    enable_target=disabled
    enable_core=disabled
    target_active=0
    core_active=0
    chmod 0755 "${INSTALL_DIR}"
    exit 73
  )
  transaction_failure_status=$?
  set -e

  [ "${transaction_failure_status}" -eq 73 ]
  [ "$(cat "${INSTALL_DIR}/.env")" = old-env ]
  [ "$(cat "${INSTALL_DIR}/uninstall.sh")" = old-uninstall ]
  for item in bin ui runtime zlm certs systemd; do
    [ "$(cat "${INSTALL_DIR}/${item}/old")" = "old-${item}" ]
    [ ! -e "${INSTALL_DIR}/${item}/new" ]
  done
  [ -L "${INSTALL_DIR}/runtime/share/postgresql/18/postgresql.conf.sample" ]
  [ "$(readlink \
    "${INSTALL_DIR}/runtime/share/postgresql/18/postgresql.conf.sample")" = \
    ../postgresql.conf.sample ]
  [ "$(cat "${INSTALL_DIR}/runtime/share/postgresql/postgresql.conf.sample")" = \
    old-sample ]
  [ "$(stat -c '%a' "${INSTALL_DIR}")" = "${transaction_install_mode}" ]
  [ "$(stat -c '%y' "${INSTALL_DIR}")" = "${transaction_install_mtime}" ]
  [ ! -e "${INSTALL_DIR}/docs" ]
  [ "$(cat "${SYSTEMD_UNIT_ROOT}/${UNIT_BASENAME}.target")" = old-target ]
  [ "$(cat "${SYSTEMD_UNIT_ROOT}/${UNIT_BASENAME}-core.service")" = old-core ]
  [ ! -e "${SYSTEMD_UNIT_ROOT}/${UNIT_BASENAME}-agent.service" ]
  [ "$(cat "$(pending_admin_handoff_path)")" = old-pending ]
  [ ! -e "$(delivered_admin_handoff_path)" ]
  [ ! -e "$(admin_handoff_state_dir)/upgrade-transaction" ]
  [ -e "${ROLLBACK_READINESS_MARKER}" ]
  grep -Fq 'daemon-reload' "${SYSTEMCTL_CALLS}"
  if grep -Eq 'stop .*(postgres|zlm|agent)\.service' "${SYSTEMCTL_CALLS}"; then
    printf 'rollback tried to stop absent control-plane units:\n%s\n' \
      "$(cat "${SYSTEMCTL_CALLS}")" >&2
    exit 1
  fi
  grep -Fq "enable ${UNIT_BASENAME}.target" "${SYSTEMCTL_CALLS}"
  grep -Fq "enable ${UNIT_BASENAME}-core.service" "${SYSTEMCTL_CALLS}"
  grep -Fq \
    "start --job-mode=ignore-dependencies ${UNIT_BASENAME}.target" \
    "${SYSTEMCTL_CALLS}"
  grep -Fq "start ${UNIT_BASENAME}-core.service" "${SYSTEMCTL_CALLS}"
)

# Rollback stop scope is role-specific: absent components must not turn an
# otherwise recoverable control-plane or worker rollback into a false failure.
(
  UNIT_BASENAME=ss-rollback-scope
  TRUSTED_POSTGRES_UNIT_COUNT=0
  INSTALL_ROLE=control-plane
  [ "$(upgrade_rollback_units)" = \
    $'ss-rollback-scope-core.service\nss-rollback-scope.target' ]
  INSTALL_ROLE=worker-host-cpu
  [ "$(upgrade_rollback_units)" = \
    $'ss-rollback-scope-zlm.service\nss-rollback-scope-agent.service\nss-rollback-scope.target' ]
)

# A systemd/DBus failure is not evidence that a unit was absent. Only an
# explicit not-found state may be snapshotted as absence.
(
  systemctl() { return 1; }
  set +e
  capture_upgrade_unit_enablement ss-unreachable.service >/dev/null 2>&1
  empty_enablement_status=$?
  set -e
  [ "${empty_enablement_status}" -ne 0 ] || {
    echo 'empty systemctl failure was misclassified as a missing unit' >&2
    exit 1
  }
  systemctl() { printf '%s\n' not-found; return 1; }
  [ "$(capture_upgrade_unit_enablement ss-missing.service)" = not-found ]
  systemctl() {
    case "$*" in
      *'is-enabled ss-old-systemd-missing.service')
        printf '%s\n' \
          'Failed to get unit file state: No such file or directory' >&2
        return 1
        ;;
      *'show --property LoadState --value ss-old-systemd-missing.service')
        printf '%s\n' not-found
        return 0
        ;;
      *) return 1 ;;
    esac
  }
  [ "$(capture_upgrade_unit_enablement \
    ss-old-systemd-missing.service)" = not-found ]
)

# Even if an armed transaction reaches EXIT with status zero, a failed
# rollback must force a non-zero result and retain the snapshot for diagnosis.
(
  set +x
  retained_snapshot="${TMP_DIR}/rollback-failure-retained"
  mkdir -p "${retained_snapshot}"
  set +e
  (
    UPGRADE_TRANSACTION_STATE=armed
    UPGRADE_TRANSACTION_DIR="${retained_snapshot}"
    restore_upgrade_transaction() { return 1; }
    cleanup_admin_password() { :; }
    trap cleanup_installer_state EXIT
    true
  ) >/dev/null 2>&1
  zero_exit_rollback_failure_status=$?
  set -e
  [ "${zero_exit_rollback_failure_status}" -ne 0 ]
  [ -d "${retained_snapshot}" ]
)

# Exact-active readiness has one global deadline, not sixty unbounded probes.
# Simulated time advances prove a never-ready partial Core exits boundedly.
(
  INSTALL_DIR="${TMP_DIR}/bounded-upgrade-readiness"
  INSTALL_ROLE=control-plane
  UNIT_BASENAME=ss-bounded-readiness
  TRUSTED_POSTGRES_UNIT_COUNT=0
  UPGRADE_ACTIVE_UNITS=(ss-bounded-readiness-core.service)
  mkdir -p "${INSTALL_DIR}"
  : >"${INSTALL_DIR}/.env"
  write_env_entry "${INSTALL_DIR}/.env" CORE_HTTP_PORT 18080
  write_env_entry "${INSTALL_DIR}/.env" CORE_HTTP_TLS_CERT_PATH ''
  readiness_attempts=0
  probe_upgrade_component_readiness_once() {
    readiness_attempts=$((readiness_attempts + 1))
    return 1
  }
  SECONDS=0
  sleep() { SECONDS=$((SECONDS + 10)); }
  set +e
  verify_restored_upgrade_readiness >/dev/null 2>&1
  bounded_readiness_status=$?
  set -e
  [ "${bounded_readiness_status}" -ne 0 ]
  [ "${readiness_attempts}" -ge 1 ]
  [ "${readiness_attempts}" -le 7 ]
  UPGRADE_ACTIVE_UNITS=()
  readiness_attempts=0
  verify_restored_upgrade_readiness >/dev/null
  [ "${readiness_attempts}" -eq 0 ]
)

# Readiness is a coherent observation, not a union of successes from different
# rounds. A Core that was ready only before the Agent became ready must not be
# forgotten and allow the upgrade to commit.
(
  set +x
  INSTALL_ROLE=all-in-one-host-cpu
  UNIT_BASENAME=ss-coherent-readiness
  UPGRADE_ACTIVE_UNITS=(
    ss-coherent-readiness-core.service
    ss-coherent-readiness-agent.service
  )
  core_probe_count=0
  agent_probe_count=0
  prepare_upgrade_readiness_configuration() { :; }
  bounded_upgrade_systemctl() { printf '%s\n' active; }
  probe_upgrade_component_readiness_once() {
    case "$1" in
      *-core.service)
        core_probe_count=$((core_probe_count + 1))
        [ "${core_probe_count}" -eq 1 ] && return 0
        SECONDS=60
        return 1
        ;;
      *-agent.service)
        agent_probe_count=$((agent_probe_count + 1))
        [ "${agent_probe_count}" -gt 1 ]
        ;;
    esac
  }
  sleep() { :; }
  SECONDS=0
  set +e
  probe_upgrade_active_components_readiness 0 60 >/dev/null 2>&1
  coherent_readiness_status=$?
  set -e
  [ "${coherent_readiness_status}" -ne 0 ] || {
    echo 'readiness combined component success from different rounds' >&2
    exit 1
  }
  [ "${core_probe_count}" -ge 2 ]
  [ "${agent_probe_count}" -ge 1 ]
)
(
  set +x
  INSTALL_ROLE=all-in-one-host-cpu
  UNIT_BASENAME=ss-final-sweep-readiness
  UPGRADE_ACTIVE_UNITS=(
    ss-final-sweep-readiness-core.service
    ss-final-sweep-readiness-agent.service
  )
  core_went_down=0
  prepare_upgrade_readiness_configuration() { :; }
  bounded_upgrade_systemctl() {
    case "${!#}" in
      *-core.service)
        if [ "${core_went_down}" -eq 1 ]; then
          SECONDS=60
          printf '%s\n' inactive
        else
          printf '%s\n' active
        fi
        ;;
      *) printf '%s\n' active ;;
    esac
  }
  probe_upgrade_component_readiness_once() {
    case "$1" in *-agent.service) core_went_down=1 ;; esac
    return 0
  }
  sleep() { :; }
  SECONDS=0
  set +e
  probe_upgrade_active_components_readiness 0 60 >/dev/null 2>&1
  final_sweep_status=$?
  set -e
  [ "${final_sweep_status}" -ne 0 ] || {
    echo 'readiness omitted the final coherent ActiveState sweep' >&2
    exit 1
  }
)

# All transaction-owned systemctl calls are bounded by the shared absolute
# phase deadline; naked DBus calls can otherwise hold both flock parents
# forever, especially after cleanup has masked TERM.
declare -F bounded_upgrade_systemctl >/dev/null || {
  echo 'installer is missing the bounded systemctl transaction wrapper' >&2
  exit 1
}
declare -F bounded_upgrade_command >/dev/null || {
  echo 'installer is missing the bounded generic transaction command wrapper' >&2
  exit 1
}
for bounded_function in \
  assert_fresh_instance_namespace_available \
  bootstrap_all_in_one_agent_identity_if_needed \
  capture_upgrade_unit_enablement \
  apply_upgrade_unit_enablement \
  install_systemd_units \
  prepare_production_security_state \
  restore_upgrade_unit_enablement \
  restore_captured_upgrade_service_state \
  start_services_if_requested \
  wait_for_upgrade_units_steady \
  capture_upgrade_service_state \
  ensure_upgrade_preflight_database_available \
  quiesce_captured_upgrade_services \
  capture_and_quiesce_upgrade_services \
  verify_upgrade_services_ready; do
  bounded_body="$(declare -f "${bounded_function}")"
  if printf '%s\n' "${bounded_body}" | grep -Eq '(^|[^_])systemctl[[:space:]]'; then
    printf 'transaction function contains an unbounded systemctl call: %s\n' \
      "${bounded_function}" >&2
    exit 1
  fi
done
bootstrap_body="$(declare -f bootstrap_all_in_one_agent_identity_if_needed)"
if printf '%s\n' "${bootstrap_body}" \
  | grep -Eq '^[[:space:]]+systemd-run[[:space:]]'; then
  echo 'all-in-one bootstrap contains an unbounded systemd-run call' >&2
  exit 1
fi
(
  set +x
  UPGRADE_RESTORE_ON_FAILURE=1
  UPGRADE_TARGET_WAS_ACTIVE=0
  INSTALL_ROLE=control-plane
  UNIT_BASENAME=ss-strict-inactive-state
  TRUSTED_POSTGRES_UNIT_COUNT=0
  UPGRADE_ACTIVE_UNITS=()
  bounded_upgrade_systemctl() {
    case "$*" in
      *'show --property ActiveState --value'*) printf '%s\n' failed ;;
      *) return 0 ;;
    esac
  }
  set +e
  restore_captured_upgrade_service_state 60 >/dev/null 2>&1
  strict_inactive_status=$?
  set -e
  [ "${strict_inactive_status}" -ne 0 ] || {
    echo 'failed/unknown systemd state was accepted as captured inactive' >&2
    exit 1
  }
)

(
  set +x
  retained_snapshot="${TMP_DIR}/rollback-readiness-retained"
  mkdir -p "${retained_snapshot}"
  UPGRADE_TRANSACTION_STATE=armed
  UPGRADE_TRANSACTION_DIR="${retained_snapshot}"
  UPGRADE_RESTORE_ON_FAILURE=1
  assert_install_transaction_lock_held() { :; }
  validate_upgrade_transaction_snapshot_for_restore() { :; }
  upgrade_rollback_units() { printf '%s\n' ss-health-core.service ss-health.target; }
  restore_upgrade_install_tree() { return 0; }
  restore_upgrade_install_root_metadata() { return 0; }
  restore_upgrade_handoff_markers() { return 0; }
  restore_upgrade_external_units() { return 0; }
  restore_upgrade_unit_enablement() { return 0; }
  restore_captured_upgrade_service_state() { return 0; }
  verify_restored_upgrade_readiness() { return 1; }
  remove_upgrade_transaction_snapshot() {
    rm -rf -- "${retained_snapshot:?}"
  }
  systemctl() { return 0; }
  set +e
  restore_upgrade_transaction >/dev/null 2>&1
  readiness_rollback_status=$?
  set -e
  [ "${readiness_rollback_status}" -ne 0 ]
  [ -d "${retained_snapshot}" ]
)

# A path replacement between flock acquisition and the post-lock identity
# check must fail closed even if the attacker leaves another secure-looking
# regular file at the original pathname.
(
  set +x
  INSTALL_DIR="${TMP_DIR}/writable-install-parent/install"
  EMULATED_SECURITY_METADATA=1
  mkdir -p "${INSTALL_DIR}"
  secure_parent_checked=0
  admin_handoff_secure_root_ancestors_status() {
    secure_parent_checked=1
    return 1
  }
  set +e
  (prepare_install_root_for_transaction false) >/dev/null 2>&1
  writable_parent_status=$?
  set -e
  [ "${writable_parent_status}" -ne 0 ] || {
    echo 'fresh install accepted an insecure writable parent ancestry' >&2
    exit 1
  }
)

# Keeping the same installation-root inode is insufficient: replacing its
# parent after lock acquisition changes who can rename the locked root.
(
  set +x
  install_parent="${TMP_DIR}/install-parent-swap"
  displaced_parent="${TMP_DIR}/install-parent-swap.displaced"
  INSTALL_DIR="${install_parent}/install"
  ADMIN_HANDOFF_STATE_ROOT="${TMP_DIR}/install-parent-swap-state"
  mkdir -p "${INSTALL_DIR}" "${ADMIN_HANDOFF_STATE_ROOT}"
  chmod 700 "${ADMIN_HANDOFF_STATE_ROOT}"
  ensure_admin_handoff_state_dir() {
    mkdir -p "$(admin_handoff_state_dir)"
    chmod 700 "${ADMIN_HANDOFF_STATE_ROOT}" "$(admin_handoff_state_dir)"
  }
  admin_handoff_secure_directory_status() {
    [ ! -L "$1" ] && [ -d "$1" ] && [ -x "$1" ]
  }
  admin_handoff_secure_file_status() {
    [ ! -L "$1" ] && [ -f "$1" ]
  }
  flock() { return 0; }
  acquire_install_transaction_lock
  mv -- "${install_parent}" "${displaced_parent}"
  mkdir -- "${install_parent}"
  mv -- "${displaced_parent}/install" "${INSTALL_DIR}"
  set +e
  (assert_install_transaction_lock_held) >/dev/null 2>&1
  swapped_parent_status=$?
  set -e
  [ "${swapped_parent_status}" -ne 0 ] || {
    echo 'installer lock accepted a replaced installation parent inode' >&2
    exit 1
  }
)

(
  set +x
  INSTALL_DIR="${TMP_DIR}/lock-replacement-install"
  ADMIN_HANDOFF_STATE_ROOT="${TMP_DIR}/lock-replacement-state"
  mkdir -p "${INSTALL_DIR}" "${ADMIN_HANDOFF_STATE_ROOT}"
  chmod 700 "${ADMIN_HANDOFF_STATE_ROOT}"
  ensure_admin_handoff_state_dir() {
    mkdir -p "$(admin_handoff_state_dir)"
    chmod 700 "${ADMIN_HANDOFF_STATE_ROOT}" "$(admin_handoff_state_dir)"
  }
  admin_handoff_secure_directory_status() {
    [ ! -L "$1" ] && [ -d "$1" ] && [ -x "$1" ]
  }
  ensure_admin_handoff_state_dir
  admin_handoff_secure_file_status() {
    [ ! -L "$1" ] && [ -f "$1" ]
  }
  flock() {
    lock_path="$(admin_handoff_state_dir)/installer.lock"
    mv -- "${lock_path}" "${lock_path}.held"
    (umask 077; : >"${lock_path}")
    return 0
  }
  set +e
  (acquire_install_transaction_lock) >/dev/null 2>&1
  replaced_lock_status=$?
  set -e
  [ "${replaced_lock_status}" -ne 0 ] || {
    echo 'installer accepted a replaced lock inode after flock acquisition' >&2
    exit 1
  }
)

# Two real child processes targeting the same fresh worker root must contend on
# the production non-blocking flock. The second process may never enter its
# critical section while the first holder is alive.
(
  set +x
  lock_install_dir="${TMP_DIR}/fresh-worker-lock-install"
  lock_state_dir="${TMP_DIR}/fresh-worker-lock-state"
  lock_ready="${TMP_DIR}/fresh-worker-lock.ready"
  lock_release="${TMP_DIR}/fresh-worker-lock.release"
  lock_helper="${TMP_DIR}/fresh-worker-lock-helper.sh"
  mkdir -p "${lock_install_dir}" "${lock_state_dir}"
  chmod 700 "${lock_state_dir}"
  cat >"${lock_helper}" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
source "$1"
INSTALL_ROLE=worker-host-cpu
INSTALL_DIR="$2"
ADMIN_HANDOFF_STATE_ROOT="$3"
mode="$4"
ready="$5"
release="$6"
admin_handoff_secure_root_ancestors_status() { return 0; }
admin_handoff_secure_directory_status() {
  [ ! -L "$1" ] && [ -d "$1" ] && [ -x "$1" ]
}
admin_handoff_secure_file_status() {
  [ ! -L "$1" ] && [ -f "$1" ]
}
id() {
  [ "${1:-}" = -u ] && { printf '%s\n' 0; return 0; }
  command id "$@"
}
install() {
  local target="${!#}"
  mkdir -- "${target}"
}
acquire_install_transaction_lock
if [ "${mode}" = hold ]; then
  : >"${ready}"
  for _ in $(seq 1 500); do
    [ ! -e "${release}" ] || exit 0
    sleep 0.01
  done
  exit 98
fi
EOF
  chmod 755 "${lock_helper}"
  bash "${lock_helper}" "${FUNCTIONS_FILE}" "${lock_install_dir}" \
    "${lock_state_dir}" hold "${lock_ready}" "${lock_release}" &
  lock_holder_pid=$!
  for _ in $(seq 1 500); do
    [ ! -e "${lock_ready}" ] || break
    sleep 0.01
  done
  [ -e "${lock_ready}" ] || {
    kill "${lock_holder_pid}" 2>/dev/null || true
    wait "${lock_holder_pid}" 2>/dev/null || true
    echo 'fresh worker lock holder did not enter its critical section' >&2
    exit 1
  }
  set +e
  bash "${lock_helper}" "${FUNCTIONS_FILE}" "${lock_install_dir}/./" \
    "${lock_state_dir}" probe "${lock_ready}" "${lock_release}" \
    >/dev/null 2>&1
  lock_contender_status=$?
  set -e
  : >"${lock_release}"
  wait "${lock_holder_pid}"
  [ "${lock_contender_status}" -ne 0 ] || {
    echo 'concurrent fresh worker installers entered the same critical section' >&2
    exit 1
  }
)

# Production mutation is wrapped by two util-linux flock parents. The
# instance lock fences systemd namespace collisions across different install
# roots, while -o ensures neither the installer nor an orphaned grandchild can
# inherit either lock descriptor.
declare -F run_command_with_installer_flocks >/dev/null || {
  echo 'installer is missing the external double-flock execution wrapper' >&2
  exit 1
}
declare -F run_readonly_check_with_external_flocks >/dev/null || {
  echo 'standalone installer diagnostics do not share the mutation locks' >&2
  exit 1
}
(
  set +x
  readonly_wrapper_args="${TMP_DIR}/readonly-wrapper.args"
  INSTALL_DIR="${TMP_DIR}/readonly-wrapper-install"
  INSTALL_TRANSACTION_GLOBAL_LOCK_PATH="${TMP_DIR}/readonly-instance.lock"
  INSTALL_TRANSACTION_PATH_LOCK_PATH="${TMP_DIR}/readonly-path.lock"
  VERIFIED_PACKAGE_CHECKSUM_FILE_SHA256="$(printf 'a%.0s' {1..64})"
  VERIFIED_PACKAGE_TREE_FINGERPRINT="$(printf 'b%.0s' {1..64})"
  prepare_external_installer_lock_files() { :; }
  stage_verified_package_root() { PACKAGE_ROOT="${TMP_DIR}/readonly-sealed-package"; }
  run_command_with_installer_flocks() {
    printf '%s\0' "$@" >"${readonly_wrapper_args}"
  }
  run_readonly_check_with_external_flocks security-preflight
  mapfile -d '' -t readonly_args <"${readonly_wrapper_args}"
  [ "${#readonly_args[@]}" -eq 9 ]
  [ "${readonly_args[0]}" = "${INSTALL_TRANSACTION_GLOBAL_LOCK_PATH}" ]
  [ "${readonly_args[1]}" = "${INSTALL_TRANSACTION_PATH_LOCK_PATH}" ]
  [ "${readonly_args[2]}" = bash ]
  [ "${readonly_args[4]}" = --_locked-readonly-check-stage ]
  [ "${readonly_args[5]}" = security-preflight ]
  [ "${readonly_args[6]}" = "${INSTALL_DIR}" ]
  [ "${readonly_args[7]}" = "${VERIFIED_PACKAGE_CHECKSUM_FILE_SHA256}" ]
  [ "${readonly_args[8]}" = "${VERIFIED_PACKAGE_TREE_FINGERPRINT}" ]
)
(
  set +x
  external_lock_root="${TMP_DIR}/external-double-flock"
  global_lock="${external_lock_root}/instance-contract.lock"
  path_lock_one="${external_lock_root}/path-one.lock"
  path_lock_two="${external_lock_root}/path-two.lock"
  holder_ready="${external_lock_root}/holder.ready"
  holder_release="${external_lock_root}/holder.release"
  orphan_pid_file="${external_lock_root}/orphan.pid"
  mkdir -p "${external_lock_root}"
  chmod 700 "${external_lock_root}"
  : >"${global_lock}"
  : >"${path_lock_one}"
  : >"${path_lock_two}"
  chmod 600 "${global_lock}" "${path_lock_one}" "${path_lock_two}"

  run_command_with_installer_flocks "${global_lock}" "${path_lock_one}" \
    bash -c ': >"$1"; while [ ! -e "$2" ]; do sleep 0.01; done' \
    double-flock-holder "${holder_ready}" "${holder_release}" &
  double_flock_holder_pid=$!
  for _ in $(seq 1 500); do
    [ ! -e "${holder_ready}" ] || break
    sleep 0.01
  done
  [ -e "${holder_ready}" ] || {
    kill "${double_flock_holder_pid}" 2>/dev/null || true
    wait "${double_flock_holder_pid}" 2>/dev/null || true
    echo 'external double-flock holder did not enter the critical section' >&2
    exit 1
  }
  set +e
  run_command_with_installer_flocks "${global_lock}" "${path_lock_two}" true
  cross_path_contender_status=$?
  set -e
  [ "${cross_path_contender_status}" -eq 75 ] || {
    echo 'same instance entered two different installation roots concurrently' >&2
    exit 1
  }
  : >"${holder_release}"
  wait "${double_flock_holder_pid}"

  run_command_with_installer_flocks "${global_lock}" "${path_lock_one}" \
    bash -c 'bash -c '\''exec sleep 30'\'' & printf "%s\n" "$!" >"$1"' \
    orphan-lock-probe "${orphan_pid_file}"
  orphan_pid="$(<"${orphan_pid_file}")"
  kill -0 "${orphan_pid}"
  run_command_with_installer_flocks "${global_lock}" "${path_lock_two}" true
  for fd_path in "/proc/${orphan_pid}/fd/"*; do
    fd_target="$(readlink "${fd_path}" 2>/dev/null || true)"
    [ "${fd_target}" != "${global_lock}" ]
    [ "${fd_target}" != "${path_lock_one}" ]
  done
  kill "${orphan_pid}" 2>/dev/null || true
  wait "${orphan_pid}" 2>/dev/null || true
)

# A process-group TERM reaches the original installer, both flock parents and
# the mutating child. The original bash must remain alive and both locks must
# remain unavailable until the child records its durable rollback terminal.
(
  set +x
  signal_root="${TMP_DIR}/double-flock-signal"
  signal_global_lock="${signal_root}/instance.lock"
  signal_path_lock="${signal_root}/path.lock"
  signal_ready="${signal_root}/stage.ready"
  signal_rollback_started="${signal_root}/rollback.started"
  signal_rollback_release="${signal_root}/rollback.release"
  signal_rollback_terminal="${signal_root}/rollback.terminal"
  signal_helper="${signal_root}/wrapper.sh"
  signal_stage="${signal_root}/stage.sh"
  mkdir -p "${signal_root}"
  chmod 700 "${signal_root}"
  : >"${signal_global_lock}"
  : >"${signal_path_lock}"
  chmod 600 "${signal_global_lock}" "${signal_path_lock}"
  cat >"${signal_stage}" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
ready="$1"
rollback_started="$2"
rollback_release="$3"
rollback_terminal="$4"
rollback() {
  trap '' HUP INT TERM
  : >"${rollback_started}"
  while [ ! -e "${rollback_release}" ]; do sleep 0.01; done
  : >"${rollback_terminal}"
  exit 143
}
trap rollback HUP INT TERM
: >"${ready}"
while true; do sleep 0.1; done
EOF
  cat >"${signal_helper}" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
source "$1"
shift
run_command_with_installer_flocks "$@"
EOF
  chmod 755 "${signal_stage}" "${signal_helper}"
  setsid bash "${signal_helper}" "${FUNCTIONS_FILE}" \
    "${signal_global_lock}" "${signal_path_lock}" \
    bash "${signal_stage}" \
    "${signal_ready}" "${signal_rollback_started}" \
    "${signal_rollback_release}" "${signal_rollback_terminal}" &
  signal_wrapper_pid=$!
  for _ in $(seq 1 500); do
    [ ! -e "${signal_ready}" ] || break
    sleep 0.01
  done
  [ -e "${signal_ready}" ] || {
    kill -KILL -- "-${signal_wrapper_pid}" 2>/dev/null || true
    wait "${signal_wrapper_pid}" 2>/dev/null || true
    echo 'signal wrapper child did not become ready' >&2
    exit 1
  }
  kill -TERM -- "-${signal_wrapper_pid}"
  for _ in $(seq 1 500); do
    [ ! -e "${signal_rollback_started}" ] || break
    sleep 0.01
  done
  [ -e "${signal_rollback_started}" ]
  kill -0 "${signal_wrapper_pid}" || {
    echo 'outer installer returned before rollback reached a durable terminal' >&2
    exit 1
  }
  set +e
  flock -n -E 75 "${signal_global_lock}" true
  signal_global_contender=$?
  flock -n -E 75 "${signal_path_lock}" true
  signal_path_contender=$?
  set -e
  [ "${signal_global_contender}" -eq 75 ]
  [ "${signal_path_contender}" -eq 75 ]
  : >"${signal_rollback_release}"
  set +e
  wait "${signal_wrapper_pid}"
  signal_wrapper_status=$?
  set -e
  [ "${signal_wrapper_status}" -eq 143 ]
  [ -e "${signal_rollback_terminal}" ]
  flock -n -E 75 "${signal_global_lock}" true
  flock -n -E 75 "${signal_path_lock}" true
)

# A fresh install may not reuse an already-published systemd namespace from a
# different root, even after the first installer has released its lock.
declare -F assert_fresh_instance_namespace_available >/dev/null || {
  echo 'installer is missing the sequential instance collision guard' >&2
  exit 1
}
(
  set +x
  UPGRADE=0
  UNIT_BASENAME=ss-contract-sequential-collision
  SYSTEMD_UNIT_ROOT="${TMP_DIR}/sequential-collision-units"
  mkdir -p "${SYSTEMD_UNIT_ROOT}"
  : >"${SYSTEMD_UNIT_ROOT}/${UNIT_BASENAME}.target"
  set +e
  (assert_fresh_instance_namespace_available) >/dev/null 2>&1
  sequential_collision_status=$?
  set -e
  [ "${sequential_collision_status}" -ne 0 ] || {
    echo 'fresh install reused an existing systemd instance namespace' >&2
    exit 1
  }
)
(
  set +x
  UPGRADE=0
  UNIT_BASENAME=ss-contract-vendor-collision
  SYSTEMD_UNIT_ROOT="${TMP_DIR}/vendor-collision-empty-etc"
  mkdir -p "${SYSTEMD_UNIT_ROOT}"
  systemctl() {
    case "$*" in
      *'LoadState'*"${UNIT_BASENAME}-core.service")
        printf '%s\n' loaded
        ;;
      *'FragmentPath'*"${UNIT_BASENAME}-core.service")
        printf '%s\n' "/usr/lib/systemd/system/${UNIT_BASENAME}-core.service"
        ;;
      *'LoadState'*) printf '%s\n' not-found ;;
      *'FragmentPath'*) printf '\n' ;;
      *) return 1 ;;
    esac
  }
  set +e
  (assert_fresh_instance_namespace_available) >/dev/null 2>&1
  vendor_collision_status=$?
  set -e
  [ "${vendor_collision_status}" -ne 0 ] || {
    echo 'fresh install reused a vendor/runtime systemd unit namespace' >&2
    exit 1
  }
)

# The real installer re-exec carries its selected state over an anonymous
# NUL-delimited descriptor. It must neither consume stdin (the TUI still owns
# the terminal) nor accept an internal stage without both external locks.
for required_lock_function in \
  emit_locked_install_plan \
  load_locked_install_plan_from_fd \
  prepare_external_installer_lock_files \
  assert_external_installer_flocks_held \
  run_locked_readonly_check_stage \
  run_readonly_check_with_external_flocks \
  run_install_with_external_flocks; do
  declare -F "${required_lock_function}" >/dev/null || {
    printf 'installer is missing locked re-exec function: %s\n' \
      "${required_lock_function}" >&2
    exit 1
  }
done
(
  set +x
  INSTALL_DIR="${TMP_DIR}/anonymous-plan-install"
  INSTANCE_NAME=contract-anonymous-plan
  INSTALL_ROLE=worker-host-cpu
  UNIT_BASENAME=ss-contract-anonymous-plan
  UPGRADE=0
  START_AFTER_INSTALL=0
  DATABASE_MODE=''
  DATABASE_URL_INPUT='postgresql://plan-secret@db.invalid/streamserver'
  INSTALL_ROLE_WAS_EXPLICIT=1
  INSTANCE_NAME_WAS_EXPLICIT=1
  INTERACTIVE_INSTALL=1
  VERIFIED_PACKAGE_CHECKSUM_FILE_SHA256="$(printf 'c%.0s' {1..64})"
  VERIFIED_PACKAGE_TREE_FINGERPRINT="$(printf 'd%.0s' {1..64})"
  exec {anonymous_plan_fd}< <(emit_locked_install_plan)
  exec 0<<<'terminal-input-sentinel'
  load_locked_install_plan_from_fd "${anonymous_plan_fd}"
  read -r terminal_sentinel
  [ "${terminal_sentinel}" = terminal-input-sentinel ]
  [ "${DATABASE_URL_INPUT}" = \
    'postgresql://plan-secret@db.invalid/streamserver' ]
  if [ -e "/proc/self/fd/${anonymous_plan_fd}" ]; then
    echo 'anonymous install plan descriptor remained open after parsing' >&2
    exit 1
  fi
)
(
  set +x
  INSTALL_DIR="${TMP_DIR}/unlocked-hidden-stage"
  INSTANCE_NAME=contract-unlocked-hidden-stage
  INSTALL_ROLE=worker-host-cpu
  UNIT_BASENAME=ss-contract-unlocked-hidden-stage
  ADMIN_HANDOFF_STATE_ROOT="${TMP_DIR}/unlocked-hidden-stage-state"
  mkdir -p "${INSTALL_DIR}" "${ADMIN_HANDOFF_STATE_ROOT}"
  chmod 700 "${ADMIN_HANDOFF_STATE_ROOT}"
  id() {
    [ "${1:-}" = -u ] && { printf '%s\n' 0; return 0; }
    command id "$@"
  }
  install() {
    local target="${!#}"
    mkdir -p -- "${target}"
    chmod 700 "${target}"
  }
  admin_handoff_assert_secure_root_ancestors() { :; }
  admin_handoff_assert_no_symlink_boundary() { :; }
  admin_handoff_assert_secure_directory() {
    [ ! -L "$1" ] && [ -d "$1" ]
  }
  admin_handoff_assert_secure_file() {
    [ ! -L "$1" ] && [ -f "$1" ]
  }
  prepare_external_installer_lock_files
  set +e
  (assert_external_installer_flocks_held) >/dev/null 2>&1
  unlocked_stage_status=$?
  set -e
  [ "${unlocked_stage_status}" -ne 0 ] || {
    echo 'hidden installer stage accepted lock files that were not held' >&2
    exit 1
  }
)
grep -Fq 'run_install_with_external_flocks' "${INSTALLER}"
grep -Fq -- '--_locked-install-stage' "${INSTALLER}"
grep -Fq -- '--_locked-readonly-check-stage' "${INSTALLER}"
if grep -Eq 'upgrade-plan|install-plan.*(mktemp|/tmp)' "${INSTALLER}"; then
  echo 'installer persists its internal re-exec plan to a named temporary file' >&2
  exit 1
fi

run_preflight() {
  local env_file="$1"
  local core_bin="$2"
  local agent_bin="${3:-}"
  local output
  local status
  set +e
  output="$(security_preflight_env "${env_file}" "${core_bin}" "${agent_bin}" 2>&1)"
  status=$?
  set -e
  PREFLIGHT_OUTPUT="${output}"
  PREFLIGHT_STATUS="${status}"
}

INSECURE_ENV="${TMP_DIR}/insecure.env"
printf '%s\n' \
  'INSTALL_ROLE=control-plane' \
  'AUTH_MODE=disabled' \
  'DATABASE_URL=postgresql://diagnostic-user:must-not-leak@127.0.0.1/streamserver' \
  'CORE_HTTP_ADDR=0.0.0.0:8080' \
  'CORE_HTTP_TLS_CERT_PATH=' \
  'CORE_HTTP_TLS_KEY_PATH=' \
  'CORE_GRPC_ADDR=0.0.0.0:50051' \
  'CORE_GRPC_TLS_CERT_PATH=' \
  'CORE_GRPC_TLS_KEY_PATH=' \
  'CORE_GRPC_TLS_CLIENT_CA_PATH=' >"${INSECURE_ENV}"

FAKE_CORE="${TMP_DIR}/media-core"
FAKE_ADMIN_MARKER="${TMP_DIR}/fake-admin-present"
printf '%s\n' \
  '#!/usr/bin/env bash' \
  'set -euo pipefail' \
  'case "$*" in' \
  "  \"auth check-admin\") [ -f '${FAKE_ADMIN_MARKER}' ] ;;" \
  '  "auth check-config")' \
  '    case "${AUTH_MODE:-}" in' \
  '      external_jwt) key="${JWT_PUBLIC_KEY:-}" ;;' \
  '      local_password) key="$(cat "${AUTH_JWT_PUBLIC_KEY_PATH:-/nonexistent}" 2>/dev/null || true)" ;;' \
  '      *) exit 1 ;;' \
  '    esac' \
  '    if printf "%s" "${key}" | openssl rsa -pubin -noout >/dev/null 2>&1; then exit 0; fi' \
  '    printf "%s" "${key}" | openssl pkey -pubin -text_pub -noout 2>/dev/null | grep -q ED25519 ;;' \
  '  *) exit 64 ;;' \
  'esac' >"${FAKE_CORE}"
chmod 755 "${FAKE_CORE}"

run_preflight "${INSECURE_ENV}" "${FAKE_CORE}"
[ "${PREFLIGHT_STATUS}" -ne 0 ] || {
  echo 'insecure production env unexpectedly passed security preflight' >&2
  exit 1
}
assert_contains "${PREFLIGHT_OUTPUT}" '[MISSING] auth/admin'
assert_contains "${PREFLIGHT_OUTPUT}" '[MISSING] HTTP TLS'
assert_contains "${PREFLIGHT_OUTPUT}" '[MISSING] gRPC mTLS'

LEGACY_INSECURE_DEV_ENV="${TMP_DIR}/legacy-insecure-dev.env"
printf '%s\n' \
  'INSTALL_ROLE=control-plane' \
  'CORE_INSECURE_DEV=true' >"${LEGACY_INSECURE_DEV_ENV}"
run_preflight "${LEGACY_INSECURE_DEV_ENV}" "${FAKE_CORE}"
[ "${PREFLIGHT_STATUS}" -ne 0 ] || {
  echo 'legacy CORE_INSECURE_DEV unexpectedly passed security preflight' >&2
  exit 1
}
assert_contains "${PREFLIGHT_OUTPUT}" \
  '[INVALID] configuration: CORE_INSECURE_DEV is unsupported'

LEGACY_STREAMSERVER_ENV="${TMP_DIR}/legacy-streamserver-env.env"
printf '%s\n' \
  'INSTALL_ROLE=control-plane' \
  'STREAMSERVER_ENV=development' >"${LEGACY_STREAMSERVER_ENV}"
run_preflight "${LEGACY_STREAMSERVER_ENV}" "${FAKE_CORE}"
[ "${PREFLIGHT_STATUS}" -ne 0 ] || {
  echo 'legacy STREAMSERVER_ENV override unexpectedly passed security preflight' >&2
  exit 1
}
assert_contains "${PREFLIGHT_OUTPUT}" \
  '[INVALID] configuration: STREAMSERVER_ENV is reserved by the native service launcher'

UNKNOWN_ROLE_ENV="${TMP_DIR}/unknown-role.env"
printf '%s\n' 'INSTALL_ROLE=unknown' >"${UNKNOWN_ROLE_ENV}"
run_preflight "${UNKNOWN_ROLE_ENV}" "${FAKE_CORE}"
[ "${PREFLIGHT_STATUS}" -ne 0 ]
assert_contains "${PREFLIGHT_OUTPUT}" '[MISSING] configuration'

CA_KEY_FILE="${TMP_DIR}/ca.key"
CA_FILE="${TMP_DIR}/client-ca.pem"
CERT_FILE="${TMP_DIR}/server.pem"
KEY_FILE="${TMP_DIR}/server.key"
BAD_SERVER_CERT_FILE="${TMP_DIR}/bad-server-profile.pem"
CLIENT_CERT_FILE="${TMP_DIR}/client.pem"
CLIENT_KEY_FILE="${TMP_DIR}/client.key"
JWT_PRIVATE_KEY_FILE="${TMP_DIR}/jwt-ed25519-private.pem"
JWT_PUBLIC_KEY_FILE="${TMP_DIR}/jwt-ed25519-public.pem"
EC_PRIVATE_KEY_FILE="${TMP_DIR}/jwt-ec-private.pem"
EC_PUBLIC_KEY_FILE="${TMP_DIR}/jwt-ec-public.pem"
AGENT_CA_KEY_FILE="${TMP_DIR}/agent-issuer-ca.key"
AGENT_CA_FILE="${TMP_DIR}/agent-issuer-ca.pem"
MANAGEMENT_CA_KEY_FILE="${TMP_DIR}/management-client-ca.key"
MANAGEMENT_CA_FILE="${TMP_DIR}/management-client-ca.pem"
MANAGEMENT_CLIENT_KEY_FILE="${TMP_DIR}/core-management-client.key"
MANAGEMENT_CLIENT_CERT_FILE="${TMP_DIR}/core-management-client.pem"
CAPABILITY_PRIVATE_KEY_FILE="${TMP_DIR}/agent-capability-private.pem"
CAPABILITY_PUBLIC_KEY_FILE="${TMP_DIR}/agent-capability-public.pem"
CORE_INSTANCE_ID_VALUE="0190d8d4-31d2-7b23-b27e-8b9b28a2ed11"

export MSYS2_ARG_CONV_EXCL='/CN='
openssl req -x509 -newkey rsa:2048 -nodes -days 2 -subj '/CN=StreamServer Test CA' \
  -addext 'basicConstraints=critical,CA:TRUE' \
  -keyout "${CA_KEY_FILE}" -out "${CA_FILE}" >/dev/null 2>&1
openssl req -newkey rsa:2048 -nodes -subj '/CN=localhost' \
  -keyout "${KEY_FILE}" -out "${TMP_DIR}/server.csr" >/dev/null 2>&1
cat >"${TMP_DIR}/server.ext" <<'EOF'
basicConstraints=critical,CA:FALSE
keyUsage=critical,digitalSignature
extendedKeyUsage=critical,serverAuth
subjectAltName=DNS:localhost
EOF
openssl x509 -req -days 2 -in "${TMP_DIR}/server.csr" \
  -CA "${CA_FILE}" -CAkey "${CA_KEY_FILE}" -CAcreateserial \
  -extfile "${TMP_DIR}/server.ext" -out "${CERT_FILE}" >/dev/null 2>&1
cat >"${TMP_DIR}/bad-server-profile.ext" <<'EOF'
basicConstraints=critical,CA:FALSE
keyUsage=critical,digitalSignature
extendedKeyUsage=critical,clientAuth
subjectAltName=DNS:localhost
EOF
openssl x509 -req -days 2 -in "${TMP_DIR}/server.csr" \
  -CA "${CA_FILE}" -CAkey "${CA_KEY_FILE}" \
  -CAserial "${TMP_DIR}/bad-server-profile.srl" -CAcreateserial \
  -extfile "${TMP_DIR}/bad-server-profile.ext" \
  -out "${BAD_SERVER_CERT_FILE}" >/dev/null 2>&1
openssl req -newkey rsa:2048 -nodes -subj '/CN=streamserver-test-agent' \
  -keyout "${CLIENT_KEY_FILE}" -out "${TMP_DIR}/client.csr" >/dev/null 2>&1
openssl x509 -req -days 2 -in "${TMP_DIR}/client.csr" \
  -CA "${CA_FILE}" -CAkey "${CA_KEY_FILE}" -CAcreateserial \
  -out "${CLIENT_CERT_FILE}" >/dev/null 2>&1
openssl genpkey -algorithm Ed25519 -out "${JWT_PRIVATE_KEY_FILE}" >/dev/null 2>&1
openssl pkey -in "${JWT_PRIVATE_KEY_FILE}" -pubout \
  -out "${JWT_PUBLIC_KEY_FILE}" >/dev/null 2>&1
openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 \
  -out "${EC_PRIVATE_KEY_FILE}" >/dev/null 2>&1
openssl pkey -in "${EC_PRIVATE_KEY_FILE}" -pubout \
  -out "${EC_PUBLIC_KEY_FILE}" >/dev/null 2>&1
openssl genpkey -algorithm Ed25519 -out "${AGENT_CA_KEY_FILE}" >/dev/null 2>&1
openssl req -x509 -new -key "${AGENT_CA_KEY_FILE}" -days 2 \
  -subj '/CN=StreamServer Test Agent Issuer' \
  -addext 'basicConstraints=critical,CA:TRUE,pathlen:0' \
  -addext 'keyUsage=critical,keyCertSign' \
  -out "${AGENT_CA_FILE}" >/dev/null 2>&1
openssl genpkey -algorithm Ed25519 -out "${MANAGEMENT_CA_KEY_FILE}" >/dev/null 2>&1
openssl req -x509 -new -key "${MANAGEMENT_CA_KEY_FILE}" -days 2 \
  -subj '/CN=StreamServer Test Management CA' \
  -addext 'basicConstraints=critical,CA:TRUE,pathlen:0' \
  -addext 'keyUsage=critical,keyCertSign' \
  -out "${MANAGEMENT_CA_FILE}" >/dev/null 2>&1
openssl genpkey -algorithm Ed25519 -out "${MANAGEMENT_CLIENT_KEY_FILE}" >/dev/null 2>&1
openssl req -new -key "${MANAGEMENT_CLIENT_KEY_FILE}" \
  -subj '/CN=StreamServer Test Core Management' \
  -out "${TMP_DIR}/core-management-client.csr" >/dev/null 2>&1
cat >"${TMP_DIR}/core-management-client.ext" <<EOF
basicConstraints=critical,CA:FALSE
keyUsage=critical,digitalSignature
extendedKeyUsage=critical,clientAuth
subjectAltName=URI:spiffe://streamserver/core/${CORE_INSTANCE_ID_VALUE}
EOF
openssl x509 -req -days 2 -in "${TMP_DIR}/core-management-client.csr" \
  -CA "${MANAGEMENT_CA_FILE}" -CAkey "${MANAGEMENT_CA_KEY_FILE}" -CAcreateserial \
  -extfile "${TMP_DIR}/core-management-client.ext" \
  -out "${MANAGEMENT_CLIENT_CERT_FILE}" >/dev/null 2>&1
openssl genpkey -algorithm Ed25519 -out "${CAPABILITY_PRIVATE_KEY_FILE}" >/dev/null 2>&1
openssl pkey -in "${CAPABILITY_PRIVATE_KEY_FILE}" -pubout \
  -out "${CAPABILITY_PUBLIC_KEY_FILE}" >/dev/null 2>&1

append_core_internal_pki_env() {
  local env_file="$1"
  cat >>"${env_file}" <<EOF
CORE_GRPC_TLS_DOMAIN_NAME='localhost'
CORE_GRPC_TLS_SERVER_CA_PATH='${CA_FILE}'
CORE_AGENT_CA_CERT_PATH='${AGENT_CA_FILE}'
CORE_AGENT_CA_KEY_PATH='${AGENT_CA_KEY_FILE}'
CORE_AGENT_CAPABILITY_JWT_PRIVATE_KEY_PATH='${CAPABILITY_PRIVATE_KEY_FILE}'
CORE_AGENT_CAPABILITY_JWT_PUBLIC_KEY_PATH='${CAPABILITY_PUBLIC_KEY_FILE}'
CORE_AGENT_CAPABILITY_TTL_SEC='60'
CORE_INSTANCE_ID='${CORE_INSTANCE_ID_VALUE}'
CORE_AGENT_MANAGEMENT_CLIENT_CERT_PATH='${MANAGEMENT_CLIENT_CERT_FILE}'
CORE_AGENT_MANAGEMENT_CLIENT_KEY_PATH='${MANAGEMENT_CLIENT_KEY_FILE}'
CORE_AGENT_MANAGEMENT_CA_PATH='${MANAGEMENT_CA_FILE}'
EOF
}

SECURE_ENV="${TMP_DIR}/secure.env"
printf '%s\n' \
  'INSTALL_ROLE=control-plane' \
  'AUTH_MODE=local_password' \
  'DATABASE_URL=postgresql://diagnostic-user:super-secret@127.0.0.1/streamserver' \
  "AUTH_JWT_PRIVATE_KEY_PATH=${JWT_PRIVATE_KEY_FILE}" \
  "AUTH_JWT_PUBLIC_KEY_PATH=${JWT_PUBLIC_KEY_FILE}" \
  'CORE_HTTP_ADDR=127.0.0.1:8080' \
  'CORE_HTTP_PUBLIC_HOST=localhost' \
  'CORE_HTTP_TLS_CERT_PATH=' \
  'CORE_HTTP_TLS_KEY_PATH=' \
  'CORE_GRPC_ADDR=127.0.0.1:50051' \
  "CORE_GRPC_TLS_CERT_PATH=${CERT_FILE}" \
  "CORE_GRPC_TLS_KEY_PATH=${KEY_FILE}" \
  "CORE_GRPC_TLS_CLIENT_CA_PATH=${AGENT_CA_FILE}" >"${SECURE_ENV}"
append_core_internal_pki_env "${SECURE_ENV}"

touch "${FAKE_ADMIN_MARKER}"
run_preflight "${SECURE_ENV}" "${FAKE_CORE}"
[ "${PREFLIGHT_STATUS}" -eq 0 ] || {
  printf 'secure production env failed preflight:\n%s\n' "${PREFLIGHT_OUTPUT}" >&2
  exit 1
}
assert_contains "${PREFLIGHT_OUTPUT}" '[OK] auth/admin'
assert_contains "${PREFLIGHT_OUTPUT}" '[OK] internal PKI'
SECURE_PREFLIGHT_OUTPUT="${PREFLIGHT_OUTPUT}"

# A fresh all-in-one host cannot have an Agent identity before its local Core
# has issued and served the one-time enrollment. The bootstrap gate therefore
# needs an explicit Core-only mode; the default gate must remain strict.
AIO_CORE_ONLY_ENV="${TMP_DIR}/all-in-one-core-only.env"
AIO_CORE_ONLY_AGENT="${TMP_DIR}/missing-all-in-one-agent"
sed 's/^INSTALL_ROLE=.*/INSTALL_ROLE=all-in-one-host-cpu/' \
  "${SECURE_ENV}" >"${AIO_CORE_ONLY_ENV}"
printf '%s\n' \
  'ZLM_HTTP_PORT=18080' \
  'ZLM_API_BASE=http://127.0.0.1:18080' >>"${AIO_CORE_ONLY_ENV}"
run_preflight "${AIO_CORE_ONLY_ENV}" "${FAKE_CORE}" "${AIO_CORE_ONLY_AGENT}"
[ "${PREFLIGHT_STATUS}" -ne 0 ]
assert_contains "${PREFLIGHT_OUTPUT}" '[MISSING] worker mTLS'
set +e
AIO_CORE_ONLY_OUTPUT="$(security_preflight_env \
  "${AIO_CORE_ONLY_ENV}" "${FAKE_CORE}" "${AIO_CORE_ONLY_AGENT}" core-only 2>&1)"
AIO_CORE_ONLY_STATUS=$?
set -e
[ "${AIO_CORE_ONLY_STATUS}" -eq 0 ] || {
  printf 'all-in-one Core-only preflight failed:\n%s\n' \
    "${AIO_CORE_ONLY_OUTPUT}" >&2
  exit 1
}
assert_contains "${AIO_CORE_ONLY_OUTPUT}" 'security preflight passed'
set +e
security_preflight_env \
  "${AIO_CORE_ONLY_ENV}" "${FAKE_CORE}" "${AIO_CORE_ONLY_AGENT}" unsafe-scope \
  >/dev/null 2>&1
INVALID_PREFLIGHT_SCOPE_STATUS=$?
set -e
[ "${INVALID_PREFLIGHT_SCOPE_STATUS}" -ne 0 ]

HTTP_TLS_ENV="${TMP_DIR}/http-tls.env"
cp "${SECURE_ENV}" "${HTTP_TLS_ENV}"
sed -i \
  -e 's|CORE_HTTP_ADDR=.*|CORE_HTTP_ADDR=0.0.0.0:8080|' \
  -e "s|CORE_HTTP_TLS_CERT_PATH=.*|CORE_HTTP_TLS_CERT_PATH=${CERT_FILE}|" \
  -e "s|CORE_HTTP_TLS_KEY_PATH=.*|CORE_HTTP_TLS_KEY_PATH=${KEY_FILE}|" \
  "${HTTP_TLS_ENV}"
run_preflight "${HTTP_TLS_ENV}" "${FAKE_CORE}"
[ "${PREFLIGHT_STATUS}" -eq 0 ] || {
  printf 'valid HTTP server identity failed preflight:\n%s\n' "${PREFLIGHT_OUTPUT}" >&2
  exit 1
}

WRONG_HTTP_SAN_ENV="${TMP_DIR}/wrong-http-san.env"
sed 's|CORE_HTTP_PUBLIC_HOST=.*|CORE_HTTP_PUBLIC_HOST=other.example.test|' \
  "${HTTP_TLS_ENV}" >"${WRONG_HTTP_SAN_ENV}"
run_preflight "${WRONG_HTTP_SAN_ENV}" "${FAKE_CORE}"
[ "${PREFLIGHT_STATUS}" -ne 0 ] || {
  echo 'HTTP certificate without the configured public-host SAN unexpectedly passed' >&2
  exit 1
}
assert_contains "${PREFLIGHT_OUTPUT}" '[INVALID] HTTP TLS'

BAD_HTTP_PROFILE_ENV="${TMP_DIR}/bad-http-profile.env"
sed "s|CORE_HTTP_TLS_CERT_PATH=.*|CORE_HTTP_TLS_CERT_PATH=${BAD_SERVER_CERT_FILE}|" \
  "${HTTP_TLS_ENV}" >"${BAD_HTTP_PROFILE_ENV}"
run_preflight "${BAD_HTTP_PROFILE_ENV}" "${FAKE_CORE}"
[ "${PREFLIGHT_STATUS}" -ne 0 ] || {
  echo 'HTTP clientAuth-only certificate unexpectedly passed as a server leaf' >&2
  exit 1
}
assert_contains "${PREFLIGHT_OUTPUT}" '[INVALID] HTTP TLS'

BAD_GRPC_PROFILE_ENV="${TMP_DIR}/bad-grpc-profile.env"
sed "s|CORE_GRPC_TLS_CERT_PATH=.*|CORE_GRPC_TLS_CERT_PATH=${BAD_SERVER_CERT_FILE}|" \
  "${SECURE_ENV}" >"${BAD_GRPC_PROFILE_ENV}"
run_preflight "${BAD_GRPC_PROFILE_ENV}" "${FAKE_CORE}"
[ "${PREFLIGHT_STATUS}" -ne 0 ] || {
  echo 'gRPC clientAuth-only certificate unexpectedly passed as a server leaf' >&2
  exit 1
}
assert_contains "${PREFLIGHT_OUTPUT}" '[INVALID] gRPC mTLS'

for internal_pki_case in capability-ttl trust-root-reuse capability-key agent-trust-bundle; do
  INVALID_INTERNAL_PKI_ENV="${TMP_DIR}/invalid-internal-pki-${internal_pki_case}.env"
  case "${internal_pki_case}" in
    capability-ttl)
      sed 's|CORE_AGENT_CAPABILITY_TTL_SEC=.*|CORE_AGENT_CAPABILITY_TTL_SEC=121|' \
        "${SECURE_ENV}" >"${INVALID_INTERNAL_PKI_ENV}"
      ;;
    trust-root-reuse)
      sed "s|CORE_AGENT_MANAGEMENT_CA_PATH=.*|CORE_AGENT_MANAGEMENT_CA_PATH=${AGENT_CA_FILE}|" \
        "${SECURE_ENV}" >"${INVALID_INTERNAL_PKI_ENV}"
      ;;
    capability-key)
      sed "s|CORE_AGENT_CAPABILITY_JWT_PUBLIC_KEY_PATH=.*|CORE_AGENT_CAPABILITY_JWT_PUBLIC_KEY_PATH=${JWT_PUBLIC_KEY_FILE}|" \
        "${SECURE_ENV}" >"${INVALID_INTERNAL_PKI_ENV}"
      ;;
    agent-trust-bundle)
      sed "s|CORE_GRPC_TLS_CLIENT_CA_PATH=.*|CORE_GRPC_TLS_CLIENT_CA_PATH=${CA_FILE}|" \
        "${SECURE_ENV}" >"${INVALID_INTERNAL_PKI_ENV}"
      ;;
  esac
  run_preflight "${INVALID_INTERNAL_PKI_ENV}" "${FAKE_CORE}"
  [ "${PREFLIGHT_STATUS}" -ne 0 ] || {
    echo "invalid internal PKI case unexpectedly passed: ${internal_pki_case}" >&2
    exit 1
  }
  assert_contains "${PREFLIGHT_OUTPUT}" '[INVALID] internal PKI'
done

for duplicate_security_key in AUTH_MODE DATABASE_URL; do
  DUPLICATE_SECURITY_ENV="${TMP_DIR}/duplicate-${duplicate_security_key}.env"
  cp "${SECURE_ENV}" "${DUPLICATE_SECURITY_ENV}"
  case "${duplicate_security_key}" in
    AUTH_MODE) printf '%s\n' '  AUTH_MODE=external_jwt' >>"${DUPLICATE_SECURITY_ENV}" ;;
    DATABASE_URL) printf '%s\n' 'DATABASE_URL=postgresql://forged/other' >>"${DUPLICATE_SECURITY_ENV}" ;;
  esac
  run_preflight "${DUPLICATE_SECURITY_ENV}" "${FAKE_CORE}"
  [ "${PREFLIGHT_STATUS}" -ne 0 ] || {
    echo "duplicate ${duplicate_security_key} unexpectedly passed preflight" >&2
    exit 1
  }
  assert_contains "${PREFLIGHT_OUTPUT}" \
    "[INVALID] configuration: ${duplicate_security_key} must appear at most once"
done
PREFLIGHT_OUTPUT="${SECURE_PREFLIGHT_OUTPUT}"

(
  ADMIN_HANDOFF_STATE_ROOT="${TMP_DIR}/unreadable-installer-state"
  mkdir -p "${ADMIN_HANDOFF_STATE_ROOT}"
  # Fault-inject the result produced when a non-root preflight cannot traverse
  # the production 0700 state root. It must be UNKNOWN/nonzero, never absent.
  admin_handoff_secure_directory_status() { return 1; }
  run_preflight "${SECURE_ENV}" "${FAKE_CORE}"
  [ "${PREFLIGHT_STATUS}" -ne 0 ] || {
    echo 'unreadable administrator handoff state false-greened preflight' >&2
    exit 1
  }
  assert_contains "${PREFLIGHT_OUTPUT}" \
    '[UNKNOWN] auth/admin: administrator handoff state is inaccessible or insecure'
)
assert_contains "${PREFLIGHT_OUTPUT}" '[OK] HTTP TLS'
assert_contains "${PREFLIGHT_OUTPUT}" '[OK] gRPC mTLS'

UNSUPPORTED_LOCAL_KEY_ENV="${TMP_DIR}/unsupported-local-key.env"
printf '%s\n' \
  'INSTALL_ROLE=control-plane' \
  'AUTH_MODE=local_password' \
  'DATABASE_URL=postgresql://127.0.0.1/unused' \
  "AUTH_JWT_PRIVATE_KEY_PATH=${EC_PRIVATE_KEY_FILE}" \
  "AUTH_JWT_PUBLIC_KEY_PATH=${EC_PUBLIC_KEY_FILE}" \
  'CORE_HTTP_ADDR=127.0.0.1:8080' \
  'CORE_HTTP_TLS_CERT_PATH=' \
  'CORE_HTTP_TLS_KEY_PATH=' \
  'CORE_GRPC_ADDR=127.0.0.1:50051' \
  "CORE_GRPC_TLS_CERT_PATH=${CERT_FILE}" \
  "CORE_GRPC_TLS_KEY_PATH=${KEY_FILE}" \
  "CORE_GRPC_TLS_CLIENT_CA_PATH=${AGENT_CA_FILE}" >"${UNSUPPORTED_LOCAL_KEY_ENV}"
append_core_internal_pki_env "${UNSUPPORTED_LOCAL_KEY_ENV}"
run_preflight "${UNSUPPORTED_LOCAL_KEY_ENV}" "${FAKE_CORE}"
[ "${PREFLIGHT_STATUS}" -ne 0 ] || {
  echo 'unsupported EC local_password JWT key pair unexpectedly passed preflight' >&2
  exit 1
}
assert_contains "${PREFLIGHT_OUTPUT}" '[INVALID] auth/admin: local_password JWT configuration'

RELATIVE_ENV="${TMP_DIR}/relative.env"
sed "s|${TMP_DIR}/||g" "${SECURE_ENV}" >"${RELATIVE_ENV}"
run_preflight "${RELATIVE_ENV}" "${FAKE_CORE}"
[ "${PREFLIGHT_STATUS}" -eq 0 ] || {
  printf 'relative security paths failed preflight:\n%s\n' "${PREFLIGHT_OUTPUT}" >&2
  exit 1
}
assert_contains "${PREFLIGHT_OUTPUT}" '[OK] auth/admin'
assert_contains "${PREFLIGHT_OUTPUT}" '[OK] gRPC mTLS'

rm -f "${FAKE_ADMIN_MARKER}"
run_preflight "${SECURE_ENV}" "${FAKE_CORE}"
[ "${PREFLIGHT_STATUS}" -ne 0 ]
assert_contains "${PREFLIGHT_OUTPUT}" '[MISSING] auth/admin'
if printf '%s' "${PREFLIGHT_OUTPUT}" | grep -Fq 'super-secret'; then
  echo 'security preflight leaked DATABASE_URL credentials' >&2
  exit 1
fi
touch "${FAKE_ADMIN_MARKER}"

INVALID_CA_FILE="${TMP_DIR}/invalid-ca.pem"
: >"${INVALID_CA_FILE}"
INVALID_TLS_ENV="${TMP_DIR}/invalid-tls.env"
sed "s|CORE_GRPC_TLS_CLIENT_CA_PATH=.*|CORE_GRPC_TLS_CLIENT_CA_PATH=${INVALID_CA_FILE}|" \
  "${SECURE_ENV}" >"${INVALID_TLS_ENV}"
run_preflight "${INVALID_TLS_ENV}" "${FAKE_CORE}"
[ "${PREFLIGHT_STATUS}" -ne 0 ]
assert_contains "${PREFLIGHT_OUTPUT}" '[INVALID] gRPC mTLS'

PARTIAL_HTTP_ENV="${TMP_DIR}/partial-http.env"
sed "s|CORE_HTTP_TLS_CERT_PATH=.*|CORE_HTTP_TLS_CERT_PATH=${CERT_FILE}|" \
  "${SECURE_ENV}" >"${PARTIAL_HTTP_ENV}"
run_preflight "${PARTIAL_HTTP_ENV}" "${FAKE_CORE}"
[ "${PREFLIGHT_STATUS}" -ne 0 ]
assert_contains "${PREFLIGHT_OUTPUT}" '[MISSING] HTTP TLS'

WORKER_ENV="${TMP_DIR}/worker.env"
WORKER_NODE_ID="0190d8d4-31d2-7b23-b27e-8b9b28a2ed22"
WORKER_GENERATION_ID="0190d8d4-31d2-7b23-b27e-8b9b28a2ed23"
printf -v ENROLLMENT_WIRE_TOKEN 'ssae1.%096d.%043d' 0 0
[ "${#ENROLLMENT_WIRE_TOKEN}" -eq 146 ]
WORKER_IDENTITY_DIR="${TMP_DIR}/worker-identity"
FAKE_AGENT="${TMP_DIR}/media-agent"
FAKE_AGENT_CALLS="${TMP_DIR}/media-agent.calls"
mkdir -p "${WORKER_IDENTITY_DIR}"
printf '%s\n' "${WORKER_GENERATION_ID}" >"${WORKER_IDENTITY_DIR}/current"
: >"${FAKE_AGENT_CALLS}"
printf '%s\n' \
  '#!/usr/bin/env bash' \
  'set -euo pipefail' \
  "WORKER_GENERATION_ID='${WORKER_GENERATION_ID}'" \
  "FAKE_AGENT_CALLS='${FAKE_AGENT_CALLS}'" \
  'if env | grep -Eq "^(ADMIN_PASSWORD|AGENT_ENROLLMENT_TOKEN|UNPERSISTED_AGENT_SETTING)="; then exit 95; fi' \
  '[ "$1" = identity ] && [ "$2" = check ] && [ "$3" = --node-id ]' \
  'node_id="$4"' \
  '[ "$5" = --identity-dir ]' \
  'identity_dir="$6"' \
  '[ "$#" -eq 6 ]' \
  '[[ "${node_id}" =~ ^[0-9a-f-]{36}$ ]]' \
  '[ "$(tr -d "\\r\\n" <"${identity_dir}/current")" = "${WORKER_GENERATION_ID}" ]' \
  'printf "%s|%s\n" "${node_id}" "${identity_dir}" >>"${FAKE_AGENT_CALLS}"' \
  >"${FAKE_AGENT}"
chmod 755 "${FAKE_AGENT}"
export UNPERSISTED_AGENT_SETTING=parent-only-agent-value
printf '%s\n' \
  'INSTALL_ROLE=worker-host-cpu' \
  'AGENT_CORE_ENDPOINT=http://core.example.test:50051' \
  "AGENT_NODE_ID=${WORKER_NODE_ID}" \
  "AGENT_IDENTITY_DIR=${WORKER_IDENTITY_DIR}" \
  'AGENT_TLS_DOMAIN_NAME=core.example.test' \
  'AGENT_ZLM_HOOK_ADDR=127.0.0.1:18082' \
  'AGENT_ZLM_HOOK_PORT=18082' \
  'AGENT_ZLM_HOOK_QUEUE_CAPACITY=64' \
  'AGENT_ZLM_HOOK_TIMEOUT_SEC=4' \
  'ZLM_HTTP_PORT=18080' \
  'ZLM_API_BASE=http://127.0.0.1:18080' \
  'ZLM_API_ALLOW_IP_RANGE=::1,127.0.0.1,10.0.0.0-10.255.255.255,172.16.0.0-172.31.255.255,192.168.0.0-192.168.255.255' \
  'ZLM_API_SECRET=abcdef0123456789abcdef0123456789' \
  'ZLM_HOOK_SHARED_SECRET=0123456789abcdef0123456789abcdef' \
  'ZLM_HOOK_BASE=http://127.0.0.1:18082/internal/zlm-hooks' >"${WORKER_ENV}"
run_preflight "${WORKER_ENV}" "${FAKE_CORE}" "${FAKE_AGENT}"
[ "${PREFLIGHT_STATUS}" -ne 0 ]
assert_contains "${PREFLIGHT_OUTPUT}" '[MISSING] worker mTLS'
[ ! -s "${FAKE_AGENT_CALLS}" ]

sed 's|AGENT_CORE_ENDPOINT=http://|AGENT_CORE_ENDPOINT=https://|' \
  "${WORKER_ENV}" >"${TMP_DIR}/secure-worker.env"
run_preflight "${TMP_DIR}/secure-worker.env" "${FAKE_CORE}" "${FAKE_AGENT}"
[ "${PREFLIGHT_STATUS}" -eq 0 ] || {
  printf 'secure worker env failed preflight:\n%s\n' "${PREFLIGHT_OUTPUT}" >&2
  exit 1
}
assert_contains "${PREFLIGHT_OUTPUT}" '[OK] worker mTLS'
[ "$(wc -l <"${FAKE_AGENT_CALLS}" | tr -d '[:space:]')" -eq 1 ]

LOOPBACK_ONLY_ZLM_ALLOWLIST_ENV="${TMP_DIR}/loopback-only-zlm-allowlist.env"
sed 's|ZLM_API_ALLOW_IP_RANGE=.*|ZLM_API_ALLOW_IP_RANGE=::1,127.0.0.1|' \
  "${TMP_DIR}/secure-worker.env" >"${LOOPBACK_ONLY_ZLM_ALLOWLIST_ENV}"
run_preflight "${LOOPBACK_ONLY_ZLM_ALLOWLIST_ENV}" "${FAKE_CORE}" "${FAKE_AGENT}"
[ "${PREFLIGHT_STATUS}" -ne 0 ] || {
  echo 'loopback-only shared ZLM HTTP allowlist unexpectedly passed worker preflight' >&2
  exit 1
}
assert_contains "${PREFLIGHT_OUTPUT}" '[INVALID] Agent/ZLM control'

REUSED_ZLM_API_SECRET_ENV="${TMP_DIR}/reused-zlm-api-secret.env"
sed 's|ZLM_API_SECRET=.*|ZLM_API_SECRET=0123456789abcdef0123456789abcdef|' \
  "${TMP_DIR}/secure-worker.env" >"${REUSED_ZLM_API_SECRET_ENV}"
run_preflight "${REUSED_ZLM_API_SECRET_ENV}" "${FAKE_CORE}" "${FAKE_AGENT}"
[ "${PREFLIGHT_STATUS}" -ne 0 ] || {
  echo 'ZLM API and Agent hook shared secret unexpectedly passed preflight' >&2
  exit 1
}
assert_contains "${PREFLIGHT_OUTPUT}" '[INVALID] Agent/ZLM control'

MISSING_ZLM_ALLOWLIST_ENV="${TMP_DIR}/missing-zlm-allowlist.env"
grep -v '^ZLM_API_ALLOW_IP_RANGE=' "${TMP_DIR}/secure-worker.env" \
  >"${MISSING_ZLM_ALLOWLIST_ENV}"
run_preflight "${MISSING_ZLM_ALLOWLIST_ENV}" "${FAKE_CORE}" "${FAKE_AGENT}"
[ "${PREFLIGHT_STATUS}" -ne 0 ] || {
  echo 'missing ZLM HTTP allowlist unexpectedly passed worker preflight' >&2
  exit 1
}
assert_contains "${PREFLIGHT_OUTPUT}" '[INVALID] Agent/ZLM control'

REMOTE_ZLM_CONTROL_ENV="${TMP_DIR}/remote-zlm-control.env"
sed 's|ZLM_API_BASE=.*|ZLM_API_BASE=http://192.0.2.10:18080|' \
  "${TMP_DIR}/secure-worker.env" >"${REMOTE_ZLM_CONTROL_ENV}"
run_preflight "${REMOTE_ZLM_CONTROL_ENV}" "${FAKE_CORE}" "${FAKE_AGENT}"
[ "${PREFLIGHT_STATUS}" -ne 0 ] || {
  echo 'remote Agent-to-ZLM control endpoint unexpectedly passed preflight' >&2
  exit 1
}
assert_contains "${PREFLIGHT_OUTPUT}" '[INVALID] Agent/ZLM control'

# The Agent-local hook credential is an independent trust boundary. Upgrade
# may preserve only an already-dedicated strong value; it must never copy or
# retain either the legacy Core hook secret or the ZLM API secret.
(
  GENERATED_HOOK_SECRET='generated-agent-hook-secret-0123456789abcdef'
  LEGACY_CORE_HOOK_SECRET='legacy-core-hook-secret-0123456789abcdef'
  LEGACY_ZLM_API_SECRET='legacy-zlm-api-secret-0123456789abcdef0'
  INDEPENDENT_HOOK_SECRET='independent-agent-hook-secret-0123456789abcdef'
  generate_secret() { printf '%s' "${GENERATED_HOOK_SECRET}"; }

  MISSING_HOOK_SECRET_ENV="${TMP_DIR}/upgrade-hook-secret-missing.env"
  printf '%s\n' \
    "HOOK_SHARED_SECRET=${LEGACY_CORE_HOOK_SECRET}" \
    "ZLM_API_SECRET=${LEGACY_ZLM_API_SECRET}" >"${MISSING_HOOK_SECRET_ENV}"
  [ "$(safe_upgrade_zlm_hook_secret "${MISSING_HOOK_SECRET_ENV}")" = \
    "${GENERATED_HOOK_SECRET}" ]

  INDEPENDENT_HOOK_SECRET_ENV="${TMP_DIR}/upgrade-hook-secret-independent.env"
  printf '%s\n' \
    "HOOK_SHARED_SECRET=${LEGACY_CORE_HOOK_SECRET}" \
    "ZLM_API_SECRET=${LEGACY_ZLM_API_SECRET}" \
    "ZLM_HOOK_SHARED_SECRET=${INDEPENDENT_HOOK_SECRET}" \
    >"${INDEPENDENT_HOOK_SECRET_ENV}"
  [ "$(safe_upgrade_zlm_hook_secret "${INDEPENDENT_HOOK_SECRET_ENV}")" = \
    "${INDEPENDENT_HOOK_SECRET}" ]

  for reused_secret_key in HOOK_SHARED_SECRET ZLM_API_SECRET; do
    REUSED_HOOK_SECRET_ENV="${TMP_DIR}/upgrade-hook-secret-reused-${reused_secret_key}.env"
    reused_value="${LEGACY_CORE_HOOK_SECRET}"
    [ "${reused_secret_key}" != ZLM_API_SECRET ] \
      || reused_value="${LEGACY_ZLM_API_SECRET}"
    printf '%s\n' \
      "HOOK_SHARED_SECRET=${LEGACY_CORE_HOOK_SECRET}" \
      "ZLM_API_SECRET=${LEGACY_ZLM_API_SECRET}" \
      "ZLM_HOOK_SHARED_SECRET=${reused_value}" \
      >"${REUSED_HOOK_SECRET_ENV}"
    [ "$(safe_upgrade_zlm_hook_secret "${REUSED_HOOK_SECRET_ENV}")" = \
      "${GENERATED_HOOK_SECRET}" ]
  done

  WEAK_HOOK_SECRET_ENV="${TMP_DIR}/upgrade-hook-secret-weak.env"
  printf '%s\n' \
    "HOOK_SHARED_SECRET=${LEGACY_CORE_HOOK_SECRET}" \
    "ZLM_API_SECRET=${LEGACY_ZLM_API_SECRET}" \
    'ZLM_HOOK_SHARED_SECRET=too-short' >"${WEAK_HOOK_SECRET_ENV}"
  [ "$(safe_upgrade_zlm_hook_secret "${WEAK_HOOK_SECRET_ENV}")" = \
    "${GENERATED_HOOK_SECRET}" ]
)

# ZLM API credentials are also Agent-local. Missing, weak, or legacy Core-hook
# values rotate; a strong existing value is preserved only when independent.
(
  GENERATED_API_SECRET='generated-zlm-api-secret-0123456789abcdef0'
  LEGACY_CORE_HOOK_SECRET='legacy-core-hook-secret-0123456789abcdef'
  INDEPENDENT_API_SECRET='independent-zlm-api-secret-0123456789abcdef'
  generate_secret() { printf '%s' "${GENERATED_API_SECRET}"; }

  MISSING_API_SECRET_ENV="${TMP_DIR}/upgrade-api-secret-missing.env"
  printf '%s\n' "HOOK_SHARED_SECRET=${LEGACY_CORE_HOOK_SECRET}" \
    >"${MISSING_API_SECRET_ENV}"
  [ "$(safe_upgrade_zlm_api_secret "${MISSING_API_SECRET_ENV}")" = \
    "${GENERATED_API_SECRET}" ]

  INDEPENDENT_API_SECRET_ENV="${TMP_DIR}/upgrade-api-secret-independent.env"
  printf '%s\n' \
    "HOOK_SHARED_SECRET=${LEGACY_CORE_HOOK_SECRET}" \
    "ZLM_API_SECRET=${INDEPENDENT_API_SECRET}" \
    >"${INDEPENDENT_API_SECRET_ENV}"
  [ "$(safe_upgrade_zlm_api_secret "${INDEPENDENT_API_SECRET_ENV}")" = \
    "${INDEPENDENT_API_SECRET}" ]

  for unsafe_api_secret in "${LEGACY_CORE_HOOK_SECRET}" too-short; do
    UNSAFE_API_SECRET_ENV="${TMP_DIR}/upgrade-api-secret-unsafe-${unsafe_api_secret##*-}.env"
    printf '%s\n' \
      "HOOK_SHARED_SECRET=${LEGACY_CORE_HOOK_SECRET}" \
      "ZLM_API_SECRET=${unsafe_api_secret}" >"${UNSAFE_API_SECRET_ENV}"
    [ "$(safe_upgrade_zlm_api_secret "${UNSAFE_API_SECRET_ENV}")" = \
      "${GENERATED_API_SECRET}" ]
  done
)

for legacy_zlm_role in worker-host-cpu all-in-one-host-cpu control-plane; do
  (
    INSTALL_DIR="${TMP_DIR}/legacy-zlm-${legacy_zlm_role}"
    EMULATED_SECURITY_METADATA=1
    mkdir -p "${INSTALL_DIR}"
    cat >"${INSTALL_DIR}/.env" <<EOF
INSTALL_ROLE='${legacy_zlm_role}'
ZLM_HTTP_PORT='18080'
AGENT_HTTP_PORT='18082'
HOOK_SHARED_SECRET='0123456789abcdef0123456789abcdef'
ZLM_API_SECRET='abcdef0123456789abcdef0123456789'
ZLM_API_ALLOW_IP_RANGE='::1,127.0.0.1'
ZLM_API_HOST='remote.example'
ZLM_API_BASE='http://remote.example:18080'
CUSTOM_KEEP='yes'
EOF
    MIGRATION_OUTPUT="$(migrate_legacy_zlm_api_endpoint 2>&1)"
    assert_contains "${MIGRATION_OUTPUT}" 'migrated legacy ZLM control endpoint'
    [ "$(env_key_occurrence_count "${INSTALL_DIR}/.env" ZLM_API_HOST)" -eq 0 ]
    [ "$(existing_env_value "${INSTALL_DIR}/.env" CUSTOM_KEEP)" = yes ]
    if role_has_worker "${legacy_zlm_role}"; then
      [ "$(env_key_occurrence_count "${INSTALL_DIR}/.env" ZLM_API_BASE)" -eq 1 ]
      [ "$(existing_env_value "${INSTALL_DIR}/.env" ZLM_API_BASE)" = \
        'http://127.0.0.1:18080' ]
      [ "$(existing_env_value "${INSTALL_DIR}/.env" ZLM_API_ALLOW_IP_RANGE)" = \
        '::1,127.0.0.1,10.0.0.0-10.255.255.255,172.16.0.0-172.31.255.255,192.168.0.0-192.168.255.255' ]
      migrated_api_secret="$(existing_env_value "${INSTALL_DIR}/.env" ZLM_API_SECRET)"
      [ "$(existing_env_value "${INSTALL_DIR}/.env" AGENT_ZLM_HOOK_PORT)" = 18083 ]
      [ "$(existing_env_value "${INSTALL_DIR}/.env" AGENT_ZLM_HOOK_ADDR)" = \
        '127.0.0.1:18083' ]
      [ "$(existing_env_value "${INSTALL_DIR}/.env" ZLM_HOOK_BASE)" = \
        'http://127.0.0.1:18083/internal/zlm-hooks' ]
      migrated_hook_secret="$(existing_env_value "${INSTALL_DIR}/.env" ZLM_HOOK_SHARED_SECRET)"
      [[ "${migrated_hook_secret}" =~ ^[A-Za-z0-9._~-]+$ ]]
      [ "${#migrated_hook_secret}" -ge 32 ]
      [ "${#migrated_hook_secret}" -le 256 ]
      [ "${migrated_hook_secret}" != '0123456789abcdef0123456789abcdef' ]
      [ "${migrated_hook_secret}" != 'abcdef0123456789abcdef0123456789' ]
      [ "${migrated_api_secret}" != "${migrated_hook_secret}" ]
      [ "${migrated_api_secret}" != '0123456789abcdef0123456789abcdef' ]
      if [ "${legacy_zlm_role}" = worker-host-cpu ]; then
        [ "$(env_key_occurrence_count "${INSTALL_DIR}/.env" HOOK_SHARED_SECRET)" -eq 0 ]
      else
        [ "$(existing_env_value "${INSTALL_DIR}/.env" HOOK_SHARED_SECRET)" = \
          '0123456789abcdef0123456789abcdef' ]
      fi
      [ "$(existing_env_value "${INSTALL_DIR}/.env" AGENT_ZLM_HOOK_QUEUE_CAPACITY)" = 64 ]
      [ "$(existing_env_value "${INSTALL_DIR}/.env" AGENT_ZLM_HOOK_TIMEOUT_SEC)" = 4 ]
    else
      [ "$(env_key_occurrence_count "${INSTALL_DIR}/.env" ZLM_API_BASE)" -eq 0 ]
      [ "$(env_key_occurrence_count "${INSTALL_DIR}/.env" ZLM_API_ALLOW_IP_RANGE)" -eq 0 ]
      [ "$(env_key_occurrence_count "${INSTALL_DIR}/.env" ZLM_API_SECRET)" -eq 0 ]
      [ "$(existing_env_value "${INSTALL_DIR}/.env" HOOK_SHARED_SECRET)" = \
        '0123456789abcdef0123456789abcdef' ]
      [ "$(env_key_occurrence_count "${INSTALL_DIR}/.env" AGENT_ZLM_HOOK_PORT)" -eq 0 ]
      [ "$(env_key_occurrence_count "${INSTALL_DIR}/.env" ZLM_HOOK_BASE)" -eq 0 ]
    fi
  )
done

(
  INSTALL_DIR="${TMP_DIR}/legacy-worker-hook-preflight"
  EMULATED_SECURITY_METADATA=1
  mkdir -p "${INSTALL_DIR}"
  awk '
    !/^(AGENT_ZLM_HOOK_ADDR|AGENT_ZLM_HOOK_PORT|AGENT_ZLM_HOOK_QUEUE_CAPACITY|AGENT_ZLM_HOOK_TIMEOUT_SEC|ZLM_HOOK_SHARED_SECRET|ZLM_HOOK_BASE|ZLM_API_BASE)=/
  ' "${TMP_DIR}/secure-worker.env" >"${INSTALL_DIR}/.env"
  printf '%s\n' \
    'ZLM_API_HOST=legacy.example' \
    'ZLM_API_BASE=http://legacy.example:18080' >>"${INSTALL_DIR}/.env"

  migrate_legacy_zlm_api_endpoint >/dev/null
  run_preflight "${INSTALL_DIR}/.env" "${FAKE_CORE}" "${FAKE_AGENT}"
  [ "${PREFLIGHT_STATUS}" -eq 0 ] || {
    printf 'migrated baseline worker env failed preflight:\n%s\n' "${PREFLIGHT_OUTPUT}" >&2
    exit 1
  }
  assert_contains "${PREFLIGHT_OUTPUT}" '[OK] Agent/ZLM hook'
)

MISSING_IDENTITY_ENV="${TMP_DIR}/missing-worker-identity.env"
sed "s|AGENT_IDENTITY_DIR=.*|AGENT_IDENTITY_DIR=${TMP_DIR}/missing-worker-identity|" \
  "${TMP_DIR}/secure-worker.env" >"${MISSING_IDENTITY_ENV}"
run_preflight "${MISSING_IDENTITY_ENV}" "${FAKE_CORE}" "${FAKE_AGENT}"
[ "${PREFLIGHT_STATUS}" -ne 0 ]
assert_contains "${PREFLIGHT_OUTPUT}" '[INVALID] worker mTLS'

DUPLICATE_WORKER_ENV="${TMP_DIR}/duplicate-worker-endpoint.env"
cp "${TMP_DIR}/secure-worker.env" "${DUPLICATE_WORKER_ENV}"
printf '%s\n' '  AGENT_CORE_ENDPOINT=https://forged.example.test:50051' \
  >>"${DUPLICATE_WORKER_ENV}"
run_preflight "${DUPLICATE_WORKER_ENV}" "${FAKE_CORE}" "${FAKE_AGENT}"
[ "${PREFLIGHT_STATUS}" -ne 0 ] || {
  echo 'duplicate AGENT_CORE_ENDPOINT unexpectedly passed worker preflight' >&2
  exit 1
}
assert_contains "${PREFLIGHT_OUTPUT}" \
  '[INVALID] configuration: AGENT_CORE_ENDPOINT must appear at most once'
if printf '%s' "${PREFLIGHT_OUTPUT}" | grep -Fq '[MISSING] worker mTLS'; then
  echo 'duplicate AGENT_CORE_ENDPOINT fell through into worker validation' >&2
  exit 1
fi

sed "s|${TMP_DIR}/||g" "${TMP_DIR}/secure-worker.env" >"${TMP_DIR}/relative-worker.env"
run_preflight "${TMP_DIR}/relative-worker.env" "${FAKE_CORE}" "${FAKE_AGENT}"
[ "${PREFLIGHT_STATUS}" -eq 0 ] || {
  printf 'relative worker TLS paths failed preflight:\n%s\n' "${PREFLIGHT_OUTPUT}" >&2
  exit 1
}
assert_contains "${PREFLIGHT_OUTPUT}" '[OK] worker mTLS'

# Enrollment is a one-time, stdin-only secret handoff.  Existing identities
# must not consume a token, and a successful enrollment must not expose the
# token through argv, environment variables, trace output, or persistent files.
for enrollment_precheck_case in invalid-url missing-ca invalid-node missing-parent; do
  (
    set +x
    INSTALL_ROLE=worker-host-cpu
    INSTALL_DIR="${TMP_DIR}/enrollment-precheck-${enrollment_precheck_case}"
    AGENT_IDENTITY_DIR="${INSTALL_DIR}/data/agent/identity"
    AGENT_ENROLLMENT_CORE_URL='https://core.example.test:8080'
    AGENT_ENROLLMENT_SERVER_CA_PATH="${CA_FILE}"
    NODE_ID="${WORKER_NODE_ID}"
    INTERACTIVE_INSTALL=1
    PROMPT_MARKER="${INSTALL_DIR}/secret-prompted"
    unset AGENT_ENROLLMENT_TOKEN
    mkdir -p "${INSTALL_DIR}/bin" "${INSTALL_DIR}/data/agent"
    printf '%s\n' '#!/usr/bin/env bash' 'exit 97' >"${INSTALL_DIR}/bin/media-agent"
    chmod 755 "${INSTALL_DIR}/bin/media-agent"
    prompt_secret_from_tty() {
      : >"${PROMPT_MARKER}"
      printf '%s' must-not-be-read
    }
    case "${enrollment_precheck_case}" in
      invalid-url) AGENT_ENROLLMENT_CORE_URL='http://core.example.test:8080' ;;
      missing-ca) AGENT_ENROLLMENT_SERVER_CA_PATH="${INSTALL_DIR}/missing-ca.pem" ;;
      invalid-node) NODE_ID=not-a-canonical-node-id ;;
      missing-parent) rmdir "${INSTALL_DIR}/data/agent" ;;
    esac
    set +e
    (run_agent_enrollment_if_needed) >/dev/null 2>&1
    ENROLLMENT_PRECHECK_STATUS=$?
    set -e
    [ "${ENROLLMENT_PRECHECK_STATUS}" -ne 0 ]
    [ ! -e "${PROMPT_MARKER}" ] || {
      echo "Agent enrollment read its token before ${enrollment_precheck_case} validation" >&2
      exit 1
    }
  )
done

(
  set +x
  INSTALL_ROLE=worker-host-cpu
  INSTALL_DIR="${TMP_DIR}/existing-enrollment"
  AGENT_IDENTITY_DIR="${INSTALL_DIR}/data/agent/identity"
  AGENT_ENROLLMENT_TOKEN="${ENROLLMENT_WIRE_TOKEN}"
  export AGENT_ENROLLMENT_TOKEN
  mkdir -p "${INSTALL_DIR}/bin" "${AGENT_IDENTITY_DIR}"
  printf '%s\n' "${WORKER_GENERATION_ID}" >"${AGENT_IDENTITY_DIR}/current"
  printf '%s\n' '#!/usr/bin/env bash' 'exit 97' >"${INSTALL_DIR}/bin/media-agent"
  chmod 755 "${INSTALL_DIR}/bin/media-agent"
  run_agent_enrollment_if_needed
  [ -z "${AGENT_ENROLLMENT_TOKEN+x}" ]
)

MISSING_ENROLLMENT_OUTPUT="${TMP_DIR}/missing-enrollment.out"
set +e
(
  set +x
  INSTALL_ROLE=worker-host-cpu
  INSTALL_DIR="${TMP_DIR}/missing-enrollment"
  AGENT_IDENTITY_DIR="${INSTALL_DIR}/data/agent/identity"
  AGENT_ENROLLMENT_CORE_URL='https://core.example.test:8080'
  AGENT_ENROLLMENT_SERVER_CA_PATH="${CA_FILE}"
  NODE_ID="${WORKER_NODE_ID}"
  unset AGENT_ENROLLMENT_TOKEN
  mkdir -p "${INSTALL_DIR}/bin" "${INSTALL_DIR}/data/agent"
  printf '%s\n' '#!/usr/bin/env bash' 'exit 97' >"${INSTALL_DIR}/bin/media-agent"
  chmod 755 "${INSTALL_DIR}/bin/media-agent"
  run_agent_enrollment_if_needed
) >"${MISSING_ENROLLMENT_OUTPUT}" 2>&1
MISSING_ENROLLMENT_STATUS=$?
set -e
[ "${MISSING_ENROLLMENT_STATUS}" -ne 0 ]
grep -Fq 'worker has no enrolled identity' "${MISSING_ENROLLMENT_OUTPUT}"

(
  set +x
  INSTALL_ROLE=worker-host-cpu
  INSTALL_DIR="${TMP_DIR}/stdin-enrollment"
  AGENT_IDENTITY_DIR="${INSTALL_DIR}/data/agent/identity"
  AGENT_ENROLLMENT_CORE_URL='https://core.example.test:8080'
  AGENT_ENROLLMENT_SERVER_CA_PATH="${CA_FILE}"
  NODE_ID="${WORKER_NODE_ID}"
  ENROLLMENT_TOKEN="${ENROLLMENT_WIRE_TOKEN}"
  AGENT_ENROLLMENT_TOKEN="${ENROLLMENT_TOKEN}"
  export AGENT_ENROLLMENT_TOKEN
  ENROLLMENT_ARGS_FILE="${INSTALL_DIR}/enrollment.args"
  ENROLLMENT_HASH_FILE="${INSTALL_DIR}/enrollment-token.sha256"
  mkdir -p "${INSTALL_DIR}/bin" "${INSTALL_DIR}/data/agent"
  printf '%s\n' \
    '#!/usr/bin/env bash' \
    'set -euo pipefail' \
    "ENROLLMENT_ARGS_FILE='${ENROLLMENT_ARGS_FILE}'" \
    "ENROLLMENT_HASH_FILE='${ENROLLMENT_HASH_FILE}'" \
    "WORKER_GENERATION_ID='${WORKER_GENERATION_ID}'" \
    'IFS= read -r token || [ -n "${token}" ]' \
    '[ "${#token}" -eq 146 ]' \
    '[[ "${token}" =~ ^ssae1[.][A-Za-z0-9_-]{96}[.][A-Za-z0-9_-]{43}$ ]]' \
    'if env | grep -Fq -- "${token}"; then exit 95; fi' \
    'if env | grep -q "^UNPERSISTED_AGENT_SETTING="; then exit 94; fi' \
    'case " $* " in *" ${token} "*) exit 96 ;; esac' \
    'printf "%s\n" "$*" >"${ENROLLMENT_ARGS_FILE}"' \
    'printf "%s" "${token}" | sha256sum | awk "{print \$1}" >"${ENROLLMENT_HASH_FILE}"' \
    'identity_dir=""' \
    'while [ "$#" -gt 0 ]; do' \
    '  if [ "$1" = --identity-dir ]; then identity_dir="$2"; shift 2; else shift; fi' \
    'done' \
    '[ -n "${identity_dir}" ]' \
    '[ -d "$(dirname "${identity_dir}")" ]' \
    'mkdir "${identity_dir}"' \
    'printf "%s\n" "${WORKER_GENERATION_ID}" >"${identity_dir}/current"' \
    >"${INSTALL_DIR}/bin/media-agent"
  chmod 755 "${INSTALL_DIR}/bin/media-agent"
  run_agent_enrollment_if_needed
  [ -z "${AGENT_ENROLLMENT_TOKEN+x}" ]
  [ -f "${AGENT_IDENTITY_DIR}/current" ] && [ ! -L "${AGENT_IDENTITY_DIR}/current" ]
  grep -Fq -- '--token-stdin' "${ENROLLMENT_ARGS_FILE}"
  ! grep -Fq -- "${ENROLLMENT_TOKEN}" "${ENROLLMENT_ARGS_FILE}"
  [ "$(printf '%s' "${ENROLLMENT_TOKEN}" | sha256sum | awk '{print $1}')" = \
    "$(tr -d '[:space:]' <"${ENROLLMENT_HASH_FILE}")" ]
  if grep -R -Fq -- "${ENROLLMENT_TOKEN}" "${INSTALL_DIR}"; then
    echo 'enrollment token was persisted under the installation directory' >&2
    exit 1
  fi
  unset ENROLLMENT_TOKEN
)

# Fresh all-in-one enrollment is two-stage: a strict Core-only preflight and
# local token creation happen before a short-lived Core serves the real HTTPS
# enrollment. The token must never enter the transient unit argv/environment or
# an installation file, and the bootstrap Core must always be stopped.
(
  set +x
  INSTALL_ROLE=all-in-one-host-cpu
  INSTALL_DIR="${TMP_DIR}/all-in-one-enrollment"
  SERVICE_USER=streamserver
  SERVICE_GROUP=streamserver
  UNIT_BASENAME=ss-contract-aio
  EMULATED_SECURITY_METADATA=1
  INTERACTIVE_INSTALL=0
  NODE_ID="${WORKER_NODE_ID}"
  AGENT_IDENTITY_DIR="${INSTALL_DIR}/data/agent/identity"
  AIO_TOKEN="${ENROLLMENT_WIRE_TOKEN}"
  AIO_CALLS="${INSTALL_DIR}/bootstrap.calls"
  AIO_SYSTEMD_ARGS="${INSTALL_DIR}/systemd-run.args"
  AIO_CORE_ACTIVE="${INSTALL_DIR}/bootstrap-core.active"
  AIO_CORE_STOPPED="${INSTALL_DIR}/bootstrap-core.stopped"
  AIO_SYSTEMCTL_ACTIVE_CALLS=0
  AIO_HOME="${INSTALL_DIR}/attacker-home"
  mkdir -p "${INSTALL_DIR}/bin" "${INSTALL_DIR}/data/agent"
  mkdir -p "${AIO_HOME}"
  printf '%s\n' \
    'url = "http://169.254.169.254/latest/meta-data"' \
    'upload-file = "/etc/shadow"' >"${AIO_HOME}/.curlrc"
  : >"${AIO_CALLS}"
  printf '%s\n' '#!/usr/bin/env bash' 'exit 97' >"${INSTALL_DIR}/bin/media-core"
  chmod 755 "${INSTALL_DIR}/bin/media-core"
  printf '%s\n' \
    '#!/usr/bin/env bash' \
    'set -euo pipefail' \
    "AIO_CALLS='${AIO_CALLS}'" \
    "WORKER_GENERATION_ID='${WORKER_GENERATION_ID}'" \
    'IFS= read -r token || [ -n "${token}" ]' \
    '[ "${#token}" -eq 146 ]' \
    '[[ "${token}" =~ ^ssae1[.][A-Za-z0-9_-]{96}[.][A-Za-z0-9_-]{43}$ ]]' \
    'if env | grep -Fq -- "${token}"; then exit 95; fi' \
    'case " $* " in *" ${token} "*) exit 96 ;; esac' \
    'identity_dir=""' \
    'while [ "$#" -gt 0 ]; do' \
    '  if [ "$1" = --identity-dir ]; then identity_dir="$2"; shift 2; else shift; fi' \
    'done' \
    '[ -n "${identity_dir}" ] && [ -d "$(dirname "${identity_dir}")" ]' \
    'mkdir "${identity_dir}"' \
    'printf "%s\n" "${WORKER_GENERATION_ID}" >"${identity_dir}/current"' \
    'printf "%s\n" enroll >>"${AIO_CALLS}"' \
    >"${INSTALL_DIR}/bin/media-agent"
  chmod 755 "${INSTALL_DIR}/bin/media-agent"
  cat >"${INSTALL_DIR}/.env" <<EOF
INSTALL_ROLE='all-in-one-host-cpu'
CORE_HTTP_PORT='18080'
CORE_GRPC_PORT='15051'
CORE_HTTP_TLS_CERT_PATH='${CERT_FILE}'
CORE_GRPC_TLS_SERVER_CA_PATH='${CA_FILE}'
AGENT_NODE_ID='${WORKER_NODE_ID}'
AGENT_IDENTITY_DIR='${AGENT_IDENTITY_DIR}'
EOF

  security_preflight_env() {
    [ "${4:-}" = core-only ]
    printf '%s\n' preflight >>"${AIO_CALLS}"
  }
  run_core_auth_from_installed_env() {
    [ "$1" = "${INSTALL_DIR}/.env" ]
    [ "$2" = "${INSTALL_DIR}/bin/media-core" ]
    [ "$3" = agent ] && [ "$4" = create-enrollment ]
    [ "$5" = --node-id ] && [ "$6" = "${WORKER_NODE_ID}" ]
    [ "$7" = --token-stdout ] && [ "$#" -eq 7 ]
    printf '%s\n' token >>"${AIO_CALLS}"
    printf '%s\n' "${AIO_TOKEN}"
  }
  systemd-run() {
    printf '%s\n' "$*" >"${AIO_SYSTEMD_ARGS}"
    if printf '%s' "$*" | grep -Fq -- "${AIO_TOKEN}"; then
      return 91
    fi
    : >"${AIO_CORE_ACTIVE}"
    printf '%s\n' systemd-run >>"${AIO_CALLS}"
  }
  systemctl() {
    case "$1" in
      is-active)
        AIO_SYSTEMCTL_ACTIVE_CALLS=$((AIO_SYSTEMCTL_ACTIVE_CALLS + 1))
        if [ "${AIO_SYSTEMCTL_ACTIVE_CALLS}" -eq 2 ]; then
          # bootstrap_deadline is intentionally supplied by Bash dynamic scope.
          # shellcheck disable=SC2154
          SECONDS=$((bootstrap_deadline - 1))
        fi
        [ -f "${AIO_CORE_ACTIVE}" ] && [ ! -f "${AIO_CORE_STOPPED}" ]
        ;;
      stop)
        : >"${AIO_CORE_STOPPED}"
        printf '%s\n' stop >>"${AIO_CALLS}"
        ;;
      reset-failed) : ;;
      *) return 0 ;;
    esac
  }
  curl() {
    [ "$1" = -q ]
    [ "$2" = --proto ] && [ "$3" = '=https' ]
    case " $* " in *' --connect-timeout 1 --max-time 1 '*) ;; *) return 94 ;; esac
    [ -f "${AIO_CORE_ACTIVE}" ] && [ ! -f "${AIO_CORE_STOPPED}" ]
    printf '%s\n' readiness >>"${AIO_CALLS}"
  }

  unset AGENT_ENROLLMENT_TOKEN
  HOME="${AIO_HOME}" bootstrap_all_in_one_agent_identity_if_needed
  [ -f "${AGENT_IDENTITY_DIR}/current" ]
  [ -f "${AIO_CORE_STOPPED}" ]
  [ "$(tr '\n' ' ' <"${AIO_CALLS}")" = \
    'preflight token systemd-run readiness enroll stop ' ]
  grep -Fq -- '--property=EnvironmentFile=' "${AIO_SYSTEMD_ARGS}"
  grep -Fq -- '/usr/bin/env STREAMSERVER_ENV=production' "${AIO_SYSTEMD_ARGS}"
  ! grep -Fq -- "${AIO_TOKEN}" "${AIO_SYSTEMD_ARGS}"
  if grep -R -Fq --exclude=bootstrap.calls -- "${AIO_TOKEN}" "${INSTALL_DIR}"; then
    echo 'all-in-one bootstrap persisted the enrollment token' >&2
    exit 1
  fi
)

# systemd-run can report an error after the transient unit has already been
# created. EXIT cleanup must stop and reset the known unit even though the
# successful-start marker was never reached.
(
  set +x
  INSTALL_ROLE=all-in-one-host-cpu
  INSTALL_DIR="${TMP_DIR}/all-in-one-ambiguous-systemd-run"
  SERVICE_USER=streamserver
  SERVICE_GROUP=streamserver
  UNIT_BASENAME=ss-contract-aio-ambiguous
  EMULATED_SECURITY_METADATA=1
  NODE_ID="${WORKER_NODE_ID}"
  AGENT_IDENTITY_DIR="${INSTALL_DIR}/data/agent/identity"
  AIO_AMBIGUOUS_CALLS="${INSTALL_DIR}/bootstrap.calls"
  AIO_AMBIGUOUS_ACTIVE="${INSTALL_DIR}/bootstrap.active"
  AIO_AMBIGUOUS_STOPPED="${INSTALL_DIR}/bootstrap.stopped"
  mkdir -p "${INSTALL_DIR}/bin" "${INSTALL_DIR}/data/agent"
  touch "${INSTALL_DIR}/bin/media-core" "${INSTALL_DIR}/bin/media-agent" \
    "${INSTALL_DIR}/http.pem" "${INSTALL_DIR}/ca.pem"
  chmod 755 "${INSTALL_DIR}/bin/media-core" "${INSTALL_DIR}/bin/media-agent"
  : >"${AIO_AMBIGUOUS_CALLS}"
  cat >"${INSTALL_DIR}/.env" <<EOF
INSTALL_ROLE='all-in-one-host-cpu'
CORE_HTTP_PORT='18080'
CORE_GRPC_PORT='15051'
CORE_HTTP_TLS_CERT_PATH='${INSTALL_DIR}/http.pem'
CORE_GRPC_TLS_SERVER_CA_PATH='${INSTALL_DIR}/ca.pem'
AGENT_NODE_ID='${WORKER_NODE_ID}'
AGENT_IDENTITY_DIR='${AGENT_IDENTITY_DIR}'
EOF

  validate_x509_ca_certificate_for_service() { return 0; }
  validate_certificate_directly_issued_by_ca_for_service() { return 0; }
  validate_certificate_san_name_for_service() { return 0; }
  security_preflight_env() { return 0; }
  run_core_auth_from_installed_env() {
    printf '%s\n' "${ENROLLMENT_WIRE_TOKEN}"
  }
  systemd-run() {
    : >"${AIO_AMBIGUOUS_ACTIVE}"
    printf '%s\n' systemd-run-created >>"${AIO_AMBIGUOUS_CALLS}"
    return 91
  }
  systemctl() {
    case "$1" in
      is-active)
        [ -f "${AIO_AMBIGUOUS_ACTIVE}" ] \
          && [ ! -f "${AIO_AMBIGUOUS_STOPPED}" ]
        ;;
      stop)
        : >"${AIO_AMBIGUOUS_STOPPED}"
        printf '%s\n' stop >>"${AIO_AMBIGUOUS_CALLS}"
        ;;
      reset-failed)
        printf '%s\n' reset-failed >>"${AIO_AMBIGUOUS_CALLS}"
        ;;
      *) return 0 ;;
    esac
  }

  set +e
  bootstrap_all_in_one_agent_identity_if_needed >/dev/null 2>&1
  AIO_AMBIGUOUS_STATUS=$?
  set -e
  [ "${AIO_AMBIGUOUS_STATUS}" -ne 0 ]
  [ -f "${AIO_AMBIGUOUS_STOPPED}" ] || {
    echo 'ambiguous systemd-run failure leaked the transient bootstrap Core' >&2
    exit 1
  }
  [ "$(tail -n 2 "${AIO_AMBIGUOUS_CALLS}" | tr '\n' ' ')" = \
    'stop reset-failed ' ]
)

RANDOM_PASSWORD_ONE="$(generate_one_time_admin_password)"
RANDOM_PASSWORD_TWO="$(generate_one_time_admin_password)"
[[ "${RANDOM_PASSWORD_ONE}" =~ ^[0-9a-f]{36}$ ]]
[[ "${RANDOM_PASSWORD_TWO}" =~ ^[0-9a-f]{36}$ ]]
[ "${RANDOM_PASSWORD_ONE}" != "${RANDOM_PASSWORD_TWO}" ]
unset RANDOM_PASSWORD_ONE RANDOM_PASSWORD_TWO

# Durable administrator handoff state is security-sensitive even though it does
# not contain the password. Refuse attacker-controlled path components instead
# of following or repairing them in place.
(
  INSTALL_DIR="${TMP_DIR}/state-path-contract-install"
  mkdir -p "${INSTALL_DIR}"
  symlink_target="${TMP_DIR}/state-path-symlink-target"
  ADMIN_HANDOFF_STATE_ROOT="${TMP_DIR}/state-path-symlink"
  mkdir -p "${symlink_target}"
  ln -s "${symlink_target}" "${ADMIN_HANDOFF_STATE_ROOT}"
  set +e
  SYMLINK_STATE_OUTPUT="$(ensure_admin_handoff_state_dir 2>&1)"
  SYMLINK_STATE_STATUS=$?
  set -e
  [ "${SYMLINK_STATE_STATUS}" -ne 0 ] || {
    echo 'administrator handoff state root symlink was unexpectedly accepted' >&2
    exit 1
  }
  if [ -L "${ADMIN_HANDOFF_STATE_ROOT}" ]; then
    assert_contains "${SYMLINK_STATE_OUTPUT}" 'symbolic link'
  fi
)

(
  INSTALL_DIR="${TMP_DIR}/state-path-contract-install"
  ADMIN_HANDOFF_STATE_ROOT="${TMP_DIR}/state-path-insecure-parent/state"
  mkdir -p "${INSTALL_DIR}" "$(dirname "${ADMIN_HANDOFF_STATE_ROOT}")"
  chmod 0777 "$(dirname "${ADMIN_HANDOFF_STATE_ROOT}")"
  # Exercise the production-root ownership/mode branch without requiring the
  # contract suite itself to run as root.
  id() {
    [ "${1:-}" = '-u' ] && { printf '%s\n' 0; return 0; }
    command id "$@"
  }
  set +e
  INSECURE_ANCESTOR_OUTPUT="$(ensure_admin_handoff_state_dir 2>&1)"
  INSECURE_ANCESTOR_STATUS=$?
  set -e
  [ "${INSECURE_ANCESTOR_STATUS}" -ne 0 ] || {
    echo 'writable administrator handoff state ancestor was unexpectedly accepted' >&2
    exit 1
  }
  assert_contains "${INSECURE_ANCESTOR_OUTPUT}" 'secure root-owned directory'
)

(
  INSTALL_DIR="${TMP_DIR}/fresh-install"
  INSTALL_ROLE="control-plane"
  INSTANCE_NAME="contract-fresh"
  ADMIN_HANDOFF_STATE_ROOT="${TMP_DIR}/fresh-installer-state"
  INTERACTIVE_INSTALL=1
  EMULATED_SECURITY_METADATA=1
  export ADMIN_PASSWORD=parent-export-marker
  flock() { return 0; }
  id() {
    [ "${1:-}" = '-u' ] && { printf '%s\n' 0; return 0; }
    command id "$@"
  }
  install() {
    local target="${!#}"
    mkdir -- "${target}"
  }
  admin_handoff_secure_root_ancestors_status() { return 0; }
  # Git for Windows does not expose enforceable POSIX owner/mode metadata.
  # The negative path tests above exercise the production validator; this
  # behavioral block keeps only the no-symlink/type checks enabled.
  admin_handoff_secure_directory_status() {
    [ ! -L "$1" ] && [ -d "$1" ] && [ -x "$1" ]
  }
  admin_handoff_secure_file_status() {
    [ ! -L "$1" ] && [ -f "$1" ]
  }
  mkdir -p "${INSTALL_DIR}/certs/auth"
  acquire_admin_handoff_lock
  REAL_OPENSSL="$(command -v openssl)"
  openssl() {
    if env | grep -q '^ADMIN_PASSWORD='; then
      echo 'openssl inherited ADMIN_PASSWORD' >&2
      return 97
    fi
    "${REAL_OPENSSL}" "$@"
  }
  prompt() { printf '%s' "${2:-}"; }
  prompt_non_empty() { printf '%s' "$2"; }
  prompt_local_tcp_port() { printf '%s' "$4"; }
  prompt_password_with_confirmation() {
    echo 'fresh install unexpectedly requested a user-selected admin password' >&2
    return 1
  }
  generate_one_time_admin_password() { printf '%s' '0123456789abcdef0123456789abcdef0123'; }
  generate_uuid() { printf '%s' '0190d8d4-31d2-7b23-b27e-8b9b28a2ed11'; }

  configure_core_values
  [ "${AUTH_MODE}" = "local_password" ]
  [ "${AUTH_ENABLED}" = "true" ]
  [ "${ADMIN_BOOTSTRAP_REQUIRED}" -eq 1 ]
  [ "${ADMIN_PASSWORD}" = "0123456789abcdef0123456789abcdef0123" ]
  [ "${CORE_HTTP_ADDR}" = "127.0.0.1:8080" ]
  [ "${CORE_GRPC_ADDR}" = "127.0.0.1:50051" ]
  ! env | grep -q '^ADMIN_PASSWORD='
  validate_certificate_key_pair \
    "${CORE_HTTP_TLS_CERT_PATH}" "${CORE_HTTP_TLS_KEY_PATH}"
  validate_certificate_key_pair \
    "${CORE_GRPC_TLS_CERT_PATH}" "${CORE_GRPC_TLS_KEY_PATH}"
  validate_x509_ca_certificate "${CORE_AGENT_CA_CERT_PATH}"
  validate_certificate_key_pair \
    "${CORE_AGENT_CA_CERT_PATH}" "${CORE_AGENT_CA_KEY_PATH}"
  validate_x509_ca_certificate "${CORE_GRPC_TLS_SERVER_CA_PATH}"
  validate_x509_ca_certificate "${CORE_AGENT_MANAGEMENT_CA_PATH}"
  validate_certificate_key_pair \
    "${CORE_AGENT_MANAGEMENT_CLIENT_CERT_PATH}" \
    "${CORE_AGENT_MANAGEMENT_CLIENT_KEY_PATH}"
  validate_private_public_key_pair \
    "${CORE_AGENT_CAPABILITY_JWT_PRIVATE_KEY_PATH}" \
    "${CORE_AGENT_CAPABILITY_JWT_PUBLIC_KEY_PATH}"
  "${REAL_OPENSSL}" verify -CAfile "${CORE_GRPC_TLS_SERVER_CA_PATH}" \
    "${CORE_GRPC_TLS_CERT_PATH}" >/dev/null
  "${REAL_OPENSSL}" verify -CAfile "${CORE_AGENT_MANAGEMENT_CA_PATH}" \
    "${CORE_AGENT_MANAGEMENT_CLIENT_CERT_PATH}" >/dev/null
  "${REAL_OPENSSL}" x509 -in "${CORE_GRPC_TLS_CERT_PATH}" -noout \
    -checkhost "${CORE_GRPC_TLS_DOMAIN_NAME}" >/dev/null
  "${REAL_OPENSSL}" x509 -in "${CORE_HTTP_TLS_CERT_PATH}" -noout \
    -checkhost "${CORE_HTTP_PUBLIC_HOST}" >/dev/null
  [ "$("${REAL_OPENSSL}" x509 -in "${CORE_AGENT_MANAGEMENT_CLIENT_CERT_PATH}" \
      -noout -ext subjectAltName | grep -o 'URI:spiffe://streamserver/core/[^,[:space:]]*' | wc -l)" -eq 1 ]
  "${REAL_OPENSSL}" x509 -in "${CORE_AGENT_MANAGEMENT_CLIENT_CERT_PATH}" \
    -noout -ext subjectAltName | grep -Fq \
    "URI:spiffe://streamserver/core/${CORE_INSTANCE_ID}"
  ROOT_FINGERPRINTS="$({
    "${REAL_OPENSSL}" x509 -in "${CORE_AGENT_CA_CERT_PATH}" -outform DER
    "${REAL_OPENSSL}" x509 -in "${CORE_GRPC_TLS_SERVER_CA_PATH}" -outform DER
    "${REAL_OPENSSL}" x509 -in "${CORE_AGENT_MANAGEMENT_CA_PATH}" -outform DER
  } | sha256sum | awk '{print $1}')"
  [ -n "${ROOT_FINGERPRINTS}" ]
  [ "$(for root in \
      "${CORE_AGENT_CA_CERT_PATH}" \
      "${CORE_GRPC_TLS_SERVER_CA_PATH}" \
      "${CORE_AGENT_MANAGEMENT_CA_PATH}"; do \
        "${REAL_OPENSSL}" x509 -in "${root}" -outform DER | sha256sum; \
      done | awk '{print $1}' | sort -u | wc -l)" -eq 3 ]
  validate_private_public_key_pair \
    "${AUTH_JWT_PRIVATE_KEY_PATH}" "${AUTH_JWT_PUBLIC_KEY_PATH}"
  PENDING_MARKER="$(pending_admin_handoff_path)"
  [ -f "${PENDING_MARKER}" ]
  if [ "${EMULATED_SECURITY_METADATA:-0}" -eq 0 ] && [ "$(id -u)" -eq 0 ]; then
    [ "$(stat -c '%a' "${PENDING_MARKER}")" = "600" ]
  fi
  ! grep -Fq '0123456789abcdef0123456789abcdef0123' "${PENDING_MARKER}"
  grep -Eq '^JWT_PUBLIC_KEY_SHA256=[0-9a-f]{64}$' "${PENDING_MARKER}"
  grep -Eq '^HANDOFF_ID=[0-9a-f-]{36}$' "${PENDING_MARKER}"

  cp "${AUTH_JWT_PRIVATE_KEY_PATH}" "${AUTH_JWT_PRIVATE_KEY_PATH}.original"
  cp "${AUTH_JWT_PUBLIC_KEY_PATH}" "${AUTH_JWT_PUBLIC_KEY_PATH}.original"
  "${REAL_OPENSSL}" genpkey -algorithm Ed25519 \
    -out "${AUTH_JWT_PRIVATE_KEY_PATH}" >/dev/null 2>&1
  "${REAL_OPENSSL}" pkey -in "${AUTH_JWT_PRIVATE_KEY_PATH}" -pubout \
    -out "${AUTH_JWT_PUBLIC_KEY_PATH}" >/dev/null 2>&1
  set +e
  STALE_FINGERPRINT_OUTPUT="$(read_pending_admin_handoff_username 2>&1)"
  STALE_FINGERPRINT_STATUS=$?
  set -e
  [ "${STALE_FINGERPRINT_STATUS}" -ne 0 ] || {
    echo 'stale administrator handoff JWT fingerprint was unexpectedly accepted' >&2
    exit 1
  }
  assert_contains "${STALE_FINGERPRINT_OUTPUT}" 'JWT public key fingerprint'
  mv -f "${AUTH_JWT_PRIVATE_KEY_PATH}.original" "${AUTH_JWT_PRIVATE_KEY_PATH}"
  mv -f "${AUTH_JWT_PUBLIC_KEY_PATH}.original" "${AUTH_JWT_PUBLIC_KEY_PATH}"

  TUI_CALL_COUNT=0
  run_streamserver_config_tui_if_requested() { TUI_CALL_COUNT=$((TUI_CALL_COUNT + 1)); }
  run_streamserver_config_tui_with_handoff_guard
  [ "${TUI_CALL_COUNT}" -eq 0 ]

  mkdir -p "${INSTALL_DIR}/bin"
  printf '%s\n' \
    '#!/usr/bin/env bash' \
    'set -euo pipefail' \
    '! env | grep -q "^ADMIN_PASSWORD="' >"${INSTALL_DIR}/bin/streamserver-config"
  chmod 755 "${INSTALL_DIR}/bin/streamserver-config"
  "${INSTALL_DIR}/bin/streamserver-config" --env-probe

  FAKE_ADMIN_STATE_FILE="${INSTALL_DIR}/fake-admin-state"
  FAKE_ADMIN_ACTION_FILE="${INSTALL_DIR}/fake-admin-actions"
  FAKE_HANDOFF_ID_FILE="${INSTALL_DIR}/fake-handoff-id"
  FAKE_HANDOFF_ID="$(read_admin_handoff_id "${PENDING_MARKER}")"
  printf '%s\n' "${FAKE_HANDOFF_ID}" >"${FAKE_HANDOFF_ID_FILE}"
  printf '%s\n' missing:0 >"${FAKE_ADMIN_STATE_FILE}"
  : >"${FAKE_ADMIN_ACTION_FILE}"
  printf '%s\n' \
    '#!/usr/bin/env bash' \
    'set -euo pipefail' \
    "FAKE_ADMIN_STATE_FILE='${FAKE_ADMIN_STATE_FILE}'" \
    "FAKE_ADMIN_ACTION_FILE='${FAKE_ADMIN_ACTION_FILE}'" \
    "FAKE_HANDOFF_ID_FILE='${FAKE_HANDOFF_ID_FILE}'" \
    'FAKE_HANDOFF_ID="$(cat "${FAKE_HANDOFF_ID_FILE}")"' \
    'if env | grep -q "^ADMIN_PASSWORD="; then exit 95; fi' \
    'case " $* " in *0123456789abcdef*) exit 98 ;; esac' \
    'if [ "$*" = "auth bootstrap-status --username admin --handoff-id ${FAKE_HANDOFF_ID}" ]; then' \
    '  cat "${FAKE_ADMIN_STATE_FILE}"' \
    'elif [ "${1:-}" = auth ] && [ "${2:-}" = recover-bootstrap-admin ] \' \
    '  && [ "${3:-}" = --username ] && [ "${4:-}" = admin ] \' \
    '  && [ "${5:-}" = --handoff-id ] && [ "${6:-}" = "${FAKE_HANDOFF_ID}" ] \' \
    '  && [ "${7:-}" = --expected-version ] && [ "${9:-}" = --password-stdin ]; then' \
    '    expected_version="${8:-}"' \
    '    password="$(cat)"' \
    '    [[ "${password}" =~ ^[0-9a-f]{36}$ ]]' \
    '    case "$(cat "${FAKE_ADMIN_STATE_FILE}")" in' \
    '      missing:0) [ "${expected_version}" = 0 ]; printf "%s\n" created >>"${FAKE_ADMIN_ACTION_FILE}" ;;' \
    '      pending-password-change:*)' \
    '        current_version="$(cut -d: -f2 "${FAKE_ADMIN_STATE_FILE}")"' \
    '        [ "${expected_version}" = "${current_version}" ]' \
    '        printf "%s\n" recovered >>"${FAKE_ADMIN_ACTION_FILE}" ;;' \
    '      *) exit 99 ;;' \
    '    esac' \
    '    next_version="$(wc -l <"${FAKE_ADMIN_ACTION_FILE}" | tr -d "[:space:]")"' \
    '    printf "pending-password-change:%s\n" "${next_version}" >"${FAKE_ADMIN_STATE_FILE}"' \
    'elif [ "$*" = "auth check-admin" ]; then' \
    '  [ "$(cat "${FAKE_ADMIN_STATE_FILE}")" != missing:0 ]' \
    'elif [ "$*" = "auth check-config" ]; then' \
    '  exit 0' \
    'else' \
    '  exit 64' \
    'fi' >"${INSTALL_DIR}/bin/media-core"
  chmod 755 "${INSTALL_DIR}/bin/media-core"
  printf '%s\n' \
    'INSTALL_ROLE=control-plane' \
    'INSTANCE_NAME=contract-fresh' \
    'DATABASE_URL=postgresql://127.0.0.1/unused' \
    'AUTH_MODE=local_password' \
    "AUTH_JWT_PRIVATE_KEY_PATH=${AUTH_JWT_PRIVATE_KEY_PATH}" \
    "AUTH_JWT_PUBLIC_KEY_PATH=${AUTH_JWT_PUBLIC_KEY_PATH}" \
    'CORE_HTTP_ADDR=127.0.0.1:8080' \
    'CORE_HTTP_TLS_CERT_PATH=' \
    'CORE_HTTP_TLS_KEY_PATH=' \
    'CORE_GRPC_ADDR=127.0.0.1:50051' \
    "CORE_GRPC_TLS_CERT_PATH=${CERT_FILE}" \
    "CORE_GRPC_TLS_KEY_PATH=${KEY_FILE}" \
    "CORE_GRPC_TLS_CLIENT_CA_PATH=${AGENT_CA_FILE}" >"${INSTALL_DIR}/.env"
  append_core_internal_pki_env "${INSTALL_DIR}/.env"

  INTERACTIVE_INSTALL=0
  set +e
  NONINTERACTIVE_UPGRADE_GATE_OUTPUT="$(security_preflight_env "${INSTALL_DIR}/.env" "${INSTALL_DIR}/bin/media-core" "" upgrade-gate 2>&1)"
  NONINTERACTIVE_UPGRADE_GATE_STATUS=$?
  set -e
  [ "${NONINTERACTIVE_UPGRADE_GATE_STATUS}" -ne 0 ]
  assert_contains "${NONINTERACTIVE_UPGRADE_GATE_OUTPUT}" 'one-time administrator password delivery is pending'
  [ ! -s "${FAKE_ADMIN_ACTION_FILE}" ]

  INTERACTIVE_INSTALL=1
  UPGRADE_GATE_OUTPUT="$(security_preflight_env "${INSTALL_DIR}/.env" "${INSTALL_DIR}/bin/media-core" "" upgrade-gate 2>&1)"
  assert_contains "${UPGRADE_GATE_OUTPUT}" '[PENDING] auth/admin: enabled administrator check is deferred until handoff recovery'
  [ ! -s "${FAKE_ADMIN_ACTION_FILE}" ]

  prepare_pending_admin_password_handoff \
    "${INSTALL_DIR}/.env" "${INSTALL_DIR}/bin/media-core"
  [ "${INITIAL_ADMIN_PASSWORD_READY}" -eq 1 ]
  [ "$(cat "${FAKE_ADMIN_ACTION_FILE}")" = created ]
  run_preflight "${INSTALL_DIR}/.env" "${INSTALL_DIR}/bin/media-core"
  [ "${PREFLIGHT_STATUS}" -eq 0 ]

  cleanup_admin_password
  run_preflight "${INSTALL_DIR}/.env" "${INSTALL_DIR}/bin/media-core"
  [ "${PREFLIGHT_STATUS}" -ne 0 ]
  assert_contains "${PREFLIGHT_OUTPUT}" 'one-time administrator password delivery is pending'
  [ -f "${PENDING_MARKER}" ]

  RECOVERY_PASSWORDS=(
    111111111111111111111111111111111111
    222222222222222222222222222222222222
    333333333333333333333333333333333333
    444444444444444444444444444444444444
    555555555555555555555555555555555555
  )
  for signal_status in 129 130 143; do
    next_password="${RECOVERY_PASSWORDS[0]}"
    RECOVERY_PASSWORDS=("${RECOVERY_PASSWORDS[@]:1}")
    generate_one_time_admin_password() { printf '%s' "${next_password}"; }
    prepare_pending_admin_password_handoff \
      "${INSTALL_DIR}/.env" "${INSTALL_DIR}/bin/media-core"
    [ "${INITIAL_ADMIN_PASSWORD_READY}" -eq 1 ]
    set +e
    ( handle_admin_password_signal "${signal_status}" )
    observed_signal_status=$?
    set -e
    [ "${observed_signal_status}" -eq "${signal_status}" ]
    cleanup_admin_password
    [ -f "${PENDING_MARKER}" ]
  done

  next_password="${RECOVERY_PASSWORDS[0]}"
  RECOVERY_PASSWORDS=("${RECOVERY_PASSWORDS[@]:1}")
  generate_one_time_admin_password() { printf '%s' "${next_password}"; }
  prepare_pending_admin_password_handoff \
    "${INSTALL_DIR}/.env" "${INSTALL_DIR}/bin/media-core"
  emit_initial_admin_credentials() { return 1; }
  set +e
  (show_initial_admin_credentials_if_needed >/dev/null 2>&1)
  EMIT_FAILURE_STATUS=$?
  set -e
  [ "${EMIT_FAILURE_STATUS}" -ne 0 ]
  cleanup_admin_password
  [ -f "${PENDING_MARKER}" ]

  next_password="${RECOVERY_PASSWORDS[0]}"
  generate_one_time_admin_password() { printf '%s' "${next_password}"; }
  prepare_pending_admin_password_handoff \
    "${INSTALL_DIR}/.env" "${INSTALL_DIR}/bin/media-core"
  CREDENTIAL_EMIT_COUNT=0
  emit_initial_admin_credentials() { CREDENTIAL_EMIT_COUNT=$((CREDENTIAL_EMIT_COUNT + 1)); }
  show_initial_admin_credentials_if_needed
  [ "${CREDENTIAL_EMIT_COUNT}" -eq 1 ]
  [ ! -f "${PENDING_MARKER}" ]
  [ -f "$(delivered_admin_handoff_path)" ]
  run_streamserver_config_tui_with_handoff_guard
  [ "${TUI_CALL_COUNT}" -eq 0 ]
  run_preflight "${INSTALL_DIR}/.env" "${INSTALL_DIR}/bin/media-core"
  [ "${PREFLIGHT_STATUS}" -eq 0 ]
  DELIVERED_MARKER="$(delivered_admin_handoff_path)"
  cp "${DELIVERED_MARKER}" "${DELIVERED_MARKER}.valid"
  printf '%s\n' 'UNEXPECTED_FIELD=must-fail-closed' >>"${DELIVERED_MARKER}"
  run_preflight "${INSTALL_DIR}/.env" "${INSTALL_DIR}/bin/media-core"
  [ "${PREFLIGHT_STATUS}" -ne 0 ]
  assert_contains "${PREFLIGHT_OUTPUT}" \
    '[UNKNOWN] auth/admin: delivered administrator handoff state is malformed or does not match the current key'
  mv -f "${DELIVERED_MARKER}.valid" "${DELIVERED_MARKER}"

  systemctl() { return 1; }
  UPGRADE=1
  UPGRADE_TARGET_WAS_ACTIVE=1
  UPGRADE_SERVICES_QUIESCED=1
  UPGRADE_RESTORE_ON_FAILURE=1
  UPGRADE_ACTIVE_UNITS=()
  UPGRADE_ACTIVE_MAIN_PIDS=()
  START_AFTER_INSTALL=1
  UNIT_BASENAME=streamserver-contract
  set +e
  ( set -e; start_services_if_requested ) >/dev/null 2>&1
  START_FAILURE_STATUS=$?
  set -e
  [ "${START_FAILURE_STATUS}" -ne 0 ]
  [ -f "$(delivered_admin_handoff_path)" ]
  ACTION_COUNT_BEFORE_RERUN="$(wc -l <"${FAKE_ADMIN_ACTION_FILE}" | tr -d ' ')"
  prepare_pending_admin_password_handoff \
    "${INSTALL_DIR}/.env" "${INSTALL_DIR}/bin/media-core"
  [ "${ADMIN_HANDOFF_DELIVERED_READY}" -eq 1 ]
  [ "$(wc -l <"${FAKE_ADMIN_ACTION_FILE}" | tr -d ' ')" = "${ACTION_COUNT_BEFORE_RERUN}" ]
  UPGRADE=0
  systemctl() { return 0; }
  start_services_if_requested
  finalize_admin_handoff_after_install_success
  [ ! -e "$(delivered_admin_handoff_path)" ]

  # A crash can leave pending after the administrator already changed the
  # password. Recovery must acknowledge completion without another reset or
  # another password generation/display.
  write_pending_admin_handoff_marker admin
  FAKE_HANDOFF_ID="$(read_admin_handoff_id "$(pending_admin_handoff_path)")"
  printf '%s\n' "${FAKE_HANDOFF_ID}" >"${FAKE_HANDOFF_ID_FILE}"
  printf '%s\n' complete >"${FAKE_ADMIN_STATE_FILE}"
  : >"${FAKE_ADMIN_ACTION_FILE}"
  cleanup_admin_password
  ADMIN_HANDOFF_DELIVERED_READY=0
  COMPLETE_UPGRADE_GATE_OUTPUT="$(security_preflight_env "${INSTALL_DIR}/.env" "${INSTALL_DIR}/bin/media-core" "" upgrade-gate 2>&1)"
  assert_contains "${COMPLETE_UPGRADE_GATE_OUTPUT}" '[PENDING] auth/admin: valid administrator handoff will be recovered after application quiesce'
  prepare_pending_admin_password_handoff \
    "${INSTALL_DIR}/.env" "${INSTALL_DIR}/bin/media-core"
  [ "${ADMIN_HANDOFF_DELIVERED_READY}" -eq 1 ]
  [ ! -s "${FAKE_ADMIN_ACTION_FILE}" ]
  [ -z "${ADMIN_PASSWORD+x}" ]
  [ -f "$(delivered_admin_handoff_path)" ]
  run_preflight "${INSTALL_DIR}/.env" "${INSTALL_DIR}/bin/media-core"
  [ "${PREFLIGHT_STATUS}" -eq 0 ]
  finalize_admin_handoff_after_install_success
  [ ! -e "$(delivered_admin_handoff_path)" ]

  write_pending_admin_handoff_marker admin
  FAKE_HANDOFF_ID="$(read_admin_handoff_id "$(pending_admin_handoff_path)")"
  printf '%s\n' "${FAKE_HANDOFF_ID}" >"${FAKE_HANDOFF_ID_FILE}"
  printf '%s\n' conflict >"${FAKE_ADMIN_STATE_FILE}"
  : >"${FAKE_ADMIN_ACTION_FILE}"
  set +e
  CONFLICT_HANDOFF_OUTPUT="$({
    prepare_pending_admin_password_handoff \
      "${INSTALL_DIR}/.env" "${INSTALL_DIR}/bin/media-core"
  } 2>&1)"
  CONFLICT_HANDOFF_STATUS=$?
  set -e
  [ "${CONFLICT_HANDOFF_STATUS}" -ne 0 ] || {
    echo 'conflicting administrator handoff unexpectedly attempted recovery' >&2
    exit 1
  }
  assert_contains "${CONFLICT_HANDOFF_OUTPUT}" '拒绝自动重置'
  [ ! -s "${FAKE_ADMIN_ACTION_FILE}" ]
  [ -f "$(pending_admin_handoff_path)" ]
)

set +e
NONINTERACTIVE_FRESH_OUTPUT="$(
  (
  INSTALL_DIR="${TMP_DIR}/noninteractive-fresh-install"
  INSTALL_ROLE="control-plane"
  INSTANCE_NAME="contract-noninteractive"
  INTERACTIVE_INSTALL=0
  mkdir -p "${INSTALL_DIR}/certs/auth"
  prompt() { printf '%s' "${2:-}"; }
  prompt_non_empty() { printf '%s' "$2"; }
  prompt_local_tcp_port() { printf '%s' "$4"; }
  prompt_password_with_confirmation() {
    echo 'unexpected-user-password-prompt' >&2
    printf '%s' 'user-selected-password'
  }
  generate_one_time_admin_password() {
    echo 'unexpected-password-generation' >&2
    printf '%s' '0123456789abcdef0123456789abcdef0123'
  }
  configure_core_values
  ) 2>&1
)"
NONINTERACTIVE_FRESH_STATUS=$?
set -e
[ "${NONINTERACTIVE_FRESH_STATUS}" -ne 0 ]
if printf '%s' "${NONINTERACTIVE_FRESH_OUTPUT}" | grep -Eq 'unexpected-(user-password-prompt|password-generation)|0123456789abcdef'; then
  echo 'noninteractive fresh install prompted for, generated, or disclosed an admin password' >&2
  exit 1
fi

export ADMIN_PASSWORD=0123456789abcdef0123456789abcdef0123
cleanup_admin_password
[ -z "${ADMIN_PASSWORD+x}" ]
[ "${INITIAL_ADMIN_PASSWORD_READY}" -eq 0 ]

UPGRADE_DIR="${TMP_DIR}/upgrade-install"
mkdir -p "${UPGRADE_DIR}/certs/auth"
cp "${JWT_PRIVATE_KEY_FILE}" "${UPGRADE_DIR}/certs/auth/jwt-private.pem"
cp "${JWT_PUBLIC_KEY_FILE}" "${UPGRADE_DIR}/certs/auth/jwt-public.pem"
printf '%s\n' \
  'AUTH_MODE=local_password' \
  'AUTH_JWT_PRIVATE_KEY_PATH=certs/auth/jwt-private.pem' \
  'AUTH_JWT_PUBLIC_KEY_PATH=certs/auth/jwt-public.pem' \
  'CORE_HTTP_ADDR=127.0.0.1:18080' \
  'CORE_HTTP_PORT=18080' \
  'CORE_HTTP_TLS_CERT_PATH=certs/http.pem' \
  'CORE_HTTP_TLS_KEY_PATH=certs/http.key' \
  'CORE_GRPC_ADDR=127.0.0.1:15051' \
  'CORE_GRPC_PORT=15051' \
  'CORE_GRPC_TLS_CERT_PATH=certs/grpc.pem' \
  'CORE_GRPC_TLS_KEY_PATH=certs/grpc.key' \
  'CORE_GRPC_TLS_CLIENT_CA_PATH=certs/client-ca.pem' >"${UPGRADE_DIR}/.env"
append_core_internal_pki_env "${UPGRADE_DIR}/.env"
UPGRADE_KEY_HASH="$(sha256sum "${UPGRADE_DIR}/certs/auth/jwt-private.pem" | awk '{print $1}')"
(
  INSTALL_DIR="${UPGRADE_DIR}"
  INSTALL_ROLE="control-plane"
  INSTANCE_NAME="contract-upgrade"
  prompt() { printf '%s' "${2:-}"; }
  prompt_non_empty() { printf '%s' "$2"; }
  prompt_local_tcp_port() { printf '%s' "$4"; }
  generate_one_time_admin_password() {
    echo 'upgrade unexpectedly generated an administrator password' >&2
    return 1
  }
  TUI_CALL_COUNT=0
  run_streamserver_config_tui_if_requested() { TUI_CALL_COUNT=$((TUI_CALL_COUNT + 1)); }

  configure_core_values
  [ "${AUTH_MODE}" = "local_password" ]
  [ "${ADMIN_BOOTSTRAP_REQUIRED}" -eq 0 ]
  [ "${AUTH_JWT_PRIVATE_KEY_PATH}" = "certs/auth/jwt-private.pem" ]
  [ "${CORE_HTTP_TLS_CERT_PATH}" = "certs/http.pem" ]
  [ "${CORE_GRPC_TLS_CLIENT_CA_PATH}" = "certs/client-ca.pem" ]
  prepare_pending_admin_password_handoff \
    "${INSTALL_DIR}/.env" "${INSTALL_DIR}/bin/must-not-be-called"
  [ -z "${ADMIN_PASSWORD+x}" ]
  run_streamserver_config_tui_with_handoff_guard
  [ "${TUI_CALL_COUNT}" -eq 1 ]
)
[ "${UPGRADE_KEY_HASH}" = "$(sha256sum "${UPGRADE_DIR}/certs/auth/jwt-private.pem" | awk '{print $1}')" ]

EXTERNAL_DIR="${TMP_DIR}/external-upgrade"
mkdir -p "${EXTERNAL_DIR}"
: >"${EXTERNAL_DIR}/.env"
EXPECTED_EXTERNAL_KEY="$(tr -d '\r' <"${JWT_PUBLIC_KEY_FILE}")"
write_env_entry "${EXTERNAL_DIR}/.env" INSTALL_ROLE control-plane
write_env_entry "${EXTERNAL_DIR}/.env" AUTH_MODE external_jwt
write_env_entry "${EXTERNAL_DIR}/.env" JWT_PUBLIC_KEY "${EXPECTED_EXTERNAL_KEY}"
write_env_entry "${EXTERNAL_DIR}/.env" DATABASE_URL postgresql://127.0.0.1/unused
write_env_entry "${EXTERNAL_DIR}/.env" CORE_HTTP_ADDR 127.0.0.1:8080
write_env_entry "${EXTERNAL_DIR}/.env" CORE_HTTP_TLS_CERT_PATH ''
write_env_entry "${EXTERNAL_DIR}/.env" CORE_HTTP_TLS_KEY_PATH ''
write_env_entry "${EXTERNAL_DIR}/.env" CORE_GRPC_ADDR 127.0.0.1:50051
write_env_entry "${EXTERNAL_DIR}/.env" CORE_GRPC_TLS_CERT_PATH "${CERT_FILE}"
write_env_entry "${EXTERNAL_DIR}/.env" CORE_GRPC_TLS_KEY_PATH "${KEY_FILE}"
write_env_entry "${EXTERNAL_DIR}/.env" CORE_GRPC_TLS_CLIENT_CA_PATH "${AGENT_CA_FILE}"
append_core_internal_pki_env "${EXTERNAL_DIR}/.env"
STORED_EXTERNAL_KEY="$(existing_env_value "${EXTERNAL_DIR}/.env" JWT_PUBLIC_KEY)"
[ "${STORED_EXTERNAL_KEY}" = "${EXPECTED_EXTERNAL_KEY}" ] || {
  printf 'multiline external JWT public key did not round-trip through EnvironmentFile (stored=%s bytes, expected=%s bytes)\n' \
    "${#STORED_EXTERNAL_KEY}" "${#EXPECTED_EXTERNAL_KEY}" >&2
  exit 1
}
(
  INSTALL_DIR="${EXTERNAL_DIR}"
  INSTALL_ROLE="control-plane"
  INSTANCE_NAME="contract-external"
  prompt() { printf '%s' "${2:-}"; }
  prompt_non_empty() { printf '%s' "$2"; }
  prompt_local_tcp_port() { printf '%s' "$4"; }

  configure_core_values
  [ "${AUTH_MODE}" = "external_jwt" ]
  [ "${AUTH_ENABLED}" = "true" ]
  [ "${JWT_PUBLIC_KEY}" = "${EXPECTED_EXTERNAL_KEY}" ]
  [ "${ADMIN_BOOTSTRAP_REQUIRED}" -eq 0 ]
)
run_preflight "${EXTERNAL_DIR}/.env" "${FAKE_CORE}"
[ "${PREFLIGHT_STATUS}" -eq 0 ] || {
  printf 'valid multiline external JWT key failed preflight:\n%s\n' "${PREFLIGHT_OUTPUT}" >&2
  exit 1
}
assert_contains "${PREFLIGHT_OUTPUT}" '[OK] auth/admin: external_jwt public key verified'

MALFORMED_EXTERNAL_ENV="${TMP_DIR}/malformed-external.env"
: >"${MALFORMED_EXTERNAL_ENV}"
write_env_entry "${MALFORMED_EXTERNAL_ENV}" INSTALL_ROLE control-plane
write_env_entry "${MALFORMED_EXTERNAL_ENV}" AUTH_MODE external_jwt
write_env_entry "${MALFORMED_EXTERNAL_ENV}" JWT_PUBLIC_KEY not-a-public-key
write_env_entry "${MALFORMED_EXTERNAL_ENV}" DATABASE_URL postgresql://127.0.0.1/unused
write_env_entry "${MALFORMED_EXTERNAL_ENV}" CORE_HTTP_ADDR 127.0.0.1:8080
write_env_entry "${MALFORMED_EXTERNAL_ENV}" CORE_HTTP_TLS_CERT_PATH ''
write_env_entry "${MALFORMED_EXTERNAL_ENV}" CORE_HTTP_TLS_KEY_PATH ''
write_env_entry "${MALFORMED_EXTERNAL_ENV}" CORE_GRPC_ADDR 127.0.0.1:50051
write_env_entry "${MALFORMED_EXTERNAL_ENV}" CORE_GRPC_TLS_CERT_PATH "${CERT_FILE}"
write_env_entry "${MALFORMED_EXTERNAL_ENV}" CORE_GRPC_TLS_KEY_PATH "${KEY_FILE}"
write_env_entry "${MALFORMED_EXTERNAL_ENV}" CORE_GRPC_TLS_CLIENT_CA_PATH "${AGENT_CA_FILE}"
append_core_internal_pki_env "${MALFORMED_EXTERNAL_ENV}"
run_preflight "${MALFORMED_EXTERNAL_ENV}" "${FAKE_CORE}"
[ "${PREFLIGHT_STATUS}" -ne 0 ] || {
  echo 'malformed external JWT public key unexpectedly passed preflight' >&2
  exit 1
}
assert_contains "${PREFLIGHT_OUTPUT}" '[INVALID] auth/admin: external_jwt public key'

grep -Fq -- '--upgrade' "${INSTALLER}"
grep -Fq -- '--security-preflight' "${INSTALLER}"
grep -Fq 'security_preflight_env "${INSTALL_DIR}/.env"' "${INSTALLER}"
grep -Fq 'prepare_package_security_probe_binaries' "${INSTALLER}"
grep -Fq '"${SECURITY_PROBE_CORE_BIN}" "${SECURITY_PROBE_AGENT_BIN}"' "${INSTALLER}"
grep -Fq 'select_readonly_security_probe_runtime_root' "${INSTALLER}"
grep -Fq 'SECURITY_PROBE_RUNTIME_ROOT="/opt"' "${INSTALLER}"
[ "$(grep -Ec '^[[:space:]]+run_readonly_check_with_external_flocks ' "${INSTALLER}")" -eq 2 ]
if grep -Fq 'READONLY_INSTALL_GLOBAL_LOCK_FD' "${INSTALLER}" \
  || grep -Fq 'READONLY_INSTALL_PATH_LOCK_FD' "${INSTALLER}"; then
  echo 'readonly diagnostics still expose inheritable lock descriptors' >&2
  exit 1
fi
if grep -Fq 'installed_core_bin="${INSTALL_DIR}/bin/media-core"' "${INSTALLER}"; then
  echo 'upgrade preflight still depends on a legacy installed media-core CLI' >&2
  exit 1
fi

# Package metadata is data, never shell code. Every field is allowlisted and
# the real executable refuses the ownership-emulation test escape hatch.
(
  set +x
  manifest_root="${TMP_DIR}/strict-package-manifest"
  manifest_marker="${TMP_DIR}/strict-package-manifest.executed"
  mkdir -p "${manifest_root}"
  cat >"${manifest_root}/package-manifest.env" <<'EOF'
BUNDLE_VERSION=v0.1.0
BUNDLE_VARIANT=cpu-only
BUNDLE_GPU_SUPPORT=false
BUNDLE_WORKER_SUPPORT=true
BUNDLE_POSTGRES_RUNTIME=true
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
  PACKAGE_ROOT="${manifest_root}"
  MANIFEST_FILE="${PACKAGE_ROOT}/package-manifest.env"
  load_manifest
  [ "${BUNDLE_VARIANT}" = cpu-only ]
  sed -i "1cBUNDLE_VERSION=\$(touch ${manifest_marker})" "${MANIFEST_FILE}"
  set +e
  (load_manifest) >/dev/null 2>&1
  manifest_code_status=$?
  set -e
  [ "${manifest_code_status}" -ne 0 ]
  [ ! -e "${manifest_marker}" ]
)
set +e
EMULATED_SECURITY_METADATA=1 bash "${INSTALLER}" --help \
  >"${TMP_DIR}/emulated-metadata-direct.log" 2>&1
emulated_metadata_direct_status=$?
set -e
[ "${emulated_metadata_direct_status}" -ne 0 ]
grep -Fq 'EMULATED_SECURITY_METADATA is test-only' \
  "${TMP_DIR}/emulated-metadata-direct.log"

# Topology and exact coverage are rejected before sha256sum can open a FIFO or
# follow a package-controlled path.
(
  set +x
  fifo_package="${TMP_DIR}/fifo-package"
  mkdir -p "${fifo_package}"
  mkfifo "${fifo_package}/payload"
  printf '%064d  payload\n' 0 >"${fifo_package}/SHA256SUMS"
  set +e
  timeout 5 bash -c '
    source "$1"
    PACKAGE_ROOT="$2"
    MEDIA_CORE_BINARY_PATH=payload
    MEDIA_AGENT_BINARY_PATH=""
    verify_package_checksums
  ' fifo-package-check "${FUNCTIONS_FILE}" "${fifo_package}" \
    >/dev/null 2>&1
  fifo_package_status=$?
  set -e
  [ "${fifo_package_status}" -ne 0 ]
  [ "${fifo_package_status}" -ne 124 ]
)

# A release upgrade must validate with the newly verified package binaries even
# when the old installed binaries predate the auth/identity probe CLIs.  The
# package itself may have been unpacked below a root-only staging directory, so
# the service-user probe executes a root-controlled copy instead of either the
# legacy installation or the package path directly.
(
  set +x
  probe_fixture="${TMP_DIR}/legacy-upgrade-probe"
  PACKAGE_ROOT="${probe_fixture}/root-only-package"
  INSTALL_DIR="${probe_fixture}/installed"
  SECURITY_PROBE_RUNTIME_ROOT="${probe_fixture}/run"
  MEDIA_CORE_BINARY_PATH=binaries/media-core-linux-amd64
  MEDIA_AGENT_BINARY_PATH=binaries/media-agent-linux-amd64
  mkdir -p "${PACKAGE_ROOT}/binaries" "${INSTALL_DIR}/bin" "${SECURITY_PROBE_RUNTIME_ROOT}"
  chmod 700 "${PACKAGE_ROOT}"
  cat >"${INSTALL_DIR}/bin/media-core" <<'EOF'
#!/usr/bin/env bash
echo 'unsupported auth subcommand' >&2
exit 97
EOF
  cat >"${INSTALL_DIR}/bin/media-agent" <<'EOF'
#!/usr/bin/env bash
echo 'unknown media-agent command' >&2
exit 98
EOF
  cat >"${PACKAGE_ROOT}/${MEDIA_CORE_BINARY_PATH}" <<'EOF'
#!/usr/bin/env bash
[ "$*" = 'auth check-config' ] || exit 41
EOF
  cat >"${PACKAGE_ROOT}/${MEDIA_AGENT_BINARY_PATH}" <<'EOF'
#!/usr/bin/env bash
[ "$*" = 'identity check --node-id 019f0000-0000-7000-8000-000000000001 --identity-dir /tmp/identity' ] || exit 42
EOF
  chmod 755 \
    "${INSTALL_DIR}/bin/media-core" "${INSTALL_DIR}/bin/media-agent" \
    "${PACKAGE_ROOT}/${MEDIA_CORE_BINARY_PATH}" \
    "${PACKAGE_ROOT}/${MEDIA_AGENT_BINARY_PATH}"
  (
    cd "${PACKAGE_ROOT}"
    sha256sum \
      "${MEDIA_CORE_BINARY_PATH}" "${MEDIA_AGENT_BINARY_PATH}" \
      >SHA256SUMS
  )

  verify_package_checksums
  [ -f "${VERIFIED_PACKAGE_CHECKSUM_SNAPSHOT:-}" ]
  [ ! -L "${VERIFIED_PACKAGE_CHECKSUM_SNAPSHOT}" ]
  [ "$(stat -c '%a' -- "${VERIFIED_PACKAGE_CHECKSUM_SNAPSHOT}")" = 600 ]
  [ "$(stat -c '%h' -- "${VERIFIED_PACKAGE_CHECKSUM_SNAPSHOT}")" = 1 ]
  verified_core_sha256="${VERIFIED_PACKAGE_CORE_SHA256}"
  checksum_saved="${probe_fixture}/SHA256SUMS.saved"
  checksum_poisoned="${probe_fixture}/SHA256SUMS.poisoned"
  cp "${PACKAGE_ROOT}/SHA256SUMS" "${checksum_saved}"
  awk -v core="${MEDIA_CORE_BINARY_PATH}" '
    $2 == core { print "0000000000000000000000000000000000000000000000000000000000000000  " $2; next }
    { print }
  ' "${checksum_saved}" >"${checksum_poisoned}"
  cp "${checksum_poisoned}" "${PACKAGE_ROOT}/SHA256SUMS"
  [ "$(verified_package_checksum_for_path "${MEDIA_CORE_BINARY_PATH}")" = "${verified_core_sha256}" ]
  cp "${checksum_saved}" "${PACKAGE_ROOT}/SHA256SUMS"
  prepare_package_security_probe_binaries
  [ "${SECURITY_PROBE_CORE_BIN}" != "${INSTALL_DIR}/bin/media-core" ]
  [ "${SECURITY_PROBE_AGENT_BIN}" != "${INSTALL_DIR}/bin/media-agent" ]
  [ "${SECURITY_PROBE_CORE_BIN}" != "${PACKAGE_ROOT}/${MEDIA_CORE_BINARY_PATH}" ]
  [ "${SECURITY_PROBE_AGENT_BIN}" != "${PACKAGE_ROOT}/${MEDIA_AGENT_BINARY_PATH}" ]
  "${SECURITY_PROBE_CORE_BIN}" auth check-config
  "${SECURITY_PROBE_AGENT_BIN}" identity check \
    --node-id 019f0000-0000-7000-8000-000000000001 \
    --identity-dir /tmp/identity
  [ ! -w "${SECURITY_PROBE_CORE_BIN}" ]
  [ ! -w "${SECURITY_PROBE_AGENT_BIN}" ]
  probe_dir="${SECURITY_PROBE_DIR}"
  : >"${probe_dir}/unexpected-entry"
  set +e
  cleanup_security_probe_binaries
  cleanup_with_foreign_entry_status=$?
  set -e
  [ "${cleanup_with_foreign_entry_status}" -ne 0 ]
  [ "${SECURITY_PROBE_DIR}" = "${probe_dir}" ]
  rm -f "${probe_dir}/unexpected-entry"
  cleanup_security_probe_binaries
  [ ! -e "${probe_dir}" ]

  printf '%s\n' malformed-checksum-record >>"${PACKAGE_ROOT}/SHA256SUMS"
  set +e
  (verify_package_checksums) >/dev/null 2>&1
  malformed_manifest_status=$?
  set -e
  [ "${malformed_manifest_status}" -ne 0 ]
  cp "${checksum_saved}" "${PACKAGE_ROOT}/SHA256SUMS"
  verify_package_checksums

  printf '%s\n' '# changed after verification' >>"${PACKAGE_ROOT}/SHA256SUMS"
  set +e
  (prepare_package_security_probe_binaries) >/dev/null 2>&1
  replaced_manifest_status=$?
  set -e
  [ "${replaced_manifest_status}" -ne 0 ]
  if find "${SECURITY_PROBE_RUNTIME_ROOT}" -mindepth 1 -maxdepth 1 -name 'probe.*' -print -quit | grep -q .; then
    echo 'manifest replacement left a staged security probe directory' >&2
    exit 1
  fi

  cp "${checksum_saved}" "${PACKAGE_ROOT}/SHA256SUMS"
  mkdir -p "${PACKAGE_ROOT}/real-binaries"
  cp "${PACKAGE_ROOT}/binaries/media-core-linux-amd64" \
    "${PACKAGE_ROOT}/real-binaries/media-core-linux-amd64"
  ln -s real-binaries "${PACKAGE_ROOT}/linked-binaries"
  MEDIA_CORE_BINARY_PATH=linked-binaries/media-core-linux-amd64
  (
    cd "${PACKAGE_ROOT}"
    sha256sum \
      "${MEDIA_CORE_BINARY_PATH}" "${MEDIA_AGENT_BINARY_PATH}" \
      >SHA256SUMS
  )
  set +e
  (verify_package_checksums) >/dev/null 2>&1
  intermediate_symlink_status=$?
  set -e
  [ "${intermediate_symlink_status}" -ne 0 ]
  if find "${SECURITY_PROBE_RUNTIME_ROOT}" -mindepth 1 -maxdepth 1 -name 'probe.*' -print -quit | grep -q .; then
    echo 'intermediate package symlink left a staged security probe directory' >&2
    exit 1
  fi
)
grep -Fq 'env_value_or_default "${existing_env_file}" "AUTH_MODE" "local_password"' "${INSTALLER}"
grep -Fq 'write_env_entry "${env_file}" CORE_HTTP_TLS_CERT_PATH' "${INSTALLER}"
grep -Fq 'write_env_entry "${env_file}" CORE_HTTP_TLS_KEY_PATH' "${INSTALLER}"
grep -Fq 'write_env_entry "${env_file}" CORE_GRPC_TLS_CERT_PATH' "${INSTALLER}"
grep -Fq 'write_env_entry "${env_file}" CORE_GRPC_TLS_KEY_PATH' "${INSTALLER}"
grep -Fq 'write_env_entry "${env_file}" CORE_GRPC_TLS_CLIENT_CA_PATH' "${INSTALLER}"
grep -Fq 'write_env_entry "${env_file}" AGENT_IDENTITY_DIR' "${INSTALLER}"
if grep -Eq 'write_env_entry "\$\{env_file\}" AGENT_(CERT|KEY|CA)_PATH' "${INSTALLER}"; then
  echo 'native installer still emits legacy static Agent certificate paths' >&2
  exit 1
fi
grep -Fq 'write_env_entry "${env_file}" AGENT_TLS_DOMAIN_NAME' "${INSTALLER}"
grep -Fq 'write_env_entry "${env_file}" AGENT_CORE_ENDPOINT "https://' "${INSTALLER}"
if grep -Fq 'write_env_entry "${env_file}" CORE_HTTP_ADDR "0.0.0.0:' "${INSTALLER}"; then
  echo 'native installer still hard-codes a public plaintext HTTP bind' >&2
  exit 1
fi
if grep -Fq 'write_env_entry "${env_file}" CORE_GRPC_ADDR "0.0.0.0:' "${INSTALLER}"; then
  echo 'native installer still hard-codes a public gRPC bind' >&2
  exit 1
fi
grep -Fq 'default_if_missing(values, "AUTH_MODE", "local_password");' "${CONFIG_TUI}"
grep -Fq '"${ROOT}/binaries/media-core-linux-amd64" --insecure-dev' "${VERIFY_SCRIPT}"
grep -Fq 'tests/native_security_contract_test.sh' "${NATIVE_WORKFLOW}"
native_validation_block="$(sed -n \
  '/- name: Validate native installer security contracts/,/- name: Set up Node.js/p' \
  "${NATIVE_WORKFLOW}")"
grep -Fq 'bash -n \' <<<"${native_validation_block}"
grep -Fq 'packaging/native/install.sh \' <<<"${native_validation_block}"
grep -Fq 'set +x' "${INSTALLER}"
grep -Fq '__ADMIN_HANDOFF_CONDITION__' "${SYSTEMD_TARGET_TEMPLATE}"
grep -Fq '__ADMIN_HANDOFF_CONDITION__' "${SYSTEMD_CORE_TEMPLATE}"
grep -Fq 'ConditionPathExists=!' "${INSTALLER}"
grep -Fq 'trap cleanup_installer_ephemeral_state EXIT' "${INSTALLER}"
grep -Fq "trap 'handle_admin_password_signal 129' HUP" "${INSTALLER}"
grep -Fq "trap 'handle_admin_password_signal 130' INT" "${INSTALLER}"
grep -Fq "trap 'handle_admin_password_signal 143' TERM" "${INSTALLER}"
grep -Fq '"${username}" "${password}" >/dev/tty' "${INSTALLER}"
if grep -Eq 'write_env_entry .*ADMIN_(PASSWORD|INITIAL)|log .*ADMIN_PASSWORD' "${INSTALLER}"; then
  echo 'administrator initial password can flow into .env or installer logs' >&2
  exit 1
fi
grep -Fq 'auth recover-bootstrap-admin --username "${ADMIN_USERNAME}"' "${INSTALLER}"
grep -Fq -- '--handoff-id "${handoff_id}" --expected-version "${expected_version}" --password-stdin' "${INSTALLER}"
grep -Fq 'require_cmd flock' "${INSTALLER}"
grep -Fq 'require_cmd install' "${INSTALLER}"
grep -Fq 'require_cmd realpath' "${INSTALLER}"
grep -Fq 'require_cmd stat' "${INSTALLER}"
grep -Fq 'require_cmd sync' "${INSTALLER}"
grep -Fq 'require_cmd systemd-run' "${INSTALLER}"
grep -Fq 'bootstrap_all_in_one_agent_identity_if_needed' "${INSTALLER}"
grep -Fq 'JWT_PUBLIC_KEY_SHA256=' "${INSTALLER}"
grep -Fq 'HANDOFF_ID=' "${INSTALLER}"
grep -Fq 'ADMIN_HANDOFF_STATE_ROOT="/var/lib/streamserver-native-installer"' "${INSTALLER}"
grep -Fq 'chown root:root "${INSTALL_DIR}"' "${INSTALLER}"
grep -Fq 'chown root:"${SERVICE_GROUP}" "${INSTALL_DIR}/.env"' "${INSTALLER}"
grep -Fq 'chmod 640 "${INSTALL_DIR}/.env"' "${INSTALLER}"
grep -Fq '__INSTALL_DIR__/data/zlm/config.ini' \
  "${REPO_ROOT}/packaging/native/templates/systemd/streamserver-zlm.service"
if grep -Fq 'chown "${SERVICE_USER}:${SERVICE_GROUP}" "${INSTALL_DIR}"' "${INSTALLER}"; then
  echo 'service account still owns the native installation control boundary' >&2
  exit 1
fi
if grep -Eq 'chown -R .*runtime/postgres' "${INSTALLER}"; then
  echo 'bundled PostgreSQL executable runtime is writable by the service account' >&2
  exit 1
fi
if grep -Fq '. "${env_file}"' "${INSTALLER}"; then
  echo 'pre-hardening administrator auth still sources the legacy environment' >&2
  exit 1
fi
[ "$(grep -Fc '  fix_permissions' "${INSTALLER}")" -ge 2 ] || {
  echo 'installer does not reassert .env ownership after the optional configuration TUI' >&2
  exit 1
}
if grep -Fq 'STREAMSERVER_INSTALLER_STATE_ROOT' "${INSTALLER}"; then
  echo 'production administrator handoff state root is still environment-overridable' >&2
  exit 1
fi

tui_line="$(grep -n '^  run_streamserver_config_tui_with_handoff_guard$' "${INSTALLER}" | cut -d: -f1)"
preflight_line="$(grep -n '^  prepare_production_security_state$' "${INSTALLER}" | cut -d: -f1)"
[ -n "${tui_line}" ] && [ -n "${preflight_line}" ] && [ "${preflight_line}" -gt "${tui_line}" ] || {
  echo 'production security preflight must run after the optional TUI save' >&2
  exit 1
}
initialize_line="$(grep -n '^  initialize_postgres_if_needed$' "${INSTALLER}" | tail -n 1 | cut -d: -f1)"
units_line="$(grep -n '^  install_systemd_units$' "${INSTALLER}" | tail -n 1 | cut -d: -f1)"
[ -n "${initialize_line}" ] && [ -n "${units_line}" ] \
  && [ "${initialize_line}" -lt "${units_line}" ] \
  && [ "${units_line}" -lt "${preflight_line}" ] || {
  echo 'all-in-one bootstrap requires PostgreSQL and systemd units before production preflight' >&2
  exit 1
}
main_definition_line="$(grep -n '^main() {' "${INSTALLER}" | cut -d: -f1)"
main_invocation_line="$(grep -n '^main "\$@"$' "${INSTALLER}" | cut -d: -f1)"
if sed -n "${main_definition_line},${main_invocation_line}p" "${INSTALLER}" \
  | grep -q '^  run_agent_enrollment_if_needed$'; then
  echo 'main still attempts Agent enrollment before the local bootstrap Core can exist' >&2
  exit 1
fi
credential_line="$(grep -n '^  show_initial_admin_credentials_if_needed$' "${INSTALLER}" | cut -d: -f1)"
start_line="$(grep -n '^  start_services_if_requested$' "${INSTALLER}" | tail -n 1 | cut -d: -f1)"
[ -n "${preflight_line}" ] && [ -n "${credential_line}" ] && [ -n "${start_line}" ] \
  && [ "${credential_line}" -gt "${preflight_line}" ] && [ "${credential_line}" -lt "${start_line}" ] || {
  echo 'initial administrator password must be delivered after strict preflight and before service start' >&2
  exit 1
}
check_only_line="$(grep -n '^  if \[ "${CHECK_ONLY}" -eq 1 \]; then$' "${INSTALLER}" | cut -d: -f1)"
locked_reexec_line="$(grep -n '^  run_install_with_external_flocks$' "${INSTALLER}" | cut -d: -f1)"
[ -n "${check_only_line}" ] && [ -n "${locked_reexec_line}" ] \
  && [ "${check_only_line}" -lt "${locked_reexec_line}" ] || {
  echo 'check-only must exit before any initial administrator password can be generated' >&2
  exit 1
}

echo 'native security contract tests passed'
