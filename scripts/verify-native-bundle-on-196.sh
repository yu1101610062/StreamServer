#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
BUNDLE_PATH=""
HOST="172.17.13.196"
SSH_TARGET=""
SSH_PORT="22"
SSH_PASSWORD=""
ACCESS_FILE=""
REMOTE_DIR=""
OUTPUT_DIR="${ROOT_DIR}/dist"
UPLOAD_METHOD="scp"
HTTP_HOST=""
HTTP_PORT=""
HTTP_SERVER_PID=""

log() {
  printf '[verify-196] %s\n' "$*"
}

fail() {
  printf '[verify-196] ERROR: %s\n' "$*" >&2
  exit 1
}

usage() {
  cat <<EOF
用法:
  $(basename "$0") --bundle PATH [--host 172.17.13.196] [--ssh-target USER@HOST]
                 [--port 22] [--access-file PATH] [--upload-method scp|http]
                 [--http-host HOST] [--http-port PORT]
                 [--remote-dir DIR] [--output-dir DIR]

说明:
  将 native 离线包上传到 196，在 196 上验证包内业务二进制和第三方运行时组件。
  验证过程不依赖 Docker；若远端缺少 Docker 也必须能完成。
  若 --access-file 中包含地址/端口/用户/密码字段，脚本会用 expect 进行密码登录；
  若文件说明使用本地 HTTP 上传，默认会启动本地 HTTP 服务并让 196 远端下载。
EOF
}

parse_args() {
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --bundle)
        [ "$#" -ge 2 ] || fail "--bundle 需要参数"
        BUNDLE_PATH="$2"
        shift 2
        ;;
      --host)
        [ "$#" -ge 2 ] || fail "--host 需要参数"
        HOST="$2"
        shift 2
        ;;
      --ssh-target)
        [ "$#" -ge 2 ] || fail "--ssh-target 需要参数"
        SSH_TARGET="$2"
        shift 2
        ;;
      --port)
        [ "$#" -ge 2 ] || fail "--port 需要参数"
        SSH_PORT="$2"
        shift 2
        ;;
      --access-file)
        [ "$#" -ge 2 ] || fail "--access-file 需要参数"
        ACCESS_FILE="$2"
        shift 2
        ;;
      --upload-method)
        [ "$#" -ge 2 ] || fail "--upload-method 需要参数"
        UPLOAD_METHOD="$2"
        shift 2
        ;;
      --http-host)
        [ "$#" -ge 2 ] || fail "--http-host 需要参数"
        HTTP_HOST="$2"
        shift 2
        ;;
      --http-port)
        [ "$#" -ge 2 ] || fail "--http-port 需要参数"
        HTTP_PORT="$2"
        shift 2
        ;;
      --remote-dir)
        [ "$#" -ge 2 ] || fail "--remote-dir 需要参数"
        REMOTE_DIR="$2"
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
}

require_cmd() {
  command -v "$1" >/dev/null 2>&1 || fail "缺少命令: $1"
}

shell_quote() {
  printf "'%s'" "$(printf '%s' "$1" | sed "s/'/'\\\\''/g")"
}

access_value() {
  local key="$1"
  sed -n "s/^${key}:[[:space:]]*//p" "${ACCESS_FILE}" | tail -n 1
}

parse_access_file() {
  [ -z "${ACCESS_FILE}" ] && return 0
  [ -f "${ACCESS_FILE}" ] || fail "access file 不存在: ${ACCESS_FILE}"

  local access_host access_port access_user access_password
  access_host="$(access_value "地址")"
  access_port="$(access_value "端口")"
  access_user="$(access_value "用户")"
  access_password="$(access_value "密码")"

  [ -n "${access_host}" ] && HOST="${access_host}"
  [ -n "${access_port}" ] && SSH_PORT="${access_port}"
  if [ -n "${access_user}" ] && [ -z "${SSH_TARGET}" ]; then
    SSH_TARGET="${access_user}@${HOST}"
  fi
  [ -n "${access_password}" ] && SSH_PASSWORD="${access_password}"
  if grep -qi 'http' "${ACCESS_FILE}"; then
    UPLOAD_METHOD="http"
  fi
}

detect_http_host() {
  [ -n "${HTTP_HOST}" ] && return 0
  if command -v ipconfig >/dev/null 2>&1; then
    HTTP_HOST="$(ipconfig getifaddr en0 2>/dev/null || true)"
    [ -n "${HTTP_HOST}" ] || HTTP_HOST="$(ipconfig getifaddr en1 2>/dev/null || true)"
  fi
  if [ -z "${HTTP_HOST}" ]; then
    HTTP_HOST="$(hostname -I 2>/dev/null | awk '{print $1}' || true)"
  fi
  [ -n "${HTTP_HOST}" ] || fail "无法自动判断本机 HTTP 地址，请传 --http-host"
}

detect_http_port() {
  [ -n "${HTTP_PORT}" ] && return 0
  require_cmd python3
  HTTP_PORT="$(python3 - <<'PY'
import socket
s = socket.socket()
s.bind(("", 0))
print(s.getsockname()[1])
s.close()
PY
)"
}

start_http_server() {
  require_cmd python3
  detect_http_host
  detect_http_port
  local bundle_dir
  bundle_dir="$(cd "$(dirname "${BUNDLE_PATH}")" && pwd)"
  log "启动本地 HTTP 服务: http://${HTTP_HOST}:${HTTP_PORT}/"
  (
    cd "${bundle_dir}"
    python3 -m http.server "${HTTP_PORT}" --bind 0.0.0.0 >/tmp/streamserver-native-verify-http.log 2>&1
  ) &
  HTTP_SERVER_PID="$!"
  sleep 1
  if ! kill -0 "${HTTP_SERVER_PID}" >/dev/null 2>&1; then
    fail "本地 HTTP 服务启动失败，日志: /tmp/streamserver-native-verify-http.log"
  fi
}

stop_http_server() {
  if [ -n "${HTTP_SERVER_PID}" ] && kill -0 "${HTTP_SERVER_PID}" >/dev/null 2>&1; then
    kill "${HTTP_SERVER_PID}" >/dev/null 2>&1 || true
    wait "${HTTP_SERVER_PID}" 2>/dev/null || true
  fi
}

ssh_expect() {
  local command="$1"
  STREAMSERVER_SSH_PASSWORD="${SSH_PASSWORD}" \
  STREAMSERVER_SSH_TARGET="${SSH_TARGET}" \
  STREAMSERVER_SSH_PORT="${SSH_PORT}" \
  STREAMSERVER_SSH_COMMAND="${command}" \
    expect -c '
set timeout -1
set target $env(STREAMSERVER_SSH_TARGET)
set port $env(STREAMSERVER_SSH_PORT)
set command $env(STREAMSERVER_SSH_COMMAND)
set pass $env(STREAMSERVER_SSH_PASSWORD)
log_user 0
spawn ssh -p $port -o StrictHostKeyChecking=accept-new -o PubkeyAuthentication=no $target $command
expect {
  -re "(?i)yes/no|fingerprint" { send -- "yes\r"; exp_continue }
  -re "(?i)password:" { send -- "$pass\r"; log_user 1; exp_continue }
  eof
}
set result [wait]
exit [lindex $result 3]
'
}

