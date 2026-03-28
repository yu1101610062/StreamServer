#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "${SCRIPT_DIR}/../.." && pwd)
COMPOSE_FILE="${REPO_ROOT}/docker-compose.e2e.yml"
DOCKER_COMPOSE_BIN=${DOCKER_COMPOSE_BIN:-docker-compose}
CORE_BASE_URL=${CORE_BASE_URL:-http://127.0.0.1:18080}
ZLM_INTERNAL_API_URL=${ZLM_INTERNAL_API_URL:-http://zlmediakit:80}
ARTIFACT_ROOT=${ARTIFACT_ROOT:-${REPO_ROOT}/.artifacts/compose-e2e}
KEEP_STACK=${KEEP_STACK:-0}

mkdir -p "${ARTIFACT_ROOT}"
RUN_ARTIFACT_DIR="${ARTIFACT_ROOT}/$(date +%Y%m%d-%H%M%S)"
mkdir -p "${RUN_ARTIFACT_DIR}"

compose() {
  "${DOCKER_COMPOSE_BIN}" -f "${COMPOSE_FILE}" "$@"
}

log() {
  printf '[compose-e2e] %s\n' "$*" >&2
}

fail() {
  printf '[compose-e2e] ERROR: %s\n' "$*" >&2
  exit 1
}

json_field() {
  local path=${1:?path required}
  python3 -c '
import json
import sys

path = [segment for segment in sys.argv[1].split(".") if segment]
value = json.load(sys.stdin)
for segment in path:
    if isinstance(value, list):
        value = value[int(segment)]
    elif isinstance(value, dict):
        value = value.get(segment)
    else:
        value = None
        break

if isinstance(value, (dict, list)):
    print(json.dumps(value))
elif value is None:
    print("")
else:
    print(value)
' "${path}"
}

compose_exec() {
  compose exec -T "$@"
}

sql_scalar() {
  local sql=${1:?sql required}
  compose_exec postgres psql -U postgres -d streamserver -Atqc "${sql}"
}

wait_until() {
  local description=${1:?description required}
  local timeout_seconds=${2:?timeout required}
  local interval_seconds=${3:?interval required}
  shift 3

  local started
  started=$(date +%s)
  while true; do
    if "$@"; then
      return 0
    fi
    if (( $(date +%s) - started >= timeout_seconds )); then
      return 1
    fi
    sleep "${interval_seconds}"
  done
}

http_ready() {
  local url=${1:?url required}
  curl -fsS "${url}" >/dev/null
}

http_not_ready() {
  local url=${1:?url required}
  ! curl -fsS "${url}" >/dev/null 2>&1
}

zlm_api_ready() {
  compose_exec media-agent \
    curl -fsS "${ZLM_INTERNAL_API_URL}/index/api/getStatistic?secret=streamserver-e2e-secret" \
    >/dev/null
}

zlm_api_get() {
  local path=${1:?path required}
  compose_exec media-agent \
    curl -fsS "${ZLM_INTERNAL_API_URL}${path}"
}

wait_http_ready() {
  local name=${1:?name required}
  local url=${2:?url required}
  if ! wait_until "${name}" 90 2 http_ready "${url}"; then
    fail "${name} did not become ready at ${url}"
  fi
}

wait_http_not_ready() {
  local name=${1:?name required}
  local url=${2:?url required}
  if ! wait_until "${name}" 45 2 http_not_ready "${url}"; then
    fail "${name} stayed ready at ${url}"
  fi
}

current_task_status() {
  local task_id=${1:?task id required}
  curl -fsS "${CORE_BASE_URL}/api/v1/tasks/${task_id}" | json_field task.status
}

task_matches_status() {
  local task_id=${1:?task id required}
  local expected=${2:?expected required}
  [[ "$(current_task_status "${task_id}")" == "${expected}" ]]
}

wait_task_status() {
  local task_id=${1:?task id required}
  local expected=${2:?expected required}
  local timeout_seconds=${3:-120}
  if ! wait_until "task ${task_id} -> ${expected}" "${timeout_seconds}" 2 task_matches_status "${task_id}" "${expected}"; then
    local status
    status=$(current_task_status "${task_id}" || true)
    fail "task ${task_id} did not reach ${expected}; current status=${status:-unknown}"
  fi
}

http_get_json() {
  local path=${1:?path required}
  curl -fsS "${CORE_BASE_URL}${path}"
}

http_post_json() {
  local path=${1:?path required}
  local idempotency_key=${2:?idempotency key required}
  local payload=${3:?payload required}
  curl -fsS \
    -X POST \
    -H "Content-Type: application/json" \
    -H "Idempotency-Key: ${idempotency_key}" \
    --data "${payload}" \
    "${CORE_BASE_URL}${path}"
}

create_task() {
  local idempotency_key=${1:?idempotency key required}
  local payload=${2:?payload required}
  http_post_json "/api/v1/tasks" "${idempotency_key}" "${payload}"
}

stop_task() {
  local task_id=${1:?task id required}
  http_post_json "/api/v1/tasks/${task_id}/stop" "stop-${task_id}-$(date +%s%N)" "{}" >/dev/null
}

start_task() {
  local task_id=${1:?task id required}
  http_post_json "/api/v1/tasks/${task_id}/start" "start-${task_id}-$(date +%s%N)" "{}" >/dev/null
}

zlm_has_stream() {
  local app=${1:?app required}
  local stream=${2:?stream required}
  zlm_api_get "/index/api/getMediaList?secret=streamserver-e2e-secret" | \
    python3 -c '
import json
import sys

app = sys.argv[1]
stream = sys.argv[2]
body = json.load(sys.stdin)
entries = body.get("data") or []
for entry in entries:
    if entry.get("app") == app and entry.get("stream") == stream:
        sys.exit(0)
sys.exit(1)
' "${app}" "${stream}"
}

assert_remote_file_exists() {
  local service=${1:?service required}
  local path=${2:?path required}
  compose_exec "${service}" sh -lc "test -s '${path}'"
}

task_matches_any_status() {
  local task_id=${1:?task id required}
  shift
  local current
  current=$(current_task_status "${task_id}")
  local expected
  for expected in "$@"; do
    if [[ "${current}" == "${expected}" ]]; then
      return 0
    fi
  done
  return 1
}

wait_task_status_any() {
  local task_id=${1:?task id required}
  shift
  if ! wait_until "task ${task_id} -> any($*)" 150 2 task_matches_any_status "${task_id}" "$@"; then
    local status
    status=$(current_task_status "${task_id}" || true)
    fail "task ${task_id} did not reach any of [$*]; current status=${status:-unknown}"
  fi
}

zlm_stream_absent() {
  local app=${1:?app required}
  local stream=${2:?stream required}
  ! zlm_has_stream "${app}" "${stream}"
}

wait_zlm_stream_online() {
  local app=${1:?app required}
  local stream=${2:?stream required}
  local description=${3:-"stream ${app}/${stream} online"}
  if ! wait_until "${description}" 120 2 zlm_has_stream "${app}" "${stream}"; then
    fail "stream ${app}/${stream} did not appear in ZLM"
  fi
}

wait_zlm_stream_absent() {
  local app=${1:?app required}
  local stream=${2:?stream required}
  local description=${3:-"stream ${app}/${stream} absent"}
  if ! wait_until "${description}" 60 2 zlm_stream_absent "${app}" "${stream}"; then
    fail "stream ${app}/${stream} still exists in ZLM"
  fi
}

wait_node_last_seen_after() {
  local previous=${1:?previous timestamp required}
  if ! wait_until "node last_seen_at after ${previous}" 90 2 node_last_seen_is_after "${previous}"; then
    local current
    current=$(sql_scalar "select coalesce(max(last_seen_at)::text, '') from media_nodes where id = '11111111-1111-1111-1111-111111111111';")
    fail "node last_seen_at did not advance after restart; current=${current:-empty}"
  fi
}

wait_source_stream_online() {
  local description=${1:-source stream online}
  if ! wait_until "${description}" 120 2 zlm_has_stream live e2e-source; then
    fail "source stream did not appear in ZLM"
  fi
}

wait_adopted_event() {
  local task_id=${1:?task id required}
  if ! wait_until "adopted event for ${task_id}" 90 2 adopted_event_exists "${task_id}"; then
    fail "task ${task_id} did not emit adopted event after agent restart"
  fi
}

node_registered() {
  [[ "$(sql_scalar "select count(*) from media_nodes where id = '11111111-1111-1111-1111-111111111111';")" == "1" ]]
}

node_health_matches() {
  local expected=${1:?expected required}
  [[ "$(sql_scalar "select case when healthy then 'true' else 'false' end from media_nodes where id = '11111111-1111-1111-1111-111111111111';")" == "${expected}" ]]
}

node_last_seen_is_after() {
  local previous=${1:?previous timestamp required}
  local current
  current=$(sql_scalar "select coalesce(max(last_seen_at)::text, '') from media_nodes where id = '11111111-1111-1111-1111-111111111111';")
  [[ -n "${current}" && "${current}" > "${previous}" ]]
}

postgres_ready() {
  compose_exec postgres pg_isready -U postgres -d streamserver >/dev/null
}

record_file_exists_for_task() {
  local task_id=${1:?task id required}
  [[ "$(sql_scalar "select count(*) from record_files where task_id = '${task_id}';")" -ge 1 ]]
}

adopted_event_exists() {
  local task_id=${1:?task id required}
  [[ "$(sql_scalar "select count(*) from task_events where task_id = '${task_id}' and event_type = 'adopted';")" -ge 1 ]]
}

relay_stream_absent() {
  local task_id=${1:?task id required}
  ! zlm_has_stream relay "${task_id}"
}

record_file_created_event_exists_for_task_format() {
  local task_id=${1:?task id required}
  local record_format=${2:?record format required}
  [[ "$(sql_scalar "select count(*) from task_events where task_id = '${task_id}' and event_type = 'record_file_created' and payload->>'record_format' = '${record_format}';")" -ge 1 ]]
}

task_event_payload_field() {
  local task_id=${1:?task id required}
  local event_type=${2:?event type required}
  local path=${3:?json path required}
  local pg_path
  pg_path=$(printf '%s' "${path}" | sed "s/\\./,/g")
  sql_scalar "select coalesce(payload #>> '{${pg_path}}', '') from task_events where task_id = '${task_id}' and event_type = '${event_type}' order by created_at desc limit 1;"
}

task_event_payload_has_value() {
  local task_id=${1:?task id required}
  local event_type=${2:?event type required}
  local path=${3:?json path required}
  [[ -n "$(task_event_payload_field "${task_id}" "${event_type}" "${path}")" ]]
}

task_log_contains() {
  local task_id=${1:?task id required}
  local needle=${2:?needle required}
  [[ "$(sql_scalar "select case when exists (
      select 1
        from task_events events,
             jsonb_array_elements_text(events.payload->'lines') as line
       where events.task_id = '${task_id}'
         and events.event_type = 'task_log_batch'
         and line ilike '%${needle}%'
    ) then 'true' else 'false' end;")" == "true" ]]
}

wait_task_log_contains() {
  local task_id=${1:?task id required}
  local needle=${2:?needle required}
  local description=${3:-"task ${task_id} log contains ${needle}"}
  if ! wait_until "${description}" 90 2 task_log_contains "${task_id}" "${needle}"; then
    fail "task ${task_id} logs did not contain expected text: ${needle}"
  fi
}

stream_readable() {
  local url=${1:?url required}
  compose_exec media-agent sh -lc "timeout 15 ffmpeg -hide_banner -nostdin -loglevel error -i '${url}' -frames:v 1 -f null - >/dev/null 2>&1"
}

wait_stream_readable() {
  local url=${1:?url required}
  local description=${2:-"stream readable ${url}"}
  if ! wait_until "${description}" 90 2 stream_readable "${url}"; then
    fail "stream was not readable: ${url}"
  fi
}

start_named_sender() {
  local name=${1:?name required}
  local format=${2:?format required}
  local output_url=${3:?output url required}
  compose_exec media-agent sh -lc "
    set -eu
    rm -f /tmp/${name}.pid /tmp/${name}.log
    nohup ffmpeg -hide_banner -nostdin -re -stream_loop -1 \
      -i /data/media/work/samples/e2e-source.mp4 \
      -c copy \
      -f ${format} \
      '${output_url}' \
      >/tmp/${name}.log 2>&1 </dev/null &
    echo \$! >/tmp/${name}.pid
  "
}

stop_named_sender() {
  local name=${1:?name required}
  compose_exec media-agent sh -lc "
    set +e
    if [ -f /tmp/${name}.pid ]; then
      pid=\$(cat /tmp/${name}.pid)
      kill \"\${pid}\" >/dev/null 2>&1 || true
      wait \"\${pid}\" >/dev/null 2>&1 || true
      rm -f /tmp/${name}.pid
    fi
  "
}

collect_artifacts() {
  set +e
  compose logs --no-color > "${RUN_ARTIFACT_DIR}/compose.log" 2>&1 || true
  compose ps > "${RUN_ARTIFACT_DIR}/compose-ps.log" 2>&1 || true
  if compose ps postgres >/dev/null 2>&1; then
    sql_scalar "select id || ',' || status::text || ',' || coalesce(assigned_node_id::text,'') from tasks order by created_at;" \
      > "${RUN_ARTIFACT_DIR}/tasks.csv" 2>/dev/null || true
    sql_scalar "select task_id::text || ',' || event_type || ',' || event_level from task_events order by created_at desc limit 50;" \
      > "${RUN_ARTIFACT_DIR}/task-events.csv" 2>/dev/null || true
  fi
  set -e
}

cleanup() {
  local exit_code=$?
  collect_artifacts
  if [[ "${KEEP_STACK}" != "1" ]]; then
    compose down -v --remove-orphans >/dev/null 2>&1 || true
  fi
  exit "${exit_code}"
}

trap cleanup EXIT

bootstrap_stack() {
  log "starting compose stack"
  compose down -v --remove-orphans >/dev/null 2>&1 || true
  compose up -d --build

  wait_http_ready "media-core" "${CORE_BASE_URL}/health/ready"
  wait_http_ready "media-agent" "http://127.0.0.1:18081/health/ready"
  if ! wait_until "zlmediakit" 90 2 zlm_api_ready; then
    fail "zlmediakit did not become ready via internal API"
  fi

  if ! wait_until "node registration" 90 2 node_registered; then
    fail "media-agent did not register expected node_id"
  fi
}

prepare_sample_media() {
  log "generating reusable sample media"
  compose_exec media-agent sh -lc '
    set -eu
    mkdir -p /data/media/work/samples /data/media/work/e2e
    ffmpeg -hide_banner -y \
      -f lavfi -i testsrc=size=320x180:rate=12 \
      -f lavfi -i sine=frequency=1000:sample_rate=48000 \
      -t 180 \
      -pix_fmt yuv420p \
      -c:v libx264 \
      -preset veryfast \
      -c:a aac \
      -shortest \
      /data/media/work/samples/e2e-source.mp4 >/tmp/e2e-sample.log 2>&1
  '
}

run_file_transcode_success() {
  log "running file_transcode success path"
  compose_exec media-agent sh -lc 'rm -f /data/media/work/e2e/transcoded.mp4'
  local payload response task_id
  payload=$(cat <<'JSON'
{
  "name": "e2e-file-transcode-success",
  "type": "file_transcode",
  "priority": 50,
  "common": {
    "tenant_id": "default",
    "created_by": "compose-e2e"
  },
  "input": {
    "kind": "file",
    "url": "/data/media/work/samples/e2e-source.mp4"
  },
  "publish": {
    "kind": "file",
    "url": "/data/media/work/e2e/transcoded.mp4",
    "format": "mp4"
  },
  "schedule": {
    "start_mode": "immediate"
  }
}
JSON
)
  response=$(create_task "compose-e2e-transcode-success" "${payload}")
  task_id=$(printf '%s' "${response}" | json_field id)
  wait_task_status "${task_id}" "SUCCEEDED" 120
  assert_remote_file_exists media-agent /data/media/work/e2e/transcoded.mp4
}

run_file_transcode_manual_start_success() {
  log "running file_transcode manual-start success path"
  compose_exec media-agent sh -lc 'rm -f /data/media/work/e2e/transcoded-manual.mp4'
  local payload response task_id
  payload=$(cat <<'JSON'
{
  "name": "e2e-file-transcode-manual",
  "type": "file_transcode",
  "priority": 50,
  "common": {
    "tenant_id": "default",
    "created_by": "compose-e2e"
  },
  "input": {
    "kind": "file",
    "url": "/data/media/work/samples/e2e-source.mp4"
  },
  "publish": {
    "kind": "file",
    "url": "/data/media/work/e2e/transcoded-manual.mp4",
    "format": "mp4"
  },
  "schedule": {
    "start_mode": "manual"
  }
}
JSON
)
  response=$(create_task "compose-e2e-transcode-manual" "${payload}")
  task_id=$(printf '%s' "${response}" | json_field id)
  wait_task_status "${task_id}" "CREATED" 30
  start_task "${task_id}"
  wait_task_status "${task_id}" "SUCCEEDED" 120
  assert_remote_file_exists media-agent /data/media/work/e2e/transcoded-manual.mp4
}

run_file_transcode_failure() {
  log "running file_transcode failure path"
  local payload response task_id
  payload=$(cat <<'JSON'
{
  "name": "e2e-file-transcode-failure",
  "type": "file_transcode",
  "priority": 50,
  "common": {
    "tenant_id": "default",
    "created_by": "compose-e2e"
  },
  "input": {
    "kind": "file",
    "url": "/data/media/work/samples/does-not-exist.mp4"
  },
  "publish": {
    "kind": "file",
    "url": "/data/media/work/e2e/missing-output.mp4",
    "format": "mp4"
  },
  "schedule": {
    "start_mode": "immediate"
  }
}
JSON
)
  response=$(create_task "compose-e2e-transcode-failure" "${payload}")
  task_id=$(printf '%s' "${response}" | json_field id)
  wait_task_status "${task_id}" "FAILED" 120
}

run_file_transcode_disk_unwritable_failure() {
  log "running file_transcode disk-unwritable failure path"
  local payload response task_id
  payload=$(cat <<'JSON'
{
  "name": "e2e-file-transcode-disk-unwritable",
  "type": "file_transcode",
  "priority": 50,
  "common": {
    "tenant_id": "default",
    "created_by": "compose-e2e"
  },
  "input": {
    "kind": "file",
    "url": "/data/media/work/samples/e2e-source.mp4"
  },
  "publish": {
    "kind": "file",
    "url": "/sys/readonly-output.mp4",
    "format": "mp4"
  },
  "schedule": {
    "start_mode": "immediate"
  }
}
JSON
)
  response=$(create_task "compose-e2e-transcode-disk-unwritable" "${payload}")
  task_id=$(printf '%s' "${response}" | json_field id)
  wait_task_status "${task_id}" "FAILED" 120
}

start_source_file_to_live() {
  log "starting long-running file_to_live source stream"
  local payload response task_id
  payload=$(cat <<'JSON'
{
  "name": "e2e-file-to-live-source",
  "type": "file_to_live",
  "priority": 50,
  "common": {
    "tenant_id": "default",
    "created_by": "compose-e2e"
  },
  "input": {
    "kind": "file",
    "url": "/data/media/work/samples/e2e-source.mp4"
  },
  "publish": {
    "kind": "zlm_ingest",
    "url": "rtmp://zlmediakit/live/e2e-source",
    "enable_rtsp": true,
    "enable_rtmp": true,
    "enable_http_ts": true,
    "enable_http_fmp4": true
  },
  "schedule": {
    "start_mode": "immediate"
  }
}
JSON
)
  response=$(create_task "compose-e2e-file-to-live-source" "${payload}")
  task_id=$(printf '%s' "${response}" | json_field id)
  wait_task_status "${task_id}" "RUNNING" 120
  wait_source_stream_online "source stream online"
  printf '%s\n' "${task_id}"
}

run_zlm_restart_fault() {
  local source_task_id=${1:?source task id required}
  log "injecting ZLM restart fault"
  compose restart zlmediakit >/dev/null
  if ! wait_until "zlmediakit after restart" 90 2 zlm_api_ready; then
    fail "zlmediakit did not recover after restart"
  fi
  wait_task_status "${source_task_id}" "RUNNING" 150
  wait_source_stream_online "source stream online after ZLM restart"
}

run_live_relay_record_and_stop() {
  log "running live_relay with recording"
  local payload response task_id
  payload=$(cat <<'JSON'
{
  "name": "e2e-live-relay-record",
  "type": "live_relay",
  "priority": 50,
  "common": {
    "tenant_id": "default",
    "created_by": "compose-e2e"
  },
  "input": {
    "kind": "rtmp",
    "url": "rtmp://zlmediakit/live/e2e-source"
  },
  "publish": {
    "enable_rtsp": true,
    "enable_rtmp": true,
    "enable_http_ts": true,
    "enable_http_fmp4": true
  },
  "record": {
    "enabled": true,
    "format": "mp4",
    "segment_sec": 3,
    "save_path": "/data/media/work/e2e/relay-records"
  },
  "schedule": {
    "start_mode": "immediate"
  }
}
JSON
)
  response=$(create_task "compose-e2e-live-relay-record" "${payload}")
  task_id=$(printf '%s' "${response}" | json_field id)
  wait_task_status "${task_id}" "RUNNING" 120
  sleep 8
  stop_task "${task_id}"
  wait_task_status "${task_id}" "CANCELED" 120

  if ! wait_until "record_file row for ${task_id}" 60 2 record_file_exists_for_task "${task_id}"; then
    fail "live_relay record_files row was not created"
  fi

  local file_path
  file_path=$(sql_scalar "select file_path from record_files where task_id = '${task_id}' order by created_at desc limit 1;")
  [[ -n "${file_path}" ]] || fail "live_relay record_files row did not contain file_path"
  assert_remote_file_exists zlmediakit "${file_path}"

  if ! wait_until "live_relay cleanup ${task_id}" 30 2 relay_stream_absent "${task_id}"; then
    fail "live_relay stream ${task_id} still exists after stop"
  fi
}

run_live_relay_hls_record_and_stop() {
  log "running live_relay with hls recording"
  local payload response task_id file_path
  payload=$(cat <<'JSON'
{
  "name": "e2e-live-relay-record-hls",
  "type": "live_relay",
  "priority": 50,
  "common": {
    "tenant_id": "default",
    "created_by": "compose-e2e"
  },
  "input": {
    "kind": "rtmp",
    "url": "rtmp://zlmediakit/live/e2e-source"
  },
  "publish": {
    "enable_rtsp": true,
    "enable_rtmp": true,
    "enable_http_ts": true,
    "enable_http_fmp4": true,
    "enable_hls": true
  },
  "record": {
    "enabled": true,
    "format": "hls",
    "save_path": "/data/media/work/e2e/relay-hls-records"
  },
  "schedule": {
    "start_mode": "immediate"
  }
}
JSON
)
  response=$(create_task "compose-e2e-live-relay-record-hls" "${payload}")
  task_id=$(printf '%s' "${response}" | json_field id)
  wait_task_status "${task_id}" "RUNNING" 120
  sleep 8
  stop_task "${task_id}"
  wait_task_status "${task_id}" "CANCELED" 120
  if ! wait_until "hls record_file event for ${task_id}" 60 2 record_file_created_event_exists_for_task_format "${task_id}" "hls"; then
    fail "live_relay hls record_file_created event was not created"
  fi
  file_path=$(sql_scalar "select file_path from record_files where task_id = '${task_id}' order by created_at desc limit 1;")
  [[ -n "${file_path}" ]] || fail "live_relay hls record_files row did not contain file_path"
  assert_remote_file_exists zlmediakit "${file_path}"
  wait_zlm_stream_absent relay "${task_id}" "live_relay hls cleanup ${task_id}"
}

run_multicast_bridge_output_success() {
  log "running multicast_bridge continuous multicast output"
  local payload response task_id output_url
  payload=$(cat <<'JSON'
{
  "name": "e2e-multicast-output",
  "type": "multicast_bridge",
  "priority": 50,
  "common": {
    "tenant_id": "default",
    "created_by": "compose-e2e"
  },
  "input": {
    "kind": "rtsp",
    "url": "rtsp://zlmediakit/live/e2e-source"
  },
  "process": {
    "mode": "passthrough"
  },
  "publish": {
    "kind": "udp_mpegts_multicast",
    "group": "239.20.20.20",
    "port": 6100,
    "interface_ip": "172.28.0.30",
    "ttl": 2,
    "reuse": true,
    "pkt_size": 1316
  },
  "schedule": {
    "start_mode": "immediate"
  }
}
JSON
)
  response=$(create_task "compose-e2e-multicast-output" "${payload}")
  task_id=$(printf '%s' "${response}" | json_field id)
  wait_task_status "${task_id}" "RUNNING" 120
  output_url="udp://239.20.20.20:6100?localaddr=172.28.0.30&reuse=1&ttl=2&pkt_size=1316"
  wait_stream_readable "${output_url}" "multicast output is readable"
  stop_task "${task_id}"
  wait_task_status "${task_id}" "CANCELED" 120
}

run_multicast_bridge_input_success() {
  log "running multicast_bridge multicast-input to zlm_ingest"
  local payload response task_id
  start_named_sender \
    "e2e-multicast-source" \
    "mpegts" \
    "udp://239.20.20.21:6200?localaddr=172.28.0.30&reuse=1&ttl=2&pkt_size=1316"
  payload=$(cat <<'JSON'
{
  "name": "e2e-multicast-input",
  "type": "multicast_bridge",
  "priority": 50,
  "common": {
    "tenant_id": "default",
    "created_by": "compose-e2e"
  },
  "input": {
    "kind": "udp_mpegts_multicast",
    "group": "239.20.20.21",
    "port": 6200,
    "interface_ip": "172.28.0.30",
    "ttl": 2,
    "reuse": true,
    "pkt_size": 1316
  },
  "process": {
    "mode": "passthrough"
  },
  "publish": {
    "kind": "zlm_ingest",
    "url": "rtmp://zlmediakit/bridge/e2e-mcast-ingest",
    "enable_rtsp": true,
    "enable_rtmp": true,
    "enable_http_ts": true,
    "enable_http_fmp4": true
  },
  "schedule": {
    "start_mode": "immediate"
  }
}
JSON
)
  response=$(create_task "compose-e2e-multicast-input" "${payload}")
  task_id=$(printf '%s' "${response}" | json_field id)
  wait_task_status "${task_id}" "RUNNING" 150
  wait_zlm_stream_online bridge "e2e-mcast-ingest" "multicast input bridge stream online"
  stop_task "${task_id}"
  wait_task_status "${task_id}" "CANCELED" 120
  wait_zlm_stream_absent bridge "e2e-mcast-ingest" "multicast input bridge cleanup"
  stop_named_sender "e2e-multicast-source"
}

run_multicast_bridge_invalid_interface_failure() {
  log "running multicast_bridge invalid-interface failure path"
  local payload response task_id
  payload=$(cat <<'JSON'
{
  "name": "e2e-multicast-invalid-interface",
  "type": "multicast_bridge",
  "priority": 50,
  "common": {
    "tenant_id": "default",
    "created_by": "compose-e2e"
  },
  "input": {
    "kind": "rtsp",
    "url": "rtsp://zlmediakit/live/e2e-source"
  },
  "process": {
    "mode": "passthrough"
  },
  "publish": {
    "kind": "udp_mpegts_multicast",
    "group": "239.20.20.22",
    "port": 6202,
    "interface_ip": "203.0.113.10",
    "ttl": 2,
    "reuse": true,
    "pkt_size": 1316
  },
  "schedule": {
    "start_mode": "immediate"
  }
}
JSON
)
  response=$(create_task "compose-e2e-multicast-invalid-interface" "${payload}")
  task_id=$(printf '%s' "${response}" | json_field id)
  wait_task_status "${task_id}" "FAILED" 120
  wait_task_log_contains "${task_id}" "Cannot assign requested address" "multicast invalid interface emits clear error"
}

start_recoverable_live_relay() {
  log "starting live_relay for agent-restart recovery"
  local payload response task_id
  payload=$(cat <<'JSON'
{
  "name": "e2e-live-relay-recoverable",
  "type": "live_relay",
  "priority": 50,
  "common": {
    "tenant_id": "default",
    "created_by": "compose-e2e"
  },
  "input": {
    "kind": "rtmp",
    "url": "rtmp://zlmediakit/live/e2e-source"
  },
  "publish": {
    "enable_rtsp": true,
    "enable_rtmp": true,
    "enable_http_ts": true,
    "enable_http_fmp4": true
  },
  "schedule": {
    "start_mode": "immediate"
  }
}
JSON
)
  response=$(create_task "compose-e2e-live-relay-recoverable" "${payload}")
  task_id=$(printf '%s' "${response}" | json_field id)
  wait_task_status "${task_id}" "RUNNING" 120
  printf '%s\n' "${task_id}"
}

run_core_restart_fault() {
  local source_task_id=${1:?source task id required}
  log "injecting media-core restart fault"
  local previous_last_seen
  previous_last_seen=$(sql_scalar "select coalesce(max(last_seen_at)::text, '') from media_nodes where id = '11111111-1111-1111-1111-111111111111';")
  compose restart media-core >/dev/null
  wait_http_ready "media-core after restart" "${CORE_BASE_URL}/health/ready"
  wait_node_last_seen_after "${previous_last_seen}"
  wait_task_status "${source_task_id}" "RUNNING" 120
}

run_postgres_outage_fault() {
  local source_task_id=${1:?source task id required}
  log "injecting PostgreSQL outage fault"
  local previous_last_seen
  previous_last_seen=$(sql_scalar "select coalesce(max(last_seen_at)::text, '') from media_nodes where id = '11111111-1111-1111-1111-111111111111';")
  compose stop postgres >/dev/null
  wait_http_not_ready "media-core during postgres outage" "${CORE_BASE_URL}/health/ready"
  compose start postgres >/dev/null
  if ! wait_until "postgres after restart" 90 2 postgres_ready; then
    fail "postgres did not become ready after restart"
  fi
  wait_http_ready "media-core after postgres restart" "${CORE_BASE_URL}/health/ready"
  wait_node_last_seen_after "${previous_last_seen}"
  wait_task_status "${source_task_id}" "RUNNING" 120
  wait_source_stream_online "source stream online after postgres outage"
}

run_control_plane_disconnect_fault() {
  local source_task_id=${1:?source task id required}
  log "injecting control-plane disconnect fault"
  local previous_last_seen
  previous_last_seen=$(sql_scalar "select coalesce(max(last_seen_at)::text, '') from media_nodes where id = '11111111-1111-1111-1111-111111111111';")
  compose_exec media-agent sh -lc '
    set -eu
    pid=$(pgrep -o -f "^/usr/local/bin/media-agent$")
    kill -STOP "${pid}"
  '
  if ! wait_until "node unhealthy after control-plane disconnect" 50 2 node_health_matches false; then
    fail "node was not marked unhealthy after control-plane disconnect"
  fi
  compose_exec media-agent sh -lc '
    set -eu
    pid=$(pgrep -o -f "^/usr/local/bin/media-agent$")
    kill -CONT "${pid}"
  '
  wait_http_ready "media-agent after control-plane reconnect" "http://127.0.0.1:18081/health/ready"
  if ! wait_until "node healthy after control-plane reconnect" 90 2 node_health_matches true; then
    fail "node did not return healthy after control-plane reconnect"
  fi
  wait_node_last_seen_after "${previous_last_seen}"
  wait_task_status "${source_task_id}" "RUNNING" 120
  wait_source_stream_online "source stream online after control-plane reconnect"
}

run_agent_restart_fault() {
  local relay_task_id=${1:?relay task id required}
  log "injecting media-agent process restart fault"
  local previous_last_seen
  previous_last_seen=$(sql_scalar "select coalesce(max(last_seen_at)::text, '') from media_nodes where id = '11111111-1111-1111-1111-111111111111';")
  compose_exec media-agent sh -lc '
    set -eu
    pid=$(pgrep -o -f "^/usr/local/bin/media-agent$")
    kill -TERM "${pid}"
  '
  wait_http_ready "media-agent after restart" "http://127.0.0.1:18081/health/ready"
  wait_node_last_seen_after "${previous_last_seen}"
  wait_adopted_event "${relay_task_id}"
  wait_task_status "${relay_task_id}" "RUNNING" 120
  if ! wait_until "relay stream online after agent restart" 60 2 zlm_has_stream relay "${relay_task_id}"; then
    fail "live_relay stream ${relay_task_id} did not stay online after agent restart"
  fi
}

run_live_relay_source_loss() {
  local source_task_id=${1:?source task id required}
  local relay_task_id=${2:?relay task id required}
  log "injecting live_relay source-loss fault"
  stop_task "${source_task_id}"
  wait_task_status "${source_task_id}" "CANCELED" 120
  wait_zlm_stream_absent live e2e-source "source stream absent after stop"
  wait_task_status_any "${relay_task_id}" "LOST" "FAILED"
}

run_rtp_receive_success_and_agent_restart_recovery() {
  log "running rtp_receive success and agent-restart recovery"
  local payload response task_id local_port previous_last_seen
  payload=$(cat <<'JSON'
{
  "name": "e2e-rtp-success",
  "type": "rtp_receive",
  "priority": 50,
  "common": {
    "tenant_id": "default",
    "created_by": "compose-e2e"
  },
  "input": {
    "kind": "gb_rtp",
    "port": 0,
    "tcp_mode": 0,
    "reuse": true
  },
  "publish": {
    "enable_rtsp": true,
    "enable_rtmp": true,
    "enable_http_ts": true,
    "enable_http_fmp4": true
  },
  "schedule": {
    "start_mode": "immediate"
  }
}
JSON
)
  response=$(create_task "compose-e2e-rtp-success" "${payload}")
  task_id=$(printf '%s' "${response}" | json_field id)
  if ! wait_until "rtp local port for ${task_id}" 60 2 task_event_payload_has_value "${task_id}" "rtp_server_opened" "local_port"; then
    fail "rtp_receive task ${task_id} did not emit local_port"
  fi
  local_port=$(task_event_payload_field "${task_id}" "rtp_server_opened" "local_port")
  [[ -n "${local_port}" ]] || fail "rtp_receive task ${task_id} local_port payload was empty"
  start_named_sender \
    "e2e-rtp-source" \
    "rtp_mpegts" \
    "rtp://zlmediakit:${local_port}?pkt_size=1316"
  wait_task_status "${task_id}" "RUNNING" 150
  previous_last_seen=$(sql_scalar "select coalesce(max(last_seen_at)::text, '') from media_nodes where id = '11111111-1111-1111-1111-111111111111';")
  compose_exec media-agent sh -lc '
    set -eu
    pid=$(pgrep -o -f "^/usr/local/bin/media-agent$")
    kill -TERM "${pid}"
  '
  wait_http_ready "media-agent after rtp recovery restart" "http://127.0.0.1:18081/health/ready"
  wait_node_last_seen_after "${previous_last_seen}"
  wait_adopted_event "${task_id}"
  wait_task_status "${task_id}" "RUNNING" 150
  stop_task "${task_id}"
  wait_task_status "${task_id}" "CANCELED" 120
  stop_named_sender "e2e-rtp-source"
}

run_rtp_receive_timeout() {
  log "running rtp_receive timeout path"
  local payload response task_id
  payload=$(cat <<'JSON'
{
  "name": "e2e-rtp-timeout",
  "type": "rtp_receive",
  "priority": 50,
  "common": {
    "tenant_id": "default",
    "created_by": "compose-e2e"
  },
  "input": {
    "kind": "gb_rtp",
    "port": 0,
    "tcp_mode": 0
  },
  "publish": {
    "enable_rtsp": true,
    "enable_rtmp": true,
    "enable_http_ts": true,
    "enable_http_fmp4": true
  },
  "schedule": {
    "start_mode": "immediate"
  }
}
JSON
)
  response=$(create_task "compose-e2e-rtp-timeout" "${payload}")
  task_id=$(printf '%s' "${response}" | json_field id)
  wait_task_status "${task_id}" "LOST" 150
}

stop_running_task_if_needed() {
  local task_id=${1:?task id required}
  local status
  status=$(current_task_status "${task_id}")
  if [[ "${status}" == "RUNNING" || "${status}" == "STARTING" || "${status}" == "DISPATCHING" || "${status}" == "RECOVERING" ]]; then
    stop_task "${task_id}"
    wait_task_status "${task_id}" "CANCELED" 120
  fi
}

main() {
  bootstrap_stack
  prepare_sample_media
  run_file_transcode_success
  run_file_transcode_manual_start_success
  run_file_transcode_failure
  run_file_transcode_disk_unwritable_failure

  local source_task_id
  source_task_id=$(start_source_file_to_live)
  run_zlm_restart_fault "${source_task_id}"

  run_live_relay_record_and_stop
  run_live_relay_hls_record_and_stop
  run_multicast_bridge_output_success
  run_multicast_bridge_input_success
  run_multicast_bridge_invalid_interface_failure

  local recoverable_relay_id
  recoverable_relay_id=$(start_recoverable_live_relay)

  run_core_restart_fault "${source_task_id}"
  run_postgres_outage_fault "${source_task_id}"
  run_control_plane_disconnect_fault "${source_task_id}"
  run_agent_restart_fault "${recoverable_relay_id}"
  run_live_relay_source_loss "${source_task_id}" "${recoverable_relay_id}"
  run_rtp_receive_success_and_agent_restart_recovery
  run_rtp_receive_timeout

  stop_running_task_if_needed "${recoverable_relay_id}"
  log "compose e2e scenarios completed successfully"
}

main "$@"
