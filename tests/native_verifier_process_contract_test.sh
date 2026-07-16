#!/usr/bin/env bash
# Globals below are consumed by functions sourced dynamically from the
# generated verifier body, which ShellCheck cannot follow.
# shellcheck disable=SC2034
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VERIFY_SCRIPT="${REPO_ROOT}/scripts/verify-native-bundle-on-target.sh"
NATIVE_WORKFLOW="${REPO_ROOT}/.github/workflows/server-native-bundles.yml"

if [ "$(uname -s)" != Linux ]; then
  echo 'SKIP: native verifier process contract requires Linux' >&2
  exit 77
fi

pid_is_live_non_zombie() {
  local pid="$1"
  local stat_line stat_rest
  [ -r "/proc/${pid}/stat" ] || return 1
  IFS= read -r stat_line 2>/dev/null <"/proc/${pid}/stat" || return 1
  stat_rest="${stat_line##*) }"
  # shellcheck disable=SC2086
  set -- ${stat_rest}
  [ "$#" -ge 1 ] && [ "${1}" != Z ]
}

if grep -Fq "(cd '\${ROOT}/runtime/zlm' &&" "${VERIFY_SCRIPT}"; then
  echo 'ZLM smoke still orphans MediaServer through a short-lived subshell' >&2
  exit 1
fi

grep -Fq 'exec \"\${VERIFY_ROOT}/runtime/zlm/lib/ld-linux-x86-64.so.2\"' "${VERIFY_SCRIPT}" || {
  echo 'ZLM smoke does not exec MediaServer as the directly tracked child' >&2
  exit 1
}

grep -Fq 'cleanup_zlm_smoke() {' "${VERIFY_SCRIPT}" || {
  echo 'ZLM smoke does not define a shared exit cleanup handler' >&2
  exit 1
}

grep -Fq 'wait \"\${pid}\" 2>/dev/null || true' "${VERIFY_SCRIPT}" || {
  echo 'ZLM smoke cleanup does not reap the tracked child' >&2
  exit 1
}

grep -Fq 'trap cleanup_zlm_smoke EXIT' "${VERIFY_SCRIPT}" || {
  echo 'ZLM smoke cleanup is not installed for every shell exit path' >&2
  exit 1
}

grep -Fq 'cleanup_local_verifier() {' "${VERIFY_SCRIPT}" || {
  echo 'native verifier does not define one local EXIT cleanup handler' >&2
  exit 1
}

grep -Fq 'rm -f -- "${REMOTE_SCRIPT_LOCAL}"' "${VERIFY_SCRIPT}" || {
  echo 'native verifier does not remove its generated local helper on failure' >&2
  exit 1
}

grep -Fq 'trap cleanup_local_verifier EXIT' "${VERIFY_SCRIPT}" || {
  echo 'native verifier local cleanup is not active across upload and SSH failures' >&2
  exit 1
}

grep -Fq 'trap '\''rm -rf \"\${tmp}\"'\'' EXIT' "${VERIFY_SCRIPT}" || {
  echo 'FFmpeg smoke does not clean its temporary directory on every exit path' >&2
  exit 1
}

grep -Fq 'rm -rf -- "${POSTGRES_SMOKE_TMP}"' "${VERIFY_SCRIPT}" || {
  echo 'PostgreSQL smoke cleanup does not remove its temporary runtime tree' >&2
  exit 1
}
grep -Fq 'rm -rf -- "${POSTGRES_SMOKE_CONTROL_DIR}"' "${VERIFY_SCRIPT}" || {
  echo 'PostgreSQL smoke cleanup does not remove its private control tree' >&2
  exit 1
}
grep -Fq 'rm -rf -- "${POSTGRES_SMOKE_TOOL_DIR}"' "${VERIFY_SCRIPT}" || {
  echo 'PostgreSQL smoke cleanup does not remove its root-owned tool tree' >&2
  exit 1
}
grep -Fq 'rm -rf -- "${POSTGRES_SMOKE_SOCKET_DIR}"' "${VERIFY_SCRIPT}" || {
  echo 'PostgreSQL smoke cleanup does not remove its short socket directory' >&2
  exit 1
}

postgres_trap_line="$(grep -n 'trap cleanup_postgres_smoke EXIT' "${VERIFY_SCRIPT}" | head -n 1 | cut -d: -f1)"
postgres_first_validation_line="$(grep -n 'pg_share_dir="$(postgres_share_dir)"' "${VERIFY_SCRIPT}" | head -n 1 | cut -d: -f1)"
[ -n "${postgres_trap_line}" ] \
  && [ -n "${postgres_first_validation_line}" ] \
  && [ "${postgres_trap_line}" -lt "${postgres_first_validation_line}" ] || {
  echo 'PostgreSQL smoke installs its EXIT cleanup only after fallible validation' >&2
  exit 1
}

postgres_bounded_block="$(sed -n \
  '/if run_bounded_capture \\/,/postgres_smoke.*; then/p' \
  "${VERIFY_SCRIPT}")"
printf '%s\n' "${postgres_bounded_block}" \
  | grep -Fq '"${POSTGRES_SMOKE_TIMEOUT_SEC}"' || {
    echo 'PostgreSQL smoke does not execute through the bounded capture deadline' >&2
    exit 1
  }

grep -Fq 'bash --noprofile --norc -e -u -o pipefail -c "$*"' \
    "${VERIFY_SCRIPT}" || {
  echo 'generic native smoke commands are not executed with strict shell semantics' >&2
  exit 1
}

if grep -Fq 'if [ "${BASH_VERSINFO[0]}" -ge 5 ]; then' "${VERIFY_SCRIPT}"; then
  echo 'bounded capture still assumes Bash 5.0 supports wait -p' >&2
  exit 1
fi
grep -Fq '[ "${BASH_VERSINFO[1]}" -ge 1 ]' "${VERIFY_SCRIPT}" || {
  echo 'bounded capture does not fall back for Bash 5.0 wait -p compatibility' >&2
  exit 1
}

gateway_smoke="$(sed -n \
  '/run_shell "media-gateway startup smoke"/,/^"$/p' "${VERIFY_SCRIPT}")"
printf '%s\n' "${gateway_smoke}" \
  | grep -Fq 'kill -KILL \"\${pid}\"' || {
  echo 'media-gateway smoke cleanup can wait forever when TERM is ignored' >&2
  exit 1
}

has_unbounded_internal_log_redirection() {
  grep -Fq '.log" 2>&1' "$1" \
    || grep -Fq '.log\" 2>&1' "$1"
}

LOG_DETECTOR_FIXTURE="$(mktemp)"
printf '%s\n' '>"${tmp}/plain.log" 2>&1' \
  '>\"\${tmp}/escaped.log\" 2>&1' >"${LOG_DETECTOR_FIXTURE}"
has_unbounded_internal_log_redirection "${LOG_DETECTOR_FIXTURE}" || {
  rm -f -- "${LOG_DETECTOR_FIXTURE}"
  echo 'internal-log detector does not recognize its escaped/unescaped fixtures' >&2
  exit 1
}
rm -f -- "${LOG_DETECTOR_FIXTURE}"

if has_unbounded_internal_log_redirection "${VERIFY_SCRIPT}"; then
  echo 'native runtime smoke still redirects service output to an unbounded internal log' >&2
  exit 1
fi

CONTRACT_TMP="$(mktemp -d)"
cleanup_contract_tmp() {
  if [ "${KEEP_NATIVE_VERIFIER_CONTRACT_TMP:-0}" = 1 ]; then
    printf 'kept native verifier process tmp: %s\n' "${CONTRACT_TMP}" >&2
  else
    rm -rf -- "${CONTRACT_TMP}"
  fi
}
trap cleanup_contract_tmp EXIT
for function_name in \
  process_starttime \
  process_pgid \
  process_group_has_live_members \
  terminate_registered_process \
  register_postgres_pid \
  registered_process_is_live \
  unregister_postgres_pid \
  cleanup_postgres_smoke; do
  sed -n "/^${function_name}() {\$/,/^}\$/p" "${VERIFY_SCRIPT}" \
    >>"${CONTRACT_TMP}/postgres-cleanup.sh"
