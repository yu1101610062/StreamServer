#!/usr/bin/env bash
set -euo pipefail

INSTALL_DIR=""
ASSUME_YES=0
DATA_POLICY=""

log() {
  printf '[streamserver-native-uninstall] %s\n' "$*"
}

warn() {
  printf '[streamserver-native-uninstall] WARN: %s\n' "$*" >&2
}

fail() {
  printf '[streamserver-native-uninstall] ERROR: %s\n' "$*" >&2
  exit 1
}

usage() {
  cat <<EOF
用法:
  ./uninstall.sh [--install-dir DIR] [--keep-data|--purge] [--yes]

说明:
  卸载 StreamServer native 实例。默认会询问是否删除数据；默认选择保留数据。

参数:
  --install-dir DIR  安装目录；在安装目录内执行时可省略
  --keep-data        删除程序、runtime、UI、systemd 文件，保留 .env、data、certs
  --purge            删除整个安装目录，包括数据和配置
  --yes              非交互确认；未指定 --purge 时默认保留数据
EOF
}

parse_args() {
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --install-dir)
        [ "$#" -ge 2 ] || fail "--install-dir 需要参数"
        INSTALL_DIR="$2"
        shift 2
        ;;
      --keep-data)
        [ -z "${DATA_POLICY}" ] || fail "--keep-data 不能与 --purge 同时使用"
        DATA_POLICY="keep"
        shift
        ;;
      --purge)
        [ -z "${DATA_POLICY}" ] || fail "--purge 不能与 --keep-data 同时使用"
        DATA_POLICY="purge"
        shift
        ;;
      --yes|-y)
        ASSUME_YES=1
        shift
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

prompt_yes_no() {
  local message="$1"
  local default_value="${2:-N}"
  local answer
  while true; do
    printf '%s [%s]: ' "${message}" "${default_value}" >&2
    read -r answer
    [ -n "${answer}" ] || answer="${default_value}"
    case "${answer}" in
      Y|y|yes|YES) return 0 ;;
      N|n|no|NO) return 1 ;;
      *) echo "请输入 Y 或 N。" >&2 ;;
    esac
  done
}

resolve_install_dir() {
  local script_dir
  script_dir="$(cd "$(dirname "$0")" && pwd)"
  if [ -z "${INSTALL_DIR}" ]; then
    if [ -f "${script_dir}/.env" ]; then
      INSTALL_DIR="${script_dir}"
    else
      fail "未指定 --install-dir，且当前脚本不在已安装实例目录内"
    fi
  fi
  [ -d "${INSTALL_DIR}" ] || fail "安装目录不存在: ${INSTALL_DIR}"
  [ ! -L "${INSTALL_DIR}" ] \
    || fail "native installation directory must not be a symbolic link"
  INSTALL_DIR="$(cd -P "${INSTALL_DIR}" && pwd -P)"
}

require_root() {
  [ "$(id -u)" -eq 0 ] || fail "卸载 systemd 服务需要 root，请使用 root 执行 uninstall.sh"
}

