#!/usr/bin/env bash
set -euo pipefail
umask 077

template_file="${1:?missing template file}"
output_file="${2:?missing output file}"
output_dir="${output_file%/*}"
[ "${output_dir}" != "${output_file}" ] || output_dir=.
[ -n "${output_dir}" ] || output_dir=/
temporary_file=""
backup_file=""

cleanup_temporary_file() {
  [ -z "${temporary_file}" ] || rm -f -- "${temporary_file}"
  [ -z "${backup_file}" ] || rm -f -- "${backup_file}"
}

trap cleanup_temporary_file EXIT
trap 'exit 1' HUP INT TERM

require_env() {
  local key="$1"
  if [ "${!key+x}" != x ] || [ -z "${!key}" ]; then
    echo "missing required env: ${key}" >&2
    exit 1
  fi
}

replace_placeholder() {
  local placeholder="$1"
  local key="$2"
  local value="${!key}"
  rendered="${rendered//"${placeholder}"/"${value}"}"
}

export AGENT_MP4_RECORD_SEGMENT_SEC="${AGENT_MP4_RECORD_SEGMENT_SEC:-7200}"

# ZLM 模板全部由环境变量渲染，缺任一关键值都直接失败，避免生成半有效配置。
for key in \
  ZLM_API_SECRET \
  ZLM_HOOK_SHARED_SECRET \
  ZLM_SERVER_ID \
  ZLM_HOOK_BASE \
  ZLM_API_ALLOW_IP_RANGE \
  ZLM_HTTP_PORT \
  ZLM_HTTPS_PORT \
  ZLM_RTMP_PORT \
  ZLM_RTMPS_PORT \
  ZLM_RTSP_PORT \
  ZLM_RTSPS_PORT \
  ZLM_RTP_PROXY_PORT \
  ZLM_RTP_PROXY_PORT_RANGE \
  ZLM_RTC_SIGNALING_PORT \
  ZLM_RTC_SIGNALING_SSL_PORT \
  ZLM_RTC_ICE_PORT \
  ZLM_RTC_ICE_TCP_PORT \
  ZLM_RTC_PORT \
  ZLM_RTC_TCP_PORT \
  ZLM_RTC_PORT_RANGE \
  ZLM_SRT_PORT \
  ZLM_SHELL_PORT \
  ZLM_ONVIF_PORT \
  ZLM_WWW_ROOT \
  ZLM_RECORD_ROOT \
  ZLM_SNAP_ROOT \
  ZLM_DEFAULT_PEM \
  AGENT_MP4_RECORD_SEGMENT_SEC; do
  require_env "${key}"
done

[ -f "${template_file}" ] && [ ! -L "${template_file}" ] || {
  echo "ZLM config template must be a regular file" >&2
  exit 1
}
[ -d "${output_dir}" ] && [ ! -L "${output_dir}" ] || {
  echo "ZLM config output directory must be a real directory" >&2
  exit 1
}
if [ -e "${output_file}" ] || [ -L "${output_file}" ]; then
  [ -f "${output_file}" ] && [ ! -L "${output_file}" ] || {
    echo "ZLM config output must be a regular file" >&2
    exit 1
  }
fi