done
# shellcheck disable=SC1090
source "${CONTRACT_TMP}/postgres-cleanup.sh"
for function_name in \
  process_starttime \
  process_pgid \
  process_group_has_live_members \
  terminate_registered_process \
  register_postgres_pid \
  registered_process_is_live \
  unregister_postgres_pid; do
  declare -F "${function_name}" >/dev/null || {
    printf 'PostgreSQL cleanup helper is missing: %s\n' "${function_name}" >&2
    exit 1
  }
done
POSTGRES_SMOKE_TMP="${CONTRACT_TMP}/postgres-failure-tree"
mkdir -p "${POSTGRES_SMOKE_TMP}"
POSTGRES_SMOKE_CONTROL_DIR="${CONTRACT_TMP}/postgres-control-tree"
POSTGRES_SMOKE_TOOL_DIR="${CONTRACT_TMP}/postgres-tool-tree"
POSTGRES_SMOKE_SOCKET_DIR="${CONTRACT_TMP}/postgres-socket-tree"
mkdir -m 700 "${POSTGRES_SMOKE_CONTROL_DIR}"
mkdir -m 755 "${POSTGRES_SMOKE_TOOL_DIR}"
mkdir -m 700 "${POSTGRES_SMOKE_SOCKET_DIR}"
POSTGRES_SMOKE_PID_REGISTRY="${POSTGRES_SMOKE_CONTROL_DIR}/pids"
: >"${POSTGRES_SMOKE_PID_REGISTRY}"
chmod 600 "${POSTGRES_SMOKE_PID_REGISTRY}"
printf '%s\n' must-be-removed >"${POSTGRES_SMOKE_TMP}/sentinel"

setsid sleep 30 &
registered_live_pid=$!
register_postgres_pid "${registered_live_pid}"
registered_process_is_live "${registered_live_pid}" || {
  kill -KILL "${registered_live_pid}" >/dev/null 2>&1 || true
  wait "${registered_live_pid}" 2>/dev/null || true
  echo 'registered process identity rejected its live process group' >&2
  exit 1
}
kill -TERM "${registered_live_pid}" >/dev/null 2>&1 || true
wait "${registered_live_pid}" 2>/dev/null || true
if registered_process_is_live "${registered_live_pid}"; then
  echo 'registered process identity accepted an exited process' >&2
  exit 1
fi
unregister_postgres_pid "${registered_live_pid}"

setsid sleep 30 &
invalid_registry_sentinel=$!
invalid_registry_start="$(process_starttime "${invalid_registry_sentinel}")"
if terminate_registered_process \
    "${invalid_registry_sentinel}" "${invalid_registry_start}" \
    "$((invalid_registry_sentinel + 1))"; then
  kill -KILL "${invalid_registry_sentinel}" >/dev/null 2>&1 || true
  wait "${invalid_registry_sentinel}" 2>/dev/null || true
  echo 'registered cleanup accepted a PID/PGID mismatch' >&2
  exit 1
fi
pid_is_live_non_zombie "${invalid_registry_sentinel}" || {
  echo 'invalid registered cleanup tuple acted on the sentinel process' >&2
  exit 1
}
kill -TERM "${invalid_registry_sentinel}" >/dev/null 2>&1 || true
wait "${invalid_registry_sentinel}" 2>/dev/null || true

ORPHAN_GROUP_CHILD_PID_FILE="${POSTGRES_SMOKE_TMP}/orphan-group-child.pid"
setsid bash -c '
  (
    trap "" HUP INT TERM
    printf "%s\n" "${BASHPID}" >"$1"
    while :; do sleep 1; done
  ) &
  wait
' orphan-group-driver "${ORPHAN_GROUP_CHILD_PID_FILE}" &
orphan_group_leader=$!
register_postgres_pid "${orphan_group_leader}"
read -r orphan_group_start orphan_group_pgid < <(awk \
  -v expected_pid="${orphan_group_leader}" \
  '$1 == expected_pid { print $2, $3; exit }' \
  "${POSTGRES_SMOKE_PID_REGISTRY}")
for _ in $(seq 1 100); do
  [ -s "${ORPHAN_GROUP_CHILD_PID_FILE}" ] && break
  sleep 0.01
done
[ -s "${ORPHAN_GROUP_CHILD_PID_FILE}" ] || {
  kill -KILL -- "-${orphan_group_pgid}" >/dev/null 2>&1 || true
  echo 'orphaned process-group fixture did not start its child' >&2
  exit 1
}
orphan_group_child="$(cat "${ORPHAN_GROUP_CHILD_PID_FILE}")"
kill -KILL "${orphan_group_leader}" >/dev/null 2>&1 || true
wait "${orphan_group_leader}" 2>/dev/null || true
pid_is_live_non_zombie "${orphan_group_child}" || {
  echo 'orphaned process-group fixture child did not survive its leader' >&2
  exit 1
}
terminate_registered_process \
  "${orphan_group_leader}" "${orphan_group_start}" "${orphan_group_pgid}"
unregister_postgres_pid "${orphan_group_leader}"
if pid_is_live_non_zombie "${orphan_group_child}"; then
  kill -KILL "${orphan_group_child}" >/dev/null 2>&1 || true
  echo 'registered cleanup left a live child after its group leader exited' >&2
  exit 1
fi

stubborn_pids=()
for _ in 1 2 3; do
  setsid bash -c 'trap "" TERM; while :; do sleep 1; done' &
  stubborn_pids+=("$!")
done
stubborn_pid="${stubborn_pids[0]}"
setsid sleep 30 &
reuse_sentinel_pid=$!
reuse_sentinel_start="$(process_starttime "${reuse_sentinel_pid}")"
reuse_sentinel_pgid="$(process_pgid "${reuse_sentinel_pid}")"
for registered_pid in "${stubborn_pids[@]}"; do
  register_postgres_pid "${registered_pid}"
done
printf '%s\t%s\t%s\n' \
  "${reuse_sentinel_pid}" "$((reuse_sentinel_start + 1))" \
  "${reuse_sentinel_pgid}" \
  >>"${POSTGRES_SMOKE_PID_REGISTRY}"
cleanup_postgres_smoke
[ ! -e "${CONTRACT_TMP}/postgres-failure-tree" ] || {
  echo 'actual PostgreSQL cleanup function left its failure-path temp tree behind' >&2
  exit 1
}
for removed_control_path in \
  "${CONTRACT_TMP}/postgres-control-tree" \
  "${CONTRACT_TMP}/postgres-tool-tree" \
  "${CONTRACT_TMP}/postgres-socket-tree"; do
  [ ! -e "${removed_control_path}" ] || {
    echo "PostgreSQL cleanup left a control path: ${removed_control_path}" >&2
    exit 1
  }
done
for registered_pid in "${stubborn_pids[@]}"; do
  if kill -0 "${registered_pid}" >/dev/null 2>&1; then
    kill -KILL "${registered_pid}" >/dev/null 2>&1 || true
    wait "${registered_pid}" 2>/dev/null || true
    echo 'PostgreSQL cleanup did not KILL every ignored-TERM process group' >&2
    exit 1
  fi
done
kill -0 "${reuse_sentinel_pid}" >/dev/null 2>&1 || {
  echo 'PostgreSQL cleanup killed a PID whose starttime no longer matched' >&2
  exit 1
}
kill "${reuse_sentinel_pid}" >/dev/null 2>&1 || true
wait "${reuse_sentinel_pid}" 2>/dev/null || true

for function_name in \
  outer_process_identity \
  outer_group_has_live_members \
  sweep_local_verifier_group \
  run_owned_local_verifier; do
  sed -n "/^${function_name}() {\$/,/^}\$/p" "${VERIFY_SCRIPT}" \
    >>"${CONTRACT_TMP}/local-group-sweep.sh"
