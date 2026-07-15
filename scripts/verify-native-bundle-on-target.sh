#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
BUNDLE_PATH=""
HOST=""
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
HTTP_SERVE_DIR=""
HTTP_SERVER_LOG=""
REMOTE_SCRIPT_LOCAL=""
LOCAL_REPORT_TMP=""
LOCAL_RUN_DIR=""
LOCAL_VERIFY_PID=""
LOCAL_VERIFY_STARTTIME=""
LOCAL_VERIFY_PGID=""
LOCAL_VERIFY_GROUP_OWNED=0
REMOTE_RUN_DIR=""
BUNDLE_SHA256=""
LOCAL_MODE=0
GPU_HARDWARE_MODE="required"
SSH_COMMAND_TIMEOUT_SEC=300
SSH_TRANSFER_TIMEOUT_SEC=7200
REMOTE_VERIFY_TIMEOUT_SEC=7200
SSH_CONNECT_TIMEOUT_SEC=10
SSH_SERVER_ALIVE_INTERVAL_SEC=15
SSH_SERVER_ALIVE_COUNT_MAX=3
OUTER_REPORT_LIMIT_BYTES=16777216

log() {
  printf '[verify-target] %s\n' "$*"
}

fail() {
  printf '[verify-target] ERROR: %s\n' "$*" >&2
  exit 1
}

usage() {
  cat <<EOF
用法:
  $(basename "$0") --bundle PATH [--host HOST] [--ssh-target USER@HOST]
                 [--port 22] [--access-file PATH] [--upload-method scp|http]
                 [--http-host HOST] [--http-port PORT]
                 [--remote-dir DIR] [--output-dir DIR]
  $(basename "$0") --local --bundle PATH [--gpu-hardware required|skip]
                 [--output-dir DIR]

说明:
  将 native 离线包上传到目标 Linux AMD64 服务器，并在目标服务器上验证包内业务二进制和第三方运行时组件。
  验证过程不依赖 Docker；若远端缺少 Docker 也必须能完成。
  若 --access-file 中包含地址/端口/用户/密码字段，脚本会用 expect 进行密码登录；
  HTTP 上传只会在显式传入 --upload-method http 时启用；默认始终使用 SCP。
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
      --local)
        LOCAL_MODE=1
        shift
        ;;
      --gpu-hardware)
        [ "$#" -ge 2 ] || fail "--gpu-hardware 需要参数"
        GPU_HARDWARE_MODE="$2"
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

deadline_exec() {
  local timeout_seconds="$1"
  shift
  timeout --signal=TERM --kill-after=135s "${timeout_seconds}s" "$@"
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
  if [ -n "${access_user}" ] && [ -n "${HOST}" ] && [ -z "${SSH_TARGET}" ]; then
    SSH_TARGET="${access_user}@${HOST}"
  fi
  [ -n "${access_password}" ] && SSH_PASSWORD="${access_password}"
  return 0
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
  [ -n "${HTTP_SERVE_DIR}" ] && [ -d "${HTTP_SERVE_DIR}" ] \
    || fail "HTTP 临时服务目录尚未准备"
  log "启动本地 HTTP 服务: http://${HTTP_HOST}:${HTTP_PORT}/"
  (
    cd "${HTTP_SERVE_DIR}"
    python3 -m http.server "${HTTP_PORT}" --bind 0.0.0.0 \
      >"${HTTP_SERVER_LOG}" 2>&1
  ) &
  HTTP_SERVER_PID="$!"
  sleep 1
  if ! kill -0 "${HTTP_SERVER_PID}" >/dev/null 2>&1; then
    fail "本地 HTTP 服务启动失败，日志: ${HTTP_SERVER_LOG}"
  fi
}

prepare_http_serve_dir() {
  local bundle_name="$1"
  HTTP_SERVE_DIR="$(mktemp -d "${TMPDIR:-/tmp}/streamserver-http-upload.XXXXXXXX")"
  HTTP_SERVER_LOG="$(mktemp "${TMPDIR:-/tmp}/streamserver-http-upload.XXXXXXXX.log")"
  chmod 700 "${HTTP_SERVE_DIR}"
  cp -- "${BUNDLE_PATH}" "${HTTP_SERVE_DIR}/${bundle_name}"
  chmod 600 "${HTTP_SERVE_DIR}/${bundle_name}"
}

stop_http_server() {
  if [ -n "${HTTP_SERVER_PID}" ] && kill -0 "${HTTP_SERVER_PID}" >/dev/null 2>&1; then
    kill -TERM "${HTTP_SERVER_PID}" >/dev/null 2>&1 || true
    for _ in $(seq 1 50); do
      kill -0 "${HTTP_SERVER_PID}" >/dev/null 2>&1 || break
      sleep 0.1
    done
    if kill -0 "${HTTP_SERVER_PID}" >/dev/null 2>&1; then
      kill -KILL "${HTTP_SERVER_PID}" >/dev/null 2>&1 || true
    fi
    wait "${HTTP_SERVER_PID}" 2>/dev/null || true
  fi
  HTTP_SERVER_PID=""
}

outer_process_identity() {
  local process_pid="$1"
  local stat_line stat_rest
  [ -r "/proc/${process_pid}/stat" ] || return 1
  IFS= read -r stat_line 2>/dev/null \
    <"/proc/${process_pid}/stat" || return 1
  stat_rest="${stat_line##*) }"
  # shellcheck disable=SC2086
  set -- ${stat_rest}
  [ "$#" -ge 20 ] || return 1
  printf '%s %s\n' "${3}" "${20}"
}

outer_group_has_live_members() {
  local expected_pgid="$1"
  local stat_file stat_line stat_rest process_state process_pgid
  for stat_file in /proc/[0-9]*/stat; do
    [ -r "${stat_file}" ] || continue
    IFS= read -r stat_line 2>/dev/null <"${stat_file}" || continue
    stat_rest="${stat_line##*) }"
    # shellcheck disable=SC2086
    set -- ${stat_rest}
    [ "$#" -ge 3 ] || continue
    process_state="${1}"
    process_pgid="${3}"
    if [ "${process_pgid}" = "${expected_pgid}" ] \
      && [ "${process_state}" != Z ]; then
      return 0
    fi
  done
  return 1
}

sweep_local_verifier_group() {
  local identity="" cleanup_status=0
  if [ "${LOCAL_VERIFY_GROUP_OWNED}" -eq 0 ] \
    && [ -n "${LOCAL_VERIFY_PID}" ]; then
    identity="$(outer_process_identity "${LOCAL_VERIFY_PID}" 2>/dev/null || true)"
    LOCAL_VERIFY_PGID="${identity%% *}"
    LOCAL_VERIFY_STARTTIME="${identity#* }"
    if [ "${LOCAL_VERIFY_PGID}" = "${LOCAL_VERIFY_PID}" ] \
      && [ -n "${LOCAL_VERIFY_STARTTIME}" ]; then
      LOCAL_VERIFY_GROUP_OWNED=1
    else
      kill -TERM "${LOCAL_VERIFY_PID}" >/dev/null 2>&1 || true
      for _ in $(seq 1 20); do
        kill -0 "${LOCAL_VERIFY_PID}" >/dev/null 2>&1 || break
        sleep 0.1
      done
      kill -KILL "${LOCAL_VERIFY_PID}" >/dev/null 2>&1 || true
    fi
  fi
  if [ "${LOCAL_VERIFY_GROUP_OWNED}" -eq 1 ] \
    && [ -n "${LOCAL_VERIFY_PGID}" ] \
    && [ "${LOCAL_VERIFY_PGID}" = "${LOCAL_VERIFY_PID}" ]; then
    kill -TERM -- "-${LOCAL_VERIFY_PGID}" >/dev/null 2>&1 || true
    for _ in $(seq 1 100); do
      outer_group_has_live_members "${LOCAL_VERIFY_PGID}" || break
      sleep 0.1
    done
    if outer_group_has_live_members "${LOCAL_VERIFY_PGID}"; then
      kill -KILL -- "-${LOCAL_VERIFY_PGID}" >/dev/null 2>&1 || true
    fi
    for _ in $(seq 1 50); do
      outer_group_has_live_members "${LOCAL_VERIFY_PGID}" || break
      sleep 0.1
    done
    outer_group_has_live_members "${LOCAL_VERIFY_PGID}" \
      && cleanup_status=1
  fi
  if [ -n "${LOCAL_VERIFY_PID}" ]; then
    wait "${LOCAL_VERIFY_PID}" 2>/dev/null || true
  fi
  LOCAL_VERIFY_PID=""
  LOCAL_VERIFY_STARTTIME=""
  LOCAL_VERIFY_PGID=""
  LOCAL_VERIFY_GROUP_OWNED=0
  return "${cleanup_status}"
}

run_owned_local_verifier() {
  local identity="" command_status
  setsid "$@" &
  LOCAL_VERIFY_PID=$!
  for _ in $(seq 1 100); do
    identity="$(outer_process_identity "${LOCAL_VERIFY_PID}" 2>/dev/null || true)"
    LOCAL_VERIFY_PGID="${identity%% *}"
    LOCAL_VERIFY_STARTTIME="${identity#* }"
    [ "${LOCAL_VERIFY_PGID}" = "${LOCAL_VERIFY_PID}" ] \
      && [ -n "${LOCAL_VERIFY_STARTTIME}" ] && break
    sleep 0.01
  done
  if [ "${LOCAL_VERIFY_PGID}" != "${LOCAL_VERIFY_PID}" ] \
    || [ -z "${LOCAL_VERIFY_STARTTIME}" ]; then
    kill -KILL "${LOCAL_VERIFY_PID}" >/dev/null 2>&1 || true
    wait "${LOCAL_VERIFY_PID}" 2>/dev/null || true
    LOCAL_VERIFY_PID=""
    return 125
  fi
  LOCAL_VERIFY_GROUP_OWNED=1
  if wait "${LOCAL_VERIFY_PID}"; then
    command_status=0
  else
    command_status=$?
  fi
  if ! sweep_local_verifier_group; then
    command_status=1
  fi
  return "${command_status}"
}

cleanup_local_verifier() {
  sweep_local_verifier_group || true
  stop_http_server
  cleanup_remote_verifier || true
  if [ -n "${REMOTE_SCRIPT_LOCAL}" ]; then
    rm -f -- "${REMOTE_SCRIPT_LOCAL}"
    REMOTE_SCRIPT_LOCAL=""
  fi
  if [ -n "${LOCAL_REPORT_TMP}" ]; then
    rm -f -- "${LOCAL_REPORT_TMP}"
    LOCAL_REPORT_TMP=""
  fi
  if [ -n "${HTTP_SERVE_DIR}" ]; then
    rm -rf -- "${HTTP_SERVE_DIR}"
    HTTP_SERVE_DIR=""
  fi
  if [ -n "${HTTP_SERVER_LOG}" ]; then
    rm -f -- "${HTTP_SERVER_LOG}"
    HTTP_SERVER_LOG=""
  fi
  if [ -n "${LOCAL_RUN_DIR}" ]; then
    rm -rf -- "${LOCAL_RUN_DIR}"
    LOCAL_RUN_DIR=""
  fi
}

cleanup_remote_verifier() {
  local cleanup_dir="${REMOTE_RUN_DIR:-}"
  [ -n "${cleanup_dir}" ] || return 0
  case "${cleanup_dir}" in
    */target-run.*) ;;
    *) return 0 ;;
  esac
  if ssh_run "rm -rf -- $(shell_quote "${cleanup_dir}")" 30 \
      >/dev/null 2>&1; then
    REMOTE_RUN_DIR=""
  fi
}

ssh_expect() {
  local command="$1"
  local timeout_seconds="${2:-${SSH_COMMAND_TIMEOUT_SEC}}"
  STREAMSERVER_SSH_PASSWORD="${SSH_PASSWORD}" \
  STREAMSERVER_SSH_TARGET="${SSH_TARGET}" \
  STREAMSERVER_SSH_PORT="${SSH_PORT}" \
  STREAMSERVER_SSH_COMMAND="${command}" \
  STREAMSERVER_SSH_TIMEOUT="${timeout_seconds}" \
  STREAMSERVER_SSH_CONNECT_TIMEOUT="${SSH_CONNECT_TIMEOUT_SEC}" \
  STREAMSERVER_SSH_ALIVE_INTERVAL="${SSH_SERVER_ALIVE_INTERVAL_SEC}" \
  STREAMSERVER_SSH_ALIVE_COUNT_MAX="${SSH_SERVER_ALIVE_COUNT_MAX}" \
    expect -c '
set timeout $env(STREAMSERVER_SSH_TIMEOUT)
set target $env(STREAMSERVER_SSH_TARGET)
set port $env(STREAMSERVER_SSH_PORT)
set command $env(STREAMSERVER_SSH_COMMAND)
set pass $env(STREAMSERVER_SSH_PASSWORD)
set connect_timeout $env(STREAMSERVER_SSH_CONNECT_TIMEOUT)
set alive_interval $env(STREAMSERVER_SSH_ALIVE_INTERVAL)
set alive_count $env(STREAMSERVER_SSH_ALIVE_COUNT_MAX)
log_user 0
spawn ssh -p $port -o StrictHostKeyChecking=accept-new -o PubkeyAuthentication=no -o ConnectTimeout=$connect_timeout -o ServerAliveInterval=$alive_interval -o ServerAliveCountMax=$alive_count $target $command
expect {
  -re "(?i)yes/no|fingerprint" { send -- "yes\r"; exp_continue }
  -re "(?i)password:" { send -- "$pass\r"; log_user 1; exp_continue }
  eof
  timeout {
    set child [exp_pid]
    catch {exec kill -TERM $child}
    after 5000
    catch {exec kill -KILL $child}
    catch {wait}
    exit 124
  }
}
set result [wait]
exit [lindex $result 3]
'
}

ssh_expect_stream() {
  local command="$1"
  local timeout_seconds="${2:-${SSH_COMMAND_TIMEOUT_SEC}}"
  STREAMSERVER_SSH_PASSWORD="${SSH_PASSWORD}" \
  STREAMSERVER_SSH_TARGET="${SSH_TARGET}" \
  STREAMSERVER_SSH_PORT="${SSH_PORT}" \
  STREAMSERVER_SSH_COMMAND="${command}" \
  STREAMSERVER_SSH_TIMEOUT="${timeout_seconds}" \
  STREAMSERVER_SSH_CONNECT_TIMEOUT="${SSH_CONNECT_TIMEOUT_SEC}" \
  STREAMSERVER_SSH_ALIVE_INTERVAL="${SSH_SERVER_ALIVE_INTERVAL_SEC}" \
  STREAMSERVER_SSH_ALIVE_COUNT_MAX="${SSH_SERVER_ALIVE_COUNT_MAX}" \
    expect -c '
set timeout $env(STREAMSERVER_SSH_TIMEOUT)
set target $env(STREAMSERVER_SSH_TARGET)
set port $env(STREAMSERVER_SSH_PORT)
set command $env(STREAMSERVER_SSH_COMMAND)
set pass $env(STREAMSERVER_SSH_PASSWORD)
set connect_timeout $env(STREAMSERVER_SSH_CONNECT_TIMEOUT)
set alive_interval $env(STREAMSERVER_SSH_ALIVE_INTERVAL)
set alive_count $env(STREAMSERVER_SSH_ALIVE_COUNT_MAX)
set payload [read stdin]
log_user 0
spawn ssh -p $port -o StrictHostKeyChecking=accept-new -o PubkeyAuthentication=no -o ConnectTimeout=$connect_timeout -o ServerAliveInterval=$alive_interval -o ServerAliveCountMax=$alive_count $target $command
expect {
  -re "(?i)yes/no|fingerprint" { send -- "yes\r"; exp_continue }
  -re "(?i)password:" { send -- "$pass\r"; log_user 1 }
  eof {
    set result [wait]
    exit [lindex $result 3]
  }
  timeout {
    set child [exp_pid]
    catch {exec kill -TERM $child}
    after 5000
    catch {exec kill -KILL $child}
    catch {wait}
    exit 124
  }
}
send -- $payload
send -- "\004"
expect {
  eof
  timeout {
    set child [exp_pid]
    catch {exec kill -TERM $child}
    after 5000
    catch {exec kill -KILL $child}
    catch {wait}
    exit 124
  }
}
set result [wait]
exit [lindex $result 3]
'
}

ssh_run() {
  local command="$1"
  local timeout_seconds="${2:-${SSH_COMMAND_TIMEOUT_SEC}}"
  if [ -n "${SSH_PASSWORD}" ]; then
    ssh_expect "${command}" "${timeout_seconds}"
  else
    deadline_exec "${timeout_seconds}" \
      ssh -p "${SSH_PORT}" \
        -o StrictHostKeyChecking=accept-new \
        -o BatchMode=yes \
        -o "ConnectTimeout=${SSH_CONNECT_TIMEOUT_SEC}" \
        -o "ServerAliveInterval=${SSH_SERVER_ALIVE_INTERVAL_SEC}" \
        -o "ServerAliveCountMax=${SSH_SERVER_ALIVE_COUNT_MAX}" \
        "${SSH_TARGET}" "${command}"
  fi
}

ssh_stream() {
  local command="$1"
  local timeout_seconds="${2:-${SSH_COMMAND_TIMEOUT_SEC}}"
  if [ -n "${SSH_PASSWORD}" ]; then
    ssh_expect_stream "${command}" "${timeout_seconds}"
  else
    deadline_exec "${timeout_seconds}" \
      ssh -p "${SSH_PORT}" \
        -o StrictHostKeyChecking=accept-new \
        -o BatchMode=yes \
        -o "ConnectTimeout=${SSH_CONNECT_TIMEOUT_SEC}" \
        -o "ServerAliveInterval=${SSH_SERVER_ALIVE_INTERVAL_SEC}" \
        -o "ServerAliveCountMax=${SSH_SERVER_ALIVE_COUNT_MAX}" \
        "${SSH_TARGET}" "${command}"
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
    STREAMSERVER_SCP_TIMEOUT="${SSH_TRANSFER_TIMEOUT_SEC}" \
    STREAMSERVER_SSH_CONNECT_TIMEOUT="${SSH_CONNECT_TIMEOUT_SEC}" \
    STREAMSERVER_SSH_ALIVE_INTERVAL="${SSH_SERVER_ALIVE_INTERVAL_SEC}" \
    STREAMSERVER_SSH_ALIVE_COUNT_MAX="${SSH_SERVER_ALIVE_COUNT_MAX}" \
      expect -c '
set timeout $env(STREAMSERVER_SCP_TIMEOUT)
set local_path $env(STREAMSERVER_SCP_LOCAL_PATH)
set target $env(STREAMSERVER_SCP_TARGET)
set port $env(STREAMSERVER_SCP_PORT)
set remote_path $env(STREAMSERVER_SCP_REMOTE_PATH)
set pass $env(STREAMSERVER_SSH_PASSWORD)
set connect_timeout $env(STREAMSERVER_SSH_CONNECT_TIMEOUT)
set alive_interval $env(STREAMSERVER_SSH_ALIVE_INTERVAL)
set alive_count $env(STREAMSERVER_SSH_ALIVE_COUNT_MAX)
log_user 0
spawn scp -P $port -o StrictHostKeyChecking=accept-new -o PubkeyAuthentication=no -o ConnectTimeout=$connect_timeout -o ServerAliveInterval=$alive_interval -o ServerAliveCountMax=$alive_count $local_path ${target}:${remote_path}
expect {
  -re "(?i)yes/no|fingerprint" { send -- "yes\r"; exp_continue }
  -re "(?i)password:" { send -- "$pass\r"; log_user 1; exp_continue }
  eof
  timeout {
    set child [exp_pid]
    catch {exec kill -TERM $child}
    after 5000
    catch {exec kill -KILL $child}
    catch {wait}
    exit 124
  }
}
set result [wait]
exit [lindex $result 3]
'
  else
    deadline_exec "${SSH_TRANSFER_TIMEOUT_SEC}" \
      scp -P "${SSH_PORT}" \
        -o StrictHostKeyChecking=accept-new \
        -o BatchMode=yes \
        -o "ConnectTimeout=${SSH_CONNECT_TIMEOUT_SEC}" \
        -o "ServerAliveInterval=${SSH_SERVER_ALIVE_INTERVAL_SEC}" \
        -o "ServerAliveCountMax=${SSH_SERVER_ALIVE_COUNT_MAX}" \
        "${local_path}" "${SSH_TARGET}:${remote_path}" >/dev/null
  fi
}