ssh_expect_stream() {
  local command="$1"
  STREAMSERVER_SSH_PASSWORD="${SSH_PASSWORD}" \
  STREAMSERVER_SSH_TARGET="${SSH_TARGET}" \
  STREAMSERVER_SSH_PORT="${SSH_PORT}" \
  STREAMSERVER_SSH_COMMAND="${command}" \
    expect -c '
set timeout -1
set target $env(STREAMSERVER_SSH_TARGET)
set port $env(STREAMSERVER_SSH_PORT)
set command $env(STREAMSERVER_SSH_COMMAND)
set pass $env(STREAMSERVER_SSH_PASSWORD)
set payload [read stdin]
log_user 0
spawn ssh -p $port -o StrictHostKeyChecking=accept-new -o PubkeyAuthentication=no $target $command
expect {
  -re "(?i)yes/no|fingerprint" { send -- "yes\r"; exp_continue }
  -re "(?i)password:" { send -- "$pass\r"; log_user 1 }
}
send -- $payload
send -- "\004"
expect eof
set result [wait]
exit [lindex $result 3]
'
}

ssh_run() {
  local command="$1"
  if [ -n "${SSH_PASSWORD}" ]; then
    ssh_expect "${command}"
  else
    ssh -p "${SSH_PORT}" "${SSH_TARGET}" "${command}"
  fi
}

ssh_stream() {
  local command="$1"
  if [ -n "${SSH_PASSWORD}" ]; then
    ssh_expect_stream "${command}"
  else
    ssh -p "${SSH_PORT}" "${SSH_TARGET}" "${command}"
  fi
}

scp_upload() {
  local local_path="$1"
  local remote_path="$2"
  if [ -n "${SSH_PASSWORD}" ]; then
    STREAMSERVER_SSH_PASSWORD="${SSH_PASSWORD}" \
    STREAMSERVER_SCP_LOCAL_PATH="${local_path}" \
    STREAMSERVER_SCP_TARGET="${SSH_TARGET}" \
    STREAMSERVER_SCP_PORT="${SSH_PORT}" \
    STREAMSERVER_SCP_REMOTE_PATH="${remote_path}" \
      expect -c '
set timeout -1
set local_path $env(STREAMSERVER_SCP_LOCAL_PATH)
set target $env(STREAMSERVER_SCP_TARGET)
set port $env(STREAMSERVER_SCP_PORT)
set remote_path $env(STREAMSERVER_SCP_REMOTE_PATH)
set pass $env(STREAMSERVER_SSH_PASSWORD)
log_user 0
spawn scp -P $port -o StrictHostKeyChecking=accept-new -o PubkeyAuthentication=no $local_path ${target}:${remote_path}
expect {
  -re "(?i)yes/no|fingerprint" { send -- "yes\r"; exp_continue }
  -re "(?i)password:" { send -- "$pass\r"; log_user 1; exp_continue }
  eof
}
set result [wait]
exit [lindex $result 3]
'
  else
    scp -P "${SSH_PORT}" "${local_path}" "${SSH_TARGET}:${remote_path}" >/dev/null
  fi
}

main() {
  parse_args "$@"
  parse_access_file
  [ -n "${BUNDLE_PATH}" ] || fail "必须传入 --bundle"
  [ -f "${BUNDLE_PATH}" ] || fail "bundle 不存在: ${BUNDLE_PATH}"
  require_cmd ssh
  if [ -n "${SSH_PASSWORD}" ]; then
    require_cmd expect
  fi
  case "${UPLOAD_METHOD}" in
    scp)
      require_cmd scp
      ;;
    http)
      require_cmd curl
      ;;
    *)
      fail "未知上传方式: ${UPLOAD_METHOD}"
      ;;
  esac

  if [ -z "${SSH_TARGET}" ]; then
    SSH_TARGET="${HOST}"
  fi
  if [ -z "${REMOTE_DIR}" ]; then
    REMOTE_DIR="/tmp/streamserver-native-verify-$(date '+%Y%m%d-%H%M%S')"
  fi
  mkdir -p "${OUTPUT_DIR}"

  local bundle_name report_name remote_bundle remote_report status
  bundle_name="$(basename "${BUNDLE_PATH}")"
  report_name="native-verification-196-$(date '+%Y%m%d-%H%M%S').md"
  remote_bundle="${REMOTE_DIR}/${bundle_name}"
  remote_report="${REMOTE_DIR}/${report_name}"

  trap stop_http_server EXIT

  log "确认 196 SSH 目标: ${SSH_TARGET}:${SSH_PORT}"
  ssh_run "set -e; hostname; uname -a; command -v systemctl >/dev/null && echo systemd_tool=present || echo systemd_tool=missing; hostname -I 2>/dev/null || true"

  log "准备 native 包到 ${SSH_TARGET}:${remote_bundle}"
  ssh_run "mkdir -p $(shell_quote "${REMOTE_DIR}")"
  if [ "${UPLOAD_METHOD}" = "http" ]; then
    start_http_server
    local bundle_url
    bundle_url="http://${HTTP_HOST}:${HTTP_PORT}/${bundle_name}"
    log "让 196 从本机 HTTP 下载 native 包"
    ssh_run "curl -fL --retry 3 --connect-timeout 10 -o $(shell_quote "${remote_bundle}") $(shell_quote "${bundle_url}")"
  else
    log "通过 scp 上传 native 包"
    scp_upload "${BUNDLE_PATH}" "${remote_bundle}"
  fi

  local remote_script_name remote_script_local remote_script
  remote_script_name="${report_name%.md}.remote.sh"
  remote_script_local="$(cd "$(dirname "${BUNDLE_PATH}")" && pwd)/${remote_script_name}"
  remote_script="${REMOTE_DIR}/${remote_script_name}"
  cat >"${remote_script_local}" <<'REMOTE'
set -euo pipefail

BUNDLE="${STREAMSERVER_VERIFY_BUNDLE}"
WORK_DIR="${STREAMSERVER_VERIFY_DIR}"
REPORT="${STREAMSERVER_VERIFY_REPORT}"
FAILURES=0

mkdir -p "${WORK_DIR}"
: >"${REPORT}"

append() {
  printf '%s\n' "$*" >>"${REPORT}"
}

section() {
  append ""
  append "## $*"
}

record_failure() {
  append "[FAIL] $*"
  FAILURES=$((FAILURES + 1))
}

record_ok() {
  append "[OK] $*"
}

run_capture() {
  local label="$1"
  shift
  append ""
  append "### ${label}"
  append "\`\`\`"
  if "$@" >>"${REPORT}" 2>&1; then
    append "\`\`\`"
    record_ok "${label}"
  else
    append "\`\`\`"
    record_failure "${label}"
  fi
}

run_shell() {
  local label="$1"
  shift
  append ""
  append "### ${label}"
  append "\`\`\`"
  if bash -lc "$*" >>"${REPORT}" 2>&1; then
    append "\`\`\`"
    record_ok "${label}"
  else
    append "\`\`\`"
    record_failure "${label}"
  fi
}

append "# StreamServer Native 196 Verification"
append ""
append "- verified_at: $(date -u '+%Y-%m-%dT%H:%M:%SZ')"
append "- host: $(hostname)"
append "- host_ips: $(hostname -I 2>/dev/null || true)"
append "- uname: $(uname -a)"
append "- bundle: $(basename "${BUNDLE}")"
append "- docker_present: $(command -v docker >/dev/null 2>&1 && echo yes || echo no)"
append "- docker_required: no"

section "Host Prerequisites"
run_capture "systemctl present" command -v systemctl
run_capture "sha256sum present" command -v sha256sum
run_capture "file present" command -v file
run_capture "ldd present" command -v ldd
run_capture "curl present" command -v curl
run_capture "openssl present" command -v openssl
if command -v docker >/dev/null 2>&1; then
  append "[INFO] Docker exists on host, but verification will not call it."
else
  append "[INFO] Docker is absent; this is acceptable for native runtime verification."
fi