mode_is_not_group_or_world_writable() {
  local mode="$1"
  [[ "${mode}" =~ ^[0-7]{3,4}$ ]] || return 1
  (( (8#${mode} & 8#022) == 0 ))
}

assert_trusted_install_control_boundary() {
  local env_file="${INSTALL_DIR}/.env"
  local mode
  [ ! -L "${INSTALL_DIR}" ] && [ -d "${INSTALL_DIR}" ] \
    || fail "native install control boundary is not a real directory"
  [ "$(stat -c '%u' -- "${INSTALL_DIR}")" = 0 ] \
    || fail "native install control boundary must be root-owned"
  mode="$(stat -c '%a' -- "${INSTALL_DIR}")" \
    || fail "cannot inspect native install control boundary"
  mode_is_not_group_or_world_writable "${mode}" \
    || fail "native install control boundary must not be group/world writable"
  [ ! -L "${env_file}" ] && [ -f "${env_file}" ] \
    || fail "native instance environment must be a regular non-symbolic file"
  [ "$(stat -c '%u' -- "${env_file}")" = 0 ] \
    || fail "native instance environment must be root-owned"
  mode="$(stat -c '%a' -- "${env_file}")" \
    || fail "cannot inspect native instance environment"
  mode_is_not_group_or_world_writable "${mode}" \
    || fail "native instance environment must not be group/world writable"
}

load_env() {
  local env_file="${INSTALL_DIR}/.env"
  [ -f "${env_file}" ] || fail "缺少实例配置: ${env_file}"
  # shellcheck disable=SC1090
  . "${env_file}"
  [ "${DEPLOY_MODE:-}" = "native" ] || fail "不是 native 实例目录: ${INSTALL_DIR}"
}

validate_loaded_native_identity() {
  local unit_basename
  case "${INSTALL_ROLE:-}" in
    control-plane|worker-host-cpu|worker-host-gpu|all-in-one-host-cpu|all-in-one-host-gpu) ;;
    *) fail "native instance has an invalid INSTALL_ROLE" ;;
  esac
  case "${INSTANCE_NAME:-}" in
    ''|-*|*[!A-Za-z0-9_.@-]*) fail "native instance has an invalid INSTANCE_NAME" ;;
  esac
  case "${INSTANCE_NAME}" in
    ss-*) unit_basename="${INSTANCE_NAME}" ;;
    *) unit_basename="ss-${INSTANCE_NAME}" ;;
  esac
  [ "${SYSTEMD_TARGET:-}" = "${unit_basename}.target" ] \
    && [ "${SYSTEMD_CORE_UNIT:-}" = "${unit_basename}-core.service" ] \
    && [ "${SYSTEMD_AGENT_UNIT:-}" = "${unit_basename}-agent.service" ] \
    && [ "${SYSTEMD_ZLM_UNIT:-}" = "${unit_basename}-zlm.service" ] \
    && [ "${SYSTEMD_POSTGRES_UNIT:-}" = "${unit_basename}-postgres.service" ] \
    || fail "native instance systemd identity does not match INSTANCE_NAME"
}