done
# shellcheck disable=SC1090
source "${CONTRACT_TMP}/local-group-sweep.sh"
LOCAL_VERIFY_PID=""
LOCAL_VERIFY_STARTTIME=""
LOCAL_VERIFY_PGID=""
LOCAL_VERIFY_GROUP_OWNED=0
LOCAL_ORPHAN_PID_FILE="${CONTRACT_TMP}/local-orphan.pid"
export LOCAL_ORPHAN_PID_FILE
set +e
run_owned_local_verifier bash -c '
    (
      trap "" HUP INT TERM
      printf "%s\n" "${BASHPID}" >"${LOCAL_ORPHAN_PID_FILE}"
      while :; do sleep 1; done
    ) &
    for _ in $(seq 1 100); do
      [ -s "${LOCAL_ORPHAN_PID_FILE}" ] && break
      sleep 0.01
    done
    exit 0
  '
LOCAL_ORPHAN_STATUS=$?
set -e
[ "${LOCAL_ORPHAN_STATUS}" -eq 0 ] || {
  echo 'owned local verifier did not preserve a normal status 0' >&2
  exit 1
}
local_orphan_pid="$(cat "${CONTRACT_TMP}/local-orphan.pid")"
if pid_is_live_non_zombie "${local_orphan_pid}"; then
  kill -KILL "${local_orphan_pid}" >/dev/null 2>&1 || true
  echo 'owned local verifier leaked a same-PGID descendant after normal exit' >&2
  exit 1
fi

setsid bash -c 'trap "" TERM; while :; do sleep 1; done' &
LOCAL_VERIFY_PID=$!
LOCAL_VERIFY_STARTTIME=""
LOCAL_VERIFY_PGID=""
LOCAL_VERIFY_GROUP_OWNED=0
local_race_pid="${LOCAL_VERIFY_PID}"
sweep_local_verifier_group
if pid_is_live_non_zombie "${local_race_pid}"; then
  kill -KILL "${local_race_pid}" >/dev/null 2>&1 || true
  echo 'local identity-race fallback left its unowned child running' >&2
  exit 1
fi

sed -n '/^copy_file_capped() {$/,/^}$/p' "${VERIFY_SCRIPT}" \
  >"${CONTRACT_TMP}/copy-file-capped.sh"
# shellcheck disable=SC1090
source "${CONTRACT_TMP}/copy-file-capped.sh"
truncate -s 17825792 "${CONTRACT_TMP}/oversized-local-report"
if copy_file_capped \
  "${CONTRACT_TMP}/oversized-local-report" \
  "${CONTRACT_TMP}/bounded-local-report" 16777216; then
  echo 'local report copy over 16 MiB unexpectedly passed' >&2
  exit 1
fi
[ ! -e "${CONTRACT_TMP}/bounded-local-report" ] || {
  echo 'local report cap left a partial destination' >&2
  exit 1
}
mkfifo "${CONTRACT_TMP}/growing-local-report.fifo"
( head -c 17825792 /dev/zero >"${CONTRACT_TMP}/growing-local-report.fifo" ) &
fifo_writer_pid=$!
if copy_file_capped \
  "${CONTRACT_TMP}/growing-local-report.fifo" \
  "${CONTRACT_TMP}/bounded-fifo-report" 16777216; then
  kill -KILL "${fifo_writer_pid}" >/dev/null 2>&1 || true
  echo 'non-regular growing local report source unexpectedly passed' >&2
  exit 1
fi
wait "${fifo_writer_pid}" 2>/dev/null || true
[ ! -e "${CONTRACT_TMP}/bounded-fifo-report" ] || {
  echo 'FIFO local report cap left a partial destination' >&2
  exit 1
}

{
  sed -n '/^process_starttime() {$/,/^}$/p' "${VERIFY_SCRIPT}"
  sed -n '/^process_is_live_non_zombie() {$/,/^}$/p' "${VERIFY_SCRIPT}"
  sed -n '/^process_pgid() {$/,/^}$/p' "${VERIFY_SCRIPT}"
  sed -n '/^process_group_has_live_members() {$/,/^}$/p' "${VERIFY_SCRIPT}"
  sed -n '/^bounded_stream_to_file() {$/,/^}$/p' "${VERIFY_SCRIPT}"
  sed -n '/^terminate_bounded_group() {$/,/^}$/p' "${VERIFY_SCRIPT}"
  sed -n '/^fifo_holder_pids() {$/,/^}$/p' "${VERIFY_SCRIPT}"
  sed -n '/^terminate_fifo_holders() {$/,/^}$/p' "${VERIFY_SCRIPT}"
  sed -n '/^run_bounded_capture() {$/,/^}$/p' "${VERIFY_SCRIPT}"
  sed -n '/^mark_report_truncated() {$/,/^}$/p' "${VERIFY_SCRIPT}"
  sed -n '/^append_capped_output() {$/,/^}$/p' "${VERIFY_SCRIPT}"
  sed -n '/^run_shell() {$/,/^}$/p' "${VERIFY_SCRIPT}"
} >"${CONTRACT_TMP}/run-shell.sh"
# shellcheck disable=SC1090
source "${CONTRACT_TMP}/run-shell.sh"
REPORT="${CONTRACT_TMP}/strict-shell.report"
RUN_WORK="${CONTRACT_TMP}/strict-shell-work"
COMMAND_OUTPUT_LIMIT_BYTES=1048576
REPORT_LIMIT_BYTES=16777216
REPORT_TRUNCATED=0
REPORT_FINALIZING=0
DEADLINE_KILL_AFTER_SEC=2
CAPTURE_KILL_AFTER_SEC=2
mkdir -p "${RUN_WORK}"
: >"${REPORT}"
FAILURES=0
SMOKE_SHELL_TIMEOUT_SEC=5
append() { :; }
enforce_report_limit() { :; }
record_ok() { :; }
record_failure() { FAILURES=$((FAILURES + 1)); }
run_shell fault-injection \
  "touch '${CONTRACT_TMP}/before'; false; touch '${CONTRACT_TMP}/after'"
[ -e "${CONTRACT_TMP}/before" ] \
  && [ ! -e "${CONTRACT_TMP}/after" ] \
  && [ "${FAILURES}" -eq 1 ] || {
  echo 'actual run_shell implementation continued after an injected failure' >&2
  exit 1
}
ESCAPED_PROCESS_PID_FILE="${CONTRACT_TMP}/escaped-fifo-holder.pid"
run_shell fifo-reader-escape \
  "setsid bash --noprofile --norc -c 'trap \"\" HUP INT TERM; printf \"%s\\n\" \"\${BASHPID}\" >\"${ESCAPED_PROCESS_PID_FILE}\"; while :; do sleep 1; done' & for _ in \$(seq 1 100); do [ -s \"${ESCAPED_PROCESS_PID_FILE}\" ] && break; sleep 0.01; done; [ -s \"${ESCAPED_PROCESS_PID_FILE}\" ]; exit 0"
[ "${FAILURES}" -eq 2 ] || {
  echo 'escaped FIFO holder did not make the bounded capture fail closed' >&2
  exit 1
}
[ -s "${ESCAPED_PROCESS_PID_FILE}" ] || {
  echo 'escaped FIFO holder fixture did not start' >&2
  exit 1
}
escaped_process_pid="$(cat "${ESCAPED_PROCESS_PID_FILE}")"
if pid_is_live_non_zombie "${escaped_process_pid}"; then
  kill -KILL "${escaped_process_pid}" >/dev/null 2>&1 || true
  echo 'bounded capture leaked a detached process holding its private FIFO' >&2
  exit 1