section "Extract Bundle"
top_dir="$(tar -tzf "${BUNDLE}" | sed -n '1s#/.*##p')"
[ -n "${top_dir}" ] || { record_failure "parse bundle top dir"; exit 1; }
rm -rf "${WORK_DIR}/${top_dir}"
tar -xzf "${BUNDLE}" -C "${WORK_DIR}"
ROOT="${WORK_DIR}/${top_dir}"
append "- extracted_root: ${ROOT}"

section "Package Shape"
run_shell "sha256sum -c SHA256SUMS" "cd '${ROOT}' && sha256sum -c SHA256SUMS"
if find "${ROOT}" \( -path '*/images/*' -o -name compose.yml -o -name docker-compose.yml -o -name streamserver-compose \) | grep -q .; then
  record_failure "native bundle contains Docker or Compose runtime assets"
else
  record_ok "no Docker or Compose runtime assets"
fi
if [ -d "${ROOT}/tools/docker" ]; then
  record_failure "native bundle contains tools/docker"
else
  record_ok "no tools/docker directory"
fi

check_static_binary() {
  local label="$1"
  local path="$2"
  [ -x "${path}" ] || { record_failure "${label} executable missing: ${path}"; return; }
  run_capture "${label} file" file "${path}"
  append ""
  append "### ${label} ldd"
  append "\`\`\`"
  if ldd_output="$(ldd "${path}" 2>&1)"; then
    printf '%s\n' "${ldd_output}" >>"${REPORT}"
    append "\`\`\`"
    if printf '%s' "${ldd_output}" | grep -Eiq 'not a dynamic executable|statically linked'; then
      record_ok "${label} is static"
    else
      record_failure "${label} is dynamically linked"
    fi
  else
    printf '%s\n' "${ldd_output}" >>"${REPORT}"
    append "\`\`\`"
    if printf '%s' "${ldd_output}" | grep -Eiq 'not a dynamic executable|statically linked'; then
      record_ok "${label} is static"
    else
      record_failure "${label} static ldd output not recognized"
    fi
  fi
}

runtime_loader() {
  local lib_dir="$1"
  if [ -x "${lib_dir}/ld-linux-x86-64.so.2" ]; then
    printf '%s\n' "${lib_dir}/ld-linux-x86-64.so.2"
  fi
}

runtime_exec() {
  local path="$1"
  local lib_dir="$2"
  shift 2
  local loader
  loader="$(runtime_loader "${lib_dir}")"
  if [ -n "${loader}" ]; then
    "${loader}" --library-path "${lib_dir}" "${path}" "$@"
  else
    env LD_LIBRARY_PATH="${lib_dir}" "${path}" "$@"
  fi
}

runtime_list_deps() {
  local path="$1"
  local lib_dir="$2"
  local loader
  loader="$(runtime_loader "${lib_dir}")"
  if [ -n "${loader}" ]; then
    "${loader}" --library-path "${lib_dir}" --list "${path}"
  else
    env LD_LIBRARY_PATH="${lib_dir}" ldd "${path}"
  fi
}

run_runtime_capture() {
  local label="$1"
  local path="$2"
  local lib_dir="$3"
  shift 3
  append ""
  append "### ${label}"
  append "\`\`\`"
  if runtime_exec "${path}" "${lib_dir}" "$@" >>"${REPORT}" 2>&1; then
    append "\`\`\`"
    record_ok "${label}"
  else
    append "\`\`\`"
    record_failure "${label}"
  fi
}

check_runtime_binary() {
  local label="$1"
  local path="$2"
  local lib_dir="$3"
  shift 3
  [ -x "${path}" ] || { record_failure "${label} executable missing: ${path}"; return; }
  run_capture "${label} file" file "${path}"
  append ""
  append "### ${label} ldd"
  append "\`\`\`"
  if runtime_list_deps "${path}" "${lib_dir}" >>"${REPORT}" 2>&1; then
    append "\`\`\`"
    if runtime_list_deps "${path}" "${lib_dir}" 2>&1 | grep -q 'not found'; then
      record_failure "${label} has unresolved dynamic dependencies"
    else
      record_ok "${label} dynamic dependencies resolved"
    fi
  else
    append "\`\`\`"
    record_failure "${label} ldd failed"
  fi
  run_runtime_capture "${label} version" "${path}" "${lib_dir}" "$@"
}

section "Business Binaries"
check_static_binary "media-core" "${ROOT}/binaries/media-core-linux-amd64"
check_static_binary "media-agent" "${ROOT}/binaries/media-agent-linux-amd64"
check_static_binary "streamserver-config" "${ROOT}/binaries/streamserver-config-linux-amd64"
run_capture "media-core auth help" "${ROOT}/binaries/media-core-linux-amd64" auth --help

section "FFmpeg Runtime"
if [ -x "${ROOT}/runtime/ffmpeg/cpu/bin/ffmpeg" ]; then
  check_runtime_binary "ffmpeg cpu" "${ROOT}/runtime/ffmpeg/cpu/bin/ffmpeg" "${ROOT}/runtime/ffmpeg/cpu/lib" -version
  check_runtime_binary "ffprobe cpu" "${ROOT}/runtime/ffmpeg/cpu/bin/ffprobe" "${ROOT}/runtime/ffmpeg/cpu/lib" -version
  run_shell "ffmpeg cpu HEVC to FLV smoke" "tmp=\$(mktemp -d); '${ROOT}/runtime/ffmpeg/cpu/lib/ld-linux-x86-64.so.2' --library-path '${ROOT}/runtime/ffmpeg/cpu/lib' '${ROOT}/runtime/ffmpeg/cpu/bin/ffmpeg' -hide_banner -f lavfi -i testsrc=size=128x72:rate=1 -t 1 -c:v libx265 -an -f flv -y \"\$tmp/hevc-test.flv\" && test -s \"\$tmp/hevc-test.flv\""
fi
if [ -x "${ROOT}/runtime/ffmpeg/gpu/bin/ffmpeg" ]; then
  check_runtime_binary "ffmpeg gpu" "${ROOT}/runtime/ffmpeg/gpu/bin/ffmpeg" "${ROOT}/runtime/ffmpeg/gpu/lib" -version
  check_runtime_binary "ffprobe gpu" "${ROOT}/runtime/ffmpeg/gpu/bin/ffprobe" "${ROOT}/runtime/ffmpeg/gpu/lib" -version
  run_shell "ffmpeg gpu encoder check" "'${ROOT}/runtime/ffmpeg/gpu/lib/ld-linux-x86-64.so.2' --library-path '${ROOT}/runtime/ffmpeg/gpu/lib' '${ROOT}/runtime/ffmpeg/gpu/bin/ffmpeg' -hide_banner -encoders 2>/dev/null | grep -q h264_nvenc && '${ROOT}/runtime/ffmpeg/gpu/lib/ld-linux-x86-64.so.2' --library-path '${ROOT}/runtime/ffmpeg/gpu/lib' '${ROOT}/runtime/ffmpeg/gpu/bin/ffmpeg' -hide_banner -encoders 2>/dev/null | grep -q hevc_nvenc"
fi

