#!/usr/bin/env bash
set -euo pipefail

FFMPEG_BIN="${FFMPEG_BIN:-ffmpeg}"
FFPROBE_BIN="${FFPROBE_BIN:-ffprobe}"
MEDIA_GATEWAY_BIN="${MEDIA_GATEWAY_BIN:-target/debug/media-gateway}"
ROOT="$(mktemp -d)"
gateway_pid=
source_pid=

cleanup() {
  [ -z "${gateway_pid:-}" ] || kill "${gateway_pid}" >/dev/null 2>&1 || true
  [ -z "${source_pid:-}" ] || kill "${source_pid}" >/dev/null 2>&1 || true
  rm -rf -- "${ROOT}"
}
trap cleanup EXIT

pick_port() {
  python3 - <<'PY'
import socket
s = socket.socket()
s.bind(('127.0.0.1', 0))
print(s.getsockname()[1])
s.close()
PY
}

source_port="$(pick_port)"
gateway_port="$(pick_port)"
mkdir -p "${ROOT}/source" "${ROOT}/work"
"${FFMPEG_BIN}" -v error -y \
  -f lavfi -i testsrc2=size=320x180:rate=25 \
  -f lavfi -i sine=frequency=1000:sample_rate=48000 \
  -t 12 -c:v libx264 -g 50 -pix_fmt yuv420p -c:a aac -movflags +faststart \
  "${ROOT}/source/input.mp4"

python3 -m http.server "${source_port}" --bind 127.0.0.1 \
  --directory "${ROOT}/source" >"${ROOT}/source.log" 2>&1 &
source_pid=$!

MEDIA_GATEWAY_BIND_ADDR="127.0.0.1:${gateway_port}" \
MEDIA_GATEWAY_PUBLIC_BASE_URL="http://127.0.0.1:${gateway_port}" \
MEDIA_GATEWAY_WORK_ROOT="${ROOT}/work" \
MEDIA_GATEWAY_FFMPEG_BIN="${FFMPEG_BIN}" \
  "${MEDIA_GATEWAY_BIN}" >"${ROOT}/gateway.log" 2>&1 &
gateway_pid=$!

for _ in $(seq 1 100); do
  curl -fsS "http://127.0.0.1:${gateway_port}/api/healthz" >/dev/null && break
  sleep 0.05
done

task_id=00000000-0000-0000-0000-000000000666
curl -fsS -X POST "http://127.0.0.1:${gateway_port}/api/prefetch" \
  -H 'content-type: application/json' \
  -d "{\"task_id\":\"${task_id}\",\"source_url\":\"http://127.0.0.1:${source_port}/input.mp4\",\"target_path\":\"imports/${task_id}/source.mp4\",\"source_kind\":\"http_mp4\",\"start_offset_sec\":4,\"duration_sec\":4}" \
  >/dev/null

status=
for _ in $(seq 1 200); do
  status="$(curl -fsS "http://127.0.0.1:${gateway_port}/api/prefetch/${task_id}")"
  python3 -c 'import json,sys; raise SystemExit(0 if json.load(sys.stdin).get("status") == "ready" else 1)' \
    <<<"${status}" && break
  python3 -c 'import json,sys; raise SystemExit(0 if json.load(sys.stdin).get("status") != "failed" else 1)' \
    <<<"${status}" || { printf '%s\n' "${status}" >&2; exit 1; }
  sleep 0.05
done

input_json="$(${FFPROBE_BIN} -v error -show_entries stream=codec_type,codec_name,width,height,r_frame_rate,sample_rate,channels -of json "${ROOT}/source/input.mp4")"
output_json="$(${FFPROBE_BIN} -v error -show_entries stream=codec_type,codec_name,width,height,r_frame_rate,sample_rate,channels -of json "${ROOT}/work/imports/${task_id}/source.mp4")"
python3 - "${input_json}" "${output_json}" <<'PY'
import json, sys
source = json.loads(sys.argv[1])["streams"]
output = json.loads(sys.argv[2])["streams"]
keys = ("codec_type", "codec_name", "width", "height", "r_frame_rate", "sample_rate", "channels")
normalize = lambda streams: [{k: stream.get(k) for k in keys if k in stream} for stream in streams]
assert normalize(source) == normalize(output), (normalize(source), normalize(output))
PY

duration="$(${FFPROBE_BIN} -v error -show_entries format=duration -of default=nk=1:nw=1 "${ROOT}/work/imports/${task_id}/source.mp4")"
python3 - "${duration}" <<'PY'
import sys
duration = float(sys.argv[1])
assert 3.0 <= duration <= 5.5, duration
PY

echo "media-gateway time-slice smoke passed"