capture_stream_capped() {
  local destination="$1"
  local byte_limit="$2"
  python3 /dev/fd/3 "${destination}" "${byte_limit}" 3<<'PY'
import pathlib
import sys

destination = pathlib.Path(sys.argv[1])
limit = int(sys.argv[2])
written = 0
overflow = False
with destination.open("wb") as output:
    while True:
        chunk = sys.stdin.buffer.read(min(1024 * 1024, limit - written + 1))
        if not chunk:
            break
        allowed = min(len(chunk), limit - written)
        if allowed > 0:
            output.write(chunk[:allowed])
            written += allowed
        if allowed != len(chunk) or written >= limit:
            overflow = bool(chunk[allowed:])
            if not overflow:
                overflow = bool(sys.stdin.buffer.read(1))
            break
if overflow:
    raise SystemExit(90)
PY
}

copy_file_capped() {
  local source="$1"
  local destination="$2"
  local byte_limit="$3"
  python3 - "${source}" "${destination}" "${byte_limit}" <<'PY'
import pathlib
import os
import stat
import sys

source = pathlib.Path(sys.argv[1])
destination = pathlib.Path(sys.argv[2])
limit = int(sys.argv[3])
source_fd = os.open(source, os.O_RDONLY | os.O_NOFOLLOW)
source_stat = os.fstat(source_fd)
if not stat.S_ISREG(source_stat.st_mode):
    os.close(source_fd)
    destination.unlink(missing_ok=True)
    raise SystemExit(90)
overflow = False
written = 0
try:
    with os.fdopen(source_fd, "rb") as input_file, destination.open("wb") as output_file:
        while True:
            chunk = input_file.read(min(1024 * 1024, limit - written + 1))
            if not chunk:
                break
            allowed = min(len(chunk), limit - written)
            if allowed > 0:
                output_file.write(chunk[:allowed])
                written += allowed
            if allowed != len(chunk) or written >= limit:
                overflow = bool(chunk[allowed:])
                if not overflow:
                    overflow = bool(input_file.read(1))
                break
finally:
    if overflow:
        destination.unlink(missing_ok=True)
if overflow:
    raise SystemExit(90)
PY
}

main() {
  parse_args "$@"
  parse_access_file
  [ -n "${BUNDLE_PATH}" ] || fail "必须传入 --bundle"
  [ -f "${BUNDLE_PATH}" ] || fail "bundle 不存在: ${BUNDLE_PATH}"
  case "${GPU_HARDWARE_MODE}" in
    required|skip) ;;
    *) fail "--gpu-hardware 必须是 required 或 skip" ;;
  esac
  require_cmd timeout
  require_cmd python3
  timeout --version 2>/dev/null | grep -Fq 'GNU coreutils' \
    || fail "timeout 必须来自 GNU coreutils"
  if [ "${LOCAL_MODE}" -eq 0 ]; then
    require_cmd ssh
    require_cmd scp
    if [ -n "${SSH_PASSWORD}" ]; then
      require_cmd expect
    fi
    case "${UPLOAD_METHOD}" in
      scp)
        ;;
      http)
        require_cmd curl
        ;;
      *)
        fail "未知上传方式: ${UPLOAD_METHOD}"
        ;;
    esac
    if [ -z "${SSH_TARGET}" ]; then
      [ -n "${HOST}" ] \
        || fail "必须通过 --host、--ssh-target 或 --access-file 指定目标服务器"
      SSH_TARGET="${HOST}"
    fi
  elif [ "${UPLOAD_METHOD}" != scp ]; then
    fail "--local 不能与远端上传参数组合使用"
  fi
  if [ -z "${REMOTE_DIR}" ]; then
    REMOTE_DIR="/tmp/streamserver-native-verify"
  fi
  mkdir -p "${OUTPUT_DIR}"
  trap cleanup_local_verifier EXIT

  require_cmd sha256sum
  BUNDLE_SHA256="$(sha256sum -- "${BUNDLE_PATH}" | awk '{print $1}')"
  [[ "${BUNDLE_SHA256}" =~ ^[0-9a-f]{64}$ ]] \
    || fail "鏃犳硶璁＄畻 bundle SHA-256"

  local bundle_name report_name remote_bundle remote_report status
  local final_report failure_zero_count failure_summary_count
  local pass_result_count result_summary_count summary_tail expected_summary
  local report_token remote_base remote_run_dir report_transfer_status
  local remote_command remote_outer_timeout report_capture_status=0
  local -a capture_statuses
  bundle_name="$(basename "${BUNDLE_PATH}")"
  LOCAL_REPORT_TMP="$(mktemp "${OUTPUT_DIR}/.native-verification-report.XXXXXXXX")"
  report_token="${LOCAL_REPORT_TMP##*.}"
  report_name="native-verification-target-${report_token}.md"

  if [ "${LOCAL_MODE}" -eq 1 ]; then
    LOCAL_RUN_DIR="$(mktemp -d "${TMPDIR:-/tmp}/streamserver-local-verify.XXXXXXXX")"
    chmod 700 "${LOCAL_RUN_DIR}"
    REMOTE_DIR="${LOCAL_RUN_DIR}"
    remote_bundle="$(cd "$(dirname "${BUNDLE_PATH}")" && pwd)/${bundle_name}"
    remote_report="${REMOTE_DIR}/${report_name}"
  else
    log "确认目标服务器 SSH 目标: ${SSH_TARGET}:${SSH_PORT}"
    ssh_run "set -e; hostname; uname -a; command -v systemctl >/dev/null && echo systemd_tool=present || echo systemd_tool=missing; hostname -I 2>/dev/null || true"

    remote_base="${REMOTE_DIR%/}"
    remote_run_dir="$(ssh_run \
      "umask 077; mkdir -p $(shell_quote "${remote_base}"); mktemp -d $(shell_quote "${remote_base}/target-run.XXXXXXXX")")"
    case "${remote_run_dir}" in
      "${remote_base}"/target-run.*) ;;
      *) fail "远端未返回预期的私有验证目录" ;;
    esac
    REMOTE_DIR="${remote_run_dir}"
    REMOTE_RUN_DIR="${remote_run_dir}"
    remote_bundle="${REMOTE_DIR}/${bundle_name}"
    remote_report="${REMOTE_DIR}/${report_name}"

    log "准备 native 包到 ${SSH_TARGET}:${remote_bundle}"
    if [ "${UPLOAD_METHOD}" = "http" ]; then
      # 大包优先让目标服务器主动下载，避免 scp 在弱网下重复握手导致失败。
      prepare_http_serve_dir "${bundle_name}"
      start_http_server
      local bundle_url
      bundle_url="http://${HTTP_HOST}:${HTTP_PORT}/${bundle_name}"
      log "让目标服务器从本机 HTTP 下载 native 包"
      ssh_run "set -e; curl -fL --retry 3 --connect-timeout 10 -o $(shell_quote "${remote_bundle}") $(shell_quote "${bundle_url}"); printf '%s  %s\\n' $(shell_quote "${BUNDLE_SHA256}") $(shell_quote "${remote_bundle}") | sha256sum -c -" \
        "${SSH_TRANSFER_TIMEOUT_SEC}"
      stop_http_server
    else
      log "通过 scp 上传 native 包"
      scp_upload "${BUNDLE_PATH}" "${remote_bundle}"
    fi
  fi

  local remote_script_name remote_script
  remote_script_name="${report_name%.md}.remote.sh"
  REMOTE_SCRIPT_LOCAL="$(mktemp "${TMPDIR:-/tmp}/streamserver-native-verifier.XXXXXXXX.sh")"
  remote_script="${REMOTE_DIR}/${remote_script_name}"
  # 远端脚本只验证 native 安装包，不调用 Docker，确保目标机运行依赖真的已随包携带。
  cat >"${REMOTE_SCRIPT_LOCAL}" <<'REMOTE'
set -euo pipefail

BUNDLE="${STREAMSERVER_VERIFY_BUNDLE}"
WORK_DIR="${STREAMSERVER_VERIFY_DIR}"
REPORT="${STREAMSERVER_VERIFY_REPORT}"
FAILURES=0
POSTGRES_SMOKE_TMP=""
POSTGRES_SMOKE_CONTROL_DIR=""
POSTGRES_SMOKE_TOOL_DIR=""
POSTGRES_SMOKE_SOCKET_DIR=""
POSTGRES_SMOKE_PID_REGISTRY=""
RUN_WORK=""
BUNDLE_VERSION=""
BUNDLE_VARIANT=""
BUNDLE_GPU_SUPPORT="false"
BUNDLE_WORKER_SUPPORT="false"
BUNDLE_POSTGRES_RUNTIME="false"
MANIFEST_VALUE=""
SMOKE_COMMAND_TIMEOUT_SEC=60
SMOKE_SHELL_TIMEOUT_SEC=120
POSTGRES_SMOKE_TIMEOUT_SEC=3600
GPU_HARDWARE_MODE="${STREAMSERVER_VERIFY_GPU_HARDWARE_MODE:-required}"
COMMAND_OUTPUT_LIMIT_BYTES=1048576
POSTGRES_OUTPUT_LIMIT_BYTES=8388608
REPORT_LIMIT_BYTES=16777216
REPORT_TRUNCATED=0
REPORT_FINALIZING=0
DEADLINE_KILL_AFTER_SEC=120
CAPTURE_KILL_AFTER_SEC=30

process_starttime() {
  local process_pid="$1"
  local stat_line stat_rest
  case "${process_pid}" in
    ''|*[!0-9]*) return 1 ;;
  esac
  [ -r "/proc/${process_pid}/stat" ] || return 1
  IFS= read -r stat_line 2>/dev/null \
    <"/proc/${process_pid}/stat" || return 1
  stat_rest="${stat_line##*) }"
  # /proc/PID/stat fields after the command name begin at field 3; starttime
  # is field 22, therefore the twentieth whitespace-delimited value here.
  # shellcheck disable=SC2086
  set -- ${stat_rest}
  [ "$#" -ge 20 ] || return 1
  case "${20}" in
    ''|*[!0-9]*) return 1 ;;
    *) printf '%s\n' "${20}" ;;
  esac
}

process_is_live_non_zombie() {
  local process_pid="$1"
  local stat_line stat_rest
  [ -r "/proc/${process_pid}/stat" ] || return 1
  IFS= read -r stat_line 2>/dev/null \
    <"/proc/${process_pid}/stat" || return 1
  stat_rest="${stat_line##*) }"
  # shellcheck disable=SC2086
  set -- ${stat_rest}
  [ "$#" -ge 1 ] || return 1
  [ "${1}" != Z ]
}

process_pgid() {
  local process_pid="$1"
  local stat_line stat_rest
  case "${process_pid}" in
    ''|*[!0-9]*) return 1 ;;
  esac
  [ -r "/proc/${process_pid}/stat" ] || return 1
  IFS= read -r stat_line 2>/dev/null \
    <"/proc/${process_pid}/stat" || return 1
  stat_rest="${stat_line##*) }"
  # Fields after comm begin at field 3; process group is field 5.
  # shellcheck disable=SC2086
  set -- ${stat_rest}
  [ "$#" -ge 3 ] || return 1
  case "${3}" in
    ''|*[!0-9]*) return 1 ;;
    *) printf '%s\n' "${3}" ;;
  esac
}

process_group_has_live_members() {
  local expected_pgid="$1"
  local stat_file stat_line stat_rest process_state process_group
  for stat_file in /proc/[0-9]*/stat; do
    [ -r "${stat_file}" ] || continue
    IFS= read -r stat_line 2>/dev/null <"${stat_file}" || continue
    stat_rest="${stat_line##*) }"
    # shellcheck disable=SC2086
    set -- ${stat_rest}
    [ "$#" -ge 3 ] || continue
    process_state="${1}"
    process_group="${3}"
    if [ "${process_group}" = "${expected_pgid}" ] \
      && [ "${process_state}" != Z ]; then
      return 0
    fi
  done
  return 1
}

terminate_registered_process() {
  local process_pid="$1"
  local expected_starttime="$2"
  local expected_pgid="$3"
  local current_starttime current_pgid
  case "${process_pid}:${expected_starttime}:${expected_pgid}" in
    *[!0-9:]*) return 1 ;;
  esac
  [ -n "${process_pid}" ] && [ -n "${expected_starttime}" ] \
    && [ "${expected_pgid}" = "${process_pid}" ] || return 1
  current_starttime="$(process_starttime "${process_pid}" 2>/dev/null || true)"
  if [ -z "${current_starttime}" ]; then
    wait "${process_pid}" 2>/dev/null || true
    process_group_has_live_members "${expected_pgid}" || return 0
  else
    current_pgid="$(process_pgid "${process_pid}" 2>/dev/null || true)"
    [ "${current_starttime}" = "${expected_starttime}" ] \
      && [ "${current_pgid}" = "${expected_pgid}" ] \
      && [ "${expected_pgid}" = "${process_pid}" ] || return 0

    current_starttime="$(process_starttime "${process_pid}" 2>/dev/null || true)"
    current_pgid="$(process_pgid "${process_pid}" 2>/dev/null || true)"
    [ "${current_starttime}" = "${expected_starttime}" ] \
      && [ "${current_pgid}" = "${expected_pgid}" ] || return 0
  fi
  kill -TERM -- "-${expected_pgid}" >/dev/null 2>&1 || true
  for _ in $(seq 1 50); do
    process_group_has_live_members "${expected_pgid}" || break
    sleep 0.1
  done
  if process_group_has_live_members "${expected_pgid}"; then
    kill -KILL -- "-${expected_pgid}" >/dev/null 2>&1 || true
  fi
  wait "${process_pid}" 2>/dev/null || true
  for _ in $(seq 1 50); do
    process_group_has_live_members "${expected_pgid}" || return 0
    sleep 0.1
  done
  process_group_has_live_members "${expected_pgid}" && return 1
  return 0
}

register_postgres_pid() {
  local process_pid="$1"
  local process_start process_group=""
  [ -n "${POSTGRES_SMOKE_PID_REGISTRY:-}" ] || return 1
  process_start="$(process_starttime "${process_pid}")" || return 1
  for _ in $(seq 1 50); do
    process_group="$(process_pgid "${process_pid}" 2>/dev/null || true)"
    [ "${process_group}" = "${process_pid}" ] && break
    sleep 0.02
  done
  [ "${process_group}" = "${process_pid}" ] || return 1
  printf '%s\t%s\t%s\n' \
    "${process_pid}" "${process_start}" "${process_group}" \
    >>"${POSTGRES_SMOKE_PID_REGISTRY}"
}

registered_process_is_live() {
  local process_pid="$1"
  local expected_starttime expected_pgid current_starttime current_pgid
  [ -f "${POSTGRES_SMOKE_PID_REGISTRY:-}" ] || return 1
  read -r expected_starttime expected_pgid < <(awk \
    -v expected_pid="${process_pid}" \
    '$1 == expected_pid { print $2, $3; exit }' \
    "${POSTGRES_SMOKE_PID_REGISTRY}")
  [ -n "${expected_starttime}" ] && [ -n "${expected_pgid}" ] \
    && [ "${expected_pgid}" = "${process_pid}" ] || return 1
  current_starttime="$(process_starttime "${process_pid}" 2>/dev/null || true)"
  current_pgid="$(process_pgid "${process_pid}" 2>/dev/null || true)"
  [ "${current_starttime}" = "${expected_starttime}" ] \
    && [ "${current_pgid}" = "${expected_pgid}" ] \
    && process_group_has_live_members "${expected_pgid}"
}

unregister_postgres_pid() {
  local process_pid="$1"
  local registry_tmp
  [ -f "${POSTGRES_SMOKE_PID_REGISTRY:-}" ] || return 0
  registry_tmp="${POSTGRES_SMOKE_PID_REGISTRY}.tmp"
  awk -v expected_pid="${process_pid}" '$1 != expected_pid' \
    "${POSTGRES_SMOKE_PID_REGISTRY}" >"${registry_tmp}"
  mv -f -- "${registry_tmp}" "${POSTGRES_SMOKE_PID_REGISTRY}"
}

cleanup_postgres_smoke() {
  local cleanup_pid cleanup_starttime cleanup_pgid cleanup_status=0
  if [ -f "${POSTGRES_SMOKE_PID_REGISTRY:-}" ]; then
    while IFS=$'\t' read -r cleanup_pid cleanup_starttime cleanup_pgid; do
      [ -n "${cleanup_pid}" ] && [ -n "${cleanup_starttime}" ] \
        && [ -n "${cleanup_pgid}" ] || continue
      terminate_registered_process \
        "${cleanup_pid}" "${cleanup_starttime}" "${cleanup_pgid}" \
        || cleanup_status=1
    done <"${POSTGRES_SMOKE_PID_REGISTRY}"
    : >"${POSTGRES_SMOKE_PID_REGISTRY}"
  fi
  if [ -n "${POSTGRES_SMOKE_TMP:-}" ]; then
    rm -rf -- "${POSTGRES_SMOKE_TMP}"
    POSTGRES_SMOKE_TMP=""
  fi
  if [ -n "${POSTGRES_SMOKE_TOOL_DIR:-}" ]; then
    rm -rf -- "${POSTGRES_SMOKE_TOOL_DIR}"
    POSTGRES_SMOKE_TOOL_DIR=""
  fi
  if [ -n "${POSTGRES_SMOKE_SOCKET_DIR:-}" ]; then
    rm -rf -- "${POSTGRES_SMOKE_SOCKET_DIR}"
    POSTGRES_SMOKE_SOCKET_DIR=""
  fi
  if [ -n "${POSTGRES_SMOKE_CONTROL_DIR:-}" ]; then
    rm -rf -- "${POSTGRES_SMOKE_CONTROL_DIR}"
    POSTGRES_SMOKE_CONTROL_DIR=""
  fi
  POSTGRES_SMOKE_PID_REGISTRY=""
  return "${cleanup_status}"
}

cleanup_remote_verifier() {
  cleanup_postgres_smoke || true
  if [ -n "${RUN_WORK:-}" ]; then
    rm -rf -- "${RUN_WORK}"
    RUN_WORK=""
  fi
}

umask 077
mkdir -p "${WORK_DIR}"
canonical_work_dir="$(cd -- "${WORK_DIR}" && pwd -P)"
if [ "${canonical_work_dir}" != "${WORK_DIR%/}" ] \
    || [ -L "${WORK_DIR}" ] \
    || [ "$(stat -c '%u' -- "${WORK_DIR}")" != "$(id -u)" ]; then
  printf 'verification work directory must be canonical, real, and owned by the verifier user\n' \
    >&2
  exit 1