fi
AGGREGATE_WRITER_ONE_PID_FILE="${CONTRACT_TMP}/aggregate-writer-one.pid"
AGGREGATE_WRITER_TWO_PID_FILE="${CONTRACT_TMP}/aggregate-writer-two.pid"
run_shell aggregate-background-output \
  "python3 -c 'import sys; sys.stdout.write(\"a\" * (700 * 1024))' & first=\$!; printf '%s\\n' \"\${first}\" >\"${AGGREGATE_WRITER_ONE_PID_FILE}\"; python3 -c 'import sys; sys.stdout.write(\"b\" * (700 * 1024))' & second=\$!; printf '%s\\n' \"\${second}\" >\"${AGGREGATE_WRITER_TWO_PID_FILE}\"; wait"
[ "${FAILURES}" -eq 3 ] || {
  echo 'two sub-limit background writers did not fail the shared output budget' >&2
  exit 1
}
for writer_pid_file in \
  "${AGGREGATE_WRITER_ONE_PID_FILE}" \
  "${AGGREGATE_WRITER_TWO_PID_FILE}"; do
  [ -s "${writer_pid_file}" ] || {
    echo "aggregate writer fixture did not record a PID: ${writer_pid_file}" >&2
    exit 1
  }
  writer_pid="$(cat "${writer_pid_file}")"
  if pid_is_live_non_zombie "${writer_pid}"; then
    kill -KILL "${writer_pid}" >/dev/null 2>&1 || true
    echo "shared output overflow leaked background writer ${writer_pid}" >&2
    exit 1
  fi
done
FAST_CAPTURE_OUTPUT="${CONTRACT_TMP}/fast-capture.output"
for iteration in $(seq 1 1000); do
  if ! run_bounded_capture \
      "${FAST_CAPTURE_OUTPUT}" 1024 2 /bin/true; then
    printf 'fast successful command was misclassified at iteration %s\n' \
      "${iteration}" >&2
    exit 1
  fi
done
for iteration in $(seq 1 1000); do
  if run_bounded_capture \
      "${FAST_CAPTURE_OUTPUT}" 1024 2 bash -c 'exit 42'; then
    printf 'fast failing command false-greened at iteration %s\n' \
      "${iteration}" >&2
    exit 1
  else
    fast_failure_status=$?
  fi
  if [ "${fast_failure_status}" -ne 42 ]; then
    printf 'fast failing command lost status 42 at iteration %s: %s\n' \
      "${iteration}" "${fast_failure_status}" >&2
    exit 1
  fi
done
rm -f -- "${FAST_CAPTURE_OUTPUT}"

extract_remote_verifier() {
  awk '
    !capturing && index($0, "REMOTE_SCRIPT_LOCAL") && index($0, "<<") {
      capturing = 1
      next
    }
    capturing && $0 == "REMOTE" { exit }
    capturing { print }
  ' "${VERIFY_SCRIPT}"
}

write_cpu_manifest() {
  local root="$1"
  printf '%s\n' \
    'BUNDLE_VERSION=v0.1.0' \
    'BUNDLE_VARIANT=cpu-only' \
    'BUNDLE_GPU_SUPPORT=false' \
    'BUNDLE_WORKER_SUPPORT=true' \
    'BUNDLE_POSTGRES_RUNTIME=true' \
    'DEPLOY_MODE=native' \
    'MEDIA_CORE_BINARY_PATH=binaries/media-core-linux-amd64' \
    'MEDIA_AGENT_BINARY_PATH=binaries/media-agent-linux-amd64' \
    'MEDIA_GATEWAY_BINARY_PATH=binaries/media-gateway-linux-amd64' \
    'STREAMSERVER_CONFIG_BINARY_PATH=binaries/streamserver-config-linux-amd64' \
    'MEDIA_CORE_UI_PATH=ui/media-core' \
    'FFMPEG_CPU_BINARY_PATH=runtime/ffmpeg/cpu/bin/ffmpeg' \
    'FFPROBE_CPU_BINARY_PATH=runtime/ffmpeg/cpu/bin/ffprobe' \
    'FFMPEG_CPU_LIB_PATH=runtime/ffmpeg/cpu/lib' \
    'FFMPEG_GPU_BINARY_PATH=runtime/ffmpeg/gpu/bin/ffmpeg' \
    'FFPROBE_GPU_BINARY_PATH=runtime/ffmpeg/gpu/bin/ffprobe' \
    'FFMPEG_GPU_LIB_PATH=runtime/ffmpeg/gpu/lib' \
    'ZLM_BINARY_PATH=runtime/zlm/MediaServer' \
    'ZLM_DEFAULT_PEM_PATH=runtime/zlm/default.pem' \
    'ZLM_LIB_PATH=runtime/zlm/lib' \
    'POSTGRES_RUNTIME_PATH=runtime/postgres' \
    'POSTGRES_BIN_PATH=runtime/postgres/bin' \
    'POSTGRES_LIB_PATH=runtime/postgres/lib' \
    'POSTGRES_EXTENSION_MANIFEST_PATH=runtime/postgres/postgres-extension-manifest.tsv' \
    >"${root}/package-manifest.env"
}

package_fixture() {
  local root="$1"
  local archive="$2"
  local variant
  variant="$(awk -F= '$1 == "BUNDLE_VARIANT" { print $2; exit }' \
    "${root}/package-manifest.env")"
  printf '%s\n' \
    "bundle_name=$(basename "${root}")" \
    'version=0.1.0' \
    'built_at=2026-01-01T00:00:00Z' \
    'builder_os=Linux' \
    'builder_arch=x86_64' \
    'git_commit=contract' \
    "bundle_variant=${variant}" \
    'target_runtime=docker-free' \
    'verification_recommended_location=target-server' \
    >"${root}/build-info.txt"
  find "${root}" \( -type f -o -type d \) \
    -exec chmod go-w,u-s,g-s,o-t {} +
  (
    cd "${root}"
    find . -type f ! -name SHA256SUMS -print \
      | LC_ALL=C sort \
      | while IFS= read -r file; do
          sha256sum "${file#./}"
        done >SHA256SUMS
  )
  chmod 644 "${root}/SHA256SUMS"
  tar -czf "${archive}" -C "$(dirname "${root}")" "$(basename "${root}")"
}