section "ZLMediaKit Runtime"
if [ -x "${ROOT}/runtime/zlm/MediaServer" ]; then
  if [ -d "${ROOT}/runtime/zlm/python" ]; then
    export PYTHONHOME="${ROOT}/runtime/zlm/python"
  fi
  [ -f "${ROOT}/runtime/zlm/default.pem" ] && record_ok "default.pem exists" || record_failure "default.pem missing"
  check_runtime_binary "MediaServer" "${ROOT}/runtime/zlm/MediaServer" "${ROOT}/runtime/zlm/lib" -v
  run_shell "ZLM statistic smoke" "
    tmp=\$(mktemp -d)
    port=\$((23000 + RANDOM % 10000))
    export ZLM_API_SECRET=verify-secret
    export ZLM_HOOK_SHARED_SECRET=verify-secret
    export ZLM_SERVER_ID=verify-196
    export ZLM_HOOK_BASE=http://127.0.0.1:9/hooks
    export ZLM_API_ALLOW_IP_RANGE='::1,127.0.0.1'
    export ZLM_HTTP_PORT=\${port}
    export ZLM_HTTPS_PORT=0
    export ZLM_RTMP_PORT=0
    export ZLM_RTMPS_PORT=0
    export ZLM_RTSP_PORT=0
    export ZLM_RTSPS_PORT=0
    export ZLM_RTP_PROXY_PORT=0
    export ZLM_RTP_PROXY_PORT_RANGE=0-0
    export ZLM_RTC_SIGNALING_PORT=0
    export ZLM_RTC_SIGNALING_SSL_PORT=0
    export ZLM_RTC_ICE_PORT=0
    export ZLM_RTC_ICE_TCP_PORT=0
    export ZLM_RTC_PORT=0
    export ZLM_RTC_TCP_PORT=0
    export ZLM_RTC_PORT_RANGE=0-0
    export ZLM_SRT_PORT=0
    export ZLM_SHELL_PORT=0
    export ZLM_ONVIF_PORT=0
    export ZLM_WWW_ROOT=\${tmp}/www
    export ZLM_RECORD_ROOT=\${tmp}/www/record
    export ZLM_SNAP_ROOT=\${tmp}/www/snap
    export ZLM_DEFAULT_PEM='${ROOT}/runtime/zlm/default.pem'
    mkdir -p \"\${ZLM_WWW_ROOT}\" \"\${ZLM_RECORD_ROOT}\" \"\${ZLM_SNAP_ROOT}\"
    '${ROOT}/templates/common/zlm.render-config.sh' '${ROOT}/templates/common/zlm.config.ini.template' \"\${tmp}/zlm.ini\"
    if [ -d '${ROOT}/runtime/zlm/python' ]; then
      export PYTHONHOME='${ROOT}/runtime/zlm/python'
    fi
    (cd '${ROOT}/runtime/zlm' && '${ROOT}/runtime/zlm/lib/ld-linux-x86-64.so.2' --library-path '${ROOT}/runtime/zlm/lib' '${ROOT}/runtime/zlm/MediaServer' -s '${ROOT}/runtime/zlm/default.pem' -c \"\${tmp}/zlm.ini\" -l 0 >\"\${tmp}/zlm.log\" 2>&1 & echo \$! >\"\${tmp}/zlm.pid\")
    pid=\$(cat \"\${tmp}/zlm.pid\")
    trap 'kill \${pid} >/dev/null 2>&1 || true' EXIT
    for i in \$(seq 1 20); do
      curl -fsS \"http://127.0.0.1:\${port}/index/api/getStatistic?secret=verify-secret\" >/dev/null && exit 0
      sleep 1
    done
    cat \"\${tmp}/zlm.log\"
    exit 1
  "
fi

section "PostgreSQL Runtime"
if [ -x "${ROOT}/runtime/postgres/bin/postgres" ]; then
  for command_name in postgres initdb pg_ctl pg_isready psql; do
    check_runtime_binary "postgres ${command_name}" "${ROOT}/runtime/postgres/bin/${command_name}" "${ROOT}/runtime/postgres/lib" --version
  done
  append ""
  append "### PostgreSQL init/start/query/extensions smoke"
  append "\`\`\`"
  postgres_smoke() {
    local tmp port pgroot pgwrap command_name runner_prefix pid
    local pg_pkglib_dir pg_share_dir pg_library_path extension_manifest loader
    local next_port_value next_port_file started_pid
    tmp="$(mktemp -d)"
    next_port_value=$((25432 + RANDOM % 10000))
    next_port_file="${tmp}/next-port"
    printf '%s\n' "${next_port_value}" >"${next_port_file}"
    POSTGRES_SMOKE_PIDS=""
    POSTGRES_SMOKE_DATA_DIRS=""
    pgroot="${ROOT}/runtime/postgres"
    pgwrap="${tmp}/pgwrap"
    mkdir -p "${pgwrap}"

    postgres_command_binary() {
      local command="$1"
      local candidate
      for candidate in "${pgroot}"/lib/postgresql/*/bin/"${command}"; do
        [ -x "${candidate}" ] || continue
        printf '%s\n' "${candidate}"
        return 0
      done
      printf '%s\n' "${pgroot}/bin/${command}"
    }

    postgres_pkglib_dir() {
      local candidate
      for candidate in "${pgroot}"/lib/postgresql/*/lib; do
        [ -d "${candidate}" ] || continue
        printf '%s\n' "${candidate}"
        return 0
      done
      printf '%s\n' "${pgroot}/lib/postgresql"
    }

    postgres_share_dir() {
      local candidate
      for candidate in "${pgroot}"/share/postgresql/* "${pgroot}"/share/*; do
        [ -d "${candidate}/extension" ] || continue
        [ -f "${candidate}/postgres.bki" ] || continue
        printf '%s\n' "${candidate}"
        return 0
      done
      return 1
    }

    postgres_command_names() {
      {
        find "${pgroot}/bin" -maxdepth 1 -type f -perm -111 -print 2>/dev/null || true
        find "${pgroot}"/lib/postgresql/*/bin -maxdepth 1 -type f -perm -111 -print 2>/dev/null || true
      } | sed 's#.*/##' | LC_ALL=C sort -u
    }

    next_port() {
      next_port_value="$(cat "${next_port_file}")"
      next_port_value=$((next_port_value + 1))
      printf '%s\n' "${next_port_value}" >"${next_port_file}"
      printf '%s\n' "${next_port_value}"
    }

    run_pg() {
      if [ "$(id -u)" -eq 0 ]; then
        runuser -u nobody -- "$@"
      else
        "$@"
      fi
    }

    init_cluster() {
      local data_dir="$1"
      mkdir -p "$(dirname "${data_dir}")"
      if [ "$(id -u)" -eq 0 ]; then
        chown -R nobody "$(dirname "${data_dir}")"
      fi
      run_pg "${pgwrap}/initdb" -D "${data_dir}" -U postgres -L "${pg_share_dir}" --encoding=UTF8 --locale=C
    }

    wait_ready() {
      local check_port="$1"
      local ready=0
      for _ in $(seq 1 60); do
        if "${pgwrap}/pg_isready" -h 127.0.0.1 -p "${check_port}" -U postgres >/dev/null 2>&1; then
          ready=1
          break
        fi
        sleep 1
      done
      [ "${ready}" -eq 1 ]
    }

    start_cluster() {
      local data_dir="$1"
      local start_port="$2"
      local log_file="$3"
      shift 3
      run_pg "${pgwrap}/postgres" -D "${data_dir}" -p "${start_port}" -k "${tmp}" -c "dynamic_library_path=${pg_pkglib_dir}" "$@" >"${log_file}" 2>&1 &
      pid=$!
      POSTGRES_SMOKE_PIDS="${POSTGRES_SMOKE_PIDS} ${pid}"
      POSTGRES_SMOKE_DATA_DIRS="${POSTGRES_SMOKE_DATA_DIRS} ${data_dir}"
      wait_ready "${start_port}" || {
        cat "${log_file}"
        return 1
      }
      started_pid="${pid}"
    }

    stop_pid() {
      local stop_pid_value="$1"
      kill "${stop_pid_value}" >/dev/null 2>&1 || true
      wait "${stop_pid_value}" 2>/dev/null || true
    }

    cleanup_postgres_smoke() {
      local cleanup_pid cleanup_data_dir cleanup_postmaster_pid
      for cleanup_data_dir in ${POSTGRES_SMOKE_DATA_DIRS:-}; do
        if [ -f "${cleanup_data_dir}/postmaster.pid" ]; then
          cleanup_postmaster_pid="$(sed -n '1p' "${cleanup_data_dir}/postmaster.pid" 2>/dev/null || true)"
          case "${cleanup_postmaster_pid}" in
            ''|*[!0-9]*) ;;
            *) kill "${cleanup_postmaster_pid}" >/dev/null 2>&1 || true ;;
          esac
        fi
      done
      for cleanup_pid in ${POSTGRES_SMOKE_PIDS:-}; do
        kill "${cleanup_pid}" >/dev/null 2>&1 || true
        wait "${cleanup_pid}" 2>/dev/null || true
      done
    }

    psql_on() {
      local check_port="$1"
      shift
      "${pgwrap}/psql" -h 127.0.0.1 -p "${check_port}" -U postgres -d postgres -v ON_ERROR_STOP=1 "$@"
    }

    pg_pkglib_dir="$(postgres_pkglib_dir)"
    pg_share_dir="$(postgres_share_dir)" || {
      echo "PostgreSQL share directory with postgres.bki and extension controls not found"
      return 1
    }
    pg_library_path="${pgroot}/lib:${pg_pkglib_dir}"
    extension_manifest="${pgroot}/postgres-extension-manifest.tsv"
    loader="${pgroot}/lib/ld-linux-x86-64.so.2"
    [ -s "${extension_manifest}" ] || {
      echo "PostgreSQL extension manifest missing: ${extension_manifest}"
      return 1
    }
    echo "pkglib_dir=${pg_pkglib_dir}"
    echo "share_dir=${pg_share_dir}"
    echo "extension_manifest=${extension_manifest}"
    echo "extension_manifest_count=$(wc -l <"${extension_manifest}")"

    command_count="$(postgres_command_names | wc -l)"
    echo "postgres_command_count=${command_count}"
    for required_command in postgres initdb pg_ctl pg_isready psql pg_dump pg_restore pg_dumpall pg_basebackup createdb createuser dropdb dropuser pg_waldump pg_verifybackup pg_recvlogical; do
      if ! postgres_command_names | grep -qx "${required_command}"; then
        echo "required PostgreSQL command missing: ${required_command}"
        return 1
      fi
    done

    while IFS= read -r command_name; do
      [ -n "${command_name}" ] || continue
      binary="$(postgres_command_binary "${command_name}")"
      argv0_mode="wrapper"
      if [ "${command_name}" = "postgres" ]; then
        argv0_mode="binary"
      fi
      cat >"${pgwrap}/${command_name}" <<EOF