fi
RUN_WORK="$(mktemp -d "${WORK_DIR%/}/target-run.XXXXXXXX")"
chmod 700 "${RUN_WORK}"
mkdir -m 700 "${RUN_WORK}/extract"
trap cleanup_remote_verifier EXIT
: >"${REPORT}"

append() {
  if [ "${REPORT_TRUNCATED}" -eq 1 ] \
    && [ "${REPORT_FINALIZING}" -eq 0 ]; then
    return 0
  fi
  printf '%s\n' "$*" >>"${REPORT}"
  enforce_report_limit || true
}

enforce_report_limit() {
  local reserve_bytes=4096
  local body_limit=$((REPORT_LIMIT_BYTES - reserve_bytes))
  local report_size
  report_size="$(wc -c <"${REPORT}")"
  [ "${report_size}" -le "${body_limit}" ] && return 0
  if [ "${REPORT_TRUNCATED}" -eq 0 ]; then
    mark_report_truncated
  else
    truncate -s "${body_limit}" "${REPORT}"
  fi
  return 1
}

mark_report_truncated() {
  local reserve_bytes=4096
  local body_limit=$((REPORT_LIMIT_BYTES - reserve_bytes))
  local marker_reserve=512
  local report_size trimmed_report
  [ "${REPORT_TRUNCATED}" -eq 0 ] || return 0
  report_size="$(wc -c <"${REPORT}")"
  if [ "${report_size}" -gt $((body_limit - marker_reserve)) ]; then
    trimmed_report="${RUN_WORK}/report-trimmed"
    head -c $((body_limit - marker_reserve)) -- "${REPORT}" \
      >"${trimmed_report}"
    mv -f -- "${trimmed_report}" "${REPORT}"
  fi
  printf '\n[TRUNCATED] verification report exceeded %s bytes\n' \
    "${REPORT_LIMIT_BYTES}" >>"${REPORT}"
  printf '[FAIL] verification report exceeded the safety limit\n' \
    >>"${REPORT}"
  REPORT_TRUNCATED=1
  FAILURES=$((FAILURES + 1))
}

append_capped_output() {
  local output_file="$1"
  local byte_limit="$2"
  local capture_status="${3:-0}"
  local output_size report_size body_limit available write_limit
  local command_truncated=0 aggregate_truncated=0
  output_size="$(wc -c <"${output_file}")"
  if [ "${REPORT_TRUNCATED}" -eq 1 ]; then
    rm -f -- "${output_file}"
    return 1
  fi
  body_limit=$((REPORT_LIMIT_BYTES - 4096))
  report_size="$(wc -c <"${REPORT}")"
  available=$((body_limit - report_size))
  [ "${available}" -gt 0 ] || available=0
  write_limit="${output_size}"
  if [ "${write_limit}" -gt "${byte_limit}" ]; then
    write_limit="${byte_limit}"
    command_truncated=1
  fi
  if [ "${capture_status}" -eq 90 ]; then
    command_truncated=1
  fi
  if [ "${write_limit}" -gt "${available}" ]; then
    write_limit=$((available > 512 ? available - 512 : 0))
    aggregate_truncated=1
  fi
  if [ "${write_limit}" -gt 0 ]; then
    head -c "${write_limit}" -- "${output_file}" >>"${REPORT}"
  fi
  if [ "${command_truncated}" -eq 1 ] \
    && [ "${aggregate_truncated}" -eq 0 ]; then
    if [ "${capture_status}" -eq 90 ]; then
      printf '\n[TRUNCATED: command output exceeded the %s byte limit]\n' \
        "${byte_limit}" >>"${REPORT}"
    else
      printf '\n[TRUNCATED: command output was %s bytes; limit is %s bytes]\n' \
        "${output_size}" "${byte_limit}" >>"${REPORT}"
    fi
  fi
  if [ "${aggregate_truncated}" -eq 1 ]; then
    mark_report_truncated
  fi
  rm -f -- "${output_file}"
  if [ "${command_truncated}" -eq 1 ] \
    || [ "${aggregate_truncated}" -eq 1 ]; then
    return 1
  fi
  return 0
}

cap_report_for_summary() {
  enforce_report_limit || true
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

deadline_exec() {
  local timeout_seconds="$1"
  shift
  timeout --signal=TERM \
    --kill-after="${DEADLINE_KILL_AFTER_SEC}s" "${timeout_seconds}s" "$@"
}

write_summary() {
  cap_report_for_summary
  REPORT_FINALIZING=1
  section "Summary"
  append "- failures: ${FAILURES}"
  if [ "${FAILURES}" -eq 0 ]; then
    append "- result: PASS"
  else
    append "- result: FAIL"
  fi
}

abort_gate_if_failed() {
  local gate_label="$1"
  if [ "${FAILURES}" -ne 0 ]; then
    append "[FAIL] ${gate_label} blocked all package code execution"
    write_summary
    printf '%s\n' "${REPORT}"
    exit "${FAILURES}"
  fi
  record_ok "${gate_label} passed"
}

read_build_info_scalar() {
  local key="$1"
  local info="${ROOT}/build-info.txt"
  local count
  count="$(awk -F= -v expected="${key}" \
    '$1 == expected { count++ } END { print count + 0 }' "${info}")"
  if [ "${count}" -ne 1 ]; then
    record_failure "build-info.txt must define ${key} exactly once"
    MANIFEST_VALUE=""
    return 1
  fi
  MANIFEST_VALUE="$(awk -v prefix="${key}=" \
    'index($0, prefix) == 1 { print substr($0, length(prefix) + 1) }' \
    "${info}")"
  if [ -z "${MANIFEST_VALUE}" ]; then
    record_failure "build-info.txt value ${key} must not be empty"
    return 1
  fi
}

expect_build_info_value() {
  local key="$1"
  local expected="$2"
  if read_build_info_scalar "${key}"; then
    [ "${MANIFEST_VALUE}" = "${expected}" ] \
      || record_failure "build-info.txt ${key} must equal ${expected}"
  fi
}

validate_bundle_identity() {
  local top_dir="$1"
  local info="${ROOT}/build-info.txt"
  local line key top_version top_variant
  if [[ ! "${top_dir}" =~ ^streamserver-native-(v[0-9]+\.[0-9]+\.[0-9]+([-+][0-9A-Za-z.-]+)?)-linux-amd64-(cpu-only|gpu-enabled|control-plane-minimal)-[0-9]{8}(-([2-9]|[1-9][0-9]+))?$ ]]; then
    record_failure \
      "archive top-level directory name does not match native builder contract"
    return 0
  fi
  top_version="${BASH_REMATCH[1]}"
  top_variant="${BASH_REMATCH[3]}"
  [ "${top_version}" = "${BUNDLE_VERSION}" ] \
    || record_failure "archive top-level version does not match package manifest"
  [ "${top_variant}" = "${BUNDLE_VARIANT}" ] \
    || record_failure "archive top-level variant does not match package manifest"

  if [ ! -f "${info}" ]; then
    record_failure "build-info.txt is missing"
    return 0
  fi
  while IFS= read -r line || [ -n "${line}" ]; do
    if [[ "${line}" != *=* ]]; then
      record_failure "build-info.txt contains a malformed line"
      continue
    fi
    key="${line%%=*}"
    case "${key}" in
      bundle_name|version|built_at|builder_os|builder_arch|git_commit|bundle_variant|target_runtime|verification_recommended_location)
        ;;
      *) record_failure "build-info.txt contains unknown key: ${key}" ;;
    esac
  done <"${info}"
  expect_build_info_value bundle_name "${top_dir}"
  expect_build_info_value version "${BUNDLE_VERSION#v}"
  expect_build_info_value bundle_variant "${BUNDLE_VARIANT}"
  expect_build_info_value builder_os Linux
  expect_build_info_value builder_arch x86_64
  expect_build_info_value target_runtime docker-free
  expect_build_info_value verification_recommended_location target-server
  if read_build_info_scalar built_at; then
    [[ "${MANIFEST_VALUE}" =~ ^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z$ ]] \
      || record_failure "build-info.txt built_at must be canonical UTC"
  fi
  if read_build_info_scalar git_commit; then
    [[ "${MANIFEST_VALUE}" =~ ^[0-9A-Za-z._-]+$ ]] \
      || record_failure "build-info.txt git_commit contains unsafe characters"
  fi
}

read_manifest_scalar() {
  local key="$1"
  local manifest="${ROOT}/package-manifest.env"
  local count
  count="$(awk -F= -v expected="${key}" \
    '$1 == expected { count++ } END { print count + 0 }' "${manifest}")"
  if [ "${count}" -ne 1 ]; then
    record_failure "package manifest must define ${key} exactly once"
    MANIFEST_VALUE=""
    return 1
  fi
  MANIFEST_VALUE="$(awk -v prefix="${key}=" \
    'index($0, prefix) == 1 { print substr($0, length(prefix) + 1) }' \
    "${manifest}")"
  if [ -z "${MANIFEST_VALUE}" ]; then
    record_failure "package manifest value ${key} must not be empty"
    return 1
  fi
}

read_manifest_boolean() {
  local key="$1"
  local destination="$2"
  if ! read_manifest_scalar "${key}"; then
    printf -v "${destination}" '%s' false
    return 0
  fi
  case "${MANIFEST_VALUE}" in
    true|false)
      printf -v "${destination}" '%s' "${MANIFEST_VALUE}"
      ;;
    *)
      record_failure "package manifest ${key} must be exactly true or false"
      printf -v "${destination}" '%s' false
      ;;
  esac
}

validate_manifest_schema() {
  local manifest="${ROOT}/package-manifest.env"
  local line key
  while IFS= read -r line || [ -n "${line}" ]; do
    if [[ "${line}" != *=* ]]; then
      record_failure "package manifest contains a malformed line"
      continue
    fi
    key="${line%%=*}"
    case "${key}" in
      BUNDLE_VERSION|BUNDLE_VARIANT|BUNDLE_GPU_SUPPORT|BUNDLE_WORKER_SUPPORT|BUNDLE_POSTGRES_RUNTIME|DEPLOY_MODE|MEDIA_CORE_BINARY_PATH|MEDIA_AGENT_BINARY_PATH|MEDIA_GATEWAY_BINARY_PATH|STREAMSERVER_CONFIG_BINARY_PATH|MEDIA_CORE_UI_PATH|FFMPEG_CPU_BINARY_PATH|FFPROBE_CPU_BINARY_PATH|FFMPEG_CPU_LIB_PATH|FFMPEG_GPU_BINARY_PATH|FFPROBE_GPU_BINARY_PATH|FFMPEG_GPU_LIB_PATH|ZLM_BINARY_PATH|ZLM_DEFAULT_PEM_PATH|ZLM_LIB_PATH|POSTGRES_RUNTIME_PATH|POSTGRES_BIN_PATH|POSTGRES_LIB_PATH|POSTGRES_EXTENSION_MANIFEST_PATH)
        ;;
      *)
        record_failure "package manifest contains unknown key: ${key}"
        ;;
    esac
  done <"${manifest}"
}

expect_manifest_value() {
  local key="$1"
  local expected="$2"
  if read_manifest_scalar "${key}"; then
    [ "${MANIFEST_VALUE}" = "${expected}" ] \
      || record_failure \
        "package manifest ${key} must equal ${expected}"
  fi
}

load_bundle_contract() {
  local manifest="${ROOT}/package-manifest.env"
  if [ ! -f "${manifest}" ]; then
    record_failure "package-manifest.env is missing"
    return 0
  fi

  validate_manifest_schema
  if read_manifest_scalar BUNDLE_VERSION; then
    BUNDLE_VERSION="${MANIFEST_VALUE}"
    [[ "${BUNDLE_VERSION}" =~ ^v[0-9]+\.[0-9]+\.[0-9]+([-+][0-9A-Za-z.-]+)?$ ]] \
      || record_failure "package manifest BUNDLE_VERSION must be a canonical v-prefixed semantic version"
  fi
  if read_manifest_scalar BUNDLE_VARIANT; then
    BUNDLE_VARIANT="${MANIFEST_VALUE}"
  fi
  read_manifest_boolean BUNDLE_GPU_SUPPORT BUNDLE_GPU_SUPPORT
  read_manifest_boolean BUNDLE_WORKER_SUPPORT BUNDLE_WORKER_SUPPORT
  read_manifest_boolean BUNDLE_POSTGRES_RUNTIME BUNDLE_POSTGRES_RUNTIME
  expect_manifest_value DEPLOY_MODE native
  expect_manifest_value MEDIA_CORE_BINARY_PATH binaries/media-core-linux-amd64
  expect_manifest_value MEDIA_AGENT_BINARY_PATH binaries/media-agent-linux-amd64
  expect_manifest_value MEDIA_GATEWAY_BINARY_PATH binaries/media-gateway-linux-amd64
  expect_manifest_value STREAMSERVER_CONFIG_BINARY_PATH binaries/streamserver-config-linux-amd64
  expect_manifest_value MEDIA_CORE_UI_PATH ui/media-core
  expect_manifest_value FFMPEG_CPU_BINARY_PATH runtime/ffmpeg/cpu/bin/ffmpeg
  expect_manifest_value FFPROBE_CPU_BINARY_PATH runtime/ffmpeg/cpu/bin/ffprobe
  expect_manifest_value FFMPEG_CPU_LIB_PATH runtime/ffmpeg/cpu/lib
  expect_manifest_value FFMPEG_GPU_BINARY_PATH runtime/ffmpeg/gpu/bin/ffmpeg
  expect_manifest_value FFPROBE_GPU_BINARY_PATH runtime/ffmpeg/gpu/bin/ffprobe
  expect_manifest_value FFMPEG_GPU_LIB_PATH runtime/ffmpeg/gpu/lib
  expect_manifest_value ZLM_BINARY_PATH runtime/zlm/MediaServer
  expect_manifest_value ZLM_DEFAULT_PEM_PATH runtime/zlm/default.pem
  expect_manifest_value ZLM_LIB_PATH runtime/zlm/lib
  expect_manifest_value POSTGRES_RUNTIME_PATH runtime/postgres
  expect_manifest_value POSTGRES_BIN_PATH runtime/postgres/bin
  expect_manifest_value POSTGRES_LIB_PATH runtime/postgres/lib
  expect_manifest_value POSTGRES_EXTENSION_MANIFEST_PATH \
    runtime/postgres/postgres-extension-manifest.tsv

  case "${BUNDLE_VARIANT}" in
    cpu-only)
      [ "${BUNDLE_GPU_SUPPORT}" = false ] \
        || record_failure "cpu-only manifest must set BUNDLE_GPU_SUPPORT=false"
      [ "${BUNDLE_WORKER_SUPPORT}" = true ] \
        || record_failure "cpu-only manifest must set BUNDLE_WORKER_SUPPORT=true"
      [ "${BUNDLE_POSTGRES_RUNTIME}" = true ] \
        || record_failure "cpu-only manifest must set BUNDLE_POSTGRES_RUNTIME=true"
      ;;
    gpu-enabled)
      [ "${BUNDLE_GPU_SUPPORT}" = true ] \
        || record_failure "gpu-enabled manifest must set BUNDLE_GPU_SUPPORT=true"
      [ "${BUNDLE_WORKER_SUPPORT}" = true ] \
        || record_failure "gpu-enabled manifest must set BUNDLE_WORKER_SUPPORT=true"
      [ "${BUNDLE_POSTGRES_RUNTIME}" = true ] \
        || record_failure "gpu-enabled manifest must set BUNDLE_POSTGRES_RUNTIME=true"
      ;;
    control-plane-minimal)
      [ "${BUNDLE_GPU_SUPPORT}" = false ] \
        || record_failure "control-plane-minimal manifest must set BUNDLE_GPU_SUPPORT=false"
      [ "${BUNDLE_WORKER_SUPPORT}" = false ] \
        || record_failure "control-plane-minimal manifest must set BUNDLE_WORKER_SUPPORT=false"
      [ "${BUNDLE_POSTGRES_RUNTIME}" = false ] \
        || record_failure "control-plane-minimal manifest must set BUNDLE_POSTGRES_RUNTIME=false"
      ;;
    *)
      record_failure "package manifest BUNDLE_VARIANT is not a supported native variant"
      ;;
  esac
}

require_business_executable() {
  local label="$1"
  local path="$2"
  if [ -f "${path}" ] && [ -x "${path}" ]; then
    record_ok "${label} executable is packaged"
  else
    record_failure "${label} executable missing: ${path}"
  fi
}

validate_bundle_shape() {
  local cpu_root="${ROOT}/runtime/ffmpeg/cpu"
  local gpu_root="${ROOT}/runtime/ffmpeg/gpu"
  local zlm_root="${ROOT}/runtime/zlm"
  local postgres_root="${ROOT}/runtime/postgres"

  require_business_executable media-core \
    "${ROOT}/binaries/media-core-linux-amd64"
  require_business_executable media-agent \
    "${ROOT}/binaries/media-agent-linux-amd64"
  require_business_executable media-gateway \
    "${ROOT}/binaries/media-gateway-linux-amd64"
  require_business_executable streamserver-config \
    "${ROOT}/binaries/streamserver-config-linux-amd64"
  if [ -f "${ROOT}/ui/media-core/index.html" ]; then
    record_ok "media-core UI is packaged"
  else
    record_failure "media-core UI index is missing"
  fi

  if [ "${BUNDLE_WORKER_SUPPORT}" = true ]; then
    if [ -x "${cpu_root}/bin/ffmpeg" ] \
      && [ -x "${cpu_root}/bin/ffprobe" ] \
      && [ -d "${cpu_root}/lib" ]; then
      record_ok "manifest-declared CPU FFmpeg runtime is packaged"
    else
      record_failure \
        "manifest declares worker support but CPU FFmpeg runtime is missing"
    fi
    if [ -x "${zlm_root}/MediaServer" ] \
      && [ -f "${zlm_root}/default.pem" ] \
      && [ -d "${zlm_root}/lib" ]; then
      record_ok "manifest-declared ZLMediaKit runtime is packaged"
    else
      record_failure \
        "manifest declares worker support but ZLMediaKit runtime is missing"
    fi
  elif [ -e "${cpu_root}" ] || [ -e "${zlm_root}" ]; then
    record_failure "manifest disables worker support but worker runtime is present"
  else
    record_ok "worker runtime is absent when worker support is disabled"
  fi

  if [ "${BUNDLE_GPU_SUPPORT}" = true ]; then
    if [ -x "${gpu_root}/bin/ffmpeg" ] \
      && [ -x "${gpu_root}/bin/ffprobe" ] \
      && [ -d "${gpu_root}/lib" ]; then
      record_ok "manifest-declared GPU FFmpeg runtime is packaged"
    else
      record_failure \
        "manifest declares GPU support but GPU FFmpeg runtime is missing"
    fi
  elif [ -e "${gpu_root}" ]; then
    record_failure "manifest disables GPU support but GPU runtime is present"
  else
    record_ok "GPU runtime is absent when GPU support is disabled"
  fi

  if [ "${BUNDLE_POSTGRES_RUNTIME}" = true ]; then
    if [ -d "${postgres_root}" ] \
      && [ -d "${postgres_root}/bin" ] \
      && [ -d "${postgres_root}/lib" ] \
      && [ -f "${postgres_root}/postgres-extension-manifest.tsv" ]; then
      record_ok "manifest-declared PostgreSQL runtime is packaged"
    else
      record_failure \
        "manifest declares PostgreSQL runtime but it is missing"
    fi
  elif [ -e "${postgres_root}" ]; then
    record_failure \
      "manifest disables PostgreSQL runtime but PostgreSQL files are present"
  else
    record_ok "PostgreSQL runtime is absent when it is disabled"
  fi
}

