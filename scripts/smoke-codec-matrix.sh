#!/usr/bin/env bash
set -euo pipefail

FFMPEG_BIN="${FFMPEG_BIN:-ffmpeg}"
FFPROBE_BIN="${FFPROBE_BIN:-ffprobe}"

log() {
  printf '[%s] %s\n' "$1" "$2"
}

require_command() {
  if ! command -v "$1" >/dev/null 2>&1; then
    printf 'missing required command: %s\n' "$1" >&2
    exit 1
  fi
}

encoder_available() {
  "$FFMPEG_BIN" -hide_banner -encoders 2>/dev/null | awk '{print $2}' | grep -qx "$1"
}

ffmpeg_run() {
  log RUN "$FFMPEG_BIN $*"
  "$FFMPEG_BIN" -hide_banner -nostdin -y "$@"
}

ffprobe_run() {
  log PROBE "$FFPROBE_BIN $*"
  "$FFPROBE_BIN" -hide_banner -v error "$@"
}

expect_ffmpeg_fail() {
  local name="$1"
  shift
  log RUN "expect failure: $name"
  if "$FFMPEG_BIN" -hide_banner -nostdin -y "$@" >/dev/null 2>&1; then
    printf 'expected ffmpeg failure but command succeeded: %s\n' "$name" >&2
    exit 1
  fi
  log OK "$name failed as expected"
}

require_encoder() {
  if ! encoder_available "$1"; then
    printf 'missing required ffmpeg encoder: %s\n' "$1" >&2
    exit 1
  fi
}

skip_if_missing_encoder() {
  if ! encoder_available "$1"; then
    log SKIP "$2 requires encoder $1"
    return 0
  fi
  return 1
}

require_command "$FFMPEG_BIN"
require_command "$FFPROBE_BIN"
require_encoder libx264
require_encoder aac

tmp_dir="$(mktemp -d "${TMPDIR:-/tmp}/streamserver-codec-smoke.XXXXXX")"
trap 'rm -rf "$tmp_dir"' EXIT

h264_aac_mp4="$tmp_dir/source-h264-aac.mp4"
h264_aac_ts="$tmp_dir/source-h264-aac.ts"

ffmpeg_run \
  -f lavfi -i testsrc=size=128x72:rate=25 \
  -f lavfi -i sine=frequency=1000:sample_rate=48000 \
  -t 1 \
  -map 0:v:0 -map 1:a:0 \
  -c:v libx264 -pix_fmt yuv420p -g 25 \
  -c:a aac -b:a 96k \
  "$h264_aac_mp4"

ffmpeg_run \
  -i "$h264_aac_mp4" \
  -map 0:v? -map 0:a? \
  -c copy \
  -f mpegts \
  "$h264_aac_ts"

mp4_copy="$tmp_dir/h264-aac-copy.mp4"
ffmpeg_run \
  -i "$h264_aac_mp4" \
  -map 0:v? -map 0:a? \
  -c:v copy -c:a copy \
  -f mp4 \
  "$mp4_copy"
ffprobe_run -show_entries stream=codec_type,codec_name -of compact=p=0:nk=1 "$mp4_copy" >/dev/null

ts_to_mp4="$tmp_dir/h264-aac-ts-to-mp4.mp4"
ffmpeg_run \
  -i "$h264_aac_ts" \
  -map 0:v? -map 0:a? \
  -c:v copy -c:a copy \
  -bsf:a aac_adtstoasc \
  -f mp4 \
  "$ts_to_mp4"
ffprobe_run -show_entries stream=codec_type,codec_name -of compact=p=0:nk=1 "$ts_to_mp4" >/dev/null

hls_dir="$tmp_dir/hls"
mkdir -p "$hls_dir"
ffmpeg_run \
  -i "$h264_aac_ts" \
  -map 0:v? -map 0:a? \
  -c:v copy -c:a copy \
  -f hls \
  -hls_time 1 \
  -hls_list_size 0 \
  -hls_segment_filename "$hls_dir/segment-%03d.ts" \
  "$hls_dir/out.m3u8"