compile_stubborn_fixture_binaries() {
  local root="$1"
  local source="${CONTRACT_TMP}/native-fixture.c"
  local binary="${CONTRACT_TMP}/native-fixture"
  cat >"${source}" <<'C'
#include <arpa/inet.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

static int address_port(const char *value) {
  const char *colon = value ? strrchr(value, ':') : NULL;
  return colon ? atoi(colon + 1) : 0;
}

static int run_http_server(const char *address_env, int gateway_mode) {
  int fd = socket(AF_INET, SOCK_STREAM, 0);
  int enabled = 1;
  struct sockaddr_in address;
  setsockopt(fd, SOL_SOCKET, SO_REUSEADDR, &enabled, sizeof(enabled));
  memset(&address, 0, sizeof(address));
  address.sin_family = AF_INET;
  address.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
  address.sin_port = htons(address_port(getenv(address_env)));
  if (fd < 0 || bind(fd, (struct sockaddr *)&address, sizeof(address)) != 0 \
      || listen(fd, 8) != 0) return 2;
  for (;;) {
    int client = accept(fd, NULL, NULL);
    char request[1024];
    const char *response;
    ssize_t received;
    ssize_t sent;
    static const char ok_response[] =
      "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nOK";
    static const char ready_response[] =
      "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n"
      "Content-Length: 18\r\nConnection: close\r\n\r\n{\"status\":\"ready\"}";
    static const char status_response[] =
      "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n"
      "Content-Length: 22\r\nConnection: close\r\n\r\n{\"queue_high_water\":0}";
    if (client < 0) continue;
    received = read(client, request, sizeof(request) - 1);
    if (received >= 0) {
      request[received] = '\0';
      response = ok_response;
      if (gateway_mode && strstr(request, "GET /api/readyz "))
        response = ready_response;
      else if (gateway_mode && strstr(request, "GET /api/status "))
        response = status_response;
      sent = write(client, response, strlen(response));
    } else {
      sent = -1;
    }
    close(client);
    if (sent < 0) continue;
  }
}

int main(int argc, char **argv) {
  const char *name = strrchr(argv[0], '/');
  name = name ? name + 1 : argv[0];
  signal(SIGPIPE, SIG_IGN);
  if (strstr(name, "media-core")) {
    if (argc >= 3 && strcmp(argv[1], "auth") == 0 \
        && strcmp(argv[2], "check-config") == 0)
      puts("authentication and Agent CA configuration is valid");
    return 0;
  }
  if (strstr(name, "media-agent")) {
    if (argc > 1) {
      fputs("unknown media-agent command\n", stderr);
      return 1;
    }
    return run_http_server("AGENT_PUBLIC_MEDIA_ADDR", 0);
  }
  if (strstr(name, "media-gateway")) {
    const char *pid_file = getenv("STUBBORN_GATEWAY_PID_FILE");
    const char *flood_bytes_value = getenv("STUBBORN_GATEWAY_FLOOD_BYTES");
    FILE *output;
    unsigned long flood_bytes = flood_bytes_value \
      ? strtoul(flood_bytes_value, NULL, 10) : 0;
    signal(SIGTERM, SIG_IGN);
    if (pid_file && (output = fopen(pid_file, "w")) != NULL) {
      fprintf(output, "%ld\n", (long)getpid());
      fclose(output);
    }
    while (flood_bytes-- > 0) putchar('x');
    if (flood_bytes_value) putchar('\n');
    puts("media-gateway ready");
    fflush(stdout);
    return run_http_server("MEDIA_GATEWAY_BIND_ADDR", 1);
  }
  if (strstr(name, "streamserver-config")) {
    int index;
    for (index = 1; index + 1 < argc; ++index) {
      if (strcmp(argv[index], "--env") == 0) {
        FILE *output = fopen(argv[index + 1], "w");
        if (!output) return 2;
        fputs("STREAMSERVER_ENV=development\n", output);
        fclose(output);
        break;
      }
    }
    return 0;
  }
  return 2;
}
C
  cc -static -O2 -s "${source}" -o "${binary}"
  for name in media-core media-agent media-gateway streamserver-config; do
    cp "${binary}" "${root}/binaries/${name}-linux-amd64"
    chmod 755 "${root}/binaries/${name}-linux-amd64"
  done
}