bounded_stream_to_file() {
  local destination="$1"
  local byte_limit="$2"
  local ready_file="${3:-}"
  if [ -n "${ready_file}" ]; then
    printf '%s\n' "${BASHPID}" >"${ready_file}"
  fi
  python3 /dev/fd/3 "${destination}" "${byte_limit}" 3<<'PY'
import pathlib
import sys

destination = pathlib.Path(sys.argv[1])
limit = int(sys.argv[2])
written = 0
overflow = False
with destination.open("wb") as output:
    while True:
        chunk = sys.stdin.buffer.read(min(1024 * 1024, limit - written + 1))
        if not chunk:
            break
        allowed = min(len(chunk), limit - written)
        if allowed > 0:
            output.write(chunk[:allowed])
            written += allowed
        if allowed != len(chunk) or written >= limit:
            overflow = bool(chunk[allowed:])
            if not overflow:
                overflow = bool(sys.stdin.buffer.read(1))
            break
if overflow:
    raise SystemExit(90)
PY
}

terminate_bounded_group() {
  local process_pid="$1"
  local process_pgid="$2"
  local cleanup_status=0
  if [ "${process_pid}" = "${process_pgid}" ]; then
    kill -TERM -- "-${process_pgid}" >/dev/null 2>&1 || true
    for _ in $(seq 1 50); do
      process_group_has_live_members "${process_pgid}" || break
      sleep 0.1
    done
    if process_group_has_live_members "${process_pgid}"; then
      kill -KILL -- "-${process_pgid}" >/dev/null 2>&1 || true
    fi
    for _ in $(seq 1 50); do
      process_group_has_live_members "${process_pgid}" || break
      sleep 0.1
    done
    process_group_has_live_members "${process_pgid}" && cleanup_status=1
  else
    cleanup_status=1
  fi
  return "${cleanup_status}"
}