# Keep every secret in this shell's memory. In particular, never interpolate a
# secret into sed, python, or another child process argv.
shopt -u patsub_replacement 2>/dev/null || true
rendered="$(<"${template_file}")"
replace_placeholder __ZLM_API_SECRET__ ZLM_API_SECRET
replace_placeholder __HOOK_SHARED_SECRET__ ZLM_HOOK_SHARED_SECRET
replace_placeholder __ZLM_SERVER_ID__ ZLM_SERVER_ID
replace_placeholder __HOOK_BASE__ ZLM_HOOK_BASE
replace_placeholder __ZLM_API_ALLOW_IP_RANGE__ ZLM_API_ALLOW_IP_RANGE
replace_placeholder __ZLM_HTTP_PORT__ ZLM_HTTP_PORT
replace_placeholder __ZLM_HTTPS_PORT__ ZLM_HTTPS_PORT
replace_placeholder __ZLM_RTMP_PORT__ ZLM_RTMP_PORT
replace_placeholder __ZLM_RTMPS_PORT__ ZLM_RTMPS_PORT
replace_placeholder __ZLM_RTSP_PORT__ ZLM_RTSP_PORT
replace_placeholder __ZLM_RTSPS_PORT__ ZLM_RTSPS_PORT
replace_placeholder __ZLM_RTP_PROXY_PORT__ ZLM_RTP_PROXY_PORT
replace_placeholder __ZLM_RTP_PROXY_PORT_RANGE__ ZLM_RTP_PROXY_PORT_RANGE
replace_placeholder __ZLM_RTC_SIGNALING_PORT__ ZLM_RTC_SIGNALING_PORT
replace_placeholder __ZLM_RTC_SIGNALING_SSL_PORT__ ZLM_RTC_SIGNALING_SSL_PORT
replace_placeholder __ZLM_RTC_ICE_PORT__ ZLM_RTC_ICE_PORT
replace_placeholder __ZLM_RTC_ICE_TCP_PORT__ ZLM_RTC_ICE_TCP_PORT
replace_placeholder __ZLM_RTC_PORT__ ZLM_RTC_PORT
replace_placeholder __ZLM_RTC_TCP_PORT__ ZLM_RTC_TCP_PORT
replace_placeholder __ZLM_RTC_PORT_RANGE__ ZLM_RTC_PORT_RANGE
replace_placeholder __ZLM_SRT_PORT__ ZLM_SRT_PORT
replace_placeholder __ZLM_SHELL_PORT__ ZLM_SHELL_PORT
replace_placeholder __ZLM_ONVIF_PORT__ ZLM_ONVIF_PORT
replace_placeholder __ZLM_WWW_ROOT__ ZLM_WWW_ROOT
replace_placeholder __ZLM_RECORD_ROOT__ ZLM_RECORD_ROOT
replace_placeholder __ZLM_SNAP_ROOT__ ZLM_SNAP_ROOT
replace_placeholder __ZLM_DEFAULT_PEM__ ZLM_DEFAULT_PEM
replace_placeholder __AGENT_MP4_RECORD_SEGMENT_SEC__ AGENT_MP4_RECORD_SEGMENT_SEC
if [[ "${rendered}" =~ __[A-Z0-9_]+__ ]]; then
  echo "ZLM config template contains an unresolved placeholder" >&2
  exit 1
fi

temporary_file="$(mktemp "${output_file}.tmp.XXXXXX")"
chmod 600 "${temporary_file}"
printf '%s\n' "${rendered}" >"${temporary_file}"
sync "${temporary_file}"

# Keep the previous inode in the same directory until the replacement rename
# is durable. This lets an ordinary directory-fsync failure restore the prior
# visible config without copying secret contents through another process.
if [ -e "${output_file}" ]; then
  chmod 600 "${output_file}"
  backup_file="$(mktemp "${output_file}.previous.XXXXXX")"
  rm -f -- "${backup_file}"
  ln -- "${output_file}" "${backup_file}"
  sync "${output_dir}"
fi
mv -fT -- "${temporary_file}" "${output_file}"
temporary_file=""
if ! sync "${output_dir}"; then
  rollback_ok=0
  if [ -n "${backup_file}" ]; then
    if mv -fT -- "${backup_file}" "${output_file}"; then
      backup_file=""
      rollback_ok=1
    fi
  elif rm -f -- "${output_file}"; then
    rollback_ok=1
  fi
  if [ "${rollback_ok}" -eq 1 ]; then
    sync "${output_dir}" >/dev/null 2>&1 || true
    echo "ZLM config directory sync failed; previous visible config restored" >&2
  else
    recovery_file="${backup_file}"
    backup_file=""
    echo "ZLM config directory sync and rollback failed; recovery inode retained at ${recovery_file}" >&2
  fi
  exit 1
fi
if [ -n "${backup_file}" ]; then
  rm -f -- "${backup_file}"
  backup_file=""
  sync "${output_dir}" >/dev/null 2>&1 || true
fi
trap - EXIT HUP INT TERM