validate_unit_name() {
  local unit="$1"
  [ -n "${unit}" ] || return 1
  case "${unit}" in
    */*|*..*|.*)
      return 1
      ;;
    *.service|*.target)
      return 0
      ;;
    *)
      return 1
      ;;
  esac
}

unique_units() {
  local seen=" "
  local unit
  # .env 可能缺少部分 unit，也可能因为手工修改出现重复，这里统一去重并过滤非法名称。
  for unit in "$@"; do
    [ -n "${unit}" ] || continue
    validate_unit_name "${unit}" || {
      warn "跳过非法 unit 名称: ${unit}"
      continue
    }
    case "${seen}" in
      *" ${unit} "*) ;;
      *)
        printf '%s\n' "${unit}"
        seen="${seen}${unit} "
        ;;
    esac
  done
}

service_units_stop_order() {
  # 先停业务和 ZLM，再停 Core/Postgres，避免卸载过程中产生新的 Hook 或任务事件。
  unique_units \
    "${SYSTEMD_AGENT_UNIT:-}" \
    "${SYSTEMD_ZLM_UNIT:-}" \
    "${SYSTEMD_CORE_UNIT:-}" \
    "${SYSTEMD_POSTGRES_UNIT:-}" \
    "${SYSTEMD_TARGET:-}"
}

unit_files_remove_order() {
  # 删除 unit 文件时先删 target，再删被 target 依赖的具体服务。
  unique_units \
    "${SYSTEMD_TARGET:-}" \
    "${SYSTEMD_POSTGRES_UNIT:-}" \
    "${SYSTEMD_CORE_UNIT:-}" \
    "${SYSTEMD_ZLM_UNIT:-}" \
    "${SYSTEMD_AGENT_UNIT:-}"
}

stop_and_remove_units() {
  local unit units=()

  if ! command -v systemctl >/dev/null 2>&1; then
    warn "缺少 systemctl，跳过 systemd unit 卸载"
    return 0
  fi

  mapfile -t units < <(service_units_stop_order)
  # stop 和 disable 分两轮执行，确保所有服务都先进入停止流程。
  for unit in "${units[@]}"; do
    log "停止 unit: ${unit}"
    systemctl stop "${unit}" >/dev/null 2>&1 || warn "停止失败或 unit 不存在: ${unit}"
  done

  for unit in "${units[@]}"; do
    log "禁用 unit: ${unit}"
    systemctl disable "${unit}" >/dev/null 2>&1 || warn "禁用失败或 unit 不存在: ${unit}"
  done

  mapfile -t units < <(unit_files_remove_order)
  for unit in "${units[@]}"; do
    rm -f "/etc/systemd/system/${unit}"
    find /etc/systemd/system -type l -name "${unit}" -delete 2>/dev/null || true
  done
  if [ -n "${SYSTEMD_TARGET:-}" ] && validate_unit_name "${SYSTEMD_TARGET}"; then
    rm -rf "/etc/systemd/system/${SYSTEMD_TARGET}.wants"
  fi

  systemctl daemon-reload >/dev/null 2>&1 || warn "systemctl daemon-reload 失败"
  systemctl reset-failed >/dev/null 2>&1 || true
}

component_count() {
  local path="$1"
  local trimmed count
  trimmed="${path#/}"
  [ -n "${trimmed}" ] || {
    printf '0\n'
    return 0
  }
  count="$(printf '%s' "${trimmed}" | awk -F/ '{ print NF }')"
  printf '%s\n' "${count}"
}

assert_safe_install_dir_for_purge() {
  local count
  # purge 是破坏性删除，必须同时满足路径层级、.env 和 native 标记三重保护。
  case "${INSTALL_DIR}" in
    ""|"/"|"/bin"|"/boot"|"/dev"|"/etc"|"/home"|"/lib"|"/lib64"|"/opt"|"/proc"|"/root"|"/run"|"/sbin"|"/sys"|"/tmp"|"/usr"|"/var")
      fail "拒绝删除高危目录: ${INSTALL_DIR}"
      ;;
  esac
  count="$(component_count "${INSTALL_DIR}")"
  # The installer supports standard two-component roots such as
  # /home/streamserver and /opt/streamserver-<instance>. Root ownership and
  # the exact native instance identity below form the destructive boundary.
  [ "${count}" -ge 2 ] || fail "安装目录层级过浅，拒绝 purge: ${INSTALL_DIR}"
  [ -f "${INSTALL_DIR}/.env" ] || fail "缺少 .env，拒绝 purge: ${INSTALL_DIR}"
  [ "${DEPLOY_MODE:-}" = "native" ] || fail "不是 native 实例，拒绝 purge: ${INSTALL_DIR}"
  assert_trusted_install_control_boundary
  validate_loaded_native_identity
}

choose_data_policy() {
  if [ -n "${DATA_POLICY}" ]; then
    return 0
  fi
  if [ "${ASSUME_YES}" -eq 1 ]; then
    # 非交互确认默认保留数据，只有显式 --purge 才删除 .env/data/certs。
    DATA_POLICY="keep"
    return 0
  fi
  if prompt_yes_no "是否删除数据和配置？选择 N 将保留 .env、data、certs" "N"; then
    DATA_POLICY="purge"
  else
    DATA_POLICY="keep"
  fi
}

remove_program_files_keep_data() {
  local item
  # keep-data 只删除程序和 runtime，保留配置、证书和媒体数据，便于重装到同一路径。
  for item in bin runtime ui zlm docs systemd uninstall.sh; do
    [ -e "${INSTALL_DIR}/${item}" ] || continue
    rm -rf "${INSTALL_DIR:?}/${item}"
    log "已删除: ${INSTALL_DIR}/${item}"
  done
  log "已保留数据和配置: ${INSTALL_DIR}/.env ${INSTALL_DIR}/data ${INSTALL_DIR}/certs"
}

purge_install_dir() {
  assert_safe_install_dir_for_purge
  rm -rf "${INSTALL_DIR}"
  log "已删除安装目录: ${INSTALL_DIR}"
}

main() {
  parse_args "$@"
  resolve_install_dir
  require_root
  assert_trusted_install_control_boundary
  load_env
  validate_loaded_native_identity
  choose_data_policy
  stop_and_remove_units
  case "${DATA_POLICY}" in
    keep)
      remove_program_files_keep_data
      ;;
    purge)
      purge_install_dir
      ;;
    *)
      fail "未知数据处理策略: ${DATA_POLICY}"
      ;;
  esac
  log "卸载完成"
}

main "$@"