fifo_holder_pids() {
  local fifo_path="$1"
  local fifo_identity fifo_uid process_dir process_pid process_uid fd_path
  fifo_identity="$(stat -Lc '%d:%i' "${fifo_path}")" || return 1
  fifo_uid="$(stat -Lc '%u' "${fifo_path}")" || return 1
  for process_dir in /proc/[0-9]*; do
    process_pid="${process_dir##*/}"
    [ "${process_pid}" != "$$" ] && [ "${process_pid}" != "${BASHPID}" ] \
      || continue
    process_uid="$(awk '/^Uid:/ { print $2; exit }' \
      "${process_dir}/status" 2>/dev/null || true)"
    [ "${process_uid}" = "${fifo_uid}" ] || continue
    for fd_path in "${process_dir}"/fd/*; do
      [ -e "${fd_path}" ] || continue
      if [ "$(stat -Lc '%d:%i' "${fd_path}" 2>/dev/null || true)" \
        = "${fifo_identity}" ]; then
        printf '%s %s\n' \
          "${process_pid}" \
          "$(process_starttime "${process_pid}" 2>/dev/null || true)"
        break
      fi
    done
  done
}

terminate_fifo_holders() {
  local fifo_path="$1"
  local holder holder_pid holder_start current_start cleanup_status=0
  local -a holders
  mapfile -t holders < <(fifo_holder_pids "${fifo_path}")
  for holder in "${holders[@]}"; do
    read -r holder_pid holder_start <<<"${holder}"
    [ -n "${holder_pid}" ] && [ -n "${holder_start}" ] || continue
    current_start="$(process_starttime "${holder_pid}" 2>/dev/null || true)"
    [ "${current_start}" = "${holder_start}" ] || continue
    kill -TERM "${holder_pid}" >/dev/null 2>&1 || true
  done
  for _ in $(seq 1 20); do
    [ -z "$(fifo_holder_pids "${fifo_path}")" ] && break
    sleep 0.1
  done
  mapfile -t holders < <(fifo_holder_pids "${fifo_path}")
  for holder in "${holders[@]}"; do
    read -r holder_pid holder_start <<<"${holder}"
    current_start="$(process_starttime "${holder_pid}" 2>/dev/null || true)"
    [ "${current_start}" = "${holder_start}" ] || continue
    kill -KILL "${holder_pid}" >/dev/null 2>&1 || true
  done
  for _ in $(seq 1 50); do
    [ -z "$(fifo_holder_pids "${fifo_path}")" ] && break
    sleep 0.1
  done
  [ -z "$(fifo_holder_pids "${fifo_path}")" ] || cleanup_status=1
  return "${cleanup_status}"
}

run_bounded_capture() {
  local output_file="$1"
  local byte_limit="$2"
  local timeout_seconds="$3"
  shift 3
  local fifo_path gate_path command_ready reader_ready completed_pid=""
  local command_pid command_pgid="" command_starttime="" reader_pid
  local reader_starttime="" command_ready_value="" reader_ready_value=""
  local first_status=0 command_status=0 reader_status=0 cleanup_status=0
  fifo_path="$(mktemp "${RUN_WORK}/capture-fifo.XXXXXXXX")"
  rm -f -- "${fifo_path}"
  mkfifo -m 600 "${fifo_path}"
  gate_path="$(mktemp "${RUN_WORK}/capture-gate.XXXXXXXX")"
  rm -f -- "${gate_path}"
  mkfifo -m 600 "${gate_path}"
  command_ready="$(mktemp "${RUN_WORK}/command-ready.XXXXXXXX")"
  reader_ready="$(mktemp "${RUN_WORK}/reader-ready.XXXXXXXX")"
  : >"${command_ready}"
  : >"${reader_ready}"
  rm -f -- "${output_file}"
  setsid bash --noprofile --norc -e -u -o pipefail -c '
    command_ready="$1"
    gate_path="$2"
    kill_after_seconds="$3"
    timeout_seconds="$4"
    shift 4
    printf "%s\n" "${BASHPID}" >"${command_ready}"
    IFS= read -r start_token <"${gate_path}"
    [ "${start_token}" = start ]
    exec timeout --signal=TERM \
      --kill-after="${kill_after_seconds}s" "${timeout_seconds}s" "$@"
  ' verifier-capture-wrapper \
    "${command_ready}" "${gate_path}" \
    "${CAPTURE_KILL_AFTER_SEC}" "${timeout_seconds}" "$@" \
    >"${fifo_path}" 2>&1 &
  command_pid=$!
  bounded_stream_to_file "${output_file}" "${byte_limit}" \
    "${reader_ready}" <"${fifo_path}" &
  reader_pid=$!
  for _ in $(seq 1 500); do
    IFS= read -r command_ready_value <"${command_ready}" || true
    IFS= read -r reader_ready_value <"${reader_ready}" || true
    command_pgid="$(process_pgid "${command_pid}" 2>/dev/null || true)"
    command_starttime="$(process_starttime "${command_pid}" 2>/dev/null || true)"
    reader_starttime="$(process_starttime "${reader_pid}" 2>/dev/null || true)"
    [ "${command_ready_value}" = "${command_pid}" ] \
      && [ "${reader_ready_value}" = "${reader_pid}" ] \
      && [ "${command_pgid}" = "${command_pid}" ] \
      && [ -n "${command_starttime}" ] \
      && [ -n "${reader_starttime}" ] \
      && process_is_live_non_zombie "${command_pid}" \
      && process_is_live_non_zombie "${reader_pid}" \
      && break
    sleep 0.01
  done
  if [ "${command_ready_value}" != "${command_pid}" ] \
    || [ "${reader_ready_value}" != "${reader_pid}" ] \
    || [ "${command_pgid}" != "${command_pid}" ] \
    || [ -z "${command_starttime}" ] \
    || [ -z "${reader_starttime}" ] \
    || ! process_is_live_non_zombie "${command_pid}" \
    || ! process_is_live_non_zombie "${reader_pid}"; then
    kill -KILL "${command_pid}" "${reader_pid}" >/dev/null 2>&1 || true
    wait "${command_pid}" 2>/dev/null || true
    wait "${reader_pid}" 2>/dev/null || true
    terminate_fifo_holders "${fifo_path}" >/dev/null 2>&1 || true
    rm -f -- \
      "${fifo_path}" "${gate_path}" "${command_ready}" "${reader_ready}"
    return 125
  fi
  if ! printf 'start\n' >"${gate_path}"; then
    terminate_bounded_group "${command_pid}" "${command_pgid}" || true
    kill -KILL "${reader_pid}" >/dev/null 2>&1 || true
    wait "${command_pid}" 2>/dev/null || true
    wait "${reader_pid}" 2>/dev/null || true
    terminate_fifo_holders "${fifo_path}" >/dev/null 2>&1 || true
    rm -f -- \
      "${fifo_path}" "${gate_path}" "${command_ready}" "${reader_ready}"
    return 125
  fi
  rm -f -- "${gate_path}" "${command_ready}" "${reader_ready}"

  if wait -n -p completed_pid "${command_pid}" "${reader_pid}"; then
    first_status=0
  else
    first_status=$?
  fi
  if [ "${completed_pid}" = "${reader_pid}" ]; then
    reader_status="${first_status}"
    if [ "${reader_status}" -ne 0 ]; then
      terminate_bounded_group "${command_pid}" "${command_pgid}" \
        || cleanup_status=1
    fi
    if wait "${command_pid}"; then command_status=0; else command_status=$?; fi
    terminate_bounded_group "${command_pid}" "${command_pgid}" \
      || cleanup_status=1
  else
    command_status="${first_status}"
    terminate_bounded_group "${command_pid}" "${command_pgid}" \
      || cleanup_status=1
    for _ in $(seq 1 50); do
      process_is_live_non_zombie "${reader_pid}" || break
      sleep 0.1
    done
    if process_is_live_non_zombie "${reader_pid}"; then
      terminate_fifo_holders "${fifo_path}" || cleanup_status=1
      for _ in $(seq 1 20); do
        process_is_live_non_zombie "${reader_pid}" || break
        sleep 0.1
      done
      if process_is_live_non_zombie "${reader_pid}"; then
        kill -KILL "${reader_pid}" >/dev/null 2>&1 || true
      fi
      wait "${reader_pid}" 2>/dev/null || true
      reader_status=91
    elif wait "${reader_pid}"; then
      reader_status=0
    else
      reader_status=$?
    fi
  fi
  rm -f -- \
    "${fifo_path}" "${gate_path}" "${command_ready}" "${reader_ready}"
  if [ "${reader_status}" -ne 0 ]; then return 90; fi
  if [ "${cleanup_status}" -ne 0 ]; then return 91; fi
  return "${command_status}"
}

run_capture() {
  local label="$1"
  local output_file command_status output_truncated=0
  shift
  append ""
  append "### ${label}"
  append "\`\`\`"
  output_file="$(mktemp "${RUN_WORK}/command-output.XXXXXXXX")"
  if run_bounded_capture "${output_file}" "${COMMAND_OUTPUT_LIMIT_BYTES}" \
      "${SMOKE_COMMAND_TIMEOUT_SEC}" "$@"; then
    command_status=0
  else
    command_status=$?
  fi
  append_capped_output \
    "${output_file}" "${COMMAND_OUTPUT_LIMIT_BYTES}" "${command_status}" \
    || output_truncated=1
  if [ "${command_status}" -eq 0 ] && [ "${output_truncated}" -eq 0 ]; then
    append "\`\`\`"
    record_ok "${label}"
  else
    append "\`\`\`"
    record_failure "${label}"
  fi
}

run_shell() {
  local label="$1"
  local output_file command_status output_truncated=0
  shift
  append ""
  append "### ${label}"
  append "\`\`\`"
  output_file="$(mktemp "${RUN_WORK}/shell-output.XXXXXXXX")"
  if run_bounded_capture "${output_file}" "${COMMAND_OUTPUT_LIMIT_BYTES}" \
      "${SMOKE_SHELL_TIMEOUT_SEC}" \
      env VERIFY_ROOT="${ROOT:-}" \
        bash --noprofile --norc -e -u -o pipefail -c "$*"; then
    command_status=0
  else
    command_status=$?
  fi
  append_capped_output \
    "${output_file}" "${COMMAND_OUTPUT_LIMIT_BYTES}" "${command_status}" \
    || output_truncated=1
  if [ "${command_status}" -eq 0 ] && [ "${output_truncated}" -eq 0 ]; then
    append "\`\`\`"
    record_ok "${label}"
  else
    append "\`\`\`"
    record_failure "${label}"
  fi
}

validate_and_extract_archive() {
  python3 - "${BUNDLE}" "${RUN_WORK}/extract" <<'PY'
import os
import pathlib
import posixpath
import shutil
import stat
import sys
import tarfile

archive_path = pathlib.Path(sys.argv[1])
extract_root = pathlib.Path(sys.argv[2])

MAX_ARCHIVE_MEMBERS = 250_000
MAX_ARCHIVE_FILE_SIZE = 16 * 1024**3
MAX_ARCHIVE_TOTAL_SIZE = 48 * 1024**3
MAX_ARCHIVE_PATH_BYTES = 1024
MAX_ARCHIVE_COMPONENT_BYTES = 255
MAX_ARCHIVE_COMPRESSION_RATIO = 200


def reject(message: str) -> None:
    raise ValueError(message)


try:
    with tarfile.open(archive_path, mode="r:gz") as archive:
        members: list[tarfile.TarInfo] = []
        probe_logical_size = 0
        member = archive.next()
        while member is not None:
            members.append(member)
            if len(members) > MAX_ARCHIVE_MEMBERS:
                reject(f"archive contains more than {MAX_ARCHIVE_MEMBERS} members")
            if member.isreg():
                if member.size > MAX_ARCHIVE_FILE_SIZE:
                    reject("archive regular file exceeds 16 GiB")
                probe_logical_size += member.size
                if probe_logical_size > MAX_ARCHIVE_TOTAL_SIZE:
                    reject("archive logical regular-file size exceeds 48 GiB")
            member = archive.next()
        if not members:
            reject("archive is empty")
        seen: set[str] = set()
        top_levels: set[str] = set()
        expected_inventory: dict[str, tuple[str, int, str]] = {}
        logical_size = 0
        for member in members:
            raw_name = member.name
            if not raw_name or raw_name.startswith("/"):
                reject(f"archive contains absolute or empty member path: {raw_name!r}")
            if any(ord(character) < 32 or ord(character) == 127 for character in raw_name):
                reject(f"archive contains control characters in member path: {raw_name!r}")
            canonical = raw_name.rstrip("/")
            if not canonical or posixpath.normpath(canonical) != canonical:
                reject(f"archive contains non-canonical member path: {raw_name}")
            parts = pathlib.PurePosixPath(canonical).parts
            if not parts or any(part in ("", ".", "..") for part in parts):
                reject(f"archive contains parent traversal member path: {raw_name}")
            if len(canonical.encode("utf-8")) > MAX_ARCHIVE_PATH_BYTES:
                reject("archive member path exceeds 1024 bytes")
            if any(
                len(part.encode("utf-8")) > MAX_ARCHIVE_COMPONENT_BYTES
                for part in parts
            ):
                reject("archive member path component exceeds 255 bytes")
            if canonical in seen:
                reject(f"archive contains duplicate member path: {canonical}")
            seen.add(canonical)
            top_levels.add(parts[0])

            if not (member.isdir() or member.isreg() or member.issym() or member.islnk()):
                reject(f"archive contains unsupported special member: {canonical}")
            mode = member.mode & 0o7777
            if not member.issym() and mode & 0o7022:
                if member.islnk():
                    reject(
                        f"archive contains dangerous hardlink mode: "
                        f"{canonical} {mode:04o}"
                    )
                reject(
                    f"archive contains dangerous regular/directory mode: "
                    f"{canonical} {mode:04o}"
                )
            if member.isreg():
                if member.size > MAX_ARCHIVE_FILE_SIZE:
                    reject("archive regular file exceeds 16 GiB")
                logical_size += member.size
                if logical_size > MAX_ARCHIVE_TOTAL_SIZE:
                    reject("archive logical regular-file size exceeds 48 GiB")
            if member.issym() or member.islnk():
                link_name = member.linkname
                if not link_name or posixpath.isabs(link_name):
                    reject(f"archive link target escapes root: {canonical} -> {link_name}")
                if member.issym():
                    target = posixpath.normpath(
                        posixpath.join(posixpath.dirname(canonical), link_name)
                    )
                else:
                    target = posixpath.normpath(link_name)
                if target == ".." or target.startswith("../"):
                    reject(f"archive link target escapes root: {canonical} -> {link_name}")
                target_parts = pathlib.PurePosixPath(target).parts
                if not target_parts or target_parts[0] != parts[0]:
                    reject(f"archive link target escapes root: {canonical} -> {link_name}")

            if member.isdir():
                member_type = "directory"
            elif member.isreg():
                member_type = "regular"
            elif member.issym():
                member_type = "symlink"
            else:
                member_type = "hardlink"
            expected_inventory[canonical] = (member_type, mode, member.linkname)

        def resolve_hardlink_target(name: str) -> str:
            current = name
            visited = {name}
            while True:
                current_type, _, current_link = expected_inventory[current]
                if current_type != "hardlink":
                    if current_type != "regular":
                        reject(f"archive hardlink target is not regular: {name}")
                    return current
                target = posixpath.normpath(current_link)
                if target not in expected_inventory:
                    reject(f"archive hardlink target is missing: {name} -> {current_link}")
                if target in visited:
                    reject(f"archive hardlink target cycle: {name}")
                visited.add(target)
                current = target

        for name, (member_type, mode, _) in expected_inventory.items():
            if member_type != "hardlink":
                continue
            target_name = resolve_hardlink_target(name)
            _, target_mode, _ = expected_inventory[target_name]
            if mode != target_mode:
                reject(
                    "archive hardlink inode has inconsistent header modes: "
                    f"{name} {mode:04o} != {target_name} {target_mode:04o}"
                )

        if len(top_levels) != 1:
            reject("archive must contain exactly one top-level directory")
        archive_size = max(archive_path.stat().st_size, 1)
        if logical_size > archive_size * MAX_ARCHIVE_COMPRESSION_RATIO:
            reject("archive compression ratio exceeds safe limit")
        required_free = logical_size + max(1024**3, logical_size // 10)
        if shutil.disk_usage(extract_root).free < required_free:
            reject("insufficient free disk space for bounded archive extraction")
        top_dir = next(iter(top_levels))
        previous_umask = os.umask(0)
        try:
            archive.extractall(path=extract_root, members=members, filter="data")
        finally:
            os.umask(previous_umask)

        actual_paths: set[str] = set()
        for directory, directories, files in os.walk(extract_root, followlinks=False):
            for entry in directories + files:
                actual_paths.add(
                    (pathlib.Path(directory) / entry)
                    .relative_to(extract_root)
                    .as_posix()
                )
        if actual_paths != set(expected_inventory):
            reject("archive extraction inventory mismatch: path set changed")

        for name, (expected_type, expected_mode, link_name) in expected_inventory.items():
            path = extract_root / name
            path_stat = path.lstat()
            if expected_type == "directory":
                type_matches = stat.S_ISDIR(path_stat.st_mode)
            elif expected_type == "regular":
                type_matches = stat.S_ISREG(path_stat.st_mode)
            elif expected_type == "symlink":
                type_matches = stat.S_ISLNK(path_stat.st_mode)
                if type_matches and os.readlink(path) != link_name:
                    reject(f"archive extraction inventory mismatch: link {name}")
            else:
                type_matches = stat.S_ISREG(path_stat.st_mode)
                target = extract_root / link_name
                if type_matches and not os.path.samefile(path, target):
                    reject(f"archive extraction inventory mismatch: hardlink {name}")
            if not type_matches:
                reject(f"archive extraction inventory mismatch: type {name}")

        for name, (expected_type, expected_mode, _) in expected_inventory.items():
            if expected_type in ("directory", "regular"):
                # Restore safe modes only through canonical owning entries.
                # A hardlink is another path to the same inode and must never be
                # chmodded independently because that would mutate its target.
                path = extract_root / name
                os.chmod(path, expected_mode, follow_symlinks=False)

        inode_modes: dict[tuple[int, int], tuple[int, str]] = {}
        for name, (expected_type, expected_mode, _) in expected_inventory.items():
            if expected_type == "symlink":
                continue
            path_stat = (extract_root / name).lstat()
            actual_mode = stat.S_IMODE(path_stat.st_mode)
            if actual_mode & 0o7022:
                reject(
                    f"archive extraction produced dangerous final mode: "
                    f"{name} {actual_mode:04o}"
                )
            if actual_mode != expected_mode:
                reject(f"archive extraction inventory mismatch: mode {name}")
            if expected_type in ("regular", "hardlink"):
                inode_key = (path_stat.st_dev, path_stat.st_ino)
                previous = inode_modes.get(inode_key)
                if previous is not None and previous[0] != expected_mode:
                    reject(
                        "archive hardlink inode has inconsistent final modes: "
                        f"{previous[1]} {previous[0]:04o} != "
                        f"{name} {expected_mode:04o}"
                    )
                inode_modes[inode_key] = (expected_mode, name)
except (OSError, tarfile.TarError, ValueError) as error:
    print(f"archive validation failed: {error}", file=sys.stderr)
    raise SystemExit(1)

root = extract_root / top_dir
try:
    root_stat = root.lstat()
except OSError as error:
    print(f"archive top-level directory is missing: {error}", file=sys.stderr)
    raise SystemExit(1)
if not root.is_dir() or root.is_symlink():
    print("archive top-level member must be a real directory", file=sys.stderr)
    raise SystemExit(1)
print(top_dir)
PY
}

validate_checksum_coverage() {
  python3 - "${ROOT}" <<'PY'
import hashlib
import os
import pathlib
import re
import stat
import sys

root = pathlib.Path(sys.argv[1])
manifest = root / "SHA256SUMS"
line_pattern = re.compile(r"^([0-9a-f]{64}) ([ *])(.+)$")

try:
    lines = manifest.read_text(encoding="utf-8").splitlines()
except OSError as error:
    print(f"cannot read SHA256SUMS: {error}", file=sys.stderr)
    raise SystemExit(1)

listed: dict[str, str] = {}
for line_number, line in enumerate(lines, start=1):
    match = line_pattern.fullmatch(line)
    if match is None:
        print(f"invalid SHA256SUMS line {line_number}", file=sys.stderr)
        raise SystemExit(1)
    digest, _, name = match.groups()
    pure = pathlib.PurePosixPath(name)
    if (
        not name
        or pure.is_absolute()
        or any(part in ("", ".", "..") for part in pure.parts)
        or pure.as_posix() != name
        or name == "SHA256SUMS"
    ):
        print(f"unsafe SHA256SUMS path: {name!r}", file=sys.stderr)
        raise SystemExit(1)
    if name in listed:
        print(f"duplicate SHA256SUMS path: {name}", file=sys.stderr)
        raise SystemExit(1)
    listed[name] = digest

actual: set[str] = set()
for directory, _, files in os.walk(root, followlinks=False):
    for filename in files:
        path = pathlib.Path(directory) / filename
        relative = path.relative_to(root).as_posix()
        try:
            mode = path.lstat().st_mode
        except OSError as error:
            print(f"cannot stat bundle file {relative}: {error}", file=sys.stderr)
            raise SystemExit(1)
        if stat.S_ISREG(mode) and relative != "SHA256SUMS":
            actual.add(relative)

listed_names = set(listed)
if actual != listed_names:
    missing = sorted(listed_names - actual)
    extra = sorted(actual - listed_names)
    if missing:
        print(
            f"listed files missing from bundle ({len(missing)}): "
            + ", ".join(missing[:10]),
            file=sys.stderr,
        )
    if extra:
        print(
            f"regular files missing from SHA256SUMS ({len(extra)}): "
            + ", ".join(extra[:10]),
            file=sys.stderr,
        )
    raise SystemExit(1)

for name, expected in sorted(listed.items()):
    digest = hashlib.sha256()
    with (root / name).open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    if digest.hexdigest() != expected:
        print(f"SHA-256 mismatch: {name}", file=sys.stderr)
        raise SystemExit(1)

print(f"verified {len(listed)} regular files with complete SHA256SUMS coverage")
PY
}

append "# StreamServer Native Target Verification"
append ""
append "- verified_at: $(date -u '+%Y-%m-%dT%H:%M:%SZ')"
append "- host: $(hostname)"
append "- host_ips: $(hostname -I 2>/dev/null || true)"
append "- uname: $(uname -a)"
append "- bundle: $(basename "${BUNDLE}")"
append "- docker_present: $(command -v docker >/dev/null 2>&1 && echo yes || echo no)"
append "- docker_required: no"

if [ "${BASH_VERSINFO[0]}" -lt 5 ]; then
  record_failure "Bash 5 or newer is required for bounded process capture"
fi
for bootstrap_command in python3 timeout setsid mkfifo truncate; do
  command -v "${bootstrap_command}" >/dev/null 2>&1 \
    || record_failure "bounded capture bootstrap command missing: ${bootstrap_command}"
done
if [ "${FAILURES}" -ne 0 ]; then
  write_summary
  printf '%s\n' "${REPORT}"
  exit "${FAILURES}"
fi

section "Host Prerequisites"
run_shell "systemctl present" "command -v systemctl"
run_shell "sha256sum present" "command -v sha256sum"
run_shell "file present" "command -v file"
run_shell "readelf present" "command -v readelf"
run_shell "ldd present" "command -v ldd"
run_shell "curl present" "command -v curl"
run_shell "openssl present" "command -v openssl"
run_shell "python3 present" "command -v python3"
run_shell "setsid present" "command -v setsid"
run_shell "mkfifo present" "command -v mkfifo"
run_capture "GNU timeout present" timeout --version
if command -v docker >/dev/null 2>&1; then
  append "[INFO] Docker exists on host, but verification will not call it."
else
  append "[INFO] Docker is absent; this is acceptable for native runtime verification."
fi

section "Extract Bundle"
append ""
append "### secure archive validation and extraction"
append "\`\`\`"
archive_validation_output="$(mktemp "${RUN_WORK}/archive-validation.XXXXXXXX")"
if top_dir="$(validate_and_extract_archive 2>"${archive_validation_output}")"; then
  append_capped_output \
    "${archive_validation_output}" "${COMMAND_OUTPUT_LIMIT_BYTES}" \
    || record_failure "archive validation output exceeded a safety limit"
  append "top_dir=${top_dir}"
  append "\`\`\`"
  record_ok "secure archive validation and extraction"
else
  append_capped_output \
    "${archive_validation_output}" "${COMMAND_OUTPUT_LIMIT_BYTES}" || true
  append "\`\`\`"
  record_failure "secure archive validation and extraction"
  write_summary
  printf '%s\n' "${REPORT}"
  exit "${FAILURES}"
fi
ROOT="${RUN_WORK}/extract/${top_dir}"
append "- extracted_root: ${ROOT}"

section "Package Shape"
append ""
append "### complete SHA256SUMS coverage"
append "\`\`\`"
checksum_validation_output="$(mktemp "${RUN_WORK}/checksum-validation.XXXXXXXX")"
if validate_checksum_coverage >"${checksum_validation_output}" 2>&1; then
  append_capped_output \
    "${checksum_validation_output}" "${COMMAND_OUTPUT_LIMIT_BYTES}" \
    || record_failure "checksum validation output exceeded a safety limit"
  append "\`\`\`"
  record_ok "SHA256SUMS covers exactly all regular files"
else
  append_capped_output \
    "${checksum_validation_output}" "${COMMAND_OUTPUT_LIMIT_BYTES}" || true
  append "\`\`\`"
  record_failure "SHA256SUMS does not cover exactly all regular files"
fi
load_bundle_contract
validate_bundle_shape
validate_bundle_identity "${top_dir}"
if [ -n "$(find "${ROOT}" \
  \( -path '*/images/*' -o -name compose.yml -o -name docker-compose.yml \
    -o -name streamserver-compose \) -print -quit)" ]; then
  record_failure "native bundle contains Docker or Compose runtime assets"
else
  record_ok "no Docker or Compose runtime assets"
fi
if [ -d "${ROOT}/tools/docker" ]; then
  record_failure "native bundle contains tools/docker"
else
  record_ok "no tools/docker directory"
fi
abort_gate_if_failed "package structure and integrity gate"

check_static_binary() {
  local label="$1"
  local path="$2"
  local file_output ldd_output file_status=0 ldd_status=0
  local output_truncated=0
  [ -x "${path}" ] || { record_failure "${label} executable missing: ${path}"; return; }

  append ""
  append "### ${label} file"
  append "\`\`\`"
  file_output="$(mktemp "${RUN_WORK}/static-file.XXXXXXXX")"
  if run_bounded_capture "${file_output}" "${COMMAND_OUTPUT_LIMIT_BYTES}" \
      "${SMOKE_COMMAND_TIMEOUT_SEC}" env LC_ALL=C file -Lb "${path}"; then
    file_status=0
  else
    file_status=$?
  fi
  if [ "${file_status}" -ne 0 ]; then
    append_capped_output "${file_output}" "${COMMAND_OUTPUT_LIMIT_BYTES}" \
      || true
    record_failure "${label} file inspection failed"
    return
  fi
  if ! grep -Eq '^ELF 64-bit LSB (pie )?executable, x86-64,' \
      "${file_output}"; then
    append_capped_output "${file_output}" "${COMMAND_OUTPUT_LIMIT_BYTES}" \
      || true
    append "\`\`\`"
    record_failure "${label} is not a Linux x86-64 ELF executable"
    return
  fi
  if ! grep -Eiq 'statically linked|static-pie linked' "${file_output}"; then
    append_capped_output "${file_output}" "${COMMAND_OUTPUT_LIMIT_BYTES}" \
      || true
    append "\`\`\`"
    record_failure "${label} is not statically linked"
    return
  fi
  append_capped_output "${file_output}" "${COMMAND_OUTPUT_LIMIT_BYTES}" \
    || output_truncated=1
  append "\`\`\`"
  record_ok "${label} file identity is Linux x86-64 static ELF"

  append ""
  append "### ${label} ldd"
  append "\`\`\`"
  ldd_output="$(mktemp "${RUN_WORK}/static-ldd.XXXXXXXX")"
  if run_bounded_capture "${ldd_output}" "${COMMAND_OUTPUT_LIMIT_BYTES}" \
      "${SMOKE_COMMAND_TIMEOUT_SEC}" env LC_ALL=C ldd "${path}"; then
    ldd_status=0
  else
    ldd_status=$?
  fi
  if grep -Eiq 'not a dynamic executable|statically linked' "${ldd_output}" \
    && [ "${ldd_status}" -ne 90 ] && [ "${ldd_status}" -ne 91 ]; then
    record_ok "${label} is static"
  else
    record_failure "${label} static ldd output not recognized"
  fi
  append_capped_output "${ldd_output}" "${COMMAND_OUTPUT_LIMIT_BYTES}" \
    || output_truncated=1
  append "\`\`\`"
  [ "${output_truncated}" -eq 0 ] \
    || record_failure "${label} inspection output exceeded a safety limit"
}

runtime_loader() {
  local lib_dir="$1"
  if [ -x "${lib_dir}/ld-linux-x86-64.so.2" ]; then
    printf '%s\n' "${lib_dir}/ld-linux-x86-64.so.2"
  fi
}

runtime_bounded_capture() {
  local output_file="$1"
  local byte_limit="$2"
  local timeout_seconds="$3"
  local operation="$4"
  local path="$5"
  local lib_dir="$6"
  shift 6
  local loader
  loader="$(runtime_loader "${lib_dir}")"
  case "${operation}" in
    execute)
      if [ -n "${loader}" ]; then
        run_bounded_capture "${output_file}" "${byte_limit}" \
          "${timeout_seconds}" \
          "${loader}" --library-path "${lib_dir}" "${path}" "$@"
      else
        run_bounded_capture "${output_file}" "${byte_limit}" \
          "${timeout_seconds}" \
          env LD_LIBRARY_PATH="${lib_dir}" "${path}" "$@"
      fi
      ;;
    dependencies)
      if [ -n "${loader}" ]; then
        run_bounded_capture "${output_file}" "${byte_limit}" \
          "${timeout_seconds}" \
          env LC_ALL=C "${loader}" --library-path "${lib_dir}" \
            --list "${path}"
      else
        run_bounded_capture "${output_file}" "${byte_limit}" \
          "${timeout_seconds}" \
          env LC_ALL=C LD_LIBRARY_PATH="${lib_dir}" ldd "${path}"
      fi
      ;;
  esac
}

run_runtime_capture() {
  local label="$1"
  local path="$2"
  local lib_dir="$3"
  local output_file command_status output_truncated=0
  shift 3
  append ""
  append "### ${label}"
  append "\`\`\`"
  output_file="$(mktemp "${RUN_WORK}/runtime-output.XXXXXXXX")"
  if runtime_bounded_capture \
      "${output_file}" "${COMMAND_OUTPUT_LIMIT_BYTES}" \
      "${SMOKE_COMMAND_TIMEOUT_SEC}" execute \
      "${path}" "${lib_dir}" "$@"; then
    command_status=0
  else
    command_status=$?
  fi
  append_capped_output "${output_file}" "${COMMAND_OUTPUT_LIMIT_BYTES}" \
    || output_truncated=1
  if [ "${command_status}" -eq 0 ] && [ "${output_truncated}" -eq 0 ]; then
    append "\`\`\`"
    record_ok "${label}"
  else
    append "\`\`\`"
    record_failure "${label}"
  fi
}

inspect_runtime_binary() {
  local label="$1"
  local path="$2"
  local lib_dir="$3"
  local dependency_file dependency_status dependency_missing=0
  local dependency_truncated=0
  shift 3
  [ -x "${path}" ] || { record_failure "${label} executable missing: ${path}"; return; }
  run_capture "${label} file" file "${path}"
  append ""
  append "### ${label} ldd"
  append "\`\`\`"
  dependency_file="$(mktemp "${RUN_WORK}/dependency-output.XXXXXXXX")"
  if runtime_bounded_capture \
      "${dependency_file}" "${COMMAND_OUTPUT_LIMIT_BYTES}" \
      "${SMOKE_COMMAND_TIMEOUT_SEC}" dependencies \
      "${path}" "${lib_dir}"; then
    dependency_status=0
  else
    dependency_status=$?
  fi
  grep -Fq 'not found' "${dependency_file}" && dependency_missing=1
  append_capped_output "${dependency_file}" "${COMMAND_OUTPUT_LIMIT_BYTES}" \
    || dependency_truncated=1
  append "\`\`\`"
  if [ "${dependency_status}" -ne 0 ]; then
    record_failure "${label} ldd failed"
  elif [ "${dependency_truncated}" -eq 1 ]; then
    record_failure "${label} dependency output exceeded the safety limit"
  elif [ "${dependency_missing}" -eq 1 ]; then
    record_failure "${label} has unresolved dynamic dependencies"
  else
    record_ok "${label} dynamic dependencies resolved"
  fi
}

check_runtime_binary() {
  local label="$1"
  local path="$2"
  local lib_dir="$3"
  shift 3
  inspect_runtime_binary "${label}" "${path}" "${lib_dir}"
  run_runtime_capture "${label} version" "${path}" "${lib_dir}" "$@"
}

inspect_shared_object() {
  local label="$1"
  local path="$2"
  local lib_dir="$3"
  local dependency_file dependency_status dependency_missing=0
  local dependency_truncated=0
  append ""
  append "### ${label} dependency inspection"
  append "\`\`\`"
  dependency_file="$(mktemp "${RUN_WORK}/shared-object-output.XXXXXXXX")"
  if runtime_bounded_capture \
      "${dependency_file}" "${COMMAND_OUTPUT_LIMIT_BYTES}" \
      "${SMOKE_COMMAND_TIMEOUT_SEC}" dependencies \
      "${path}" "${lib_dir}"; then
    dependency_status=0
  else
    dependency_status=$?
  fi
  grep -Fq 'not found' "${dependency_file}" && dependency_missing=1
  append_capped_output "${dependency_file}" "${COMMAND_OUTPUT_LIMIT_BYTES}" \
    || dependency_truncated=1
  append "\`\`\`"
  if [ "${dependency_status}" -ne 0 ]; then
    record_failure "${label} dependency inspection failed"
  elif [ "${dependency_truncated}" -eq 1 ]; then
    record_failure "${label} dependency output exceeded the safety limit"
  elif [ "${dependency_missing}" -eq 1 ]; then
    record_failure "${label} has unresolved dynamic dependencies"
  else
    record_ok "${label} dynamic dependencies resolved"
  fi
}

inspect_elf_preexec() {
  local label="$1"
  local path="$2"
  local linkage="$3"
  local file_output header_output program_output dynamic_output
  local file_status=0 header_status=0 program_status=0 dynamic_status=0
  local output_truncated=0
  [ -x "${path}" ] || [ -f "${path}" ] \
    || { record_failure "${label} is missing before ELF inspection"; return; }
  file_output="$(mktemp "${RUN_WORK}/preexec-file.XXXXXXXX")"
  header_output="$(mktemp "${RUN_WORK}/preexec-header.XXXXXXXX")"
  program_output="$(mktemp "${RUN_WORK}/preexec-program.XXXXXXXX")"
  dynamic_output="$(mktemp "${RUN_WORK}/preexec-dynamic.XXXXXXXX")"
  if run_bounded_capture \
      "${file_output}" "${COMMAND_OUTPUT_LIMIT_BYTES}" \
      "${SMOKE_COMMAND_TIMEOUT_SEC}" env LC_ALL=C file -Lb "${path}"; then
    file_status=0
  else
    file_status=$?
  fi
  if [ "${file_status}" -ne 0 ]; then
    append_capped_output "${file_output}" "${COMMAND_OUTPUT_LIMIT_BYTES}" \
      || true
    rm -f -- "${header_output}" "${program_output}" "${dynamic_output}"
    record_failure "${label} file inspection failed before execution gate"
    return
  fi
  if ! grep -Eq \
      '^ELF 64-bit LSB (pie )?(executable|shared object), x86-64,' \
      "${file_output}"; then
    append_capped_output "${file_output}" "${COMMAND_OUTPUT_LIMIT_BYTES}" \
      || true
    rm -f -- "${header_output}" "${program_output}" "${dynamic_output}"
    record_failure "${label} is not a Linux x86-64 ELF before execution gate"
    return
  fi
  append_capped_output "${file_output}" "${COMMAND_OUTPUT_LIMIT_BYTES}" \
    || output_truncated=1
  run_bounded_capture "${header_output}" "${COMMAND_OUTPUT_LIMIT_BYTES}" \
    "${SMOKE_COMMAND_TIMEOUT_SEC}" env LC_ALL=C readelf -W -h "${path}" \
    || header_status=$?
  run_bounded_capture "${program_output}" "${COMMAND_OUTPUT_LIMIT_BYTES}" \
    "${SMOKE_COMMAND_TIMEOUT_SEC}" env LC_ALL=C readelf -W -l "${path}" \
    || program_status=$?
  run_bounded_capture "${dynamic_output}" "${COMMAND_OUTPUT_LIMIT_BYTES}" \
    "${SMOKE_COMMAND_TIMEOUT_SEC}" env LC_ALL=C readelf -W -d "${path}" \
    || dynamic_status=$?
  if [ "${header_status}" -ne 0 ] || [ "${program_status}" -ne 0 ] \
    || [ "${dynamic_status}" -ne 0 ]; then
    append_capped_output "${header_output}" "${COMMAND_OUTPUT_LIMIT_BYTES}" \
      || true
    append_capped_output "${program_output}" "${COMMAND_OUTPUT_LIMIT_BYTES}" \
      || true
    append_capped_output "${dynamic_output}" "${COMMAND_OUTPUT_LIMIT_BYTES}" \
      || true
    record_failure "${label} readelf inspection failed before execution gate"
    return
  fi
  grep -Fq 'Class:                             ELF64' "${header_output}" \
    && grep -Fq "Data:                              2's complement, little endian" "${header_output}" \
    && grep -Fq 'Machine:                           Advanced Micro Devices X86-64' "${header_output}" \
    || { record_failure "${label} readelf identity is not ELF64 little-endian x86-64"; return; }
  append_capped_output "${header_output}" "${COMMAND_OUTPUT_LIMIT_BYTES}" \
    || output_truncated=1
  case "${linkage}" in
    static)
      if grep -Fq ' INTERP ' "${program_output}" \
        || grep -Fq '(NEEDED)' "${dynamic_output}"; then
        record_failure "${label} is not static according to readelf"
      else
        record_ok "${label} passed pure static ELF inspection"
      fi
      ;;
    dynamic)
      if grep -Fq '(NEEDED)' "${dynamic_output}"; then
        record_ok "${label} passed pure dynamic ELF/NEEDED inspection"
      else
        record_failure "${label} has no dynamic NEEDED entries"
      fi
      ;;
    shared)
      record_ok "${label} passed pure shared-object ELF inspection"
      ;;
  esac
  append_capped_output "${program_output}" "${COMMAND_OUTPUT_LIMIT_BYTES}" \
    || output_truncated=1
  append_capped_output "${dynamic_output}" "${COMMAND_OUTPUT_LIMIT_BYTES}" \
    || output_truncated=1
  [ "${output_truncated}" -eq 0 ] \
    || record_failure "${label} inspection output exceeded a safety limit"
}

section "Pre-execution Binary Inspection"
inspect_elf_preexec "media-core" "${ROOT}/binaries/media-core-linux-amd64" static
inspect_elf_preexec "media-agent" "${ROOT}/binaries/media-agent-linux-amd64" static
inspect_elf_preexec "media-gateway" "${ROOT}/binaries/media-gateway-linux-amd64" static
inspect_elf_preexec "streamserver-config" "${ROOT}/binaries/streamserver-config-linux-amd64" static
if [ "${BUNDLE_WORKER_SUPPORT}" = true ]; then
  inspect_elf_preexec "ffmpeg cpu" \
    "${ROOT}/runtime/ffmpeg/cpu/bin/ffmpeg" \
    dynamic
  inspect_elf_preexec "ffprobe cpu" \
    "${ROOT}/runtime/ffmpeg/cpu/bin/ffprobe" \
    dynamic
  inspect_elf_preexec "MediaServer" \
    "${ROOT}/runtime/zlm/MediaServer" dynamic
  certificate_output="$(mktemp "${RUN_WORK}/certificate-output.XXXXXXXX")"
  if run_bounded_capture \
      "${certificate_output}" "${COMMAND_OUTPUT_LIMIT_BYTES}" \
      "${SMOKE_COMMAND_TIMEOUT_SEC}" \
      openssl x509 -in "${ROOT}/runtime/zlm/default.pem" -noout; then
    append_capped_output \
      "${certificate_output}" "${COMMAND_OUTPUT_LIMIT_BYTES}" \
      || record_failure "ZLMediaKit certificate output exceeded a safety limit"
    record_ok "ZLMediaKit default certificate is parseable"
  else
    append_capped_output \
      "${certificate_output}" "${COMMAND_OUTPUT_LIMIT_BYTES}" || true
    record_failure "ZLMediaKit default certificate inspection failed"
  fi
fi
if [ "${BUNDLE_GPU_SUPPORT}" = true ]; then
  inspect_elf_preexec "ffmpeg gpu" \
    "${ROOT}/runtime/ffmpeg/gpu/bin/ffmpeg" \
    dynamic
  inspect_elf_preexec "ffprobe gpu" \
    "${ROOT}/runtime/ffmpeg/gpu/bin/ffprobe" \
    dynamic
fi
if [ "${BUNDLE_POSTGRES_RUNTIME}" = true ]; then
  for command_name in postgres initdb pg_ctl pg_isready psql pg_basebackup pg_receivewal pg_recvlogical; do
    inspect_elf_preexec "postgres ${command_name}" \
      "${ROOT}/runtime/postgres/bin/${command_name}" \
      dynamic
  done
  while IFS= read -r -d '' shared_object; do
    inspect_elf_preexec \
      "postgres shared object ${shared_object#"${ROOT}/"}" \
      "${shared_object}" shared
  done < <(find "${ROOT}/runtime/postgres" -type f -name '*.so*' -print0)
fi
abort_gate_if_failed "binary and runtime inspection gate"

if [ "${BUNDLE_POSTGRES_RUNTIME}" = true ]; then
  while IFS= read -r -d '' shared_object; do
    inspect_shared_object \
      "postgres shared object ${shared_object#"${ROOT}/"}" \
      "${shared_object}" "${ROOT}/runtime/postgres/lib"
  done < <(find "${ROOT}/runtime/postgres" -type f -name '*.so*' -print0)
fi

section "Business Binaries"
export -f process_starttime process_is_live_non_zombie
check_static_binary "media-core" "${ROOT}/binaries/media-core-linux-amd64"
check_static_binary "media-agent" "${ROOT}/binaries/media-agent-linux-amd64"
check_static_binary "media-gateway" "${ROOT}/binaries/media-gateway-linux-amd64"
check_static_binary "streamserver-config" "${ROOT}/binaries/streamserver-config-linux-amd64"
run_capture "media-core auth help" "${ROOT}/binaries/media-core-linux-amd64" auth --help
run_shell "media-core auth check-config smoke" "
  tmp=\$(mktemp -d)
  trap 'rm -rf -- \"\${tmp}\"' EXIT
  openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 \
    -out \"\${tmp}/jwt-private.pem\" >/dev/null 2>&1
  openssl pkey -in \"\${tmp}/jwt-private.pem\" -pubout \
    -out \"\${tmp}/jwt-public.pem\" >/dev/null 2>&1
  openssl req -x509 -newkey rsa:2048 -nodes -days 1 \
    -subj /CN=streamserver-native-verifier \
    -keyout \"\${tmp}/listener-key.pem\" \
    -out \"\${tmp}/listener-cert.pem\" >/dev/null 2>&1
  cd \"\${tmp}\"
  env -i \
    PATH=\"\${PATH}\" \
    HOME=\"\${tmp}\" \
    STREAMSERVER_ENV=development \
    DATABASE_URL=postgresql://127.0.0.1:9/native_verifier \
    AUTH_MODE=local_password \
    AUTH_JWT_PRIVATE_KEY_PATH=\"\${tmp}/jwt-private.pem\" \
    AUTH_JWT_PUBLIC_KEY_PATH=\"\${tmp}/jwt-public.pem\" \
    CORE_HTTP_ADDR=127.0.0.1:18443 \
    CORE_HTTP_TLS_CERT_PATH=\"\${tmp}/listener-cert.pem\" \
    CORE_HTTP_TLS_KEY_PATH=\"\${tmp}/listener-key.pem\" \
    CORE_GRPC_ADDR=127.0.0.1:15051 \
    CORE_GRPC_TLS_CERT_PATH=\"\${tmp}/listener-cert.pem\" \
    CORE_GRPC_TLS_KEY_PATH=\"\${tmp}/listener-key.pem\" \
    CORE_GRPC_TLS_CLIENT_CA_PATH=\"\${tmp}/listener-cert.pem\" \
    SOURCE_GATEWAY_BASE_URL=https://172.21.26.25/bohui/media/ \
    SOURCE_GATEWAY_TLS_INSECURE_SKIP_VERIFY=true \
    SOURCE_GATEWAY_PREFETCH_POLL_MS=1000 \
    SOURCE_GATEWAY_PREFETCH_TIMEOUT_MS=600000 \
    STORAGE_ALLOWLIST=\"\${tmp}\" \
    "\${VERIFY_ROOT}/binaries/media-core-linux-amd64" auth check-config \
      | grep -Fq 'authentication and Agent CA configuration is valid'
"
run_shell "media-core Source Gateway config fail-closed smoke" '
  if output=$(env -i \
    PATH="${PATH}" \
    HOME="${TMPDIR:-/tmp}" \
    STREAMSERVER_ENV=development \
    SOURCE_GATEWAY_TLS_INSECURE_SKIP_VERIFY=not-a-boolean \
    "${VERIFY_ROOT}/binaries/media-core-linux-amd64" auth check-config 2>&1); then
    exit 1
  fi
  printf "%s\n" "${output}" | grep -Fq "SOURCE_GATEWAY_TLS_INSECURE_SKIP_VERIFY must be true or false"

  if output=$(env -i \
    PATH="${PATH}" \
    HOME="${TMPDIR:-/tmp}" \
    STREAMSERVER_ENV=development \
    DATABASE_URL=postgresql://127.0.0.1:9/native_verifier \
    SOURCE_GATEWAY_BASE_URL=http://172.21.26.25/bohui/media/ \
    "${VERIFY_ROOT}/binaries/media-core-linux-amd64" auth check-config 2>&1); then
    exit 1
  fi
  printf "%s\n" "${output}" | grep -Fq "SOURCE_GATEWAY_BASE_URL must use https"
'
run_shell "media-agent command parser smoke" \
  "output=\$(\"\${VERIFY_ROOT}/binaries/media-agent-linux-amd64\" __native_verifier_smoke__ 2>&1) && exit 1; printf '%s\\n' \"\${output}\" | grep -Fq 'unknown media-agent command'"
run_shell "media-agent liveness/readiness smoke" "
  tmp=\$(mktemp -d)
  pid=
  cleanup_agent_smoke() {
    if [ -n \"\${pid:-}\" ] && kill -0 \"\${pid}\" >/dev/null 2>&1; then
      kill -TERM \"\${pid}\" >/dev/null 2>&1 || true
      for _ in \$(seq 1 50); do
        kill -0 \"\${pid}\" >/dev/null 2>&1 || break
        sleep 0.1
      done
      if kill -0 \"\${pid}\" >/dev/null 2>&1; then
        kill -KILL \"\${pid}\" >/dev/null 2>&1 || true
      fi
    fi
    if [ -n \"\${pid:-}\" ]; then
      wait \"\${pid}\" 2>/dev/null || true
    fi
    rm -rf -- \"\${tmp}\"
  }
  trap cleanup_agent_smoke EXIT
  mkdir -p \"\${tmp}/work\" \"\${tmp}/mp4\" \"\${tmp}/hls\"
  openssl req -x509 -newkey rsa:2048 -nodes -days 1 \
    -subj /CN=streamserver-agent-verifier \
    -keyout \"\${tmp}/management-key.pem\" \
    -out \"\${tmp}/management-cert.pem\" >/dev/null 2>&1
  openssl genpkey -algorithm ED25519 \
    -out \"\${tmp}/capability-private.pem\" >/dev/null 2>&1
  openssl pkey -in \"\${tmp}/capability-private.pem\" -pubout \
    -out \"\${tmp}/capability-public.pem\" >/dev/null 2>&1
  read -r public_port management_port hook_port < <(python3 - <<'PY'
import socket
sockets = [socket.socket() for _ in range(3)]
try:
    for listener in sockets:
        listener.bind(('127.0.0.1', 0))
    print(*(listener.getsockname()[1] for listener in sockets))
finally:
    for listener in sockets:
        listener.close()
PY
  )
  cd \"\${tmp}\"
  env -i \
    PATH=\"\${PATH}\" \
    HOME=\"\${tmp}\" \
    STREAMSERVER_ENV=development \
    AGENT_PUBLIC_MEDIA_ADDR=127.0.0.1:\${public_port} \
    AGENT_MANAGEMENT_ADDR=127.0.0.1:\${management_port} \
    AGENT_ZLM_HOOK_ADDR=127.0.0.1:\${hook_port} \
    AGENT_MANAGEMENT_TLS_CERT_PATH=\"\${tmp}/management-cert.pem\" \
    AGENT_MANAGEMENT_TLS_KEY_PATH=\"\${tmp}/management-key.pem\" \
    AGENT_MANAGEMENT_TLS_CLIENT_CA_PATH=\"\${tmp}/management-cert.pem\" \
    AGENT_MANAGEMENT_CAPABILITY_JWT_PUBLIC_KEY_PATH=\"\${tmp}/capability-public.pem\" \
    AGENT_NODE_ID=018f47f0-4f17-7d2a-a5c4-ff9b570a3a01 \
    AGENT_NODE_NAME=native-verifier \
    AGENT_CORE_ENDPOINT=http://127.0.0.1:9 \
    AGENT_IDENTITY_DIR= \
    AGENT_STREAM_ADDR=http://127.0.0.1:\${public_port} \
    PUBLIC_MEDIA_BASE_URL=http://127.0.0.1:\${public_port} \
    AGENT_MAX_LIVE_RUNTIME_SLOTS=1 \
    AGENT_MAX_VOD_RUNTIME_SLOTS=1 \
    FFMPEG_BIN=/bin/true \
    FFPROBE_BIN=/bin/true \
    WORK_ROOT=\"\${tmp}/work\" \
    ZLM_HOOK_SHARED_SECRET=native-verifier-hook-secret \
    ZLM_API_BASE=http://127.0.0.1:9 \
    ZLM_OUTPUT_MP4_ROOT=\"\${tmp}/mp4\" \
    ZLM_OUTPUT_HLS_ROOT=\"\${tmp}/hls\" \
    "\${VERIFY_ROOT}/binaries/media-agent-linux-amd64" &
  pid=\$!
  for _ in \$(seq 1 100); do
    if curl --fail --silent --show-error \
      \"http://127.0.0.1:\${public_port}/health/live\" >/dev/null 2>&1 \
      && curl --fail --silent --show-error \
        \"http://127.0.0.1:\${public_port}/health/ready\" >/dev/null 2>&1; then
      kill -0 \"\${pid}\" >/dev/null 2>&1
      exit 0
    fi
    if ! kill -0 \"\${pid}\" >/dev/null 2>&1; then
      exit 1
    fi
    sleep 0.1
  done
  exit 1
"
run_shell "media-gateway startup smoke" "
  tmp=\$(mktemp -d)
  pid=
  gateway_port=\$(python3 - <<'PY'
import socket
listener = socket.socket()
try:
    listener.bind(('127.0.0.1', 0))
    print(listener.getsockname()[1])
finally:
    listener.close()
PY
  )
  cleanup_gateway_smoke() {
    if [ -n \"\${pid:-}\" ] && kill -0 \"\${pid}\" >/dev/null 2>&1; then
      kill \"\${pid}\" >/dev/null 2>&1 || true
      for _ in \$(seq 1 50); do
        kill -0 \"\${pid}\" >/dev/null 2>&1 || break
        sleep 0.1
      done
      if kill -0 \"\${pid}\" >/dev/null 2>&1; then
        kill -KILL \"\${pid}\" >/dev/null 2>&1 || true
      fi
    fi
    if [ -n \"\${pid:-}\" ]; then
      wait \"\${pid}\" 2>/dev/null || true
    fi
    rm -rf -- \"\${tmp}\"
  }
  trap cleanup_gateway_smoke EXIT
  mkdir -p \"\${tmp}/work\"
  MEDIA_GATEWAY_BIND_ADDR=127.0.0.1:\${gateway_port} \
  MEDIA_GATEWAY_PUBLIC_BASE_URL=http://127.0.0.1:1 \
  MEDIA_GATEWAY_WORK_ROOT=\"\${tmp}/work\" \
  MEDIA_GATEWAY_FFMPEG_BIN=\"\${VERIFY_ROOT}/runtime/ffmpeg/cpu/bin/ffmpeg\" \
  RUST_LOG=info \
    "\${VERIFY_ROOT}/binaries/media-gateway-linux-amd64" &
  pid=\$!
  for _ in \$(seq 1 50); do
    if curl --fail --silent --show-error \
      \"http://127.0.0.1:\${gateway_port}/api/healthz\" >/dev/null 2>&1; then
      kill -0 \"\${pid}\" >/dev/null 2>&1
      echo 'gateway health endpoint is ready while process is alive'
      exit 0
    fi
    if ! kill -0 \"\${pid}\" >/dev/null 2>&1; then
      exit 1
    fi
    sleep 0.1
  done
  exit 1
"
run_capture "streamserver-config help" \
  "${ROOT}/binaries/streamserver-config-linux-amd64" --help
run_shell "streamserver-config non-interactive smoke" "
  tmp=\$(mktemp -d)
  trap 'rm -rf -- \"\${tmp}\"' EXIT
  HOME=\"\${tmp}\" \
    "\${VERIFY_ROOT}/binaries/streamserver-config-linux-amd64" \
      --env \"\${tmp}/component.env\" \
      --non-interactive \
      --no-restart-prompt
  test -s \"\${tmp}/component.env\"
  grep -Fq \"SOURCE_GATEWAY_BASE_URL=''\" \"\${tmp}/component.env\"
  grep -Fq \"SOURCE_GATEWAY_TLS_INSECURE_SKIP_VERIFY='false'\" \"\${tmp}/component.env\"
  grep -Fq \"SOURCE_GATEWAY_PREFETCH_POLL_MS='1000'\" \"\${tmp}/component.env\"
  grep -Fq \"SOURCE_GATEWAY_PREFETCH_TIMEOUT_MS='600000'\" \"\${tmp}/component.env\"
"

section "FFmpeg Runtime"
if [ "${BUNDLE_WORKER_SUPPORT}" = true ]; then
  check_runtime_binary "ffmpeg cpu" "${ROOT}/runtime/ffmpeg/cpu/bin/ffmpeg" "${ROOT}/runtime/ffmpeg/cpu/lib" -version
  check_runtime_binary "ffprobe cpu" "${ROOT}/runtime/ffmpeg/cpu/bin/ffprobe" "${ROOT}/runtime/ffmpeg/cpu/lib" -version
  run_shell "ffmpeg cpu HTTPS verification defaults stay disabled" '
    loader="${VERIFY_ROOT}/runtime/ffmpeg/cpu/lib/ld-linux-x86-64.so.2"
    library_path="${VERIFY_ROOT}/runtime/ffmpeg/cpu/lib"
    ffmpeg="${VERIFY_ROOT}/runtime/ffmpeg/cpu/bin/ffmpeg"
    ffprobe="${VERIFY_ROOT}/runtime/ffmpeg/cpu/bin/ffprobe"
    for binary in "${ffmpeg}" "${ffprobe}"; do
      output=$("${loader}" --library-path "${library_path}" "${binary}" -hide_banner -h protocol=tls 2>&1)
      printf "%s\n" "${output}" | grep -Eq -- "tls_verify.*default false"
      printf "%s\n" "${output}" | grep -Eq -- "(^|[[:space:]])-verify[[:space:]].*default false"
    done
  '
  run_shell "ffmpeg cpu HEVC to FLV smoke" "tmp=\$(mktemp -d); trap 'rm -rf \"\${tmp}\"' EXIT; \"\${VERIFY_ROOT}/runtime/ffmpeg/cpu/lib/ld-linux-x86-64.so.2\" --library-path \"\${VERIFY_ROOT}/runtime/ffmpeg/cpu/lib\" \"\${VERIFY_ROOT}/runtime/ffmpeg/cpu/bin/ffmpeg\" -hide_banner -f lavfi -i testsrc=size=128x72:rate=1 -t 1 -c:v libx265 -an -f flv -y \"\$tmp/hevc-test.flv\" && test -s \"\$tmp/hevc-test.flv\""
fi
if [ "${BUNDLE_GPU_SUPPORT}" = true ]; then
  check_runtime_binary "ffmpeg gpu" "${ROOT}/runtime/ffmpeg/gpu/bin/ffmpeg" "${ROOT}/runtime/ffmpeg/gpu/lib" -version
  check_runtime_binary "ffprobe gpu" "${ROOT}/runtime/ffmpeg/gpu/bin/ffprobe" "${ROOT}/runtime/ffmpeg/gpu/lib" -version
  run_shell "ffmpeg gpu encoder check" "\"\${VERIFY_ROOT}/runtime/ffmpeg/gpu/lib/ld-linux-x86-64.so.2\" --library-path \"\${VERIFY_ROOT}/runtime/ffmpeg/gpu/lib\" \"\${VERIFY_ROOT}/runtime/ffmpeg/gpu/bin/ffmpeg\" -hide_banner -encoders 2>/dev/null | grep -q h264_nvenc && \"\${VERIFY_ROOT}/runtime/ffmpeg/gpu/lib/ld-linux-x86-64.so.2\" --library-path \"\${VERIFY_ROOT}/runtime/ffmpeg/gpu/lib\" \"\${VERIFY_ROOT}/runtime/ffmpeg/gpu/bin/ffmpeg\" -hide_banner -encoders 2>/dev/null | grep -q hevc_nvenc"
  if [ "${GPU_HARDWARE_MODE}" = required ]; then
    run_shell "ffmpeg gpu h264_nvenc hardware encode smoke" "command -v nvidia-smi >/dev/null && nvidia-smi >/dev/null && \"\${VERIFY_ROOT}/runtime/ffmpeg/gpu/lib/ld-linux-x86-64.so.2\" --library-path \"\${VERIFY_ROOT}/runtime/ffmpeg/gpu/lib\" \"\${VERIFY_ROOT}/runtime/ffmpeg/gpu/bin/ffmpeg\" -v error -hide_banner -nostdin -f lavfi -i testsrc2=size=640x360:rate=15 -t 1 -c:v h264_nvenc -an -f null -"
    run_shell "ffmpeg gpu hevc_nvenc hardware encode smoke" "command -v nvidia-smi >/dev/null && nvidia-smi >/dev/null && \"\${VERIFY_ROOT}/runtime/ffmpeg/gpu/lib/ld-linux-x86-64.so.2\" --library-path \"\${VERIFY_ROOT}/runtime/ffmpeg/gpu/lib\" \"\${VERIFY_ROOT}/runtime/ffmpeg/gpu/bin/ffmpeg\" -v error -hide_banner -nostdin -f lavfi -i testsrc2=size=640x360:rate=15 -t 1 -c:v hevc_nvenc -an -f null -"
  else
    append "[SKIP] GPU hardware encode smokes explicitly skipped; runtime shape, load, versions, dependencies, and NVENC encoder registration were still verified."
  fi
fi

section "ZLMediaKit Runtime"
if [ "${BUNDLE_WORKER_SUPPORT}" = true ]; then
  if [ -d "${ROOT}/runtime/zlm/python" ]; then
    export PYTHONHOME="${ROOT}/runtime/zlm/python"
  fi
  [ -f "${ROOT}/runtime/zlm/default.pem" ] && record_ok "default.pem exists" || record_failure "default.pem missing"
  check_runtime_binary "MediaServer" "${ROOT}/runtime/zlm/MediaServer" "${ROOT}/runtime/zlm/lib" -v
  run_shell "ZLM statistic smoke" "
    tmp=\$(mktemp -d)
    port=\$((23000 + RANDOM % 10000))
    export ZLM_API_SECRET=verify-api-secret-0123456789abcdef
    export ZLM_HOOK_SHARED_SECRET=verify-hook-secret-0123456789abcde
    export ZLM_SERVER_ID=verify-target
    export ZLM_HOOK_BASE=http://127.0.0.1:9/hooks
    export ZLM_API_ALLOW_IP_RANGE='::1,127.0.0.1,10.0.0.0-10.255.255.255,172.16.0.0-172.31.255.255,192.168.0.0-192.168.255.255'
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
    export ZLM_DEFAULT_PEM="\${VERIFY_ROOT}/runtime/zlm/default.pem"
    export AGENT_MP4_RECORD_SEGMENT_SEC=7200
    mkdir -p \"\${ZLM_WWW_ROOT}\" \"\${ZLM_RECORD_ROOT}\" \"\${ZLM_SNAP_ROOT}\"
    \"\${VERIFY_ROOT}/templates/common/zlm.render-config.sh\" \"\${VERIFY_ROOT}/templates/common/zlm.config.ini.template\" \"\${tmp}/zlm.ini\"
    if [ -d \"\${VERIFY_ROOT}/runtime/zlm/python\" ]; then
      export PYTHONHOME=\"\${VERIFY_ROOT}/runtime/zlm/python\"
    fi
    pid=
    pid_starttime=
    zlm_process_matches() {
      [ -n \"\${pid:-}\" ] && [ -n \"\${pid_starttime:-}\" ] \
        && [ \"\$(process_starttime \"\${pid}\" 2>/dev/null || true)\" \
          = \"\${pid_starttime}\" ] \
        && process_is_live_non_zombie \"\${pid}\"
    }
    cleanup_zlm_smoke() {
      if zlm_process_matches; then
        kill \"\${pid}\" >/dev/null 2>&1 || true
        for cleanup_attempt in \$(seq 1 50); do
          zlm_process_matches || break
          sleep 0.1
        done
        if zlm_process_matches; then
          kill -KILL \"\${pid}\" >/dev/null 2>&1 || true
        fi
      fi
      if [ -n \"\${pid:-}\" ]; then
        wait \"\${pid}\" 2>/dev/null || true
      fi
      rm -rf \"\${tmp}\"
    }
    trap cleanup_zlm_smoke EXIT
    (
      cd \"\${VERIFY_ROOT}/runtime/zlm\"
      exec \"\${VERIFY_ROOT}/runtime/zlm/lib/ld-linux-x86-64.so.2\" --library-path \"\${VERIFY_ROOT}/runtime/zlm/lib\" \"\${VERIFY_ROOT}/runtime/zlm/MediaServer\" -s \"\${VERIFY_ROOT}/runtime/zlm/default.pem\" -c \"\${tmp}/zlm.ini\" -l 0
    ) &
    pid=\$!
    pid_starttime=\$(process_starttime \"\${pid}\")
    for i in \$(seq 1 20); do
      if printf 'url = \"http://127.0.0.1:%s/index/api/getStatistic?secret=%s\"\n' \
          \"\${port}\" \"\${ZLM_API_SECRET}\" \
          | curl --fail --silent --show-error --config - >/dev/null; then
        [ \"\$(process_starttime \"\${pid}\" 2>/dev/null || true)\" \
          = \"\${pid_starttime}\" ] \
          && process_is_live_non_zombie \"\${pid}\" \
          || exit 1
        echo 'ZLM statistic endpoint is ready while process is alive'
        exit 0
      fi
      [ \"\$(process_starttime \"\${pid}\" 2>/dev/null || true)\" \
        = \"\${pid_starttime}\" ] \
        && process_is_live_non_zombie \"\${pid}\" \
        || exit 1
      sleep 1
    done
    exit 1
  "
fi

section "PostgreSQL Runtime"
if [ "${BUNDLE_POSTGRES_RUNTIME}" = true ]; then
  for command_name in postgres initdb pg_ctl pg_isready psql; do
    check_runtime_binary "postgres ${command_name}" "${ROOT}/runtime/postgres/bin/${command_name}" "${ROOT}/runtime/postgres/lib" --version
  done
  append ""
  append "### PostgreSQL init/start/query/extensions smoke"
  append "\`\`\`"
  postgres_smoke() {
    local tmp control_dir socket_dir port pgroot pgwrap command_name runner_prefix pid
    local pg_pkglib_dir pg_share_dir pg_library_path extension_manifest loader
    local next_port_value next_port_file started_pid nobody_uid="" nobody_gid=""
    tmp="${POSTGRES_SMOKE_TMP}"
    control_dir="${POSTGRES_SMOKE_CONTROL_DIR}"
    socket_dir="${POSTGRES_SMOKE_SOCKET_DIR}"
    [ -d "${tmp}" ] && [ -d "${control_dir}" ] \
      && [ -d "${POSTGRES_SMOKE_TOOL_DIR}" ] \
      && [ -d "${socket_dir}" ] \
      && [ -f "${POSTGRES_SMOKE_PID_REGISTRY}" ] || return 1
    trap cleanup_postgres_smoke EXIT
    next_port_value=$((25432 + RANDOM % 10000))
    next_port_file="${control_dir}/next-port"
    printf '%s\n' "${next_port_value}" >"${next_port_file}"
    pgroot="${ROOT}/runtime/postgres"
    pgwrap="${POSTGRES_SMOKE_TOOL_DIR}/pgwrap"
    mkdir -m 755 -p "${pgwrap}"

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
        setpriv --reuid="${nobody_uid}" --regid="${nobody_gid}" \
          --clear-groups -- "$@"
      else
        "$@"
      fi
    }

    init_cluster() {
      local data_dir="$1"
      mkdir -p "${data_dir}"
      if [ "$(id -u)" -eq 0 ]; then
        chown "${nobody_uid}:${nobody_gid}" "${data_dir}"
        chmod 700 "${data_dir}"
      fi
      run_pg "${pgwrap}/initdb" -D "${data_dir}" -U postgres \
        -L "${pg_share_dir}" --encoding=UTF8 --locale=C --data-checksums
    }

    wait_ready() {
      local check_port="$1"
      local expected_pid="$2"
      for _ in $(seq 1 60); do
        registered_process_is_live "${expected_pid}" || return 1
        if "${pgwrap}/pg_isready" -h "${socket_dir}" -p "${check_port}" \
            -U postgres >/dev/null 2>&1; then
          registered_process_is_live "${expected_pid}" || return 1
          return 0
        fi
        sleep 1
      done
      return 1
    }

    start_cluster() {
      local data_dir="$1"
      local start_port="$2"
      shift 2
      if [ "$(id -u)" -eq 0 ]; then
        setsid setpriv --reuid="${nobody_uid}" --regid="${nobody_gid}" \
          --clear-groups -- \
          "${pgwrap}/postgres" -D "${data_dir}" -p "${start_port}" \
          -k "${socket_dir}" -c "dynamic_library_path=${pg_pkglib_dir}" "$@" \
          -c listen_addresses=127.0.0.1 &
      else
        setsid "${pgwrap}/postgres" -D "${data_dir}" -p "${start_port}" \
          -k "${socket_dir}" -c "dynamic_library_path=${pg_pkglib_dir}" "$@" \
          -c listen_addresses=127.0.0.1 &
      fi
      pid=$!
      register_postgres_pid "${pid}"
      wait_ready "${start_port}" "${pid}" || {
        return 1
      }
      started_pid="${pid}"
    }

    stop_pid() {
      local stop_pid_value="$1"
      local stop_starttime stop_pgid
      read -r stop_starttime stop_pgid < <(awk -v expected_pid="${stop_pid_value}" \
        '$1 == expected_pid { print $2, $3; exit }' \
        "${POSTGRES_SMOKE_PID_REGISTRY}")
      if [ -n "${stop_starttime}" ] && [ -n "${stop_pgid}" ]; then
        terminate_registered_process \
          "${stop_pid_value}" "${stop_starttime}" "${stop_pgid}"
      fi
      unregister_postgres_pid "${stop_pid_value}"
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
      command -v setpriv >/dev/null
      nobody_uid="$(id -u nobody)"
      nobody_gid="$(id -g nobody)"
      chmod 711 "${WORK_DIR}" "${RUN_WORK}" "${RUN_WORK}/extract"
      [ "$(stat -Lc '%u:%a' "${control_dir}")" = "0:700" ]
      [ "$(stat -Lc '%u:%a' "${POSTGRES_SMOKE_PID_REGISTRY}")" = "0:600" ]
      [ "$(stat -Lc '%u:%a' "${POSTGRES_SMOKE_TOOL_DIR}")" = "0:755" ]
      [ "$(stat -Lc '%u:%a' "${pgwrap}")" = "0:755" ]
      [ -z "$(find "${pgwrap}" -maxdepth 1 -type f \
        \( ! -uid 0 -o -perm /022 \) -print -quit)" ]
      setpriv --reuid="${nobody_uid}" --regid="${nobody_gid}" \
        --clear-groups -- sh -c \
        'test ! -w "$1" && test ! -w "$2" && test -x "$2"' \
        verifier-permissions "${POSTGRES_SMOKE_PID_REGISTRY}" \
        "${pgwrap}/postgres"
      echo "PostgreSQL root control state is isolated from the runtime user"
      chown "${nobody_uid}:${nobody_gid}" "${tmp}"
      chmod 700 "${tmp}"
      chown "${nobody_uid}:${nobody_gid}" "${socket_dir}"
      chmod 700 "${socket_dir}"
    fi
    port="$(next_port)"
    init_cluster "${tmp}/data" || return 1
    start_cluster "${tmp}/data" "${port}" || return 1
    pid="${started_pid}"
    "${pgwrap}/psql" -h 127.0.0.1 -p "${port}" -U postgres -d postgres -v ON_ERROR_STOP=1 -c 'select 1;' >/dev/null || return 1

    cut -f1,2 "${extension_manifest}" | LC_ALL=C sort >"${tmp}/expected-extensions.tsv"
    "${pgwrap}/psql" -h 127.0.0.1 -p "${port}" -U postgres -d postgres -A -t -F $'\t' \
      -v ON_ERROR_STOP=1 \
      -c "select name, coalesce(default_version, '') from pg_available_extensions order by name collate \"C\";" \
      >"${tmp}/actual-extensions.tsv" || return 1
    if ! diff -u "${tmp}/expected-extensions.tsv" "${tmp}/actual-extensions.tsv"; then
      echo "pg_available_extensions does not match runtime-source extension manifest"
      return 1
    fi
    echo "pg_available_extensions matches runtime-source extension manifest"

    extension_so_count="$(find "${pg_pkglib_dir}" -type f -name '*.so' -print \
      | wc -l)"
    echo "extension_so_count=${extension_so_count}; dependencies already passed bounded loader inspection"

    create_failures=0
    create_count=0
    while IFS=$'\t' read -r extension_name default_version _control_file; do
      [ -n "${extension_name}" ] || continue
      create_count=$((create_count + 1))
      sql_extension_name="$(printf '%s' "${extension_name}" | sed 's/"/""/g')"
      if "${pgwrap}/psql" -h 127.0.0.1 -p "${port}" -U postgres -d postgres \
        -v ON_ERROR_STOP=1 \
        -c "CREATE EXTENSION IF NOT EXISTS \"${sql_extension_name}\";"; then
        printf '[extension create ok] %s %s\n' "${extension_name}" "${default_version}"
      else
        create_failures=$((create_failures + 1))
        printf '[extension create fail] %s %s\n' "${extension_name}" "${default_version}"
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
    setsid env \
    DATABASE_URL="postgresql://postgres@127.0.0.1:${port}/streamserver_verify_migrations" \
    AUTH_MODE=disabled \
    CORE_HTTP_ADDR="127.0.0.1:${core_http_port}" \
    CORE_GRPC_ADDR="127.0.0.1:${core_grpc_port}" \
    STORAGE_ALLOWLIST="${tmp}" \
    STREAMSERVER_UI_DIR="${ROOT}/ui/media-core" \
    LOG_LEVEL=info \
    "${ROOT}/binaries/media-core-linux-amd64" --insecure-dev &
    core_pid=$!
    register_postgres_pid "${core_pid}"
    core_ready=0
    for _ in $(seq 1 60); do
      if curl -fsS "http://127.0.0.1:${core_http_port}/health/ready" >/dev/null 2>&1; then
        registered_process_is_live "${core_pid}" || return 1
        echo "media-core readiness endpoint is ready while process is alive"
        core_ready=1
        break
      fi
      if ! registered_process_is_live "${core_pid}"; then
        return 1
      fi
      sleep 1
    done
    [ "${core_ready}" -eq 1 ] || return 1
    migration_count="$("${pgwrap}/psql" -h 127.0.0.1 -p "${port}" -U postgres -d streamserver_verify_migrations -A -t -c 'select count(*) from _sqlx_migrations;')"
    [ "${migration_count}" -gt 0 ] || {
      echo "media-core migration count is zero"
      return 1
    }
    "${pgwrap}/psql" -h 127.0.0.1 -p "${port}" -U postgres -d streamserver_verify_migrations -v ON_ERROR_STOP=1 \
      -c "select to_regclass('public.tasks'), to_regclass('public.media_nodes');" >/dev/null
    stop_pid "${core_pid}"
    echo "media-core migration smoke ok: migrations=${migration_count}"

    run_pg "${pgwrap}/pg_ctl" -D "${tmp}/data" -m fast stop >/dev/null 2>&1 || true
    stop_pid "${pid}"
    "${pgwrap}/pg_controldata" "${tmp}/data" >/dev/null
    "${pgwrap}/pg_checksums" --check -D "${tmp}/data"
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
    init_cluster "${tmp}/ssl-data" || return 1
    cp "${ssl_dir}/server.crt" "${ssl_dir}/server.key" "${ssl_dir}/ca.crt" "${tmp}/ssl-data/"
    cat >"${tmp}/ssl-data/pg_hba.conf" <<'EOF'
local all all trust
hostssl certdb cert_user 127.0.0.1/32 cert clientcert=verify-full
hostnossl certdb cert_user 127.0.0.1/32 reject
host all all 127.0.0.1/32 trust
EOF
    if [ "$(id -u)" -eq 0 ]; then
      chown -R "${nobody_uid}:${nobody_gid}" "${tmp}/ssl-data"
    fi
    start_cluster "${tmp}/ssl-data" "${ssl_port}" \
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
      chown -R "${nobody_uid}:${nobody_gid}" "${pitr_archive}"
    fi
    init_cluster "${tmp}/pitr-primary" || return 1
    cat >>"${tmp}/pitr-primary/pg_hba.conf" <<'EOF'
host replication all 127.0.0.1/32 trust
host all all 127.0.0.1/32 trust
EOF
    start_cluster "${tmp}/pitr-primary" "${pitr_primary_port}" \
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
    archive_oldest="$(find "${pitr_archive}" -type f -print -quit)"
    [ -n "${archive_oldest}" ] || {
      echo "PITR archive is empty"
      return 1
    }
    pitr_waldump_ok=0
    while IFS= read -r wal_file; do
      [ -n "${wal_file}" ] || continue
      if "${pgwrap}/pg_waldump" -n 1 "${wal_file}"; then
        pitr_waldump_ok=1
        break
      fi
    done < <(find "${pitr_archive}" -type f -size +0 -print | LC_ALL=C sort)
    [ "${pitr_waldump_ok}" -eq 1 ] || {
      echo "pg_waldump did not find a valid archived WAL segment"
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
      chown -R "${nobody_uid}:${nobody_gid}" "${tmp}/pitr-restore"
    fi
    start_cluster "${tmp}/pitr-restore" "${pitr_restore_port}" || return 1
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
    init_cluster "${tmp}/repl-primary" || return 1
    cat >>"${tmp}/repl-primary/pg_hba.conf" <<'EOF'
host replication all 127.0.0.1/32 trust
host all all 127.0.0.1/32 trust
EOF
    start_cluster "${tmp}/repl-primary" "${repl_primary_port}" \
      -c wal_level=replica \
      -c max_wal_senders=5 \
      -c hot_standby=on || return 1
    repl_primary_pid="${started_pid}"
    "${pgwrap}/psql" -h 127.0.0.1 -p "${repl_primary_port}" -U postgres -d postgres -v ON_ERROR_STOP=1 \
      -c "create table repl_items(id int primary key, note text); insert into repl_items values (1, 'before');" >/dev/null
    "${pgwrap}/pg_basebackup" -h 127.0.0.1 -p "${repl_primary_port}" -U postgres -D "${tmp}/repl-standby" -X stream -R -Fp >/dev/null
    cat >>"${tmp}/repl-standby/postgresql.auto.conf" <<EOF
port = ${repl_standby_port}
unix_socket_directories = '${socket_dir}'
EOF
    if [ "$(id -u)" -eq 0 ]; then
      chown -R "${nobody_uid}:${nobody_gid}" "${tmp}/repl-standby"
    fi
    start_cluster "${tmp}/repl-standby" "${repl_standby_port}" || return 1
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
      return 1
    }
    stop_pid "${repl_standby_pid}"
    stop_pid "${repl_primary_pid}"
    echo "physical replication smoke ok"

    echo "logical replication smoke"
    logical_pub_port="$(next_port)"
    logical_sub_port="$(next_port)"
    init_cluster "${tmp}/logical-pub" || return 1
    init_cluster "${tmp}/logical-sub" || return 1
    cat >>"${tmp}/logical-pub/pg_hba.conf" <<'EOF'
host replication all 127.0.0.1/32 trust
host all all 127.0.0.1/32 trust
EOF
    start_cluster "${tmp}/logical-pub" "${logical_pub_port}" \
      -c wal_level=logical \
      -c max_replication_slots=5 \
      -c max_wal_senders=5 || return 1
    logical_pub_pid="${started_pid}"
    start_cluster "${tmp}/logical-sub" "${logical_sub_port}" \
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
      return 1
    }
    "${pgwrap}/psql" -h 127.0.0.1 -p "${logical_sub_port}" -U postgres -d postgres -v ON_ERROR_STOP=1 \
      -c "drop subscription native_verify_sub;" >/dev/null
    stop_pid "${logical_sub_pid}"
    stop_pid "${logical_pub_pid}"
    echo "logical replication smoke ok"

  }
  POSTGRES_SMOKE_TMP="$(mktemp -d "${RUN_WORK}/postgres-smoke.XXXXXXXX")"
  POSTGRES_SMOKE_CONTROL_DIR="$(mktemp -d \
    "${RUN_WORK}/postgres-control.XXXXXXXX")"
  POSTGRES_SMOKE_TOOL_DIR="$(mktemp -d \
    "${RUN_WORK}/postgres-tools.XXXXXXXX")"
  POSTGRES_SMOKE_SOCKET_DIR="$(mktemp -d /tmp/ss-pg.XXXXXXXX)"
  chmod 700 "${POSTGRES_SMOKE_CONTROL_DIR}"
  chmod 755 "${POSTGRES_SMOKE_TOOL_DIR}"
  chmod 700 "${POSTGRES_SMOKE_SOCKET_DIR}"
  if [ "${#POSTGRES_SMOKE_SOCKET_DIR}" -gt 40 ]; then
    printf 'PostgreSQL smoke socket directory is unexpectedly long\n' >&2
    exit 1
  fi
  POSTGRES_SMOKE_PID_REGISTRY="${POSTGRES_SMOKE_CONTROL_DIR}/registered-processes.tsv"
  : >"${POSTGRES_SMOKE_PID_REGISTRY}"
  chmod 600 "${POSTGRES_SMOKE_PID_REGISTRY}"
  postgres_output="$(mktemp "${RUN_WORK}/postgres-output.XXXXXXXX")"
  postgres_output_truncated=0
  set +e
  export ROOT WORK_DIR RUN_WORK POSTGRES_SMOKE_TMP POSTGRES_SMOKE_CONTROL_DIR \
    POSTGRES_SMOKE_TOOL_DIR POSTGRES_SMOKE_SOCKET_DIR \
    POSTGRES_SMOKE_PID_REGISTRY
  export -f \
    postgres_smoke \
    cleanup_postgres_smoke \
    process_starttime \
    process_pgid \
    process_group_has_live_members \
    terminate_registered_process \
    register_postgres_pid \
    registered_process_is_live \
    unregister_postgres_pid
  if run_bounded_capture \
      "${postgres_output}" "${POSTGRES_OUTPUT_LIMIT_BYTES}" \
      "${POSTGRES_SMOKE_TIMEOUT_SEC}" \
      bash --noprofile --norc -e -u -o pipefail -c 'postgres_smoke'; then
    postgres_smoke_status=0
  else
    postgres_smoke_status=$?
  fi
  cleanup_postgres_smoke || postgres_smoke_status=1
  set -e
  append_capped_output "${postgres_output}" "${POSTGRES_OUTPUT_LIMIT_BYTES}" \
    || postgres_output_truncated=1
  if [ "${postgres_output_truncated}" -eq 1 ]; then
    postgres_smoke_status=1
  fi
  if [ "${postgres_smoke_status}" -eq 0 ]; then
    append "\`\`\`"
    record_ok "PostgreSQL init/start/query/extensions smoke"
  else
    append "\`\`\`"
    record_failure "PostgreSQL init/start/query/extensions smoke"
  fi
fi

write_summary
printf '%s\n' "${REPORT}"
exit "${FAILURES}"
REMOTE

  if [ "${LOCAL_MODE}" -eq 1 ]; then
    log "开始本机目标运行时验证，不执行 systemd 安装"
    set +e
    run_owned_local_verifier \
      timeout --signal=TERM --kill-after=150s "${REMOTE_VERIFY_TIMEOUT_SEC}s" \
      env \
        STREAMSERVER_VERIFY_BUNDLE="${remote_bundle}" \
        STREAMSERVER_VERIFY_DIR="${REMOTE_DIR}" \
        STREAMSERVER_VERIFY_REPORT="${remote_report}" \
        STREAMSERVER_VERIFY_GPU_HARDWARE_MODE="${GPU_HARDWARE_MODE}" \
        bash --noprofile --norc "${REMOTE_SCRIPT_LOCAL}"
    status=$?
    set -e
  else
    log "上传远端验证脚本"
    scp_upload "${REMOTE_SCRIPT_LOCAL}" "${remote_script}"

    remote_outer_timeout=$((REMOTE_VERIFY_TIMEOUT_SEC + 180))
    remote_command="
set -u
remote_run_dir=$(shell_quote "${REMOTE_DIR}")
remote_launcher_pid=
remote_launcher_starttime=
remote_child_pid=
remote_child_starttime=
remote_child_pgid=
remote_group_owned=0
remote_start_gate=\${remote_run_dir}/remote-start.gate
remote_start_ready=\${remote_run_dir}/remote-start.ready
remote_process_identity() {
  process_pid=\$1
  [ -r \"/proc/\${process_pid}/stat\" ] || return 1
  IFS= read -r stat_line 2>/dev/null \
    <\"/proc/\${process_pid}/stat\" || return 1
  stat_rest=\${stat_line##*) }
  set -- \${stat_rest}
  [ \"\$#\" -ge 20 ] || return 1
  printf '%s %s\\n' \"\${3}\" \"\${20}\"
}
remote_group_has_live_members() {
  expected_pgid=\$1
  for stat_file in /proc/[0-9]*/stat; do
    [ -r \"\${stat_file}\" ] || continue
    IFS= read -r stat_line 2>/dev/null <\"\${stat_file}\" || continue
    stat_rest=\${stat_line##*) }
    set -- \${stat_rest}
    [ \"\$#\" -ge 3 ] || continue
    if [ \"\${3}\" = \"\${expected_pgid}\" ] && [ \"\${1}\" != Z ]; then
      return 0
    fi
  done
  return 1
}
cleanup_remote_group() {
  cleanup_status=0
  if [ \"\${remote_group_owned}\" -eq 0 ] \
    && [ -n \"\${remote_launcher_pid}\" ]; then
    for _ in \$(seq 1 20); do
      IFS= read -r remote_child_pid <\"\${remote_start_ready}\" || true
      case \"\${remote_child_pid}\" in
        ''|*[!0-9]*) remote_child_pid= ;;
        *) break ;;
      esac
      sleep 0.05
    done
  fi
  if [ \"\${remote_group_owned}\" -eq 0 ] \
    && [ -n \"\${remote_child_pid}\" ]; then
    identity=\$(remote_process_identity \"\${remote_child_pid}\" 2>/dev/null || true)
    remote_child_pgid=\${identity%% *}
    remote_child_starttime=\${identity#* }
    if [ \"\${remote_child_pgid}\" = \"\${remote_child_pid}\" ] \
      && [ -n \"\${remote_child_starttime}\" ]; then
      remote_group_owned=1
    else
      kill -TERM \"\${remote_child_pid}\" >/dev/null 2>&1 || true
      for _ in \$(seq 1 20); do
        kill -0 \"\${remote_child_pid}\" >/dev/null 2>&1 || break
        sleep 0.1
      done
      kill -KILL \"\${remote_child_pid}\" >/dev/null 2>&1 || true
    fi
  fi
  if [ \"\${remote_group_owned}\" -eq 1 ] \
    && [ \"\${remote_child_pgid}\" = \"\${remote_child_pid}\" ]; then
    kill -TERM -- \"-\${remote_child_pgid}\" >/dev/null 2>&1 || true
    for _ in \$(seq 1 100); do
      remote_group_has_live_members \"\${remote_child_pgid}\" || break
      sleep 0.1
    done
    if remote_group_has_live_members \"\${remote_child_pgid}\"; then
      kill -KILL -- \"-\${remote_child_pgid}\" >/dev/null 2>&1 || true
    fi
    for _ in \$(seq 1 50); do
      remote_group_has_live_members \"\${remote_child_pgid}\" || break
      sleep 0.1
    done
    remote_group_has_live_members \"\${remote_child_pgid}\" \
      && cleanup_status=1
  fi
  if [ -n \"\${remote_launcher_pid}\" ]; then
    if [ \"\${remote_group_owned}\" -eq 0 ]; then
      launcher_identity=\$(remote_process_identity \
        \"\${remote_launcher_pid}\" 2>/dev/null || true)
      launcher_starttime=\${launcher_identity#* }
      if [ \"\${launcher_starttime}\" = \"\${remote_launcher_starttime}\" ]; then
        kill -TERM \"\${remote_launcher_pid}\" >/dev/null 2>&1 || true
        for _ in \$(seq 1 20); do
          kill -0 \"\${remote_launcher_pid}\" >/dev/null 2>&1 || break
          sleep 0.1
        done
        kill -KILL \"\${remote_launcher_pid}\" >/dev/null 2>&1 || true
      fi
    fi
    wait \"\${remote_launcher_pid}\" 2>/dev/null || true
  fi
  remote_launcher_pid=
  remote_launcher_starttime=
  remote_child_pid=
  remote_child_starttime=
  remote_child_pgid=
  remote_group_owned=0
  return \"\${cleanup_status}\"
}
cleanup_remote_run() {
  cleanup_remote_group || true
  if [ -n \"\${remote_run_dir}\" ]; then
    rm -rf -- \"\${remote_run_dir}\"
    remote_run_dir=
  fi
}
handle_remote_signal() {
  signal_status=\$1
  trap - HUP INT TERM
  cleanup_remote_run
  exit \"\${signal_status}\"
}
trap cleanup_remote_run EXIT
trap 'handle_remote_signal 129' HUP
trap 'handle_remote_signal 130' INT
trap 'handle_remote_signal 143' TERM
remote_status=0
remote_cleanup_failed=0
if ! command -v timeout >/dev/null 2>&1 \
  || ! timeout --version 2>/dev/null | grep -Fq 'GNU coreutils' \
  || ! command -v setsid >/dev/null 2>&1 \
  || ! command -v mkfifo >/dev/null 2>&1; then
  remote_status=125
else
  rm -f -- \"\${remote_start_gate}\" \"\${remote_start_ready}\"
  mkfifo -m 600 \"\${remote_start_gate}\"
  : >\"\${remote_start_ready}\"
  chmod 600 \"\${remote_start_ready}\"
  setsid --fork --wait bash --noprofile --norc -e -u -o pipefail -c '
    ready_file=\$1
    gate_file=\$2
    shift 2
    printf \"%s\\n\" \"\${BASHPID}\" >\"\${ready_file}\"
    IFS= read -r start_token <\"\${gate_file}\"
    [ \"\${start_token}\" = start ]
    exec \"\$@\"
  ' remote-verifier-launch \
    \"\${remote_start_ready}\" \"\${remote_start_gate}\" \
    timeout --signal=TERM --kill-after=150s ${REMOTE_VERIFY_TIMEOUT_SEC}s \
      env \
      STREAMSERVER_VERIFY_BUNDLE=$(shell_quote "${remote_bundle}") \
      STREAMSERVER_VERIFY_DIR=$(shell_quote "${REMOTE_DIR}") \
      STREAMSERVER_VERIFY_REPORT=$(shell_quote "${remote_report}") \
      STREAMSERVER_VERIFY_GPU_HARDWARE_MODE=$(shell_quote "${GPU_HARDWARE_MODE}") \
      bash --noprofile --norc \
        $(shell_quote "${remote_script}") >/dev/null 2>&1 &
  remote_launcher_pid=\$!
  launcher_identity=\$(remote_process_identity \
    \"\${remote_launcher_pid}\" 2>/dev/null || true)
  remote_launcher_starttime=\${launcher_identity#* }
  remote_ready_value=
  for _ in \$(seq 1 500); do
    IFS= read -r remote_ready_value <\"\${remote_start_ready}\" || true
    case \"\${remote_ready_value}\" in
      ''|*[!0-9]*) remote_child_pid= ;;
      *) remote_child_pid=\${remote_ready_value} ;;
    esac
    identity=\$(remote_process_identity \"\${remote_child_pid}\" 2>/dev/null || true)
    remote_child_pgid=\${identity%% *}
    remote_child_starttime=\${identity#* }
    [ -n \"\${remote_launcher_starttime}\" ] \
      && [ \"\${remote_child_pgid}\" = \"\${remote_child_pid}\" ] \
      && [ -n \"\${remote_child_starttime}\" ] && break
    sleep 0.01
  done
  if [ -z \"\${remote_launcher_starttime}\" ] \
    || [ -z \"\${remote_child_pid}\" ] \
    || [ \"\${remote_child_pgid}\" != \"\${remote_child_pid}\" ] \
    || [ -z \"\${remote_child_starttime}\" ]; then
    printf '%s\\n' \
      \"remote verifier ownership handshake failed: pid=\${remote_child_pid} ready=\${remote_ready_value:-missing} pgid=\${remote_child_pgid:-missing} starttime=\${remote_child_starttime:-missing}\" \
      >&2
    remote_status=125
    cleanup_remote_run
  else
    remote_group_owned=1
    if ! printf 'start\\n' >\"\${remote_start_gate}\"; then
      remote_status=125
      remote_cleanup_failed=1
      cleanup_remote_group || true
    fi
    rm -f -- \"\${remote_start_gate}\" \"\${remote_start_ready}\"
  fi
  if [ \"\${remote_status}\" -ne 125 ]; then
    wait \"\${remote_launcher_pid}\"
    remote_status=\$?
    if ! cleanup_remote_group; then
      remote_cleanup_failed=1
      remote_status=1
    fi
  fi
fi
if [ \"\${remote_cleanup_failed}\" -eq 0 ] \
  && [ -f $(shell_quote "${remote_report}") ]; then
  cat -- $(shell_quote "${remote_report}")
else
  remote_status=1
fi
exit \"\${remote_status}\"
"

    log "开始远端验证，不使用 Docker"
    set +e
    ssh_run "${remote_command}" "${remote_outer_timeout}" \
      | capture_stream_capped \
        "${LOCAL_REPORT_TMP}" "${OUTER_REPORT_LIMIT_BYTES}"
    capture_statuses=("${PIPESTATUS[@]}")
    status="${capture_statuses[0]}"
    report_capture_status="${capture_statuses[1]}"
    if [ "${report_capture_status}" -ne 0 ]; then
      status=1
    fi
    set -e
  fi
  rm -f -- "${REMOTE_SCRIPT_LOCAL}"
  REMOTE_SCRIPT_LOCAL=""

  log "收集目标验证报告: ${remote_report}"
  final_report="${OUTPUT_DIR}/${report_name}"
  set +e
  if [ "${LOCAL_MODE}" -eq 1 ]; then
    copy_file_capped \
      "${remote_report}" "${LOCAL_REPORT_TMP}" "${OUTER_REPORT_LIMIT_BYTES}"
    report_transfer_status=$?
  else
    if [ "${report_capture_status}" -eq 0 ] \
      && [ -s "${LOCAL_REPORT_TMP}" ]; then
      report_transfer_status=0
    else
      report_transfer_status=1
    fi
  fi
  set -e
  if [ "${report_transfer_status}" -ne 0 ]; then
    rm -f -- "${LOCAL_REPORT_TMP}"
    LOCAL_REPORT_TMP=""
    log "ERROR: 远端验证报告下载失败，未发布本地报告"
    if [ "${status}" -eq 0 ]; then
      status=1
    fi
    exit "${status}"
  fi

  failure_zero_count="$(grep -Fxc -- '- failures: 0' "${LOCAL_REPORT_TMP}" || true)"
  failure_summary_count="$(grep -Ec '^- failures: ' "${LOCAL_REPORT_TMP}" || true)"
  pass_result_count="$(grep -Fxc -- '- result: PASS' "${LOCAL_REPORT_TMP}" || true)"
  result_summary_count="$(grep -Ec '^- result: ' "${LOCAL_REPORT_TMP}" || true)"
  summary_tail="$(tail -n 3 "${LOCAL_REPORT_TMP}")"
  expected_summary="$(printf '%s\n' \
    '## Summary' \
    '- failures: 0' \
    '- result: PASS')"
  if [ "${failure_zero_count}" -ne 1 ] \
    || [ "${failure_summary_count}" -ne 1 ] \
    || [ "${pass_result_count}" -ne 1 ] \
    || [ "${result_summary_count}" -ne 1 ] \
    || [ "${summary_tail}" != "${expected_summary}" ]; then
    log "ERROR: 远端验证报告缺少唯一的成功摘要"
    if [ "${status}" -eq 0 ]; then
      status=1
    fi
  fi

  mv -f -- "${LOCAL_REPORT_TMP}" "${final_report}"
  LOCAL_REPORT_TMP=""
  log "本地报告: ${final_report}"
  exit "${status}"
}

main "$@"