test -s "$hls_dir/out.m3u8"
ffprobe_run -show_entries stream=codec_type,codec_name -of compact=p=0:nk=1 "$hls_dir/out.m3u8" >/dev/null

both_dir="$tmp_dir/both"
mkdir -p "$both_dir"
ffmpeg_run \
  -i "$h264_aac_ts" \
  -c:v copy -c:a copy \
  -map 0:v? -map 0:a? \
  -bsf:a aac_adtstoasc \
  -f mp4 \
  "$both_dir/out.mp4" \
  -c:v copy -c:a copy \
  -map 0:v? -map 0:a? \
  -f hls \
  -hls_time 1 \
  -hls_list_size 0 \
  -hls_segment_filename "$both_dir/segment-%03d.ts" \
  "$both_dir/out.m3u8"
ffprobe_run -show_entries stream=codec_type,codec_name -of compact=p=0:nk=1 "$both_dir/out.mp4" >/dev/null
ffprobe_run -show_entries stream=codec_type,codec_name -of compact=p=0:nk=1 "$both_dir/out.m3u8" >/dev/null

matroska_out="$tmp_dir/h264-aac.mkv"
ffmpeg_run \
  -i "$h264_aac_mp4" \
  -map 0:v? -map 0:a? \
  -c:v copy -c:a copy \
  -f matroska \
  "$matroska_out"
ffprobe_run -show_entries format=format_name -of default=nk=1:nw=1 "$matroska_out" | grep -q 'matroska'

expect_ffmpeg_fail "invalid mkv muxer" \
  -i "$h264_aac_mp4" \
  -map 0:v? -map 0:a? \
  -c:v copy -c:a copy \
  -f mkv \
  "$tmp_dir/invalid-muxer.mkv"

expect_ffmpeg_fail "h264/aac webm output" \
  -i "$h264_aac_mp4" \
  -map 0:v? -map 0:a? \
  -c:v copy -c:a copy \
  -f webm \
  "$tmp_dir/h264-aac.webm"

if ! skip_if_missing_encoder libmp3lame "h264/mp3 flv"; then
  flv_out="$tmp_dir/h264-mp3.flv"
  ffmpeg_run \
    -f lavfi -i testsrc=size=128x72:rate=25 \
    -f lavfi -i sine=frequency=800:sample_rate=44100 \
    -t 1 \
    -map 0:v:0 -map 1:a:0 \
    -c:v libx264 -pix_fmt yuv420p \
    -c:a libmp3lame \
    -f flv \
    "$flv_out"
  ffprobe_run -show_entries stream=codec_type,codec_name -of compact=p=0:nk=1 "$flv_out" >/dev/null
fi

if ! skip_if_missing_encoder libx265 "hevc/aac matroska"; then
  hevc_mkv="$tmp_dir/hevc-aac.mkv"
  ffmpeg_run \
    -f lavfi -i testsrc=size=128x72:rate=25 \
    -f lavfi -i sine=frequency=1200:sample_rate=48000 \
    -t 1 \
    -map 0:v:0 -map 1:a:0 \
    -c:v libx265 -pix_fmt yuv420p \
    -c:a aac \
    -f matroska \
    "$hevc_mkv"
  ffprobe_run -show_entries stream=codec_type,codec_name -of compact=p=0:nk=1 "$hevc_mkv" >/dev/null
fi

if encoder_available libvpx-vp9 && encoder_available libopus; then
  webm_ok="$tmp_dir/vp9-opus.webm"
  ffmpeg_run \
    -f lavfi -i testsrc=size=128x72:rate=25 \
    -f lavfi -i sine=frequency=1400:sample_rate=48000 \
    -t 1 \
    -map 0:v:0 -map 1:a:0 \
    -c:v libvpx-vp9 -deadline realtime -cpu-used 8 \
    -c:a libopus \
    -f webm \
    "$webm_ok"
  ffprobe_run -show_entries stream=codec_type,codec_name -of compact=p=0:nk=1 "$webm_ok" >/dev/null
else
  log SKIP "vp9/opus webm requires libvpx-vp9 and libopus"
fi

log PASS "codec smoke matrix completed"