if [ "$(uname -s)" = Linux ]; then
  CONTRACT_FAILURES=0
  contract_failure() {
    printf '%s\n' "$*" >&2
    CONTRACT_FAILURES=$((CONTRACT_FAILURES + 1))
  }

  REMOTE_BODY="${CONTRACT_TMP}/remote-verifier.sh"
  extract_remote_verifier >"${REMOTE_BODY}"
  bash -n "${REMOTE_BODY}" || {
    echo 'generated native target verifier body has invalid shell syntax' >&2
    exit 1
  }
  grep -Fq 'section "Package Shape"' "${REMOTE_BODY}" || {
    echo 'could not extract the generated native target verifier body' >&2
    exit 1
  }
  if grep -Fq '.log' "${REMOTE_BODY}"; then
    contract_failure \
      'generated target verifier contains an internal .log path outside the shared output FIFO'
  fi

  SHAPE_STAGE="${CONTRACT_TMP}/shape-stage"
  SHAPE_ROOT="${SHAPE_STAGE}/streamserver-native-v0.1.0-linux-amd64-cpu-only-20260101"
  mkdir -p "${SHAPE_ROOT}/binaries" "${SHAPE_ROOT}/ui/media-core"
  CONTROL_ROOT="${SHAPE_STAGE}/streamserver-native-v0.1.0-linux-amd64-control-plane-minimal-20260101"
  rm -rf -- "${CONTROL_ROOT}"
  mv -- "${SHAPE_ROOT}" "${CONTROL_ROOT}"
  SHAPE_ROOT="${CONTROL_ROOT}"
  write_cpu_manifest "${SHAPE_ROOT}"
  printf '%s\n' '<!doctype html><title>contract</title>' \
    >"${SHAPE_ROOT}/ui/media-core/index.html"
  for binary in media-core media-agent media-gateway streamserver-config; do
    printf '%s\n' '#!/usr/bin/env sh' 'exit 0' \
      >"${SHAPE_ROOT}/binaries/${binary}-linux-amd64"
    chmod 755 "${SHAPE_ROOT}/binaries/${binary}-linux-amd64"
  done
  package_fixture "${SHAPE_ROOT}" "${CONTRACT_TMP}/missing-runtime.tar.gz"

  set +e
  STREAMSERVER_VERIFY_BUNDLE="${CONTRACT_TMP}/missing-runtime.tar.gz" \
  STREAMSERVER_VERIFY_DIR="${CONTRACT_TMP}/shape-work" \
  STREAMSERVER_VERIFY_REPORT="${CONTRACT_TMP}/shape.report" \
    bash "${REMOTE_BODY}" >/dev/null 2>&1
  SHAPE_STATUS=$?
  set -e
  if [ "${SHAPE_STATUS}" -eq 0 ]; then
    contract_failure \
      'cpu-only manifest false-greened without its declared runtimes or ELF business binaries'
  else
    for expected_failure in \
      'manifest declares worker support but CPU FFmpeg runtime is missing' \
      'manifest declares worker support but ZLMediaKit runtime is missing' \
      'manifest declares PostgreSQL runtime but it is missing'; do
      grep -Fq "${expected_failure}" "${CONTRACT_TMP}/shape.report" || \
        contract_failure \
          "native verifier did not report required shape failure: ${expected_failure}"
    done
  fi

  printf '%s\n' \
    'BUNDLE_VARIANT=cpu-only' \
    'BUNDLE_WORKER_SUPPORT=true' \
    >>"${SHAPE_ROOT}/package-manifest.env"
  sed -i 's/^BUNDLE_GPU_SUPPORT=false$/BUNDLE_GPU_SUPPORT=FALSE/' \
    "${SHAPE_ROOT}/package-manifest.env"
  package_fixture "${SHAPE_ROOT}" "${CONTRACT_TMP}/invalid-manifest.tar.gz"
  set +e
  STREAMSERVER_VERIFY_BUNDLE="${CONTRACT_TMP}/invalid-manifest.tar.gz" \
  STREAMSERVER_VERIFY_DIR="${CONTRACT_TMP}/invalid-manifest-work" \
  STREAMSERVER_VERIFY_REPORT="${CONTRACT_TMP}/invalid-manifest.report" \
    bash "${REMOTE_BODY}" >/dev/null 2>&1
  INVALID_MANIFEST_STATUS=$?
  set -e
  [ "${INVALID_MANIFEST_STATUS}" -ne 0 ] || \
    contract_failure 'invalid or duplicate manifest values were accepted'
  for expected_failure in \
    'package manifest must define BUNDLE_VARIANT exactly once' \
    'package manifest must define BUNDLE_WORKER_SUPPORT exactly once' \
    'package manifest BUNDLE_GPU_SUPPORT must be exactly true or false'; do
    grep -Fq "${expected_failure}" \
      "${CONTRACT_TMP}/invalid-manifest.report" || \
      contract_failure \
        "native verifier did not report strict manifest failure: ${expected_failure}"
  done

  write_cpu_manifest "${SHAPE_ROOT}"
  sed -i \
    -e 's/^BUNDLE_VARIANT=cpu-only$/BUNDLE_VARIANT=control-plane-minimal/' \
    -e 's/^BUNDLE_WORKER_SUPPORT=true$/BUNDLE_WORKER_SUPPORT=false/' \
    -e 's/^BUNDLE_POSTGRES_RUNTIME=true$/BUNDLE_POSTGRES_RUNTIME=false/' \
    "${SHAPE_ROOT}/package-manifest.env"
  mkdir -p "${SHAPE_ROOT}/runtime/ffmpeg/gpu"
  package_fixture "${SHAPE_ROOT}" "${CONTRACT_TMP}/forbidden-runtime.tar.gz"
  set +e
  STREAMSERVER_VERIFY_BUNDLE="${CONTRACT_TMP}/forbidden-runtime.tar.gz" \
  STREAMSERVER_VERIFY_DIR="${CONTRACT_TMP}/forbidden-runtime-work" \
  STREAMSERVER_VERIFY_REPORT="${CONTRACT_TMP}/forbidden-runtime.report" \
    bash "${REMOTE_BODY}" >/dev/null 2>&1
  FORBIDDEN_RUNTIME_STATUS=$?
  set -e
  [ "${FORBIDDEN_RUNTIME_STATUS}" -ne 0 ] || \
    contract_failure \
      'control-plane-minimal bundle accepted a forbidden GPU runtime'
  grep -Fq 'manifest disables GPU support but GPU runtime is present' \
    "${CONTRACT_TMP}/forbidden-runtime.report" || \
    contract_failure \
      'native verifier did not reject a disabled runtime that was packaged'

  rm -rf -- "${SHAPE_ROOT}/runtime"
  write_cpu_manifest "${SHAPE_ROOT}"
  sed -i \
    -e 's/^BUNDLE_VARIANT=cpu-only$/BUNDLE_VARIANT=gpu-enabled/' \
    -e 's/^BUNDLE_GPU_SUPPORT=false$/BUNDLE_GPU_SUPPORT=true/' \
    "${SHAPE_ROOT}/package-manifest.env"
  package_fixture "${SHAPE_ROOT}" "${CONTRACT_TMP}/missing-gpu-runtime.tar.gz"
  set +e
  STREAMSERVER_VERIFY_BUNDLE="${CONTRACT_TMP}/missing-gpu-runtime.tar.gz" \
  STREAMSERVER_VERIFY_DIR="${CONTRACT_TMP}/missing-gpu-runtime-work" \
  STREAMSERVER_VERIFY_REPORT="${CONTRACT_TMP}/missing-gpu-runtime.report" \
    bash "${REMOTE_BODY}" >/dev/null 2>&1
  MISSING_GPU_RUNTIME_STATUS=$?
  set -e
  [ "${MISSING_GPU_RUNTIME_STATUS}" -ne 0 ] || \
    contract_failure 'gpu-enabled bundle accepted a missing GPU runtime'
  grep -Fq 'manifest declares GPU support but GPU FFmpeg runtime is missing' \
    "${CONTRACT_TMP}/missing-gpu-runtime.report" || \
    contract_failure \
      'native verifier did not enforce the gpu-enabled runtime contract'

  write_cpu_manifest "${SHAPE_ROOT}"
  sed -i \
    -e 's/^BUNDLE_VARIANT=cpu-only$/BUNDLE_VARIANT=control-plane-minimal/' \
    -e 's/^BUNDLE_WORKER_SUPPORT=true$/BUNDLE_WORKER_SUPPORT=false/' \
    -e 's/^BUNDLE_POSTGRES_RUNTIME=true$/BUNDLE_POSTGRES_RUNTIME=false/' \
    "${SHAPE_ROOT}/package-manifest.env"
  compile_stubborn_fixture_binaries "${SHAPE_ROOT}"
  package_fixture "${SHAPE_ROOT}" "${CONTRACT_TMP}/stubborn-gateway.tar.gz"
  set +e
  timeout --signal=TERM --kill-after=1s 15s \
    env \
      STREAMSERVER_VERIFY_BUNDLE="${CONTRACT_TMP}/stubborn-gateway.tar.gz" \
      STREAMSERVER_VERIFY_DIR="${CONTRACT_TMP}/stubborn-gateway-work" \
      STREAMSERVER_VERIFY_REPORT="${CONTRACT_TMP}/stubborn-gateway.report" \
      STUBBORN_GATEWAY_PID_FILE="${CONTRACT_TMP}/stubborn-gateway.pid" \
      bash "${REMOTE_BODY}" >/dev/null 2>&1
  STUBBORN_GATEWAY_STATUS=$?
  set -e
  [ "${STUBBORN_GATEWAY_STATUS}" -ne 124 ] || \
    contract_failure 'ignored-TERM media-gateway made the target verifier hang'
  grep -Fq '[OK] media-gateway startup smoke' \
    "${CONTRACT_TMP}/stubborn-gateway.report" || \
    contract_failure 'media-gateway startup smoke did not observe the stubborn fixture'
  if [ -s "${CONTRACT_TMP}/stubborn-gateway.pid" ]; then
    STUBBORN_GATEWAY_PID="$(cat "${CONTRACT_TMP}/stubborn-gateway.pid")"
    if pid_is_live_non_zombie "${STUBBORN_GATEWAY_PID}"; then
      kill -KILL "${STUBBORN_GATEWAY_PID}" >/dev/null 2>&1 || true
      contract_failure 'media-gateway smoke did not KILL and reap its ignored-TERM child'
    fi
  else
    contract_failure 'stubborn media-gateway fixture did not start'
  fi

  rm -f -- "${CONTRACT_TMP}/flood-gateway.pid"
  rm -rf -- "${CONTRACT_TMP}/flood-gateway-work"
  set +e
  timeout --signal=TERM --kill-after=1s 30s \
    env \
      STREAMSERVER_VERIFY_BUNDLE="${CONTRACT_TMP}/stubborn-gateway.tar.gz" \
      STREAMSERVER_VERIFY_DIR="${CONTRACT_TMP}/flood-gateway-work" \
      STREAMSERVER_VERIFY_REPORT="${CONTRACT_TMP}/flood-gateway.report" \
      STUBBORN_GATEWAY_PID_FILE="${CONTRACT_TMP}/flood-gateway.pid" \
      STUBBORN_GATEWAY_FLOOD_BYTES=2097152 \
      bash "${REMOTE_BODY}" >/dev/null 2>&1
  FLOOD_GATEWAY_STATUS=$?
  set -e
  [ "${FLOOD_GATEWAY_STATUS}" -ne 0 ] \
    && [ "${FLOOD_GATEWAY_STATUS}" -ne 124 ] || \
    contract_failure \
      '2 MiB background runtime output did not fail promptly at the 1 MiB write-time cap'
  grep -Fq '[FAIL] media-gateway startup smoke' \
    "${CONTRACT_TMP}/flood-gateway.report" || \
    contract_failure \
      'background runtime output overflow did not fail its owning smoke'
  [ "$(wc -c <"${CONTRACT_TMP}/flood-gateway.report")" -le 16777216 ] || \
    contract_failure 'background runtime overflow produced an oversized report'
  if [ -s "${CONTRACT_TMP}/flood-gateway.pid" ]; then
    FLOOD_GATEWAY_PID="$(cat "${CONTRACT_TMP}/flood-gateway.pid")"
    if pid_is_live_non_zombie "${FLOOD_GATEWAY_PID}"; then
      kill -KILL "${FLOOD_GATEWAY_PID}" >/dev/null 2>&1 || true
      contract_failure \
        'background runtime overflow left the output producer alive'
    fi
  else
    contract_failure 'background runtime overflow fixture did not start'
  fi

  FAIL_OPEN_BIN="${CONTRACT_TMP}/fail-open-bin"
  mkdir -p "${FAIL_OPEN_BIN}" "${CONTRACT_TMP}/outer-output"
  printf '%s\n' '#!/usr/bin/env sh' 'exit 0' >"${FAIL_OPEN_BIN}/scp"
  printf '%s\n' \
    '#!/usr/bin/env sh' \
    'case "$*" in' \
    '  *"mktemp -d"*)' \
    '    printf "%s\n" "/tmp/native-verifier-contract/target-run.process"' \
    '    exit 0' \
    '    ;;' \
    '  *"cat "*native-verification-target-*.md*)' \
    '    case "${NATIVE_VERIFIER_REPORT_MODE:-transfer-fail}" in' \
    '      transfer-fail) exit 64 ;;' \
    '      invalid-summary)' \
    '        printf "%s\n" "- failures: 1" "- result: FAIL"' \
    '        exit 0' \
    '        ;;' \
    '      spoofed-prefix)' \
    '        printf "%s\n" "- failures: 0" "- result: PASS" "truncated command output"' \
    '        exit 0' \
    '        ;;' \
    '      oversized-report)' \
    "        python3 -c 'import sys; sys.stdout.write(\"x\" * (17 * 1024 * 1024))'" \
    '        printf "%s\n" "## Summary" "- failures: 0" "- result: PASS"' \
    '        exit 0' \
    '        ;;' \
    '    esac' \
    '    ;;' \
    '  *) exit 0 ;;' \
    'esac' >"${FAIL_OPEN_BIN}/ssh"
  chmod 755 "${FAIL_OPEN_BIN}/scp" "${FAIL_OPEN_BIN}/ssh"
  : >"${CONTRACT_TMP}/outer-bundle.tar.gz"

  set +e
  PATH="${FAIL_OPEN_BIN}:${PATH}" \
    bash "${VERIFY_SCRIPT}" \
      --bundle "${CONTRACT_TMP}/outer-bundle.tar.gz" \
      --ssh-target contract.invalid \
      --remote-dir /tmp/native-verifier-contract \
      --output-dir "${CONTRACT_TMP}/outer-output" \
      >/dev/null 2>&1
  REPORT_TRANSFER_STATUS=$?
  set -e
  [ "${REPORT_TRANSFER_STATUS}" -ne 0 ] || \
    contract_failure \
      'native verifier false-greened after the final report transfer failed'
  if find "${CONTRACT_TMP}/outer-output" -type f -print -quit | grep -q .; then
    contract_failure \
      'native verifier published a partial local report after transfer failure'
  fi

  mkdir -p "${CONTRACT_TMP}/spoofed-prefix-output"
  set +e
  NATIVE_VERIFIER_REPORT_MODE=spoofed-prefix \
  PATH="${FAIL_OPEN_BIN}:${PATH}" \
    bash "${VERIFY_SCRIPT}" \
      --bundle "${CONTRACT_TMP}/outer-bundle.tar.gz" \
      --ssh-target contract.invalid \
      --remote-dir /tmp/native-verifier-contract \
      --output-dir "${CONTRACT_TMP}/spoofed-prefix-output" \
      >/dev/null 2>&1
  SPOOFED_PREFIX_STATUS=$?
  set -e
  [ "${SPOOFED_PREFIX_STATUS}" -ne 0 ] || \
    contract_failure \
      'native verifier accepted success-like lines outside the report footer'

  mkdir -p "${CONTRACT_TMP}/invalid-summary-output"
  set +e
  NATIVE_VERIFIER_REPORT_MODE=invalid-summary \
  PATH="${FAIL_OPEN_BIN}:${PATH}" \
    bash "${VERIFY_SCRIPT}" \
      --bundle "${CONTRACT_TMP}/outer-bundle.tar.gz" \
      --ssh-target contract.invalid \
      --remote-dir /tmp/native-verifier-contract \
      --output-dir "${CONTRACT_TMP}/invalid-summary-output" \
      >/dev/null 2>&1
  INVALID_SUMMARY_STATUS=$?
  set -e
  [ "${INVALID_SUMMARY_STATUS}" -ne 0 ] || \
    contract_failure \
      'native verifier false-greened when the downloaded report said FAIL'
  INVALID_REPORT_COUNT="$(find "${CONTRACT_TMP}/invalid-summary-output" \
    -type f -name 'native-verification-target-*.md' | wc -l)"
  [ "${INVALID_REPORT_COUNT}" -eq 1 ] || \
    contract_failure \
      'native verifier did not atomically publish the complete downloaded report'

  mkdir -p "${CONTRACT_TMP}/oversized-output"
  set +e
  NATIVE_VERIFIER_REPORT_MODE=oversized-report \
  PATH="${FAIL_OPEN_BIN}:${PATH}" \
    bash "${VERIFY_SCRIPT}" \
      --bundle "${CONTRACT_TMP}/outer-bundle.tar.gz" \
      --ssh-target contract.invalid \
      --remote-dir /tmp/native-verifier-contract \
      --output-dir "${CONTRACT_TMP}/oversized-output" \
      >/dev/null 2>&1
  OVERSIZED_STATUS=$?
  set -e
  [ "${OVERSIZED_STATUS}" -ne 0 ] || \
    contract_failure 'outer report stream over 16 MiB false-greened'
  if find "${CONTRACT_TMP}/oversized-output" -type f -print -quit | grep -q .; then
    contract_failure 'outer report stream cap published a partial report'
  fi

  DISCONNECT_BIN="${CONTRACT_TMP}/disconnect-bin"
  DISCONNECT_REMOTE_BASE="${CONTRACT_TMP}/disconnect-remote"
  DISCONNECT_SENTINEL="${CONTRACT_TMP}/disconnect-child"
  mkdir -p \
    "${DISCONNECT_BIN}" \
    "${DISCONNECT_REMOTE_BASE}" \
    "${CONTRACT_TMP}/disconnect-output"
  cat >"${DISCONNECT_BIN}/scp" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