#!/usr/bin/env sh
loader='${loader}'
library_path='${pg_library_path}'
binary='${binary}'
argv0_mode='${argv0_mode}'
case "\${argv0_mode}" in
  wrapper) runtime_argv0="\$0" ;;
  *) runtime_argv0="\${binary}" ;;
esac
exec "\${loader}" --library-path "\${library_path}" --argv0 "\${runtime_argv0}" "\${binary}" "\$@"
EOF
      chmod 755 "${pgwrap}/${command_name}"
    done < <(postgres_command_names)

    while IFS= read -r command_name; do
      [ -n "${command_name}" ] || continue
      if "${pgwrap}/${command_name}" --version >/dev/null 2>&1; then
        printf '[postgres tool version ok] %s\n' "${command_name}"
      else
        printf '[postgres tool version fail] %s\n' "${command_name}"
        return 1
      fi
    done < <(postgres_command_names)

    if [ "$(id -u)" -eq 0 ]; then
      command -v runuser >/dev/null
      chown -R nobody "${tmp}"
    fi
    trap cleanup_postgres_smoke EXIT

    port="$(next_port)"
    init_cluster "${tmp}/data" >"${tmp}/initdb.log" 2>&1 || {
      cat "${tmp}/initdb.log"
      return 1
    }
    start_cluster "${tmp}/data" "${port}" "${tmp}/postgres.log" || return 1
    pid="${started_pid}"
    "${pgwrap}/psql" -h 127.0.0.1 -p "${port}" -U postgres -d postgres -v ON_ERROR_STOP=1 -c 'select 1;' >/dev/null || return 1

    cut -f1,2 "${extension_manifest}" | sort >"${tmp}/expected-extensions.tsv"
    "${pgwrap}/psql" -h 127.0.0.1 -p "${port}" -U postgres -d postgres -A -t -F $'\t' \
      -v ON_ERROR_STOP=1 \
      -c "select name, coalesce(default_version, '') from pg_available_extensions order by name;" \
      >"${tmp}/actual-extensions.tsv" || return 1
    if ! diff -u "${tmp}/expected-extensions.tsv" "${tmp}/actual-extensions.tsv"; then
      echo "pg_available_extensions does not match Docker-source extension manifest"
      return 1
    fi
    echo "pg_available_extensions matches Docker-source extension manifest"

    extension_so_count=0
    unresolved_extension_so_count=0
    while IFS= read -r extension_so; do
      [ -n "${extension_so}" ] || continue
      extension_so_count=$((extension_so_count + 1))
      if [ -x "${loader}" ]; then
        "${loader}" --library-path "${pg_library_path}" --list "${extension_so}" >"${tmp}/extension-ldd.out" 2>&1 \
          || LD_LIBRARY_PATH="${pg_library_path}" ldd "${extension_so}" >"${tmp}/extension-ldd.out" 2>&1 \
          || true
      else
        LD_LIBRARY_PATH="${pg_library_path}" ldd "${extension_so}" >"${tmp}/extension-ldd.out" 2>&1 || true
      fi
      if grep -q 'not found' "${tmp}/extension-ldd.out"; then
        unresolved_extension_so_count=$((unresolved_extension_so_count + 1))
        echo "[extension dependency unresolved] ${extension_so}"
        cat "${tmp}/extension-ldd.out"
      fi
    done < <(find "${pg_pkglib_dir}" -type f -name "*.so" -print | sort)
    echo "extension_so_count=${extension_so_count}"
    [ "${unresolved_extension_so_count}" -eq 0 ] || return 1

    create_failures=0
    create_count=0
    while IFS=$'\t' read -r extension_name default_version _control_file; do
      [ -n "${extension_name}" ] || continue
      create_count=$((create_count + 1))
      sql_extension_name="$(printf '%s' "${extension_name}" | sed 's/"/""/g')"
      if "${pgwrap}/psql" -h 127.0.0.1 -p "${port}" -U postgres -d postgres \
        -v ON_ERROR_STOP=1 \
        -c "CREATE EXTENSION IF NOT EXISTS \"${sql_extension_name}\";" >>"${tmp}/create-extensions.log" 2>&1; then
        printf '[extension create ok] %s %s\n' "${extension_name}" "${default_version}"
      else
        create_failures=$((create_failures + 1))
        printf '[extension create fail] %s %s\n' "${extension_name}" "${default_version}"
        tail -40 "${tmp}/create-extensions.log"
      fi
    done <"${extension_manifest}"
    echo "extension_create_count=${create_count}"
    [ "${create_failures}" -eq 0 ] || return 1

    echo "database client/admin/dump/restore smoke"
    run_pg "${pgwrap}/pg_ctl" status -D "${tmp}/data" >/dev/null
    "${pgwrap}/createdb" -h 127.0.0.1 -p "${port}" -U postgres native_verify_dump_src
    "${pgwrap}/createuser" -h 127.0.0.1 -p "${port}" -U postgres native_verify_role
    "${pgwrap}/psql" -h 127.0.0.1 -p "${port}" -U postgres -d native_verify_dump_src -v ON_ERROR_STOP=1 <<'SQL' >/dev/null
