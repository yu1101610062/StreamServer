#!/usr/bin/env bash
set -euo pipefail

template_file="${1:?missing template file}"
output_file="${2:?missing output file}"

escape_sed_replacement() {
  printf '%s' "$1" | sed 's/[&|]/\\&/g'
}

require_env() {
  local key="$1"
  local value
  value="$(printenv "${key}" 2>/dev/null || true)"
  if [ -z "${value}" ]; then
    echo "missing required env: ${key}" >&2
    exit 1
  fi
}

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
  ZLM_DEFAULT_PEM; do
  require_env "${key}"
done

sed \
  -e "s|__ZLM_API_SECRET__|$(escape_sed_replacement "${ZLM_API_SECRET}")|g" \
  -e "s|__HOOK_SHARED_SECRET__|$(escape_sed_replacement "${ZLM_HOOK_SHARED_SECRET}")|g" \
  -e "s|__ZLM_SERVER_ID__|$(escape_sed_replacement "${ZLM_SERVER_ID}")|g" \
  -e "s|__HOOK_BASE__|$(escape_sed_replacement "${ZLM_HOOK_BASE}")|g" \
  -e "s|__ZLM_API_ALLOW_IP_RANGE__|$(escape_sed_replacement "${ZLM_API_ALLOW_IP_RANGE}")|g" \
  -e "s|__ZLM_HTTP_PORT__|$(escape_sed_replacement "${ZLM_HTTP_PORT}")|g" \
  -e "s|__ZLM_HTTPS_PORT__|$(escape_sed_replacement "${ZLM_HTTPS_PORT}")|g" \
  -e "s|__ZLM_RTMP_PORT__|$(escape_sed_replacement "${ZLM_RTMP_PORT}")|g" \
  -e "s|__ZLM_RTMPS_PORT__|$(escape_sed_replacement "${ZLM_RTMPS_PORT}")|g" \
  -e "s|__ZLM_RTSP_PORT__|$(escape_sed_replacement "${ZLM_RTSP_PORT}")|g" \
  -e "s|__ZLM_RTSPS_PORT__|$(escape_sed_replacement "${ZLM_RTSPS_PORT}")|g" \
  -e "s|__ZLM_RTP_PROXY_PORT__|$(escape_sed_replacement "${ZLM_RTP_PROXY_PORT}")|g" \
  -e "s|__ZLM_RTP_PROXY_PORT_RANGE__|$(escape_sed_replacement "${ZLM_RTP_PROXY_PORT_RANGE}")|g" \
  -e "s|__ZLM_RTC_SIGNALING_PORT__|$(escape_sed_replacement "${ZLM_RTC_SIGNALING_PORT}")|g" \
  -e "s|__ZLM_RTC_SIGNALING_SSL_PORT__|$(escape_sed_replacement "${ZLM_RTC_SIGNALING_SSL_PORT}")|g" \
  -e "s|__ZLM_RTC_ICE_PORT__|$(escape_sed_replacement "${ZLM_RTC_ICE_PORT}")|g" \
  -e "s|__ZLM_RTC_ICE_TCP_PORT__|$(escape_sed_replacement "${ZLM_RTC_ICE_TCP_PORT}")|g" \
  -e "s|__ZLM_RTC_PORT__|$(escape_sed_replacement "${ZLM_RTC_PORT}")|g" \
  -e "s|__ZLM_RTC_TCP_PORT__|$(escape_sed_replacement "${ZLM_RTC_TCP_PORT}")|g" \
  -e "s|__ZLM_RTC_PORT_RANGE__|$(escape_sed_replacement "${ZLM_RTC_PORT_RANGE}")|g" \
  -e "s|__ZLM_SRT_PORT__|$(escape_sed_replacement "${ZLM_SRT_PORT}")|g" \
  -e "s|__ZLM_SHELL_PORT__|$(escape_sed_replacement "${ZLM_SHELL_PORT}")|g" \
  -e "s|__ZLM_ONVIF_PORT__|$(escape_sed_replacement "${ZLM_ONVIF_PORT}")|g" \
  -e "s|__ZLM_WWW_ROOT__|$(escape_sed_replacement "${ZLM_WWW_ROOT}")|g" \
  -e "s|__ZLM_RECORD_ROOT__|$(escape_sed_replacement "${ZLM_RECORD_ROOT}")|g" \
  -e "s|__ZLM_SNAP_ROOT__|$(escape_sed_replacement "${ZLM_SNAP_ROOT}")|g" \
  -e "s|__ZLM_DEFAULT_PEM__|$(escape_sed_replacement "${ZLM_DEFAULT_PEM}")|g" \
  "${template_file}" >"${output_file}"