previous=
current=
for argument in "$@"; do
  previous="${current}"
  current="${argument}"
done
local_path="${previous}"
remote_path="${current#*:}"
mkdir -p "$(dirname "${remote_path}")"
case "${local_path}" in
  *.tar.gz) cp -- "${local_path}" "${remote_path}" ;;
  *)
    cat >"${remote_path}" <<'HELPER'
#!/usr/bin/env bash
if [ "${DISCONNECT_MODE:-signal}" = normal ]; then
  cat >"${STREAMSERVER_VERIFY_REPORT}" <<'REPORT'
# Native verifier orphan contract

## Summary
- failures: 0
- result: PASS
REPORT
  (
    trap '' HUP INT TERM
    printf '%s\n' "${BASHPID}" >"${DISCONNECT_SENTINEL}.child"
    while :; do sleep 1; done
  ) &
  for _ in $(seq 1 100); do
    [ -s "${DISCONNECT_SENTINEL}.child" ] && break
    sleep 0.01
  done
  exit 0
fi
trap '' HUP INT TERM
printf '%s\n' "${BASHPID}" >"${DISCONNECT_SENTINEL}.leader"
(
  trap '' HUP INT TERM
  printf '%s\n' "${BASHPID}" >"${DISCONNECT_SENTINEL}.child"
  while :; do sleep 1; done
) &
wait
HELPER
    chmod 700 "${remote_path}"
    ;;
esac
SH
  cat >"${DISCONNECT_BIN}/ssh" <<'SH'
#!/usr/bin/env bash
set -u
command=
for argument in "$@"; do command="${argument}"; done
case "${command}" in
  *'hostname; uname -a;'*) exit 0 ;;
  *'mktemp -d'*) bash -c "${command}" ;;
  *'cleanup_remote_run()'*)
    if [ "${DISCONNECT_MODE:-signal}" = normal ]; then
      bash -c "${command}"
      wrapper_status=$?
      printf '%s\n' "${wrapper_status}" >"${DISCONNECT_STATUS_FILE}"
      exit "${wrapper_status}"
    fi
    bash -c "${command}" &
    wrapper_pid=$!
    wait_file="${DISCONNECT_SENTINEL}.child"
    [ "${DISCONNECT_MODE:-signal}" = immediate ] \
      && wait_file="${DISCONNECT_SENTINEL}.race"
    for _ in $(seq 1 100); do
      [ -s "${wait_file}" ] && break
      kill -0 "${wrapper_pid}" >/dev/null 2>&1 || break
      sleep 0.05
    done
    kill -HUP "${wrapper_pid}" >/dev/null 2>&1 || true
    wait "${wrapper_pid}"
    wrapper_status=$?
    printf '%s\n' "${wrapper_status}" >"${DISCONNECT_STATUS_FILE}"
    exit 255
    ;;
  *'rm -rf --'*) bash -c "${command}" ;;
  *) exit 0 ;;