CREATE EXTENSION IF NOT EXISTS amcheck;
CREATE TABLE verify_items(id integer primary key, name text not null);
INSERT INTO verify_items VALUES (1, 'before'), (2, 'after');
SELECT lo_create(987654);
SQL
    "${pgwrap}/clusterdb" -h 127.0.0.1 -p "${port}" -U postgres -d native_verify_dump_src >/dev/null
    "${pgwrap}/reindexdb" -h 127.0.0.1 -p "${port}" -U postgres -d native_verify_dump_src >/dev/null
    "${pgwrap}/vacuumdb" -h 127.0.0.1 -p "${port}" -U postgres -d native_verify_dump_src >/dev/null
    "${pgwrap}/vacuumlo" -n -h 127.0.0.1 -p "${port}" -U postgres native_verify_dump_src >/dev/null
    "${pgwrap}/oid2name" -h 127.0.0.1 -p "${port}" -U postgres -d native_verify_dump_src >/dev/null
    "${pgwrap}/pg_amcheck" -h 127.0.0.1 -p "${port}" -U postgres -d native_verify_dump_src >/dev/null
    "${pgwrap}/pg_dump" -h 127.0.0.1 -p "${port}" -U postgres -d native_verify_dump_src -Fc -f "${tmp}/native_verify_dump_src.dump"
    "${pgwrap}/createdb" -h 127.0.0.1 -p "${port}" -U postgres native_verify_dump_restore
    "${pgwrap}/pg_restore" -h 127.0.0.1 -p "${port}" -U postgres -d native_verify_dump_restore "${tmp}/native_verify_dump_src.dump"
    dump_restore_count="$("${pgwrap}/psql" -h 127.0.0.1 -p "${port}" -U postgres -d native_verify_dump_restore -A -t -c 'select count(*) from verify_items;')"
    [ "${dump_restore_count}" = "2" ] || {
      echo "pg_restore row count mismatch: ${dump_restore_count}"
      return 1
    }
    "${pgwrap}/pg_restore" -l "${tmp}/native_verify_dump_src.dump" >/dev/null
    "${pgwrap}/pg_dumpall" -h 127.0.0.1 -p "${port}" -U postgres --globals-only >"${tmp}/globals.sql"
    "${pgwrap}/pg_dumpall" -h 127.0.0.1 -p "${port}" -U postgres --schema-only >"${tmp}/schema.sql"
    grep -q native_verify_role "${tmp}/globals.sql"
    grep -q verify_items "${tmp}/schema.sql"
    "${pgwrap}/dropuser" -h 127.0.0.1 -p "${port}" -U postgres native_verify_role
    "${pgwrap}/dropdb" -h 127.0.0.1 -p "${port}" -U postgres native_verify_dump_restore
    echo "database client/admin/dump/restore smoke ok"

    echo "media-core migration smoke"
    "${pgwrap}/createdb" -h 127.0.0.1 -p "${port}" -U postgres streamserver_verify_migrations
    core_http_port="$(next_port)"
    core_grpc_port="$(next_port)"
    DATABASE_URL="postgresql://postgres@127.0.0.1:${port}/streamserver_verify_migrations" \
    AUTH_MODE=disabled \
    CORE_HTTP_ADDR="127.0.0.1:${core_http_port}" \
    CORE_GRPC_ADDR="127.0.0.1:${core_grpc_port}" \
    STORAGE_ALLOWLIST="${tmp}" \
    STREAMSERVER_UI_DIR="${ROOT}/ui/media-core" \
    LOG_LEVEL=info \
      "${ROOT}/binaries/media-core-linux-amd64" >"${tmp}/media-core.log" 2>&1 &
    core_pid=$!
    POSTGRES_SMOKE_PIDS="${POSTGRES_SMOKE_PIDS} ${core_pid}"
    core_ready=0
    for _ in $(seq 1 60); do
      if curl -fsS "http://127.0.0.1:${core_http_port}/health/ready" >/dev/null 2>&1; then
        core_ready=1
        break
      fi
      if ! kill -0 "${core_pid}" >/dev/null 2>&1; then
        cat "${tmp}/media-core.log"
        return 1
      fi
      sleep 1
    done
    [ "${core_ready}" -eq 1 ] || {
      cat "${tmp}/media-core.log"
      return 1
    }
    migration_count="$("${pgwrap}/psql" -h 127.0.0.1 -p "${port}" -U postgres -d streamserver_verify_migrations -A -t -c 'select count(*) from _sqlx_migrations;')"
    [ "${migration_count}" -gt 0 ] || {
      echo "media-core migration count is zero"
      return 1
    }
    "${pgwrap}/psql" -h 127.0.0.1 -p "${port}" -U postgres -d streamserver_verify_migrations -v ON_ERROR_STOP=1 \
      -c "select to_regclass('public.tasks'), to_regclass('public.media_nodes');" >/dev/null
    stop_pid "${core_pid}"
    echo "media-core migration smoke ok: migrations=${migration_count}"

    run_pg "${pgwrap}/pg_ctl" -D "${tmp}/data" -m fast stop >/dev/null 2>&1 || stop_pid "${pid}"
    "${pgwrap}/pg_controldata" "${tmp}/data" >/dev/null
    if ! "${pgwrap}/pg_checksums" --check -D "${tmp}/data" >"${tmp}/pg-checksums.log" 2>&1; then
      if grep -Eiq 'disable|not enabled' "${tmp}/pg-checksums.log"; then
        cat "${tmp}/pg-checksums.log"
      else
        cat "${tmp}/pg-checksums.log"
        return 1
      fi
    fi
    run_pg "${pgwrap}/pg_resetwal" -n "${tmp}/data" >/dev/null
    echo "stopped-cluster control/checksum/resetwal smoke ok"

    echo "SSL client certificate and pg_hba smoke"
    ssl_dir="${tmp}/ssl"
    mkdir -p "${ssl_dir}"
    cat >"${ssl_dir}/server.cnf" <<'EOF'
[req]
distinguished_name = dn
req_extensions = req_ext
prompt = no
[dn]
CN = 127.0.0.1
[req_ext]
subjectAltName = IP:127.0.0.1,DNS:localhost
EOF
    openssl genrsa -out "${ssl_dir}/ca.key" 2048 >/dev/null 2>&1
    openssl req -x509 -new -nodes -key "${ssl_dir}/ca.key" -sha256 -days 2 -subj "/CN=StreamServer Native Verify CA" -out "${ssl_dir}/ca.crt" >/dev/null 2>&1
    openssl genrsa -out "${ssl_dir}/server.key" 2048 >/dev/null 2>&1
    openssl req -new -key "${ssl_dir}/server.key" -out "${ssl_dir}/server.csr" -config "${ssl_dir}/server.cnf" >/dev/null 2>&1
    openssl x509 -req -in "${ssl_dir}/server.csr" -CA "${ssl_dir}/ca.crt" -CAkey "${ssl_dir}/ca.key" -CAcreateserial -out "${ssl_dir}/server.crt" -days 2 -sha256 -extensions req_ext -extfile "${ssl_dir}/server.cnf" >/dev/null 2>&1
    openssl genrsa -out "${ssl_dir}/client.key" 2048 >/dev/null 2>&1
    openssl req -new -key "${ssl_dir}/client.key" -subj "/CN=cert_user" -out "${ssl_dir}/client.csr" >/dev/null 2>&1
    openssl x509 -req -in "${ssl_dir}/client.csr" -CA "${ssl_dir}/ca.crt" -CAkey "${ssl_dir}/ca.key" -CAcreateserial -out "${ssl_dir}/client.crt" -days 2 -sha256 >/dev/null 2>&1
    chmod 600 "${ssl_dir}/server.key" "${ssl_dir}/client.key"
    ssl_port="$(next_port)"
    init_cluster "${tmp}/ssl-data" >"${tmp}/ssl-initdb.log" 2>&1 || {
      cat "${tmp}/ssl-initdb.log"
      return 1
    }
    cp "${ssl_dir}/server.crt" "${ssl_dir}/server.key" "${ssl_dir}/ca.crt" "${tmp}/ssl-data/"
    cat >"${tmp}/ssl-data/pg_hba.conf" <<'EOF'
