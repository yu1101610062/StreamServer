#!/usr/bin/env bash
set -euo pipefail

FFMPEG_IMAGE="${FFMPEG_IMAGE:-jrottenberg/ffmpeg:7.1-ubuntu2404}"
ZLM_IMAGE="${ZLM_IMAGE:-streamserver/zlmediakit:master-linux-amd64}"
HTTP_PORT="${HTTP_PORT:-18080}"
STREAM_APP="${STREAM_APP:-live}"
STREAM_NAME="${STREAM_NAME:-hevcflv}"
ZLM_SECRET="${ZLM_SECRET:-$(openssl rand -hex 16)}"

NETWORK_NAME="streamserver-hevc-flv-$RANDOM-$$"
ZLM_CONTAINER="zlm-hevc-flv-$RANDOM-$$"
PUBLISHER_CONTAINER="ffmpeg-hevc-flv-publisher-$RANDOM-$$"
SEED_CONTAINER="zlm-hevc-flv-seed-$RANDOM-$$"
TMP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/streamserver-hevc-flv.XXXXXX")"

log() {
  printf '[hevc-flv-validate] %s\n' "$*"
}

fail() {
  printf '[hevc-flv-validate] ERROR: %s\n' "$*" >&2
  exit 1
}

cleanup() {
  docker rm -f "${PUBLISHER_CONTAINER}" "${ZLM_CONTAINER}" "${SEED_CONTAINER}" >/dev/null 2>&1 || true
  docker network rm "${NETWORK_NAME}" >/dev/null 2>&1 || true
  rm -rf "${TMP_DIR}"
}

trap cleanup EXIT

require_cmd() {
  command -v "$1" >/dev/null 2>&1 || fail "缺少命令: $1"
}

wait_for_http() {
  local url="$1"
  local retries="${2:-20}"
  local i
  for i in $(seq 1 "${retries}"); do
    if curl -fsS "${url}" >/dev/null 2>&1; then
      return 0
    fi
    sleep 1
  done
  return 1
}

wait_for_stream_registration() {
  local api_url="$1"
  local retries="${2:-20}"
  local body=""
  local i
  for i in $(seq 1 "${retries}"); do
    body="$(curl -fsS "${api_url}" 2>/dev/null || true)"
    if printf '%s' "${body}" | grep -q '"codec_id_name"[[:space:]]*:[[:space:]]*"H265"'; then
      return 0
    fi
    sleep 1
  done
  printf '%s\n' "${body}" >&2
  return 1
}

probe_stream() {
  local url="$1"
  local output
  output="$(docker run --rm --network "${NETWORK_NAME}" --entrypoint ffprobe "${FFMPEG_IMAGE}" \
    -v error -show_streams -select_streams v:0 -of json "${url}")"
  printf '%s' "${output}" | grep -q '"codec_name"[[:space:]]*:[[:space:]]*"hevc"' \
    || fail "拉流校验失败: ${url}"
}

require_cmd docker
require_cmd curl
require_cmd openssl

mkdir -p "${TMP_DIR}/www"
docker network create "${NETWORK_NAME}" >/dev/null

log "准备 ZLMediaKit 配置"
docker create --name "${SEED_CONTAINER}" "${ZLM_IMAGE}" >/dev/null
docker cp "${SEED_CONTAINER}:/opt/media/conf/config.ini" "${TMP_DIR}/config.ini"
docker rm -f "${SEED_CONTAINER}" >/dev/null
sed -i.bak \
  -e 's/^apiDebug=.*/apiDebug=0/' \
  -e "s/^secret=.*/secret=${ZLM_SECRET}/" \
  "${TMP_DIR}/config.ini"
rm -f "${TMP_DIR}/config.ini.bak"

log "启动独立 ZLMediaKit"
docker run -d --rm \
  --name "${ZLM_CONTAINER}" \
  --network "${NETWORK_NAME}" \
  -p "127.0.0.1:${HTTP_PORT}:80" \
  -v "${TMP_DIR}/config.ini:/opt/media/conf/config.ini:ro" \
  -v "${TMP_DIR}/www:/opt/media/bin/www" \
  "${ZLM_IMAGE}" \
  ./MediaServer -s default.pem -c ../conf/config.ini -l 0 >/dev/null

wait_for_http "http://127.0.0.1:${HTTP_PORT}/index/api/getStatistic?secret=${ZLM_SECRET}" 20 \
  || fail "ZLMediaKit HTTP API 未就绪"

log "启动 HEVC+FLV 推流"
docker run -d --rm \
  --name "${PUBLISHER_CONTAINER}" \
  --network "${NETWORK_NAME}" \
  --entrypoint ffmpeg \
  "${FFMPEG_IMAGE}" \
  -hide_banner \
  -re \
  -f lavfi \
  -i testsrc=size=128x72:rate=5 \
  -t 20 \
  -pix_fmt yuv420p \
  -c:v libx265 \
  -an \
  -f flv \
  "rtmp://${ZLM_CONTAINER}/${STREAM_APP}/${STREAM_NAME}" >/dev/null

wait_for_stream_registration \
  "http://127.0.0.1:${HTTP_PORT}/index/api/getMediaList?secret=${ZLM_SECRET}&schema=rtmp&vhost=__defaultVhost__&app=${STREAM_APP}&stream=${STREAM_NAME}" \
  20 || fail "ZLMediaKit 未注册 H265 RTMP 流"

log "验证 RTSP/RTMP/HTTP-FLV 可回拉 HEVC"
probe_stream "rtsp://${ZLM_CONTAINER}/${STREAM_APP}/${STREAM_NAME}"
probe_stream "rtmp://${ZLM_CONTAINER}:1935/${STREAM_APP}/${STREAM_NAME}"
probe_stream "http://${ZLM_CONTAINER}/${STREAM_APP}/${STREAM_NAME}.live.flv"

log "验证通过: ${FFMPEG_IMAGE} 可将 HEVC+FLV 推入 ${ZLM_IMAGE} 并由 ZLMediaKit 重新暴露"