esac
SH
  cat >"${DISCONNECT_BIN}/setsid" <<'SH'
#!/usr/bin/env bash
set -u
if [ "${DISCONNECT_MODE:-signal}" = immediate ]; then
  trap '' TERM
  printf '%s\n' "${BASHPID}" >"${DISCONNECT_SENTINEL}.race"
  while :; do sleep 1; done
fi
exec /usr/bin/setsid "$@"
SH
  chmod 755 \
    "${DISCONNECT_BIN}/scp" \
    "${DISCONNECT_BIN}/ssh" \
    "${DISCONNECT_BIN}/setsid"
  : >"${CONTRACT_TMP}/disconnect-bundle.tar.gz"
  set +e
  DISCONNECT_SENTINEL="${DISCONNECT_SENTINEL}" \
  DISCONNECT_STATUS_FILE="${CONTRACT_TMP}/disconnect-status" \
  PATH="${DISCONNECT_BIN}:${PATH}" \
    bash "${VERIFY_SCRIPT}" \
      --bundle "${CONTRACT_TMP}/disconnect-bundle.tar.gz" \
      --ssh-target contract.invalid \
      --remote-dir "${DISCONNECT_REMOTE_BASE}" \
      --output-dir "${CONTRACT_TMP}/disconnect-output" \
      >/dev/null 2>&1
  DISCONNECT_OUTER_STATUS=$?
  set -e
  [ "${DISCONNECT_OUTER_STATUS}" -ne 0 ] || \
    contract_failure 'simulated SSH disconnect unexpectedly passed'
  [ "$(cat "${CONTRACT_TMP}/disconnect-status" 2>/dev/null || true)" = 129 ] || \
    contract_failure 'remote HUP cleanup did not exit with status 129'
  for pid_file in "${DISCONNECT_SENTINEL}.leader" "${DISCONNECT_SENTINEL}.child"; do
    if [ ! -s "${pid_file}" ]; then
      contract_failure "disconnect fixture did not start: ${pid_file}"
      continue
    fi
    leaked_pid="$(cat "${pid_file}")"
    if pid_is_live_non_zombie "${leaked_pid}"; then
      kill -KILL "${leaked_pid}" >/dev/null 2>&1 || true
      contract_failure "remote HUP cleanup leaked process ${leaked_pid}"
    fi
  done
  if find "${DISCONNECT_REMOTE_BASE}" -mindepth 1 -print -quit | grep -q .; then
    contract_failure 'remote HUP/outer fallback cleanup left a private run directory'
  fi

  rm -f -- "${DISCONNECT_SENTINEL}.leader" "${DISCONNECT_SENTINEL}.child" \
    "${DISCONNECT_SENTINEL}.race" "${CONTRACT_TMP}/disconnect-status"
  set +e
  DISCONNECT_MODE=immediate \
  DISCONNECT_SENTINEL="${DISCONNECT_SENTINEL}" \
  DISCONNECT_STATUS_FILE="${CONTRACT_TMP}/disconnect-status" \
  PATH="${DISCONNECT_BIN}:${PATH}" \
    bash "${VERIFY_SCRIPT}" \
      --bundle "${CONTRACT_TMP}/disconnect-bundle.tar.gz" \
      --ssh-target contract.invalid \
      --remote-dir "${DISCONNECT_REMOTE_BASE}" \
      --output-dir "${CONTRACT_TMP}/disconnect-output" \
      >/dev/null 2>&1
  IMMEDIATE_OUTER_STATUS=$?
  set -e
  [ "${IMMEDIATE_OUTER_STATUS}" -ne 0 ] || \
    contract_failure 'immediate remote HUP identity race unexpectedly passed'
  [ "$(cat "${CONTRACT_TMP}/disconnect-status" 2>/dev/null || true)" = 129 ] || \
    contract_failure 'immediate remote HUP did not exit with status 129'
  if [ -s "${DISCONNECT_SENTINEL}.race" ]; then
    race_pid="$(cat "${DISCONNECT_SENTINEL}.race")"
    if pid_is_live_non_zombie "${race_pid}"; then
      kill -KILL "${race_pid}" >/dev/null 2>&1 || true
      contract_failure 'remote identity-race fallback leaked its unowned child'
    fi
  else
    contract_failure 'remote identity-race fixture did not enter the race window'
  fi
  if find "${DISCONNECT_REMOTE_BASE}" -mindepth 1 -print -quit | grep -q .; then
    contract_failure 'remote identity-race cleanup left a private run directory'
  fi

  rm -f -- "${DISCONNECT_SENTINEL}.leader" "${DISCONNECT_SENTINEL}.child" \
    "${DISCONNECT_SENTINEL}.race" "${CONTRACT_TMP}/disconnect-status"
  rm -rf -- "${CONTRACT_TMP}/disconnect-normal-output"
  mkdir -p "${CONTRACT_TMP}/disconnect-normal-output"
  set +e
  DISCONNECT_MODE=normal \
  DISCONNECT_SENTINEL="${DISCONNECT_SENTINEL}" \
  DISCONNECT_STATUS_FILE="${CONTRACT_TMP}/disconnect-status" \
  PATH="${DISCONNECT_BIN}:${PATH}" \
    bash "${VERIFY_SCRIPT}" \
      --bundle "${CONTRACT_TMP}/disconnect-bundle.tar.gz" \
      --ssh-target contract.invalid \
      --remote-dir "${DISCONNECT_REMOTE_BASE}" \
      --output-dir "${CONTRACT_TMP}/disconnect-normal-output" \
      >/dev/null 2>&1
  NORMAL_OUTER_STATUS=$?
  set -e
  [ "${NORMAL_OUTER_STATUS}" -eq 0 ] || \
    contract_failure 'normal remote PASS with an orphan descendant did not complete'
  [ "$(cat "${CONTRACT_TMP}/disconnect-status" 2>/dev/null || true)" = 0 ] || \
    contract_failure 'normal remote wrapper did not preserve status 0 after group sweep'
  NORMAL_REPORT_COUNT="$(find "${CONTRACT_TMP}/disconnect-normal-output" \
    -type f -name 'native-verification-target-*.md' | wc -l)"
  [ "${NORMAL_REPORT_COUNT}" -eq 1 ] || \
    contract_failure 'normal remote wrapper did not publish exactly one PASS report'
  if [ -s "${DISCONNECT_SENTINEL}.child" ]; then
    normal_child_pid="$(cat "${DISCONNECT_SENTINEL}.child")"
    if pid_is_live_non_zombie "${normal_child_pid}"; then
      kill -KILL "${normal_child_pid}" >/dev/null 2>&1 || true
      contract_failure 'normal remote completion leaked its same-PGID descendant'
    fi
  else
    contract_failure 'normal remote orphan fixture did not start its descendant'
  fi
  if find "${DISCONNECT_REMOTE_BASE}" -mindepth 1 -print -quit | grep -q .; then
    contract_failure 'normal remote completion left a private run directory'
  fi

  [ "${CONTRACT_FAILURES}" -eq 0 ] || {
    printf 'native verifier contract failures: %s\n' \
      "${CONTRACT_FAILURES}" >&2
    exit 1
  }
fi

grep -Fq 'bash tests/native_verifier_locale_contract_test.sh' "${NATIVE_WORKFLOW}" || {
  echo 'native verifier locale contract is not wired into the Linux bundle gate' >&2
  exit 1
}

grep -Fq 'bash tests/native_verifier_process_contract_test.sh' "${NATIVE_WORKFLOW}" || {
  echo 'native verifier process contract is not wired into the Linux bundle gate' >&2
  exit 1
}

echo 'native verifier process cleanup contract tests passed'