local all all trust
hostssl certdb cert_user 127.0.0.1/32 cert clientcert=verify-full
hostnossl certdb cert_user 127.0.0.1/32 reject
host all all 127.0.0.1/32 trust
EOF
    if [ "$(id -u)" -eq 0 ]; then
      chown -R nobody "${tmp}/ssl-data"
    fi
    start_cluster "${tmp}/ssl-data" "${ssl_port}" "${tmp}/ssl-postgres.log" \
      -c ssl=on \
      -c ssl_cert_file=server.crt \
      -c ssl_key_file=server.key \
      -c ssl_ca_file=ca.crt || return 1
    ssl_pid="${started_pid}"
    "${pgwrap}/createuser" -h 127.0.0.1 -p "${ssl_port}" -U postgres cert_user
    "${pgwrap}/createdb" -h 127.0.0.1 -p "${ssl_port}" -U postgres -O cert_user certdb
    ssl_used="$(PGSSLCERT="${ssl_dir}/client.crt" PGSSLKEY="${ssl_dir}/client.key" PGSSLROOTCERT="${ssl_dir}/ca.crt" \
      "${pgwrap}/psql" "host=127.0.0.1 port=${ssl_port} user=cert_user dbname=certdb sslmode=verify-full" -A -t \
        -c 'select ssl from pg_stat_ssl where pid = pg_backend_pid();')"
    [ "${ssl_used}" = "t" ] || {
      echo "SSL connection did not report ssl=true: ${ssl_used}"
      return 1
    }
    if PGSSLCERT="${ssl_dir}/client.crt" PGSSLKEY="${ssl_dir}/client.key" PGSSLROOTCERT="${ssl_dir}/ca.crt" \
      "${pgwrap}/psql" "host=127.0.0.1 port=${ssl_port} user=cert_user dbname=certdb sslmode=disable" -c 'select 1;' >/dev/null 2>&1; then
      echo "non-SSL cert_user connection unexpectedly succeeded"
      return 1
    fi
    stop_pid "${ssl_pid}"
    echo "SSL client certificate and pg_hba smoke ok"

    echo "PITR and pg_basebackup smoke"
    pitr_primary_port="$(next_port)"
    pitr_restore_port="$(next_port)"
    pitr_archive="${tmp}/pitr-archive"
    mkdir -p "${pitr_archive}"
    if [ "$(id -u)" -eq 0 ]; then
      chown -R nobody "${pitr_archive}"
    fi
    init_cluster "${tmp}/pitr-primary" >"${tmp}/pitr-initdb.log" 2>&1 || {
      cat "${tmp}/pitr-initdb.log"
      return 1
    }
    cat >>"${tmp}/pitr-primary/pg_hba.conf" <<'EOF'
host replication all 127.0.0.1/32 trust
host all all 127.0.0.1/32 trust
EOF
    start_cluster "${tmp}/pitr-primary" "${pitr_primary_port}" "${tmp}/pitr-primary.log" \
      -c wal_level=replica \
      -c archive_mode=on \
      -c "archive_command=cp %p ${pitr_archive}/%f" \
      -c max_wal_senders=5 || return 1
    pitr_pid="${started_pid}"
    "${pgwrap}/createdb" -h 127.0.0.1 -p "${pitr_primary_port}" -U postgres pitrdb
    "${pgwrap}/psql" -h 127.0.0.1 -p "${pitr_primary_port}" -U postgres -d pitrdb -v ON_ERROR_STOP=1 \
      -c "create table pitr_items(id int primary key, note text); insert into pitr_items values (1, 'before'); checkpoint;" >/dev/null
    "${pgwrap}/pg_basebackup" -h 127.0.0.1 -p "${pitr_primary_port}" -U postgres -D "${tmp}/pitr-basebackup" -X stream -Fp --checkpoint=fast >/dev/null
    "${pgwrap}/pg_verifybackup" "${tmp}/pitr-basebackup" >/dev/null
    "${pgwrap}/psql" -h 127.0.0.1 -p "${pitr_primary_port}" -U postgres -d pitrdb -v ON_ERROR_STOP=1 \
      -c "select pg_create_restore_point('native_verify_pitr'); insert into pitr_items values (2, 'after'); select pg_switch_wal();" >/dev/null
    sleep 2
    archive_oldest="$(find "${pitr_archive}" -type f | head -n 1 || true)"
    [ -n "${archive_oldest}" ] || {
      echo "PITR archive is empty"
      return 1
    }
    pitr_waldump_ok=0
    : >"${tmp}/pitr-waldump.log"
    while IFS= read -r wal_file; do
      [ -n "${wal_file}" ] || continue
      if "${pgwrap}/pg_waldump" "${wal_file}" >"${tmp}/pitr-waldump.log" 2>&1; then
        pitr_waldump_ok=1
        break
      fi
    done < <(find "${pitr_archive}" -type f -size +0 -print | LC_ALL=C sort)
    [ "${pitr_waldump_ok}" -eq 1 ] || {
      echo "pg_waldump did not find a valid archived WAL segment"
      cat "${tmp}/pitr-waldump.log"
      return 1
    }
    "${pgwrap}/pg_archivecleanup" -n "${pitr_archive}" "$(basename "${archive_oldest}")" >/dev/null
    stop_pid "${pitr_pid}"
    cp -a "${tmp}/pitr-basebackup" "${tmp}/pitr-restore"
    rm -f "${tmp}/pitr-restore/standby.signal"
    touch "${tmp}/pitr-restore/recovery.signal"
    cat >>"${tmp}/pitr-restore/postgresql.auto.conf" <<EOF
restore_command = 'cp ${pitr_archive}/%f %p'
recovery_target_name = 'native_verify_pitr'
recovery_target_action = 'promote'
EOF
    if [ "$(id -u)" -eq 0 ]; then
      chown -R nobody "${tmp}/pitr-restore"
    fi
    start_cluster "${tmp}/pitr-restore" "${pitr_restore_port}" "${tmp}/pitr-restore.log" || return 1
    pitr_restore_pid="${started_pid}"
    pitr_count="$("${pgwrap}/psql" -h 127.0.0.1 -p "${pitr_restore_port}" -U postgres -d pitrdb -A -t -c 'select count(*) from pitr_items;')"
    [ "${pitr_count}" = "1" ] || {
      echo "PITR row count mismatch: ${pitr_count}"
      return 1
    }
    stop_pid "${pitr_restore_pid}"
    echo "PITR and pg_basebackup smoke ok"

    echo "physical replication smoke"
    repl_primary_port="$(next_port)"
    repl_standby_port="$(next_port)"
    init_cluster "${tmp}/repl-primary" >"${tmp}/repl-initdb.log" 2>&1 || {
      cat "${tmp}/repl-initdb.log"
      return 1
    }
    cat >>"${tmp}/repl-primary/pg_hba.conf" <<'EOF'
host replication all 127.0.0.1/32 trust
host all all 127.0.0.1/32 trust
EOF
    start_cluster "${tmp}/repl-primary" "${repl_primary_port}" "${tmp}/repl-primary.log" \
      -c wal_level=replica \
      -c max_wal_senders=5 \
      -c hot_standby=on || return 1
    repl_primary_pid="${started_pid}"
    "${pgwrap}/psql" -h 127.0.0.1 -p "${repl_primary_port}" -U postgres -d postgres -v ON_ERROR_STOP=1 \
      -c "create table repl_items(id int primary key, note text); insert into repl_items values (1, 'before');" >/dev/null
    "${pgwrap}/pg_basebackup" -h 127.0.0.1 -p "${repl_primary_port}" -U postgres -D "${tmp}/repl-standby" -X stream -R -Fp >/dev/null
    cat >>"${tmp}/repl-standby/postgresql.auto.conf" <<EOF
port = ${repl_standby_port}
unix_socket_directories = '${tmp}'
EOF
    if [ "$(id -u)" -eq 0 ]; then
      chown -R nobody "${tmp}/repl-standby"
    fi
    start_cluster "${tmp}/repl-standby" "${repl_standby_port}" "${tmp}/repl-standby.log" || return 1
    repl_standby_pid="${started_pid}"
    standby_recovery="$("${pgwrap}/psql" -h 127.0.0.1 -p "${repl_standby_port}" -U postgres -d postgres -A -t -c 'select pg_is_in_recovery();')"
    [ "${standby_recovery}" = "t" ] || {
      echo "standby is not in recovery: ${standby_recovery}"
      return 1
    }
    "${pgwrap}/psql" -h 127.0.0.1 -p "${repl_primary_port}" -U postgres -d postgres -v ON_ERROR_STOP=1 \
      -c "insert into repl_items values (2, 'replicated');" >/dev/null
    replicated=0
    for _ in $(seq 1 60); do
      repl_count="$("${pgwrap}/psql" -h 127.0.0.1 -p "${repl_standby_port}" -U postgres -d postgres -A -t -c 'select count(*) from repl_items;' 2>/dev/null || echo 0)"
      if [ "${repl_count}" = "2" ]; then
        replicated=1
        break
      fi
      sleep 1
    done
    [ "${replicated}" -eq 1 ] || {
      cat "${tmp}/repl-standby.log"
      return 1
    }
    stop_pid "${repl_standby_pid}"
    stop_pid "${repl_primary_pid}"
    echo "physical replication smoke ok"

    echo "logical replication smoke"
    logical_pub_port="$(next_port)"
    logical_sub_port="$(next_port)"
    init_cluster "${tmp}/logical-pub" >"${tmp}/logical-pub-initdb.log" 2>&1 || {
      cat "${tmp}/logical-pub-initdb.log"
      return 1
    }
    init_cluster "${tmp}/logical-sub" >"${tmp}/logical-sub-initdb.log" 2>&1 || {
      cat "${tmp}/logical-sub-initdb.log"
      return 1
    }
    cat >>"${tmp}/logical-pub/pg_hba.conf" <<'EOF'
host replication all 127.0.0.1/32 trust
host all all 127.0.0.1/32 trust
EOF
    start_cluster "${tmp}/logical-pub" "${logical_pub_port}" "${tmp}/logical-pub.log" \
      -c wal_level=logical \
      -c max_replication_slots=5 \
      -c max_wal_senders=5 || return 1
    logical_pub_pid="${started_pid}"
    start_cluster "${tmp}/logical-sub" "${logical_sub_port}" "${tmp}/logical-sub.log" \
      -c max_replication_slots=5 \
      -c max_wal_senders=5 || return 1
    logical_sub_pid="${started_pid}"
    "${pgwrap}/psql" -h 127.0.0.1 -p "${logical_pub_port}" -U postgres -d postgres -v ON_ERROR_STOP=1 \
      -c "create table logical_items(id int primary key, note text); create publication native_verify_pub for table logical_items;" >/dev/null
    "${pgwrap}/psql" -h 127.0.0.1 -p "${logical_sub_port}" -U postgres -d postgres -v ON_ERROR_STOP=1 \
      -c "create table logical_items(id int primary key, note text);" >/dev/null
    "${pgwrap}/pg_recvlogical" -h 127.0.0.1 -p "${logical_pub_port}" -U postgres -d postgres -S native_verify_decoding --create-slot -P test_decoding >/dev/null
    "${pgwrap}/pg_recvlogical" -h 127.0.0.1 -p "${logical_pub_port}" -U postgres -d postgres -S native_verify_decoding --drop-slot >/dev/null
    "${pgwrap}/psql" -h 127.0.0.1 -p "${logical_sub_port}" -U postgres -d postgres -v ON_ERROR_STOP=1 \
      -c "create subscription native_verify_sub connection 'host=127.0.0.1 port=${logical_pub_port} user=postgres dbname=postgres' publication native_verify_pub with (copy_data = false);" >/dev/null
    "${pgwrap}/psql" -h 127.0.0.1 -p "${logical_pub_port}" -U postgres -d postgres -v ON_ERROR_STOP=1 \
      -c "insert into logical_items values (1, 'logical');" >/dev/null
    logical_replicated=0
    for _ in $(seq 1 90); do
      logical_count="$("${pgwrap}/psql" -h 127.0.0.1 -p "${logical_sub_port}" -U postgres -d postgres -A -t -c 'select count(*) from logical_items;' 2>/dev/null || echo 0)"
      if [ "${logical_count}" = "1" ]; then
        logical_replicated=1
        break
      fi
      sleep 1
    done
    [ "${logical_replicated}" -eq 1 ] || {
      cat "${tmp}/logical-pub.log"
      cat "${tmp}/logical-sub.log"
      return 1
    }
    "${pgwrap}/psql" -h 127.0.0.1 -p "${logical_sub_port}" -U postgres -d postgres -v ON_ERROR_STOP=1 \
      -c "drop subscription native_verify_sub;" >/dev/null
    stop_pid "${logical_sub_pid}"
    stop_pid "${logical_pub_pid}"
    echo "logical replication smoke ok"

    cleanup_postgres_smoke
    trap - EXIT
  }
  if postgres_smoke >>"${REPORT}" 2>&1; then
    append "\`\`\`"
    record_ok "PostgreSQL init/start/query/extensions smoke"
  else
    append "\`\`\`"
    record_failure "PostgreSQL init/start/query/extensions smoke"
  fi
fi

section "Summary"
append "- failures: ${FAILURES}"
if [ "${FAILURES}" -eq 0 ]; then
  append "- result: PASS"
else
  append "- result: FAIL"
fi
printf '%s\n' "${REPORT}"
exit "${FAILURES}"
REMOTE

  log "上传远端验证脚本"
  if [ "${UPLOAD_METHOD}" = "http" ]; then
    ssh_run "curl -fL --retry 3 --connect-timeout 10 -o $(shell_quote "${remote_script}") $(shell_quote "http://${HTTP_HOST}:${HTTP_PORT}/${remote_script_name}")"
  else
    scp_upload "${remote_script_local}" "${remote_script}"
  fi

  log "开始远端验证，不使用 Docker"
  set +e
  ssh_run "STREAMSERVER_VERIFY_BUNDLE=$(shell_quote "${remote_bundle}") STREAMSERVER_VERIFY_DIR=$(shell_quote "${REMOTE_DIR}") STREAMSERVER_VERIFY_REPORT=$(shell_quote "${remote_report}") bash $(shell_quote "${remote_script}")"
  status=$?
  set -e
  rm -f "${remote_script_local}"

  log "下载远端验证报告: ${remote_report}"
  ssh_run "cat $(shell_quote "${remote_report}")" >"${OUTPUT_DIR}/${report_name}" || true
  log "本地报告: ${OUTPUT_DIR}/${report_name}"
  exit "${status}"
}

main "$@"
