#!/usr/bin/env bash
set -euo pipefail

PACKAGE_ROOT="$(cd "$(dirname "$0")" && pwd)"
MANIFEST_FILE="${PACKAGE_ROOT}/package-manifest.env"
DEPLOY_DOC_SOURCE="${PACKAGE_ROOT}/docs/17-离线部署打包与安装.md"
CERT_SOURCE_DIR="${PACKAGE_ROOT}/certs"

if [ ! -f "${MANIFEST_FILE}" ]; then
  echo "缺少 ${MANIFEST_FILE}" >&2
  exit 1
fi

# shellcheck disable=SC1090
. "${MANIFEST_FILE}"

BUNDLE_VARIANT="${BUNDLE_VARIANT:-gpu-enabled}"
BUNDLE_GPU_SUPPORT="${BUNDLE_GPU_SUPPORT:-true}"
MEDIA_AGENT_GPU_IMAGE="${MEDIA_AGENT_GPU_IMAGE:-}"
MEDIA_AGENT_GPU_IMAGE_ARCHIVE="${MEDIA_AGENT_GPU_IMAGE_ARCHIVE:-}"

COMPOSE_CMD=()
COMPOSE_CMD_DISPLAY=""
COMPOSE_FILE_NAME="compose.yml"
STACK_SYSTEMD_UNIT_NAME=""
SYSTEMD_SERVICE_USE_SUDO="false"
IS_UPGRADE="false"
INSTALL_ROLE=""
EXISTING_INSTALL_ROLE=""
EXISTING_PROJECT_NAME=""
INSTALL_BACKUP_DIR=""
PRESERVED_ENV_SOURCE=""
RESERVED_LOCAL_TCP_PORTS=""

log() {
  printf '[streamserver-install] %s\n' "$*"
}

fail() {
  printf '[streamserver-install] ERROR: %s\n' "$*" >&2
  exit 1
}

require_cmd() {
  command -v "$1" >/dev/null 2>&1 || fail "缺少命令: $1"
}

ensure_linux_amd64() {
  [ "$(uname -s)" = "Linux" ] || fail "安装脚本只能在 Linux 上运行"
  case "$(uname -m)" in
    x86_64|amd64) ;;
    *) fail "目标主机必须是 Linux AMD64，当前架构为 $(uname -m)" ;;
  esac
}

detect_compose_cmd() {
  if docker compose version >/dev/null 2>&1; then
    COMPOSE_CMD=(docker compose)
    COMPOSE_CMD_DISPLAY="docker compose"
    return 0
  fi
  if command -v docker-compose >/dev/null 2>&1 && docker-compose version >/dev/null 2>&1; then
    COMPOSE_CMD=(docker-compose)
    COMPOSE_CMD_DISPLAY="docker-compose"
    return 0
  fi
  fail "缺少 Compose 命令，请安装 docker compose 插件或 docker-compose"
}

compose_cmd() {
  [ "${#COMPOSE_CMD[@]}" -gt 0 ] || fail "Compose 命令尚未初始化"
  "${COMPOSE_CMD[@]}" "$@"
}

compose_with_file() {
  compose_cmd -f "${COMPOSE_FILE_NAME}" "$@"
}

systemd_available() {
  command -v systemctl >/dev/null 2>&1 && [ -d /run/systemd/system ]
}

sanitize_systemd_fragment() {
  printf '%s' "$1" | sed 's/[^A-Za-z0-9_.@-]/-/g; s/-\{2,\}/-/g; s/^-//; s/-$//'
}

compose_systemd_unit_name() {
  local safe_name
  safe_name="$(sanitize_systemd_fragment "${PROJECT_NAME:-streamserver}")"
  [ -n "${safe_name}" ] || safe_name="ss"
  case "${safe_name}" in
    ss-*) printf '%s' "${safe_name}.service" ;;
    *) printf '%s' "ss-${safe_name}.service" ;;
  esac
}

current_user_is_root() {
  [ "$(id -u)" -eq 0 ]
}

systemd_use_sudo() {
  [ "${SYSTEMD_SERVICE_USE_SUDO}" = "true" ]
}

systemctl_cmd_display() {
  if systemd_use_sudo; then
    printf '%s' "sudo systemctl"
  else
    printf '%s' "systemctl"
  fi
}

journalctl_cmd_display() {
  if systemd_use_sudo; then
    printf '%s' "sudo journalctl"
  else
    printf '%s' "journalctl"
  fi
}

run_systemctl() {
  if systemd_use_sudo; then
    sudo systemctl "$@"
  else
    systemctl "$@"
  fi
}

ensure_docker_ready() {
  require_cmd docker
  docker info >/dev/null 2>&1 || fail "Docker 不可用，请先启动 Docker Engine"
  detect_compose_cmd
}

prompt() {
  local message="$1"
  local default_value="${2:-}"
  local answer
  if [ -n "${default_value}" ]; then
    printf '%s [%s]: ' "${message}" "${default_value}" >&2
  else
    printf '%s: ' "${message}" >&2
  fi
  read -r answer
  if [ -z "${answer}" ]; then
    answer="${default_value}"
  fi
  printf '%s' "${answer}"
}

prompt_non_empty() {
  local message="$1"
  local default_value="${2:-}"
  local answer
  while true; do
    answer="$(prompt "${message}" "${default_value}")"
    if [ -n "${answer}" ]; then
      printf '%s' "${answer}"
      return 0
    fi
    echo "输入不能为空。" >&2
  done
}

prompt_yes_no() {
  local message="$1"
  local default_value="${2:-Y}"
  local answer
  while true; do
    answer="$(prompt "${message}" "${default_value}")"
    case "${answer}" in
      Y|y|yes|YES) return 0 ;;
      N|n|no|NO) return 1 ;;
      *) echo "请输入 Y 或 N。" >&2 ;;
    esac
  done
}

prompt_secret() {
  local message="$1"
  local answer
  printf '%s: ' "${message}" >&2
  read -r -s answer
  printf '\n' >&2
  printf '%s' "${answer}"
}

prompt_password_with_confirmation() {
  local message="$1"
  local password
  local confirm
  while true; do
    password="$(prompt_secret "${message}")"
    [ -n "${password}" ] || {
      echo "输入不能为空。" >&2
      continue
    }
    [ "${#password}" -ge 8 ] || {
      echo "密码至少需要 8 个字符。" >&2
      continue
    }
    confirm="$(prompt_secret "再次输入以确认")"
    if [ "${password}" = "${confirm}" ]; then
      printf '%s' "${password}"
      return 0
    fi
    echo "两次输入的密码不一致，请重试。" >&2
  done
}

normalize_csv_labels() {
  local raw="${1:-}"
  local part
  local trimmed
  local existing
  local joined=""
  local duplicate=0
  local normalized_parts=()

  IFS=',' read -r -a parts <<< "${raw}"
  for part in "${parts[@]}"; do
    trimmed="$(printf '%s' "${part}" | sed 's/^[[:space:]]*//;s/[[:space:]]*$//')"
    [ -n "${trimmed}" ] || continue

    duplicate=0
    for existing in "${normalized_parts[@]}"; do
      if [ "${existing}" = "${trimmed}" ]; then
        duplicate=1
        break
      fi
    done
    [ "${duplicate}" -eq 1 ] && continue
    normalized_parts+=("${trimmed}")
  done

  for part in "${normalized_parts[@]}"; do
    if [ -n "${joined}" ]; then
      joined="${joined},${part}"
    else
      joined="${part}"
    fi
  done

  printf '%s' "${joined}"
}

collect_agent_labels() {
  local default_label="$1"
  local existing_labels="${2:-${default_label}}"
  local extra_labels
  local extra_default

  printf '当前节点默认会写入算力标签: %s\n' "${default_label}" >&2
  extra_default="$(extra_agent_labels_from_existing "${existing_labels}")"
  extra_labels="$(prompt "额外节点标签（英文逗号分隔，可留空）" "${extra_default}")"
  extra_labels="$(extra_agent_labels_from_existing "${extra_labels}")"
  printf '%s' "$(normalize_csv_labels "${default_label},${extra_labels}")"
}

extra_agent_labels_from_existing() {
  local raw="${1:-}"
  local part
  local trimmed
  local joined=""

  IFS=',' read -r -a parts <<< "${raw}"
  for part in "${parts[@]}"; do
    trimmed="$(printf '%s' "${part}" | sed 's/^[[:space:]]*//;s/[[:space:]]*$//')"
    [ -n "${trimmed}" ] || continue
    case "${trimmed}" in
      cpu|gpu) continue ;;
    esac
    if [ -n "${joined}" ]; then
      joined="${joined},${trimmed}"
    else
      joined="${trimmed}"
    fi
  done

  normalize_csv_labels "${joined}"
}

generate_uuid() {
  if command -v uuidgen >/dev/null 2>&1; then
    uuidgen | tr '[:upper:]' '[:lower:]'
    return 0
  fi
  if [ -r /proc/sys/kernel/random/uuid ]; then
    cat /proc/sys/kernel/random/uuid
    return 0
  fi
  fail "无法生成 UUID，请安装 uuidgen"
}

generate_secret() {
  od -An -N16 -tx1 /dev/urandom | tr -d ' \n'
}

detect_primary_ip() {
  local ip
  if command -v ip >/dev/null 2>&1; then
    ip="$(ip route get 1.1.1.1 2>/dev/null | awk '/src/ { for (i = 1; i <= NF; i++) if ($i == "src") { print $(i + 1); exit } }')"
    if [ -n "${ip}" ]; then
      printf '%s' "${ip}"
      return 0
    fi
  fi
  if command -v hostname >/dev/null 2>&1; then
    ip="$(hostname -I 2>/dev/null | awk '{ print $1 }')"
    if [ -n "${ip}" ]; then
      printf '%s' "${ip}"
      return 0
    fi
  fi
  printf '%s' "127.0.0.1"
}

discover_ipv4_interfaces() {
  require_cmd ip
  ip -o -4 addr show up scope global 2>/dev/null \
    | awk '!seen[$2]++ { split($4, cidr, "/"); print $2 "|" cidr[1] }'
}

detect_primary_interface_entry() {
  if ! command -v ip >/dev/null 2>&1; then
    return 0
  fi
  ip route get 1.1.1.1 2>/dev/null \
    | awk '{
        for (i = 1; i <= NF; i++) {
          if ($i == "dev") dev = $(i + 1)
          if ($i == "src") src = $(i + 1)
        }
      }
      END {
        if (dev != "" && src != "") {
          print dev "|" src
        }
      }'
}

print_interface_options() {
  local entry
  local index=1
  for entry in "$@"; do
    printf '  %d) %s (%s)\n' \
      "${index}" \
      "${entry%%|*}" \
      "${entry#*|}" >&2
    index=$((index + 1))
  done
}

resolve_interface_choice() {
  local choice="$1"
  shift
  local entries=("$@")
  local entry
  local index=1

  if [[ "${choice}" =~ ^[0-9]+$ ]]; then
    for entry in "${entries[@]}"; do
      if [ "${index}" -eq "${choice}" ]; then
        printf '%s' "${entry}"
        return 0
      fi
      index=$((index + 1))
    done
    return 1
  fi

  for entry in "${entries[@]}"; do
    if [ "${entry%%|*}" = "${choice}" ]; then
      printf '%s' "${entry}"
      return 0
    fi
  done
  return 1
}

prompt_interface_selection() {
  local label="$1"
  local default_name="$2"
  local default_ip="$3"
  shift 3
  local entries=("$@")
  local answer
  local selected

  while true; do
    printf '%s 可用网卡（输入编号或网卡名）:\n' "${label}" >&2
    print_interface_options "${entries[@]}"
    answer="$(prompt "${label}" "${default_name}")"
    if selected="$(resolve_interface_choice "${answer}" "${entries[@]}")"; then
      printf '%s' "${selected}"
      return 0
    fi
    printf '无效选择，请输入上面的编号或网卡名。默认推荐 %s (%s)\n' "${default_name}" "${default_ip}" >&2
  done
}

configure_host_interface_defaults() {
  local primary_entry
  local multicast_entry
  local default_entry
  local default_name
  local default_ip
  local entry
  local found_default=0
  local entries=()

  mapfile -t entries < <(discover_ipv4_interfaces)
  [ "${#entries[@]}" -gt 0 ] || fail "未检测到可用的 IPv4 网卡，无法为 host 工作节点生成绑定配置"

  default_entry="$(detect_primary_interface_entry)"
  if [ -z "${default_entry}" ]; then
    default_entry="${entries[0]}"
  else
    default_name="${default_entry%%|*}"
    for entry in "${entries[@]}"; do
      if [ "${entry%%|*}" = "${default_name}" ]; then
        default_entry="${entry}"
        found_default=1
        break
      fi
    done
    if [ "${found_default}" -ne 1 ]; then
      default_entry="${entries[0]}"
    fi
  fi
  default_name="${default_entry%%|*}"
  default_ip="${default_entry#*|}"

  echo "host 工作节点需要分别选择主网卡和组播网卡。" >&2
  echo "建议：普通流量走主网卡；真实组播收发优先使用独立组播网卡，没有时可与主网卡相同。" >&2

  primary_entry="$(prompt_interface_selection "主网卡" "${default_name}" "${default_ip}" "${entries[@]}")"
  PRIMARY_INTERFACE_NAME="${primary_entry%%|*}"
  PRIMARY_INTERFACE_IP="${primary_entry#*|}"

  multicast_entry="$(prompt_interface_selection "组播网卡" "${PRIMARY_INTERFACE_NAME}" "${PRIMARY_INTERFACE_IP}" "${entries[@]}")"
  MULTICAST_INTERFACE_NAME="${multicast_entry%%|*}"
  MULTICAST_INTERFACE_IP="${multicast_entry#*|}"
}

escape_sed_replacement() {
  printf '%s' "$1" | sed 's/[\\/&|]/\\&/g'
}

archive_for_image_key() {
  case "$1" in
    postgres) printf '%s' "${POSTGRES_IMAGE_ARCHIVE}" ;;
    media-core) printf '%s' "${MEDIA_CORE_IMAGE_ARCHIVE}" ;;
    media-agent) printf '%s' "${MEDIA_AGENT_IMAGE_ARCHIVE}" ;;
    media-agent-gpu) printf '%s' "${MEDIA_AGENT_GPU_IMAGE_ARCHIVE}" ;;
    zlmediakit) printf '%s' "${ZLM_IMAGE_ARCHIVE}" ;;
    *) fail "未知镜像标识: $1" ;;
  esac
}

binary_rel_for_key() {
  case "$1" in
    media-core) printf '%s' "${MEDIA_CORE_BINARY_PATH:-}" ;;
    media-agent) printf '%s' "${MEDIA_AGENT_BINARY_PATH:-}" ;;
    streamserver-config) printf '%s' "${STREAMSERVER_CONFIG_BINARY_PATH:-}" ;;
    *) fail "未知二进制标识: $1" ;;
  esac
}

ui_rel_for_key() {
  case "$1" in
    media-core) printf '%s' "${MEDIA_CORE_UI_PATH:-}" ;;
    *) fail "未知前端静态资源标识: $1" ;;
  esac
}

load_image_archive() {
  local archive_rel="$1"
  local archive_path="${PACKAGE_ROOT}/${archive_rel}"
  [ -f "${archive_path}" ] || fail "缺少镜像归档 ${archive_rel}"
  log "加载镜像 ${archive_rel}"
  docker load -i "${archive_path}" >/dev/null
}

ensure_images_loaded() {
  local key
  for key in "$@"; do
    load_image_archive "$(archive_for_image_key "${key}")"
  done
}

install_host_binary() {
  local install_dir="$1"
  local binary_key="$2"
  local binary_rel
  local source_path
  local target_path
  local temp_path

  binary_rel="$(binary_rel_for_key "${binary_key}")"
  [ -n "${binary_rel}" ] || fail "离线包未声明 ${binary_key} 二进制路径"

  source_path="${PACKAGE_ROOT}/${binary_rel}"
  [ -f "${source_path}" ] || fail "缺少宿主机挂载二进制 ${binary_rel}"

  mkdir -p "${install_dir}/bin"
  target_path="${install_dir}/bin/${binary_key}"
  temp_path="$(mktemp "${install_dir}/bin/.${binary_key}.XXXXXX")"
  cp "${source_path}" "${temp_path}"
  chmod 755 "${temp_path}"
  # Atomically replace the target so upgrades can swap binaries even while the
  # previous inode is still being executed inside a running container.
  mv -f "${temp_path}" "${target_path}"
  log "已写入宿主机挂载二进制: ${target_path}"
}

install_host_binaries() {
  local install_dir="$1"
  shift
  local binary_key

  for binary_key in "$@"; do
    install_host_binary "${install_dir}" "${binary_key}"
  done
}

install_host_ui() {
  local install_dir="$1"
  local ui_key="$2"
  local ui_rel
  local source_path
  local target_path

  ui_rel="$(ui_rel_for_key "${ui_key}")"
  [ -n "${ui_rel}" ] || fail "离线包未声明 ${ui_key} 前端静态资源路径"

  source_path="${PACKAGE_ROOT}/${ui_rel}"
  [ -d "${source_path}" ] || fail "缺少宿主机挂载前端静态资源 ${ui_rel}"

  target_path="${install_dir}/ui"
  rm -rf "${target_path}"
  mkdir -p "${target_path}"
  cp -R "${source_path}/." "${target_path}/"
  log "已写入宿主机挂载前端静态资源: ${target_path}"
}

run_streamserver_config_tui_if_requested() {
  local install_dir="$1"
  local config_bin="${install_dir}/bin/streamserver-config"
  local env_file="${install_dir}/.env"

  [ -t 0 ] || return 0
  [ -x "${config_bin}" ] || return 0
  log "配置界面可复核/调整端口、网卡、鉴权、节点标签、最大同时任务数和存储宿主机路径。"
  log "如需把录制产物目录挂载到 NAS/NFS 等网络存储，请在这一步进入配置并确认挂载目录。"
  if prompt_yes_no "是否现在打开配置界面？" "Y"; then
    "${config_bin}" --env "${env_file}" --no-restart-prompt
  else
    "${config_bin}" --env "${env_file}" --non-interactive --no-restart-prompt
  fi
}

write_compose_wrapper_script() {
  local install_dir="$1"
  local wrapper_path="${install_dir}/bin/streamserver-compose"

  mkdir -p "${install_dir}/bin"
  cat >"${wrapper_path}" <<EOF
#!/usr/bin/env bash
set -euo pipefail

INSTALL_DIR="\$(cd "\$(dirname "\$0")/.." && pwd)"
cd "\${INSTALL_DIR}"

if docker compose version >/dev/null 2>&1; then
  exec docker compose -f "${COMPOSE_FILE_NAME}" "\$@"
fi

exec docker-compose -f "${COMPOSE_FILE_NAME}" "\$@"
EOF
  chmod 755 "${wrapper_path}"
}

install_compose_autostart_service() {
  local install_dir="$1"
  local wrapper_path
  local unit_path
  local unit_tmp
  local should_use_sudo="false"

  STACK_SYSTEMD_UNIT_NAME=""
  SYSTEMD_SERVICE_USE_SUDO="false"
  write_compose_wrapper_script "${install_dir}"

  if ! systemd_available; then
    log "当前主机未检测到 systemd，跳过开机自启动服务安装。"
    return 0
  fi

  if ! current_user_is_root; then
    if ! command -v sudo >/dev/null 2>&1; then
      log "检测到 systemd，但当前用户不是 root 且缺少 sudo，跳过开机自启动服务安装。"
      return 0
    fi

    if ! prompt_yes_no "检测到 systemd，是否使用 sudo 安装并启用开机自启动服务？" "N"; then
      log "用户选择不安装 systemd 服务，将按无 systemd 模式继续。"
      return 0
    fi

    sudo -v || fail "sudo 验证失败，无法安装 systemd 服务"
    should_use_sudo="true"
  fi

  wrapper_path="${install_dir}/bin/streamserver-compose"
  STACK_SYSTEMD_UNIT_NAME="$(compose_systemd_unit_name)"
  unit_path="/etc/systemd/system/${STACK_SYSTEMD_UNIT_NAME}"
  require_cmd mktemp
  require_cmd install
  unit_tmp="$(mktemp "/tmp/${STACK_SYSTEMD_UNIT_NAME}.XXXXXX")"

  cat >"${unit_tmp}" <<EOF
[Unit]
Description=StreamServer Compose Stack (${PROJECT_NAME})
Requires=docker.service
Wants=network-online.target
After=docker.service network-online.target
RequiresMountsFor=${install_dir}

[Service]
Type=oneshot
RemainAfterExit=yes
WorkingDirectory=${install_dir}
ExecStartPre=/bin/sh -c 'until docker info >/dev/null 2>&1; do sleep 1; done'
ExecStart=${wrapper_path} up -d
ExecStop=-${wrapper_path} down
ExecReload=${wrapper_path} up -d
TimeoutStartSec=0
TimeoutStopSec=300

[Install]
WantedBy=multi-user.target
EOF

  if [ "${should_use_sudo}" = "true" ]; then
    sudo install -m 644 "${unit_tmp}" "${unit_path}"
    SYSTEMD_SERVICE_USE_SUDO="true"
  else
    install -m 644 "${unit_tmp}" "${unit_path}"
  fi
  rm -f "${unit_tmp}"

  run_systemctl daemon-reload
  run_systemctl enable "${STACK_SYSTEMD_UNIT_NAME}" >/dev/null
  log "已安装并启用开机自启动服务: ${STACK_SYSTEMD_UNIT_NAME}"
}

ensure_nvidia_runtime_ready() {
  require_cmd nvidia-smi
  nvidia-smi >/dev/null 2>&1 || fail "NVIDIA 驱动不可用，请先确认宿主机可以正常执行 nvidia-smi"
  docker info --format '{{json .Runtimes}}' 2>/dev/null | grep -q '"nvidia"' \
    || fail "Docker 未检测到 nvidia runtime，请先安装并配置 nvidia-container-toolkit"
}

reset_install_context() {
  IS_UPGRADE="false"
  EXISTING_INSTALL_ROLE=""
  EXISTING_PROJECT_NAME=""
  INSTALL_BACKUP_DIR=""
  PRESERVED_ENV_SOURCE=""
  RESERVED_LOCAL_TCP_PORTS=""
}

is_managed_install_dir() {
  local install_dir="$1"
  [ -f "${install_dir}/.env" ] && [ -f "${install_dir}/${COMPOSE_FILE_NAME}" ]
}

env_key_exists() {
  local env_file="$1"
  local key="$2"
  existing_env_value "${env_file}" "${key}" >/dev/null 2>&1
}

infer_install_role_from_env() {
  local env_file="$1"
  local explicit_role=""
  local acceleration_mode="cpu"

  explicit_role="$(existing_env_value "${env_file}" "INSTALL_ROLE" 2>/dev/null || true)"
  case "${explicit_role}" in
    control-plane|worker-host-cpu|worker-host-gpu|all-in-one-host-cpu|all-in-one-host-gpu)
      printf '%s' "${explicit_role}"
      return 0
      ;;
  esac

  acceleration_mode="$(env_value_or_default "${env_file}" "AGENT_ACCELERATION_MODE" "cpu")"

  if env_key_exists "${env_file}" "POSTGRES_IMAGE" \
    && env_key_exists "${env_file}" "MEDIA_CORE_IMAGE" \
    && env_key_exists "${env_file}" "MEDIA_AGENT_IMAGE" \
    && env_key_exists "${env_file}" "ZLM_IMAGE"; then
    if [ "${acceleration_mode}" = "gpu" ]; then
      printf '%s' "all-in-one-host-gpu"
    else
      printf '%s' "all-in-one-host-cpu"
    fi
    return 0
  fi

  if env_key_exists "${env_file}" "POSTGRES_IMAGE" \
    && env_key_exists "${env_file}" "MEDIA_CORE_IMAGE" \
    && ! env_key_exists "${env_file}" "MEDIA_AGENT_IMAGE"; then
    printf '%s' "control-plane"
    return 0
  fi

  if env_key_exists "${env_file}" "MEDIA_AGENT_IMAGE" \
    && env_key_exists "${env_file}" "ZLM_IMAGE" \
    && ! env_key_exists "${env_file}" "POSTGRES_IMAGE"; then
    if [ "${acceleration_mode}" = "gpu" ]; then
      printf '%s' "worker-host-gpu"
    else
      printf '%s' "worker-host-cpu"
    fi
    return 0
  fi

  fail "无法根据 ${env_file} 推断已有部署角色，请检查旧 .env 是否完整"
}

prepare_existing_install_context() {
  local install_dir="$1"
  local expected_role="$2"
  local env_file="${install_dir}/.env"

  reset_install_context
  if ! is_managed_install_dir "${install_dir}"; then
    return 0
  fi

  IS_UPGRADE="true"
  EXISTING_INSTALL_ROLE="$(infer_install_role_from_env "${env_file}")"
  [ "${EXISTING_INSTALL_ROLE}" = "${expected_role}" ] \
    || fail "检测到已有部署角色为 ${EXISTING_INSTALL_ROLE}，与当前选择的 ${expected_role} 不一致。原地升级只允许同角色升级。"

  EXISTING_PROJECT_NAME="$(existing_env_value "${env_file}" "COMPOSE_PROJECT_NAME" 2>/dev/null || true)"
  [ -n "${EXISTING_PROJECT_NAME}" ] || fail "已有部署缺少 COMPOSE_PROJECT_NAME，无法进入原地升级"
  log "检测到已有 ${EXISTING_INSTALL_ROLE} 部署，将进入原地升级模式。"
}

require_upgrade_project_name_unchanged() {
  [ "${IS_UPGRADE}" = "true" ] || return 0
  [ "${PROJECT_NAME}" = "${EXISTING_PROJECT_NAME}" ] \
    || fail "原地升级不允许修改 COMPOSE_PROJECT_NAME。已有值为 ${EXISTING_PROJECT_NAME}，请保持不变，或改用新的安装目录做新部署。"
}

next_backup_dir() {
  local install_dir="${1%/}"
  local timestamp
  local backup_dir
  local suffix=1

  timestamp="$(date '+%Y%m%d-%H%M%S')"
  backup_dir="${install_dir}.backup-${timestamp}"
  while [ -e "${backup_dir}" ]; do
    backup_dir="${install_dir}.backup-${timestamp}-${suffix}"
    suffix=$((suffix + 1))
  done
  printf '%s' "${backup_dir}"
}

backup_install_item() {
  local install_dir="$1"
  local backup_dir="$2"
  local relative_path="$3"
  local source_path="${install_dir}/${relative_path}"

  [ -e "${source_path}" ] || return 0
  mkdir -p "${backup_dir}/$(dirname "${relative_path}")"
  cp -R "${source_path}" "${backup_dir}/${relative_path}"
}

create_upgrade_backup() {
  local install_dir="$1"

  [ "${IS_UPGRADE}" = "true" ] || return 0
  INSTALL_BACKUP_DIR="$(next_backup_dir "${install_dir}")"
  mkdir -p "${INSTALL_BACKUP_DIR}"
  backup_install_item "${install_dir}" "${INSTALL_BACKUP_DIR}" ".env"
  backup_install_item "${install_dir}" "${INSTALL_BACKUP_DIR}" "${COMPOSE_FILE_NAME}"
  backup_install_item "${install_dir}" "${INSTALL_BACKUP_DIR}" "docker-compose.yml"
  backup_install_item "${install_dir}" "${INSTALL_BACKUP_DIR}" "bin"
  backup_install_item "${install_dir}" "${INSTALL_BACKUP_DIR}" "ui"
  backup_install_item "${install_dir}" "${INSTALL_BACKUP_DIR}" "zlm"
  backup_install_item "${install_dir}" "${INSTALL_BACKUP_DIR}" "docs"
  PRESERVED_ENV_SOURCE="${INSTALL_BACKUP_DIR}/.env"
  log "已创建升级备份: ${INSTALL_BACKUP_DIR}"
}

managed_env_keys() {
  cat <<'EOF'
INSTALL_ROLE
COMPOSE_PROJECT_NAME
POSTGRES_IMAGE
MEDIA_CORE_IMAGE
MEDIA_AGENT_IMAGE
ZLM_IMAGE
POSTGRES_DB
POSTGRES_USER
POSTGRES_PASSWORD
POSTGRES_PORT
CORE_HTTP_HOST
CORE_HTTP_PORT
CORE_GRPC_HOST
CORE_GRPC_PORT
HOOK_SHARED_SECRET
HOOK_SOURCE_ALLOWLIST
STORAGE_ALLOWLIST
AUTH_MODE
AUTH_ENABLED
JWT_PUBLIC_KEY
AUTH_JWT_PRIVATE_KEY_PATH
AUTH_JWT_PUBLIC_KEY_PATH
AUTH_ACCESS_TOKEN_TTL
AUTH_REFRESH_TOKEN_TTL
NODE_ID
AGENT_NODE_NAME
PUBLIC_HOST
ZLM_API_HOST
AGENT_HTTP_PORT
ZLM_HTTP_PORT
ZLM_HTTPS_PORT
ZLM_RTMP_PORT
ZLM_RTMPS_PORT
ZLM_RTSP_PORT
ZLM_RTSPS_PORT
ZLM_RTP_PROXY_PORT
ZLM_RTP_PROXY_PORT_RANGE
ZLM_RTC_SIGNALING_PORT
ZLM_RTC_SIGNALING_SSL_PORT
ZLM_RTC_ICE_PORT
ZLM_RTC_ICE_TCP_PORT
ZLM_RTC_PORT
ZLM_RTC_TCP_PORT
ZLM_RTC_PORT_RANGE
ZLM_SRT_PORT
ZLM_SHELL_PORT
ZLM_ONVIF_PORT
AGENT_PRIMARY_INTERFACE_NAME
AGENT_PRIMARY_INTERFACE_IP
OUTPUT_MOUNT_RELATIVE_PREFIX_MP4
OUTPUT_MOUNT_RELATIVE_PREFIX_HLS
ZLM_WWW_MOUNT_HOST_DIR
ZLM_OUTPUT_MOUNT_HOST_DIR
ZLM_WWW_HOST_DIR
ZLM_OUTPUT_HOST_DIR
AGENT_MULTICAST_INTERFACE_NAME
AGENT_MULTICAST_INTERFACE_IP
AGENT_NETWORK_MODE
AGENT_ACCELERATION_MODE
AGENT_LABELS
AGENT_MAX_RUNTIME_SLOTS
AGENT_HLS_RECORD_SEGMENT_SEC
AGENT_ARTIFACT_CLEANUP_ENABLED
AGENT_ARTIFACT_CLEANUP_THRESHOLD_PERCENT
AGENT_ARTIFACT_CLEANUP_STRATEGY
AGENT_ARTIFACT_CLEANUP_CHECK_INTERVAL_SEC
WORK_ROOT
UPLOAD_MAX_BYTES
UPLOAD_ALLOWED_EXTENSIONS
UPLOAD_PROBE_TIMEOUT_SEC
PUBLIC_MEDIA_BASE_URL
EOF
}

is_managed_env_key() {
  local key="$1"
  case "${key}" in
    AGENT_ARTIFACT_CLEANUP_PRE*ALLOCATE_PERCENT|AGENT_ARTIFACT_CLEANUP_PRE*ALLOCATE_HEADROOM_PERCENT)
      return 0
      ;;
  esac
  managed_env_keys | grep -Fxq -- "${key}"
}

append_preserved_custom_env_entries() {
  local env_file="$1"
  local source_env_file="$2"
  local line=""
  local trimmed_line=""
  local key=""
  local appended=0

  [ "${IS_UPGRADE}" = "true" ] || return 0
  [ -f "${source_env_file}" ] || return 0

  while IFS= read -r line || [ -n "${line}" ]; do
    trimmed_line="$(printf '%s' "${line}" | sed 's/^[[:space:]]*//')"
    case "${trimmed_line}" in
      ''|'#'*) continue ;;
      *=*) ;;
      *) continue ;;
    esac

    key="${trimmed_line%%=*}"
    [ -n "${key}" ] || continue
    if is_managed_env_key "${key}"; then
      continue
    fi

    if [ "${appended}" -eq 0 ]; then
      write_env_blank_line "${env_file}"
      printf '# 以下为升级时从旧 .env 保留的自定义键。\n' >>"${env_file}"
    fi
    printf '%s\n' "${trimmed_line}" >>"${env_file}"
    appended=$((appended + 1))
  done < "${source_env_file}"
}

ensure_runtime_support() {
  local install_dir="$1"
  install_compose_autostart_service "${install_dir}"
  mkdir -p "${install_dir}/certs/custom"
}

log_runtime_commands() {
  local install_dir="$1"
  if [ -n "${STACK_SYSTEMD_UNIT_NAME}" ]; then
    log "  $(systemctl_cmd_display) status ${STACK_SYSTEMD_UNIT_NAME}"
    log "  $(journalctl_cmd_display) -u ${STACK_SYSTEMD_UNIT_NAME} -f"
  fi
  log "  ${install_dir}/bin/streamserver-compose ps"
  log "  ${install_dir}/bin/streamserver-compose logs -f"
}

install_role_has_agent() {
  local install_role="$1"
  case "${install_role}" in
    worker-host-cpu|worker-host-gpu|all-in-one-host-cpu|all-in-one-host-gpu) return 0 ;;
    *) return 1 ;;
  esac
}

log_output_storage_notice() {
  local install_role="$1"

  install_role_has_agent "${install_role}" || return 0

  log "存储布局提示:"
  log "  - 在线播放宿主机路径: ${ZLM_WWW_MOUNT_HOST_DIR:-${ZLM_WWW_HOST_DIR:-./data/zlm/www}}，建议本机磁盘。"
  log "  - 录制产物宿主机路径: ${ZLM_OUTPUT_MOUNT_HOST_DIR:-${ZLM_OUTPUT_HOST_DIR:-./data/zlm/www/output}}，可挂载网络存储。"
  log "  - 安装脚本不会自动执行 mount；如需网络存储，请先挂载到录制产物宿主机路径后再启动服务。"
}

restart_existing_stack() {
  local install_dir="$1"

  if [ -n "${STACK_SYSTEMD_UNIT_NAME}" ]; then
    run_systemctl restart "${STACK_SYSTEMD_UNIT_NAME}"
    return 0
  fi

  (
    cd "${install_dir}"
    compose_with_file down
    compose_with_file up -d
  )
}

report_upgrade_service_failure() {
  local install_dir="$1"
  local service_name="$2"
  local reason="$3"

  log "升级后服务 ${service_name} 未通过校验: ${reason}"
  log "请先检查以下命令输出："
  log "  ${install_dir}/bin/streamserver-compose ps"
  log "  ${install_dir}/bin/streamserver-compose logs --tail=200 ${service_name}"
  if [ -n "${INSTALL_BACKUP_DIR}" ]; then
    log "本次升级备份目录: ${INSTALL_BACKUP_DIR}"
  fi
  fail "升级后服务校验失败"
}

wait_for_service_ready_after_upgrade() {
  local install_dir="$1"
  local service_name="$2"
  local timeout_seconds="${3:-120}"
  local container_id=""
  local state=""
  local waited=0

  while [ "${waited}" -lt "${timeout_seconds}" ]; do
    container_id="$(
      cd "${install_dir}" &&
      compose_with_file ps -q "${service_name}" 2>/dev/null | head -n 1
    )"
    if [ -n "${container_id}" ]; then
      state="$(
        docker inspect --format '{{if .State.Health}}{{.State.Health.Status}}{{else}}{{.State.Status}}{{end}}' "${container_id}" 2>/dev/null || true
      )"
      case "${state}" in
        healthy|running)
          return 0
          ;;
        unhealthy|exited|dead)
          report_upgrade_service_failure "${install_dir}" "${service_name}" "当前状态为 ${state}"
          ;;
      esac
    fi
    sleep 2
    waited=$((waited + 2))
  done

  report_upgrade_service_failure "${install_dir}" "${service_name}" "等待 ${timeout_seconds} 秒后仍未就绪"
}

validate_upgraded_stack() {
  local install_dir="$1"
  local install_role="$2"
  local service_names=()
  local service_name

  case "${install_role}" in
    control-plane)
      service_names=(postgres media-core)
      ;;
    worker-host-cpu|worker-host-gpu)
      service_names=(zlmediakit media-agent)
      ;;
    all-in-one-host-cpu|all-in-one-host-gpu)
      service_names=(postgres media-core zlmediakit media-agent)
      ;;
    *)
      fail "未知升级角色 ${install_role}"
      ;;
  esac

  for service_name in "${service_names[@]}"; do
    wait_for_service_ready_after_upgrade "${install_dir}" "${service_name}" 120
  done
}

finalize_deployment() {
  local install_dir="$1"
  local install_role="$2"
  local grpc_host_hint="${3:-<control-plane-host>}"
  local grpc_port_hint="${4:-50051}"

  if [ "${IS_UPGRADE}" = "true" ]; then
    ensure_runtime_support "${install_dir}"
    restart_existing_stack "${install_dir}"
    validate_upgraded_stack "${install_dir}" "${install_role}"
    log "原地升级已完成。"
    if [ -n "${INSTALL_BACKUP_DIR}" ]; then
      log "升级备份目录: ${INSTALL_BACKUP_DIR}"
    fi
    log "常用命令:"
    log_runtime_commands "${install_dir}"
    log_output_storage_notice "${install_role}"
    return 0
  fi

  show_tls_notice "${install_dir}" "${grpc_host_hint}" "${grpc_port_hint}" || return 0
  start_stack_if_requested "${install_dir}" "${install_role}"
}

prepare_install_dir() {
  local install_dir="$1"
  if [ "${IS_UPGRADE}" = "true" ]; then
    prompt_yes_no "检测到目录 ${install_dir} 中已有受管部署，将执行原地升级、创建备份并自动重启，是否继续？" "N" \
      || fail "用户取消升级"
  elif [ -e "${install_dir}" ] && [ -n "$(find "${install_dir}" -mindepth 1 -maxdepth 1 2>/dev/null | head -n 1)" ]; then
    prompt_yes_no "目录 ${install_dir} 已存在且非空，是否继续覆盖模板文件？" "N" || fail "用户取消安装"
  fi
  mkdir -p "${install_dir}"
  mkdir -p "${install_dir}/docs"
}

prepare_install_target() {
  local install_dir="$1"
  prepare_install_dir "${install_dir}"
  create_upgrade_backup "${install_dir}"
}

configure_auth_defaults() {
  AUTH_MODE="disabled"
  AUTH_ENABLED="false"
  JWT_PUBLIC_KEY=""
  AUTH_JWT_PRIVATE_KEY_PATH=""
  AUTH_JWT_PUBLIC_KEY_PATH=""
  AUTH_ACCESS_TOKEN_TTL="15m"
  AUTH_REFRESH_TOKEN_TTL="7d"
  AUTH_BOOTSTRAP_ADMIN_USERNAME=""
  AUTH_BOOTSTRAP_ADMIN_PASSWORD=""
}

prompt_local_auth_configuration() {
  local existing_env_file="${1:-}"
  local default_answer="N"
  local existing_enabled=0

  configure_auth_defaults
  if [ -n "${existing_env_file}" ] && [ -f "${existing_env_file}" ]; then
    AUTH_MODE="$(env_value_or_default "${existing_env_file}" "AUTH_MODE" "${AUTH_MODE}")"
    AUTH_ENABLED="$(env_value_or_default "${existing_env_file}" "AUTH_ENABLED" "${AUTH_ENABLED}")"
    JWT_PUBLIC_KEY="$(env_value_or_default "${existing_env_file}" "JWT_PUBLIC_KEY" "${JWT_PUBLIC_KEY}")"
    AUTH_JWT_PRIVATE_KEY_PATH="$(env_value_or_default "${existing_env_file}" "AUTH_JWT_PRIVATE_KEY_PATH" "${AUTH_JWT_PRIVATE_KEY_PATH}")"
    AUTH_JWT_PUBLIC_KEY_PATH="$(env_value_or_default "${existing_env_file}" "AUTH_JWT_PUBLIC_KEY_PATH" "${AUTH_JWT_PUBLIC_KEY_PATH}")"
    AUTH_ACCESS_TOKEN_TTL="$(env_value_or_default "${existing_env_file}" "AUTH_ACCESS_TOKEN_TTL" "${AUTH_ACCESS_TOKEN_TTL}")"
    AUTH_REFRESH_TOKEN_TTL="$(env_value_or_default "${existing_env_file}" "AUTH_REFRESH_TOKEN_TTL" "${AUTH_REFRESH_TOKEN_TTL}")"
  fi
  if [ "${AUTH_MODE}" = "local_password" ] || [ "${AUTH_ENABLED}" = "true" ]; then
    default_answer="Y"
    existing_enabled=1
  fi
  if ! prompt_yes_no "是否启用控制面板内建用户名密码鉴权？" "${default_answer}"; then
    configure_auth_defaults
    return 0
  fi

  AUTH_MODE="local_password"
  AUTH_ENABLED="true"
  if [ "${existing_enabled}" -eq 1 ]; then
    return 0
  fi

  AUTH_BOOTSTRAP_ADMIN_USERNAME="$(prompt_non_empty "管理员用户名" "admin")"
  AUTH_BOOTSTRAP_ADMIN_PASSWORD="$(prompt_password_with_confirmation "管理员密码")"
}

prepare_local_auth_assets() {
  local install_dir="$1"
  local auth_dir
  local private_key_host_path
  local public_key_host_path

  [ "${AUTH_MODE}" = "local_password" ] || return 0

  require_cmd openssl
  auth_dir="${install_dir}/certs/auth"
  if [ -n "${AUTH_JWT_PRIVATE_KEY_PATH}" ] && [ -n "${AUTH_JWT_PUBLIC_KEY_PATH}" ]; then
    private_key_host_path="${install_dir}${AUTH_JWT_PRIVATE_KEY_PATH}"
    public_key_host_path="${install_dir}${AUTH_JWT_PUBLIC_KEY_PATH}"
    if [ -f "${private_key_host_path}" ] && [ -f "${public_key_host_path}" ]; then
      return 0
    fi
  fi

  private_key_host_path="${auth_dir}/jwt-ed25519-private.pem"
  public_key_host_path="${auth_dir}/jwt-ed25519-public.pem"

  mkdir -p "${auth_dir}"
  openssl genpkey -algorithm Ed25519 -out "${private_key_host_path}" >/dev/null 2>&1
  openssl pkey -in "${private_key_host_path}" -pubout -out "${public_key_host_path}" >/dev/null 2>&1
  chmod 600 "${private_key_host_path}"
  chmod 644 "${public_key_host_path}"

  AUTH_JWT_PRIVATE_KEY_PATH="/certs/auth/$(basename "${private_key_host_path}")"
  AUTH_JWT_PUBLIC_KEY_PATH="/certs/auth/$(basename "${public_key_host_path}")"
  JWT_PUBLIC_KEY=""
}

set_env_file_value() {
  local env_file="$1"
  local key="$2"
  local value="$3"
  local temp_path

  temp_path="$(mktemp "${env_file}.XXXXXX")"
  awk -v key="${key}" -v value="${value}" '
    BEGIN { written = 0 }
    /^[[:space:]]*#/ { print; next }
    index($0, "=") > 0 {
      current_key = substr($0, 1, index($0, "=") - 1)
      if (current_key == key) {
        print key "=" value
        written = 1
        next
      }
    }
    { print }
    END {
      if (!written) {
        print key "=" value
      }
    }
  ' "${env_file}" >"${temp_path}"
  mv -f "${temp_path}" "${env_file}"
}

sync_local_auth_env_entries() {
  local env_file="$1"

  set_env_file_value "${env_file}" "AUTH_MODE" "${AUTH_MODE}"
  set_env_file_value "${env_file}" "AUTH_ENABLED" "${AUTH_ENABLED}"
  set_env_file_value "${env_file}" "JWT_PUBLIC_KEY" "${JWT_PUBLIC_KEY}"
  set_env_file_value "${env_file}" "AUTH_JWT_PRIVATE_KEY_PATH" "${AUTH_JWT_PRIVATE_KEY_PATH}"
  set_env_file_value "${env_file}" "AUTH_JWT_PUBLIC_KEY_PATH" "${AUTH_JWT_PUBLIC_KEY_PATH}"
  set_env_file_value "${env_file}" "AUTH_ACCESS_TOKEN_TTL" "${AUTH_ACCESS_TOKEN_TTL}"
  set_env_file_value "${env_file}" "AUTH_REFRESH_TOKEN_TTL" "${AUTH_REFRESH_TOKEN_TTL}"
}

ensure_local_auth_after_config_tui() {
  local install_dir="$1"
  local env_file="${install_dir}/.env"

  [ "${AUTH_MODE}" = "local_password" ] || [ "${AUTH_ENABLED}" = "true" ] || return 0

  AUTH_MODE="local_password"
  AUTH_ENABLED="true"
  [ -n "${AUTH_ACCESS_TOKEN_TTL}" ] || AUTH_ACCESS_TOKEN_TTL="15m"
  [ -n "${AUTH_REFRESH_TOKEN_TTL}" ] || AUTH_REFRESH_TOKEN_TTL="7d"
  prepare_local_auth_assets "${install_dir}"
  sync_local_auth_env_entries "${env_file}"

  if [ -z "${AUTH_BOOTSTRAP_ADMIN_USERNAME}" ]; then
    AUTH_BOOTSTRAP_ADMIN_USERNAME="$(prompt_non_empty "管理员用户名" "admin")"
  fi
  if [ -z "${AUTH_BOOTSTRAP_ADMIN_PASSWORD}" ]; then
    AUTH_BOOTSTRAP_ADMIN_PASSWORD="$(prompt_password_with_confirmation "管理员密码")"
  fi
}

wait_for_compose_service_ready() {
  local install_dir="$1"
  local service_name="$2"
  local timeout_seconds="${3:-90}"
  local container_id=""
  local state=""
  local waited=0

  while [ "${waited}" -lt "${timeout_seconds}" ]; do
    container_id="$(
      cd "${install_dir}" &&
      compose_with_file ps -q "${service_name}" 2>/dev/null | head -n 1
    )"
    if [ -n "${container_id}" ]; then
      state="$(
        docker inspect --format '{{if .State.Health}}{{.State.Health.Status}}{{else}}{{.State.Status}}{{end}}' "${container_id}" 2>/dev/null || true
      )"
      case "${state}" in
        healthy|running)
          return 0
          ;;
        unhealthy|exited|dead)
          fail "服务 ${service_name} 启动失败，当前状态为 ${state}"
          ;;
      esac
    fi
    sleep 2
    waited=$((waited + 2))
  done

  fail "等待服务 ${service_name} 就绪超时"
}

bootstrap_local_admin_if_needed() {
  local install_dir="$1"
  local output
  local postgres_container_id=""
  local postgres_state=""
  local postgres_was_running=0

  [ "${AUTH_MODE}" = "local_password" ] || return 0

  postgres_container_id="$(
    cd "${install_dir}" &&
    compose_with_file ps -q postgres 2>/dev/null | head -n 1
  )"
  if [ -n "${postgres_container_id}" ]; then
    postgres_state="$(
      docker inspect --format '{{.State.Status}}' "${postgres_container_id}" 2>/dev/null || true
    )"
    if [ "${postgres_state}" = "running" ]; then
      postgres_was_running=1
    fi
  fi

  log "已启用本地用户名密码鉴权，准备初始化管理员账号 ${AUTH_BOOTSTRAP_ADMIN_USERNAME}"
  (
    cd "${install_dir}"
    compose_with_file up -d postgres >/dev/null
  )
  wait_for_compose_service_ready "${install_dir}" "postgres" 90

  if ! output="$(
    cd "${install_dir}" &&
    printf '%s' "${AUTH_BOOTSTRAP_ADMIN_PASSWORD}" | \
      compose_with_file run --rm --no-deps -T media-core \
        auth bootstrap-admin --username "${AUTH_BOOTSTRAP_ADMIN_USERNAME}" --password-stdin 2>&1
  )"; then
    (
      cd "${install_dir}"
      if [ "${postgres_was_running}" -ne 1 ]; then
        compose_with_file stop postgres >/dev/null 2>&1 || true
      fi
    )
    if printf '%s' "${output}" | grep -q "an enabled admin user already exists"; then
      log "检测到数据库中已存在启用中的管理员账号，跳过 bootstrap-admin。"
      return 0
    fi
    printf '%s\n' "${output}" >&2
    fail "初始化管理员账号失败"
  fi

  log "已完成管理员账号初始化。"
  (
    cd "${install_dir}"
    if [ "${postgres_was_running}" -ne 1 ]; then
      compose_with_file stop postgres >/dev/null 2>&1 || true
    fi
  )
}

existing_env_value() {
  local env_file="$1"
  local key="$2"
  [ -f "${env_file}" ] || return 1
  awk -F= -v key="${key}" '
    $1 == key {
      print substr($0, index($0, "=") + 1)
      found = 1
      exit
    }
    END {
      if (!found) {
        exit 1
      }
    }
  ' "${env_file}"
}

env_value_or_default() {
  local env_file="$1"
  local key="$2"
  local default_value="$3"
  local value
  if value="$(existing_env_value "${env_file}" "${key}")"; then
    printf '%s' "${value}"
  else
    printf '%s' "${default_value}"
  fi
}

load_runtime_env() {
  local env_file="$1"
  [ -f "${env_file}" ] || return 0
  set -a
  # shellcheck disable=SC1090
  . "${env_file}"
  set +a
}

describe_tcp_port_usage_via_proc() {
  local port="$1"
  local port_hex
  local inode_list=""
  local inode
  local pid_dir
  local pid
  local fd
  local link_target
  local seen_pids=" "
  local comm
  local cmdline
  local output=""

  port_hex="$(printf '%04X' "${port}")"
  inode_list="$(
    awk -v port_hex="${port_hex}" '
      FNR == 1 { next }
      $4 == "0A" {
        split($2, local_addr, ":")
        if (toupper(local_addr[2]) == port_hex) {
          print $10
        }
      }
    ' /proc/net/tcp /proc/net/tcp6 2>/dev/null | sort -u
  )"
  [ -n "${inode_list}" ] || return 0

  while IFS= read -r inode; do
    [ -n "${inode}" ] || continue
    for pid_dir in /proc/[0-9]*; do
      [ -d "${pid_dir}" ] || continue
      pid="${pid_dir##*/}"
      case " ${seen_pids} " in
        *" ${pid} "*) continue ;;
      esac
      for fd in "${pid_dir}"/fd/*; do
        [ -e "${fd}" ] || continue
        link_target="$(readlink "${fd}" 2>/dev/null || true)"
        if [ "${link_target}" = "socket:[${inode}]" ]; then
          comm="$(cat "${pid_dir}/comm" 2>/dev/null || true)"
          cmdline="$(tr '\0' ' ' < "${pid_dir}/cmdline" 2>/dev/null | sed 's/[[:space:]]*$//')"
          [ -n "${comm}" ] || comm="unknown"
          if [ -n "${cmdline}" ]; then
            output="${output}pid=${pid} program=${comm} cmd=${cmdline}"$'\n'
          else
            output="${output}pid=${pid} program=${comm}"$'\n'
          fi
          seen_pids="${seen_pids}${pid} "
          break
        fi
      done
    done
  done <<< "${inode_list}"

  if [ -n "${output}" ]; then
    printf '%s' "${output%$'\n'}"
    return 0
  fi

  while IFS= read -r inode; do
    [ -n "${inode}" ] || continue
    output="${output}socket inode=${inode}（已监听，但未能解析进程信息）"$'\n'
  done <<< "${inode_list}"
  printf '%s' "${output%$'\n'}"
}

describe_tcp_port_usage() {
  local port="$1"
  local ss_output=""
  local proc_output=""

  if command -v ss >/dev/null 2>&1; then
    ss_output="$(ss -H -ltnp "sport = :${port}" 2>/dev/null | sed '/^[[:space:]]*$/d' || true)"
    if [ -n "${ss_output}" ]; then
      if printf '%s' "${ss_output}" | grep -q 'users:'; then
        printf '%s' "${ss_output}"
        return 0
      fi
      proc_output="$(describe_tcp_port_usage_via_proc "${port}")"
      if [ -n "${proc_output}" ]; then
        printf '%s' "${proc_output}"
      else
        printf '%s' "${ss_output}"
      fi
      return 0
    fi
  fi

  proc_output="$(describe_tcp_port_usage_via_proc "${port}")"
  [ -n "${proc_output}" ] && printf '%s' "${proc_output}"
}

upgrade_existing_tcp_port_key() {
  local env_file="$1"
  local key="$2"
  [ "${IS_UPGRADE}" = "true" ] || return 1
  env_key_exists "${env_file}" "${key}"
}

session_reserved_tcp_port_usage() {
  local port="$1"
  case " ${RESERVED_LOCAL_TCP_PORTS} " in
    *" ${port} "*) printf '%s' "当前安装流程已为其他组件预留端口 ${port}" ;;
  esac
}

describe_local_tcp_port_conflict() {
  local port="$1"
  local skip_host_check="${2:-false}"
  local usage=""

  if [ "${skip_host_check}" != "true" ]; then
    usage="$(describe_tcp_port_usage "${port}")"
    if [ -n "${usage}" ]; then
      printf '%s' "${usage}"
      return 0
    fi
  fi

  usage="$(session_reserved_tcp_port_usage "${port}")"
  [ -n "${usage}" ] && printf '%s' "${usage}"
}

reserve_local_tcp_port() {
  local port="$1"
  validate_port_number "reserved_tcp_port" "${port}"
  case " ${RESERVED_LOCAL_TCP_PORTS} " in
    *" ${port} "*) return 0 ;;
  esac
  if [ -n "${RESERVED_LOCAL_TCP_PORTS}" ]; then
    RESERVED_LOCAL_TCP_PORTS="${RESERVED_LOCAL_TCP_PORTS} ${port}"
  else
    RESERVED_LOCAL_TCP_PORTS="${port}"
  fi
}

find_next_available_tcp_port() {
  local start_port="$1"
  local skip_host_check="${2:-false}"
  local candidate=$((start_port + 1))

  while [ "${candidate}" -le 65535 ]; do
    if [ -z "$(describe_local_tcp_port_conflict "${candidate}" "${skip_host_check}")" ]; then
      printf '%s' "${candidate}"
      return 0
    fi
    candidate=$((candidate + 1))
  done

  fail "从端口 ${start_port} 开始向后未找到空闲 TCP 端口，请手动清理端口占用后重试"
}

print_tcp_port_usage_details() {
  local usage="$1"
  [ -n "${usage}" ] || return 0
  echo "占用程序信息:" >&2
  printf '%s\n' "${usage}" | sed 's/^/  /' >&2
}

announce_default_local_tcp_port_conflict() {
  local label="$1"
  local default_port="$2"
  local usage="$3"
  local suggested_port="$4"

  printf '%s 默认端口 %s 已被占用。\n' "${label}" "${default_port}" >&2
  print_tcp_port_usage_details "${usage}"
  printf '已临时选中空闲端口 %s 作为当前默认值，请确认或改成其他端口。\n' "${suggested_port}" >&2
}

prompt_available_local_tcp_port() {
  local key="$1"
  local label="$2"
  local original_default="$3"
  local suggested_port="$4"
  local answer
  local usage=""

  while true; do
    answer="$(prompt_non_empty "${label}（原默认端口 ${original_default} 已被占用；当前默认值 ${suggested_port} 是临时选出的空闲端口）" "${suggested_port}")"
    validate_port_number "${key}" "${answer}"
    usage="$(describe_local_tcp_port_conflict "${answer}")"
    if [ -z "${usage}" ]; then
      printf '%s' "${answer}"
      return 0
    fi

    printf '端口 %s 已被占用，不能直接使用。\n' "${answer}" >&2
    print_tcp_port_usage_details "${usage}"
    suggested_port="$(find_next_available_tcp_port "${answer}")"
    printf '已重新临时选中空闲端口 %s 作为当前默认值，请确认或改成其他端口。\n' "${suggested_port}" >&2
  done
}

resolve_default_local_tcp_port() {
  local env_file="$1"
  local key="$2"
  local label="$3"
  local built_in_default="$4"
  local skip_host_check="false"
  local current_value
  local usage=""
  local suggested_port

  # Upgrades only check host-level occupancy for newly introduced TCP port keys.
  if upgrade_existing_tcp_port_key "${env_file}" "${key}"; then
    skip_host_check="true"
  fi

  if current_value="$(existing_env_value "${env_file}" "${key}")"; then
    validate_port_number "${key}" "${current_value}"
    usage="$(describe_local_tcp_port_conflict "${current_value}" "${skip_host_check}")"
    [ -z "${usage}" ] || fail "${label} ${current_value} 与当前安装流程中的其他端口冲突：${usage}"
    printf '%s' "${current_value}"
    return 0
  fi

  validate_port_number "${key}" "${built_in_default}"
  usage="$(describe_local_tcp_port_conflict "${built_in_default}" "${skip_host_check}")"
  if [ -z "${usage}" ]; then
    printf '%s' "${built_in_default}"
    return 0
  fi

  suggested_port="$(find_next_available_tcp_port "${built_in_default}" "${skip_host_check}")"
  announce_default_local_tcp_port_conflict "${label}" "${built_in_default}" "${usage}" "${suggested_port}"
  prompt_available_local_tcp_port "${key}" "${label}" "${built_in_default}" "${suggested_port}"
}

prompt_local_tcp_port() {
  local env_file="$1"
  local key="$2"
  local label="$3"
  local built_in_default="$4"
  local skip_host_check="false"
  local answer
  local usage=""
  local suggested_port

  if upgrade_existing_tcp_port_key "${env_file}" "${key}"; then
    skip_host_check="true"
  fi

  if answer="$(existing_env_value "${env_file}" "${key}")"; then
    answer="$(prompt_non_empty "${label}" "${answer}")"
    validate_port_number "${key}" "${answer}"
    usage="$(describe_local_tcp_port_conflict "${answer}" "${skip_host_check}")"
    [ -z "${usage}" ] || fail "${label} ${answer} 与当前安装流程中的其他端口冲突：${usage}"
    printf '%s' "${answer}"
    return 0
  fi

  validate_port_number "${key}" "${built_in_default}"
  usage="$(describe_local_tcp_port_conflict "${built_in_default}" "${skip_host_check}")"
  if [ -z "${usage}" ]; then
    suggested_port="${built_in_default}"
  else
    suggested_port="$(find_next_available_tcp_port "${built_in_default}" "${skip_host_check}")"
    announce_default_local_tcp_port_conflict "${label}" "${built_in_default}" "${usage}" "${suggested_port}"
  fi

  while true; do
    answer="$(prompt_non_empty "${label}" "${suggested_port}")"
    validate_port_number "${key}" "${answer}"
    usage="$(describe_local_tcp_port_conflict "${answer}" "${skip_host_check}")"
    if [ -z "${usage}" ]; then
      printf '%s' "${answer}"
      return 0
    fi

    printf '端口 %s 已被占用，不能直接使用。\n' "${answer}" >&2
    print_tcp_port_usage_details "${usage}"
    suggested_port="$(find_next_available_tcp_port "${answer}" "${skip_host_check}")"
    printf '已重新临时选中空闲端口 %s 作为当前默认值，请确认或改成其他端口。\n' "${suggested_port}" >&2
  done
}

assign_default_local_tcp_port() {
  local target_var="$1"
  shift
  local selected_port

  selected_port="$(resolve_default_local_tcp_port "$@")"
  reserve_local_tcp_port "${selected_port}"
  printf -v "${target_var}" '%s' "${selected_port}"
}

assign_prompt_local_tcp_port() {
  local target_var="$1"
  shift
  local selected_port

  selected_port="$(prompt_local_tcp_port "$@")"
  reserve_local_tcp_port "${selected_port}"
  printf -v "${target_var}" '%s' "${selected_port}"
}

write_env_reset() {
  : >"$1"
}

write_env_entry() {
  local env_file="$1"
  local comment="$2"
  local key="$3"
  local value="$4"
  printf '# %s\n%s=%s\n' "${comment}" "${key}" "${value}" >>"${env_file}"
}

write_env_blank_line() {
  printf '\n' >>"$1"
}

write_env_example() {
  local env_file="$1"
  local comment="$2"
  local example="$3"
  printf '# %s\n# %s\n' "${comment}" "${example}" >>"${env_file}"
}

validate_port_number() {
  local key="$1"
  local value="$2"
  local allow_zero="${3:-false}"

  case "${value}" in
    ''|*[!0-9]*)
      fail "${key} 必须是 0-65535 之间的整数"
      ;;
  esac

  if [ "${allow_zero}" = "true" ] && [ "${value}" = "0" ]; then
    return 0
  fi

  if [ "${value}" -lt 1 ] || [ "${value}" -gt 65535 ]; then
    fail "${key} 必须是 1-65535 之间的端口"
  fi
}

validate_port_range() {
  local key="$1"
  local value="$2"
  local start_port
  local end_port

  case "${value}" in
    *-*) ;;
    *)
      fail "${key} 必须使用 start-end 格式"
      ;;
  esac

  start_port="${value%%-*}"
  end_port="${value#*-}"
  validate_port_number "${key}" "${start_port}" true
  validate_port_number "${key}" "${end_port}" true
  if [ "${start_port}" -gt "${end_port}" ]; then
    fail "${key} 的起始端口不能大于结束端口"
  fi
}

validate_positive_integer() {
  local key="$1"
  local value="$2"

  case "${value}" in
    ''|*[!0-9]*)
      fail "${key} 必须是大于 0 的整数"
      ;;
  esac

  if [ "${value}" -lt 1 ]; then
    fail "${key} 必须是大于 0 的整数"
  fi
}

validate_percent_value() {
  local key="$1"
  local value="$2"

  if ! awk -v value="${value}" 'BEGIN {
    if (value !~ /^([0-9]+)(\.[0-9]+)?$/) {
      exit 1
    }
    numeric = value + 0
    if (numeric < 0 || numeric > 100) {
      exit 1
    }
  }'; then
    fail "${key} 必须是 0-100 之间的数字"
  fi
}

validate_artifact_cleanup_config() {
  case "${AGENT_ARTIFACT_CLEANUP_STRATEGY}" in
    delete_oldest_then_reject|reject_only) ;;
    *)
      fail "AGENT_ARTIFACT_CLEANUP_STRATEGY 必须是 delete_oldest_then_reject/reject_only 之一"
      ;;
  esac

  validate_percent_value "AGENT_ARTIFACT_CLEANUP_THRESHOLD_PERCENT" "${AGENT_ARTIFACT_CLEANUP_THRESHOLD_PERCENT}"
  validate_positive_integer "AGENT_ARTIFACT_CLEANUP_CHECK_INTERVAL_SEC" "${AGENT_ARTIFACT_CLEANUP_CHECK_INTERVAL_SEC}"
}

validate_hls_record_segment_config() {
  case "${AGENT_HLS_RECORD_SEGMENT_SEC}" in
    30|60) ;;
    *) fail "AGENT_HLS_RECORD_SEGMENT_SEC 必须是 30 或 60" ;;
  esac
}

validate_upload_config() {
  local extension
  local -a upload_extensions

  validate_positive_integer "UPLOAD_MAX_BYTES" "${UPLOAD_MAX_BYTES}"
  validate_positive_integer "UPLOAD_PROBE_TIMEOUT_SEC" "${UPLOAD_PROBE_TIMEOUT_SEC}"
  [ -n "${UPLOAD_ALLOWED_EXTENSIONS}" ] || fail "UPLOAD_ALLOWED_EXTENSIONS 不能为空"

  IFS=',' read -r -a upload_extensions <<<"${UPLOAD_ALLOWED_EXTENSIONS}"
  for extension in "${upload_extensions[@]}"; do
    extension="$(printf '%s' "${extension}" | tr '[:upper:]' '[:lower:]' | sed 's/^[[:space:]]*//;s/[[:space:]]*$//;s/^\\.//')"
    case "${extension}" in
      ''|*/*|*\\*|*.*)
        fail "UPLOAD_ALLOWED_EXTENSIONS 只能包含不带点号的文件扩展名"
        ;;
    esac
  done
}

configure_output_storage_defaults() {
  local existing_env_file="$1"
  local install_dir
  local default_www_host_dir
  local default_output_host_dir
  local legacy_www_host_dir
  local legacy_output_host_dir

  install_dir="$(dirname "${existing_env_file}")"
  case "${install_dir}" in
    /*) ;;
    *) install_dir="$(pwd)/${install_dir}" ;;
  esac
  default_www_host_dir="$(host_dir_path "${install_dir}" "./data/zlm/www")"
  default_output_host_dir="$(host_dir_path "${install_dir}" "./data/zlm/www/output")"
  legacy_www_host_dir="$(env_value_or_default "${existing_env_file}" "ZLM_WWW_HOST_DIR" "")"
  legacy_output_host_dir="$(env_value_or_default "${existing_env_file}" "ZLM_OUTPUT_HOST_DIR" "")"
  ZLM_WWW_MOUNT_HOST_DIR="$(env_value_or_default "${existing_env_file}" "ZLM_WWW_MOUNT_HOST_DIR" "${legacy_www_host_dir:-${default_www_host_dir}}")"
  ZLM_OUTPUT_MOUNT_HOST_DIR="$(env_value_or_default "${existing_env_file}" "ZLM_OUTPUT_MOUNT_HOST_DIR" "${legacy_output_host_dir:-${default_output_host_dir}}")"
  ZLM_WWW_MOUNT_HOST_DIR="$(host_dir_path "${install_dir}" "${ZLM_WWW_MOUNT_HOST_DIR}")"
  ZLM_OUTPUT_MOUNT_HOST_DIR="$(host_dir_path "${install_dir}" "${ZLM_OUTPUT_MOUNT_HOST_DIR}")"
  OUTPUT_MOUNT_RELATIVE_PREFIX_MP4="output"
  OUTPUT_MOUNT_RELATIVE_PREFIX_HLS="output"
}

set_default_zlm_port_config() {
  local existing_env_file="$1"
  assign_default_local_tcp_port ZLM_HTTP_PORT "${existing_env_file}" "ZLM_HTTP_PORT" "ZLM HTTP 宿主机监听端口" "80"
  ZLM_HTTPS_PORT="$(env_value_or_default "${existing_env_file}" "ZLM_HTTPS_PORT" "0")"
  assign_default_local_tcp_port ZLM_RTMP_PORT "${existing_env_file}" "ZLM_RTMP_PORT" "ZLM RTMP 宿主机监听端口" "1935"
  ZLM_RTMPS_PORT="$(env_value_or_default "${existing_env_file}" "ZLM_RTMPS_PORT" "0")"
  assign_default_local_tcp_port ZLM_RTSP_PORT "${existing_env_file}" "ZLM_RTSP_PORT" "ZLM RTSP 宿主机监听端口" "554"
  ZLM_RTSPS_PORT="$(env_value_or_default "${existing_env_file}" "ZLM_RTSPS_PORT" "0")"
  ZLM_RTP_PROXY_PORT="$(env_value_or_default "${existing_env_file}" "ZLM_RTP_PROXY_PORT" "0")"
  ZLM_RTP_PROXY_PORT_RANGE="$(env_value_or_default "${existing_env_file}" "ZLM_RTP_PROXY_PORT_RANGE" "0-0")"
  ZLM_RTC_SIGNALING_PORT="$(env_value_or_default "${existing_env_file}" "ZLM_RTC_SIGNALING_PORT" "0")"
  ZLM_RTC_SIGNALING_SSL_PORT="$(env_value_or_default "${existing_env_file}" "ZLM_RTC_SIGNALING_SSL_PORT" "0")"
  ZLM_RTC_ICE_PORT="$(env_value_or_default "${existing_env_file}" "ZLM_RTC_ICE_PORT" "0")"
  ZLM_RTC_ICE_TCP_PORT="$(env_value_or_default "${existing_env_file}" "ZLM_RTC_ICE_TCP_PORT" "0")"
  ZLM_RTC_PORT="$(env_value_or_default "${existing_env_file}" "ZLM_RTC_PORT" "0")"
  ZLM_RTC_TCP_PORT="$(env_value_or_default "${existing_env_file}" "ZLM_RTC_TCP_PORT" "0")"
  ZLM_RTC_PORT_RANGE="$(env_value_or_default "${existing_env_file}" "ZLM_RTC_PORT_RANGE" "0-0")"
  ZLM_SRT_PORT="$(env_value_or_default "${existing_env_file}" "ZLM_SRT_PORT" "0")"
  ZLM_SHELL_PORT="$(env_value_or_default "${existing_env_file}" "ZLM_SHELL_PORT" "0")"
  ZLM_ONVIF_PORT="$(env_value_or_default "${existing_env_file}" "ZLM_ONVIF_PORT" "0")"
}

validate_zlm_port_config() {
  validate_port_number "ZLM_HTTP_PORT" "${ZLM_HTTP_PORT}"
  validate_port_number "ZLM_HTTPS_PORT" "${ZLM_HTTPS_PORT}" true
  validate_port_number "ZLM_RTMP_PORT" "${ZLM_RTMP_PORT}"
  validate_port_number "ZLM_RTMPS_PORT" "${ZLM_RTMPS_PORT}" true
  validate_port_number "ZLM_RTSP_PORT" "${ZLM_RTSP_PORT}"
  validate_port_number "ZLM_RTSPS_PORT" "${ZLM_RTSPS_PORT}" true
  validate_port_number "ZLM_RTP_PROXY_PORT" "${ZLM_RTP_PROXY_PORT}" true
  validate_port_range "ZLM_RTP_PROXY_PORT_RANGE" "${ZLM_RTP_PROXY_PORT_RANGE}"
  validate_port_number "ZLM_RTC_SIGNALING_PORT" "${ZLM_RTC_SIGNALING_PORT}" true
  validate_port_number "ZLM_RTC_SIGNALING_SSL_PORT" "${ZLM_RTC_SIGNALING_SSL_PORT}" true
  validate_port_number "ZLM_RTC_ICE_PORT" "${ZLM_RTC_ICE_PORT}" true
  validate_port_number "ZLM_RTC_ICE_TCP_PORT" "${ZLM_RTC_ICE_TCP_PORT}" true
  validate_port_number "ZLM_RTC_PORT" "${ZLM_RTC_PORT}" true
  validate_port_number "ZLM_RTC_TCP_PORT" "${ZLM_RTC_TCP_PORT}" true
  validate_port_range "ZLM_RTC_PORT_RANGE" "${ZLM_RTC_PORT_RANGE}"
  validate_port_number "ZLM_SRT_PORT" "${ZLM_SRT_PORT}" true
  validate_port_number "ZLM_SHELL_PORT" "${ZLM_SHELL_PORT}" true
  validate_port_number "ZLM_ONVIF_PORT" "${ZLM_ONVIF_PORT}" true
}

write_control_plane_env() {
  local env_file="$1"
  write_env_reset "${env_file}"
  write_env_entry "${env_file}" "当前安装角色。" "INSTALL_ROLE" "${INSTALL_ROLE}"
  write_env_entry "${env_file}" "Compose 项目名。" "COMPOSE_PROJECT_NAME" "${PROJECT_NAME}"
  write_env_entry "${env_file}" "PostgreSQL 镜像名。" "POSTGRES_IMAGE" "${POSTGRES_IMAGE}"
  write_env_entry "${env_file}" "控制面板镜像名。" "MEDIA_CORE_IMAGE" "${MEDIA_CORE_IMAGE}"
  write_env_entry "${env_file}" "PostgreSQL 数据库名。" "POSTGRES_DB" "${POSTGRES_DB}"
  write_env_entry "${env_file}" "PostgreSQL 用户名。" "POSTGRES_USER" "${POSTGRES_USER}"
  write_env_entry "${env_file}" "PostgreSQL 密码。" "POSTGRES_PASSWORD" "${POSTGRES_PASSWORD}"
  write_env_entry "${env_file}" "数据库宿主机监听端口。" "POSTGRES_PORT" "${POSTGRES_PORT}"
  write_env_entry "${env_file}" "控制面板网页和 HTTP API 端口。" "CORE_HTTP_PORT" "${CORE_HTTP_PORT}"
  write_env_entry "${env_file}" "控制面板内部通信端口。" "CORE_GRPC_PORT" "${CORE_GRPC_PORT}"
  write_env_entry "${env_file}" "ZLM Hook 与 API 共用密钥。" "HOOK_SHARED_SECRET" "${HOOK_SHARED_SECRET}"
  write_env_entry "${env_file}" "允许访问 Hook 接口的源 IP 白名单，多个值用逗号分隔，留空表示不限制。" "HOOK_SOURCE_ALLOWLIST" "${HOOK_SOURCE_ALLOWLIST}"
  write_env_entry "${env_file}" "允许访问宿主机挂载存储的路径白名单，多个值用逗号分隔。" "STORAGE_ALLOWLIST" "${STORAGE_ALLOWLIST}"
  write_env_entry "${env_file}" "鉴权模式，disabled 表示关闭，local_password 表示启用内建用户名密码。" "AUTH_MODE" "${AUTH_MODE}"
  write_env_entry "${env_file}" "是否启用鉴权，true 表示启用。" "AUTH_ENABLED" "${AUTH_ENABLED}"
  write_env_entry "${env_file}" "JWT 公钥内容，留空时由 AUTH_JWT_PUBLIC_KEY_PATH 指向文件。" "JWT_PUBLIC_KEY" "${JWT_PUBLIC_KEY}"
  write_env_entry "${env_file}" "JWT 私钥文件路径。" "AUTH_JWT_PRIVATE_KEY_PATH" "${AUTH_JWT_PRIVATE_KEY_PATH}"
  write_env_entry "${env_file}" "JWT 公钥文件路径。" "AUTH_JWT_PUBLIC_KEY_PATH" "${AUTH_JWT_PUBLIC_KEY_PATH}"
  write_env_entry "${env_file}" "访问令牌有效期，例如 15m。" "AUTH_ACCESS_TOKEN_TTL" "${AUTH_ACCESS_TOKEN_TTL}"
  write_env_entry "${env_file}" "刷新令牌有效期，例如 7d。" "AUTH_REFRESH_TOKEN_TTL" "${AUTH_REFRESH_TOKEN_TTL}"
  write_env_blank_line "${env_file}"
  write_env_example "${env_file}" "HTTPS 默认关闭，当前应用不内置 HTTPS 监听；如需 HTTPS，请在反向代理中终止 TLS 后转发到控制面板 HTTP 端口。" "CORE_GRPC_TLS_CERT_PATH=/certs/self-signed/media-core.pem"
  printf '# CORE_GRPC_TLS_KEY_PATH=/certs/self-signed/media-core.key\n# CORE_GRPC_TLS_CLIENT_CA_PATH=/certs/self-signed/ca.pem\n' >>"${env_file}"
  append_preserved_custom_env_entries "${env_file}" "${PRESERVED_ENV_SOURCE}"
}

write_worker_host_env() {
  local env_file="$1"
  local media_agent_image="$2"
  local acceleration_mode="$3"
  local agent_labels="$4"
  write_env_reset "${env_file}"
  write_env_entry "${env_file}" "当前安装角色。" "INSTALL_ROLE" "${INSTALL_ROLE}"
  write_env_entry "${env_file}" "Compose 项目名。" "COMPOSE_PROJECT_NAME" "${PROJECT_NAME}"
  write_env_entry "${env_file}" "工作节点镜像名。" "MEDIA_AGENT_IMAGE" "${media_agent_image}"
  write_env_entry "${env_file}" "流媒体服务镜像名。" "ZLM_IMAGE" "${ZLM_IMAGE}"
  write_env_entry "${env_file}" "当前工作节点 UUID。" "NODE_ID" "${NODE_ID}"
  write_env_entry "${env_file}" "当前工作节点名称。" "AGENT_NODE_NAME" "${AGENT_NODE_NAME}"
  write_env_entry "${env_file}" "control-plane HTTP 地址或域名。" "CORE_HTTP_HOST" "${CORE_HTTP_HOST}"
  write_env_entry "${env_file}" "control-plane HTTP 端口。" "CORE_HTTP_PORT" "${CORE_HTTP_PORT}"
  write_env_entry "${env_file}" "control-plane gRPC 地址或域名。" "CORE_GRPC_HOST" "${CORE_GRPC_HOST}"
  write_env_entry "${env_file}" "control-plane gRPC 端口。" "CORE_GRPC_PORT" "${CORE_GRPC_PORT}"
  write_env_entry "${env_file}" "当前工作节点对外可访问的主机名或 IP。" "PUBLIC_HOST" "${PUBLIC_HOST}"
  write_env_entry "${env_file}" "工作节点访问本机流媒体服务接口使用的主机名或 IP。" "ZLM_API_HOST" "${ZLM_API_HOST}"
  write_env_entry "${env_file}" "工作节点本地接口端口。" "AGENT_HTTP_PORT" "${AGENT_HTTP_PORT}"
  write_env_entry "${env_file}" "ZLM HTTP 宿主机监听端口。" "ZLM_HTTP_PORT" "${ZLM_HTTP_PORT}"
  write_env_entry "${env_file}" "ZLM HTTPS 宿主机监听端口，0 表示关闭。" "ZLM_HTTPS_PORT" "${ZLM_HTTPS_PORT}"
  write_env_entry "${env_file}" "ZLM RTMP 宿主机监听端口。" "ZLM_RTMP_PORT" "${ZLM_RTMP_PORT}"
  write_env_entry "${env_file}" "ZLM RTMPS 宿主机监听端口，0 表示关闭。" "ZLM_RTMPS_PORT" "${ZLM_RTMPS_PORT}"
  write_env_entry "${env_file}" "ZLM RTSP 宿主机监听端口。" "ZLM_RTSP_PORT" "${ZLM_RTSP_PORT}"
  write_env_entry "${env_file}" "ZLM RTSPS 宿主机监听端口，0 表示关闭。" "ZLM_RTSPS_PORT" "${ZLM_RTSPS_PORT}"
  write_env_entry "${env_file}" "ZLM RTP Proxy 宿主机监听端口，0 表示关闭。" "ZLM_RTP_PROXY_PORT" "${ZLM_RTP_PROXY_PORT}"
  write_env_entry "${env_file}" "ZLM RTP Proxy 随机端口范围，使用 start-end 格式，0-0 表示关闭。" "ZLM_RTP_PROXY_PORT_RANGE" "${ZLM_RTP_PROXY_PORT_RANGE}"
  write_env_entry "${env_file}" "ZLM WebRTC 信令端口，0 表示关闭。" "ZLM_RTC_SIGNALING_PORT" "${ZLM_RTC_SIGNALING_PORT}"
  write_env_entry "${env_file}" "ZLM WebRTC TLS 信令端口，0 表示关闭。" "ZLM_RTC_SIGNALING_SSL_PORT" "${ZLM_RTC_SIGNALING_SSL_PORT}"
  write_env_entry "${env_file}" "ZLM STUN/TURN UDP 端口，0 表示关闭。" "ZLM_RTC_ICE_PORT" "${ZLM_RTC_ICE_PORT}"
  write_env_entry "${env_file}" "ZLM STUN/TURN TCP 端口，0 表示关闭。" "ZLM_RTC_ICE_TCP_PORT" "${ZLM_RTC_ICE_TCP_PORT}"
  write_env_entry "${env_file}" "ZLM WebRTC UDP 媒体端口，0 表示关闭。" "ZLM_RTC_PORT" "${ZLM_RTC_PORT}"
  write_env_entry "${env_file}" "ZLM WebRTC TCP 媒体端口，0 表示关闭。" "ZLM_RTC_TCP_PORT" "${ZLM_RTC_TCP_PORT}"
  write_env_entry "${env_file}" "ZLM WebRTC/TURN 分配端口范围，使用 start-end 格式，0-0 表示关闭。" "ZLM_RTC_PORT_RANGE" "${ZLM_RTC_PORT_RANGE}"
  write_env_entry "${env_file}" "ZLM SRT 宿主机监听端口，0 表示关闭。" "ZLM_SRT_PORT" "${ZLM_SRT_PORT}"
  write_env_entry "${env_file}" "ZLM Shell 宿主机监听端口，0 表示关闭。" "ZLM_SHELL_PORT" "${ZLM_SHELL_PORT}"
  write_env_entry "${env_file}" "ZLM ONVIF 宿主机监听端口，0 表示关闭。" "ZLM_ONVIF_PORT" "${ZLM_ONVIF_PORT}"
  write_env_entry "${env_file}" "主网卡名称。" "AGENT_PRIMARY_INTERFACE_NAME" "${PRIMARY_INTERFACE_NAME}"
  write_env_entry "${env_file}" "主网卡 IP。" "AGENT_PRIMARY_INTERFACE_IP" "${PRIMARY_INTERFACE_IP}"
  write_env_entry "${env_file}" "服务挂载源宿主机目录，用于在线播放临时文件，建议本机磁盘。" "ZLM_WWW_MOUNT_HOST_DIR" "${ZLM_WWW_MOUNT_HOST_DIR}"
  write_env_entry "${env_file}" "服务挂载源宿主机目录，用于录制和转码产物，可挂载网络存储。" "ZLM_OUTPUT_MOUNT_HOST_DIR" "${ZLM_OUTPUT_MOUNT_HOST_DIR}"
  write_env_entry "${env_file}" "组播网卡名称。" "AGENT_MULTICAST_INTERFACE_NAME" "${MULTICAST_INTERFACE_NAME}"
  write_env_entry "${env_file}" "组播网卡 IP。" "AGENT_MULTICAST_INTERFACE_IP" "${MULTICAST_INTERFACE_IP}"
  write_env_entry "${env_file}" "ZLM Hook 与 API 共用密钥，需与 control-plane 一致。" "HOOK_SHARED_SECRET" "${HOOK_SHARED_SECRET}"
  write_env_entry "${env_file}" "当前节点网络模式，固定 host。" "AGENT_NETWORK_MODE" "${AGENT_NETWORK_MODE}"
  write_env_entry "${env_file}" "当前节点算力模式，由安装角色决定。" "AGENT_ACCELERATION_MODE" "${acceleration_mode}"
  write_env_entry "${env_file}" "节点标签，固定包含算力标签 cpu/gpu，额外标签用英文逗号分隔。" "AGENT_LABELS" "${agent_labels}"
  write_env_entry "${env_file}" "最大同时任务数，0 表示自动估算。" "AGENT_MAX_RUNTIME_SLOTS" "${AGENT_MAX_RUNTIME_SLOTS}"
  write_env_entry "${env_file}" "工作节点托管 HLS 录制默认分片秒数，可选 30/60；任务接口显式传值时优先。" "AGENT_HLS_RECORD_SEGMENT_SEC" "${AGENT_HLS_RECORD_SEGMENT_SEC}"
  write_env_entry "${env_file}" "是否开启产物清理。" "AGENT_ARTIFACT_CLEANUP_ENABLED" "${AGENT_ARTIFACT_CLEANUP_ENABLED}"
  write_env_entry "${env_file}" "产物清理触发阈值，单位百分比。" "AGENT_ARTIFACT_CLEANUP_THRESHOLD_PERCENT" "${AGENT_ARTIFACT_CLEANUP_THRESHOLD_PERCENT}"
  write_env_entry "${env_file}" "产物清理策略，可选 delete_oldest_then_reject/reject_only。" "AGENT_ARTIFACT_CLEANUP_STRATEGY" "${AGENT_ARTIFACT_CLEANUP_STRATEGY}"
  write_env_entry "${env_file}" "产物清理检查周期，单位秒。" "AGENT_ARTIFACT_CLEANUP_CHECK_INTERVAL_SEC" "${AGENT_ARTIFACT_CLEANUP_CHECK_INTERVAL_SEC}"
  write_env_entry "${env_file}" "工作节点工作目录。" "WORK_ROOT" "${WORK_ROOT}"
  write_env_entry "${env_file}" "手动上传单文件最大字节数。" "UPLOAD_MAX_BYTES" "${UPLOAD_MAX_BYTES}"
  write_env_entry "${env_file}" "允许手动上传的视频扩展名，多个值用逗号分隔。" "UPLOAD_ALLOWED_EXTENSIONS" "${UPLOAD_ALLOWED_EXTENSIONS}"
  write_env_entry "${env_file}" "手动上传后 ffprobe 探测超时，单位秒。" "UPLOAD_PROBE_TIMEOUT_SEC" "${UPLOAD_PROBE_TIMEOUT_SEC}"
  write_env_entry "${env_file}" "上传文件对外访问基址，留空时使用请求 Host 生成。" "PUBLIC_MEDIA_BASE_URL" "${PUBLIC_MEDIA_BASE_URL}"
  write_env_blank_line "${env_file}"
  write_env_example "${env_file}" "mTLS 默认关闭，如需开启，请将 AGENT_CORE_ENDPOINT 改为 https 后再补充证书路径。" "AGENT_CORE_ENDPOINT=https://${CORE_GRPC_HOST}:${CORE_GRPC_PORT}"
  printf '# AGENT_CERT_PATH=/certs/self-signed/media-agent.pem\n# AGENT_KEY_PATH=/certs/self-signed/media-agent.key\n# AGENT_CA_PATH=/certs/self-signed/ca.pem\n# AGENT_TLS_DOMAIN_NAME=streamserver-core.local\n' >>"${env_file}"
  append_preserved_custom_env_entries "${env_file}" "${PRESERVED_ENV_SOURCE}"
}

write_all_in_one_host_env() {
  local env_file="$1"
  local media_agent_image="$2"
  local acceleration_mode="$3"
  local agent_labels="$4"
  write_env_reset "${env_file}"
  write_env_entry "${env_file}" "当前安装角色。" "INSTALL_ROLE" "${INSTALL_ROLE}"
  write_env_entry "${env_file}" "Compose 项目名。" "COMPOSE_PROJECT_NAME" "${PROJECT_NAME}"
  write_env_entry "${env_file}" "PostgreSQL 镜像名。" "POSTGRES_IMAGE" "${POSTGRES_IMAGE}"
  write_env_entry "${env_file}" "控制面板镜像名。" "MEDIA_CORE_IMAGE" "${MEDIA_CORE_IMAGE}"
  write_env_entry "${env_file}" "工作节点镜像名。" "MEDIA_AGENT_IMAGE" "${media_agent_image}"
  write_env_entry "${env_file}" "流媒体服务镜像名。" "ZLM_IMAGE" "${ZLM_IMAGE}"
  write_env_entry "${env_file}" "PostgreSQL 数据库名。" "POSTGRES_DB" "${POSTGRES_DB}"
  write_env_entry "${env_file}" "PostgreSQL 用户名。" "POSTGRES_USER" "${POSTGRES_USER}"
  write_env_entry "${env_file}" "PostgreSQL 密码。" "POSTGRES_PASSWORD" "${POSTGRES_PASSWORD}"
  write_env_entry "${env_file}" "数据库宿主机监听端口。" "POSTGRES_PORT" "${POSTGRES_PORT}"
  write_env_entry "${env_file}" "控制面板网页和 HTTP API 端口。" "CORE_HTTP_PORT" "${CORE_HTTP_PORT}"
  write_env_entry "${env_file}" "控制面板内部通信端口。" "CORE_GRPC_PORT" "${CORE_GRPC_PORT}"
  write_env_entry "${env_file}" "工作节点本地接口端口。" "AGENT_HTTP_PORT" "${AGENT_HTTP_PORT}"
  write_env_entry "${env_file}" "ZLM HTTP 宿主机监听端口。" "ZLM_HTTP_PORT" "${ZLM_HTTP_PORT}"
  write_env_entry "${env_file}" "ZLM HTTPS 宿主机监听端口，0 表示关闭。" "ZLM_HTTPS_PORT" "${ZLM_HTTPS_PORT}"
  write_env_entry "${env_file}" "ZLM RTMP 宿主机监听端口。" "ZLM_RTMP_PORT" "${ZLM_RTMP_PORT}"
  write_env_entry "${env_file}" "ZLM RTMPS 宿主机监听端口，0 表示关闭。" "ZLM_RTMPS_PORT" "${ZLM_RTMPS_PORT}"
  write_env_entry "${env_file}" "ZLM RTSP 宿主机监听端口。" "ZLM_RTSP_PORT" "${ZLM_RTSP_PORT}"
  write_env_entry "${env_file}" "ZLM RTSPS 宿主机监听端口，0 表示关闭。" "ZLM_RTSPS_PORT" "${ZLM_RTSPS_PORT}"
  write_env_entry "${env_file}" "ZLM RTP Proxy 宿主机监听端口，0 表示关闭。" "ZLM_RTP_PROXY_PORT" "${ZLM_RTP_PROXY_PORT}"
  write_env_entry "${env_file}" "ZLM RTP Proxy 随机端口范围，使用 start-end 格式，0-0 表示关闭。" "ZLM_RTP_PROXY_PORT_RANGE" "${ZLM_RTP_PROXY_PORT_RANGE}"
  write_env_entry "${env_file}" "ZLM WebRTC 信令端口，0 表示关闭。" "ZLM_RTC_SIGNALING_PORT" "${ZLM_RTC_SIGNALING_PORT}"
  write_env_entry "${env_file}" "ZLM WebRTC TLS 信令端口，0 表示关闭。" "ZLM_RTC_SIGNALING_SSL_PORT" "${ZLM_RTC_SIGNALING_SSL_PORT}"
  write_env_entry "${env_file}" "ZLM STUN/TURN UDP 端口，0 表示关闭。" "ZLM_RTC_ICE_PORT" "${ZLM_RTC_ICE_PORT}"
  write_env_entry "${env_file}" "ZLM STUN/TURN TCP 端口，0 表示关闭。" "ZLM_RTC_ICE_TCP_PORT" "${ZLM_RTC_ICE_TCP_PORT}"
  write_env_entry "${env_file}" "ZLM WebRTC UDP 媒体端口，0 表示关闭。" "ZLM_RTC_PORT" "${ZLM_RTC_PORT}"
  write_env_entry "${env_file}" "ZLM WebRTC TCP 媒体端口，0 表示关闭。" "ZLM_RTC_TCP_PORT" "${ZLM_RTC_TCP_PORT}"
  write_env_entry "${env_file}" "ZLM WebRTC/TURN 分配端口范围，使用 start-end 格式，0-0 表示关闭。" "ZLM_RTC_PORT_RANGE" "${ZLM_RTC_PORT_RANGE}"
  write_env_entry "${env_file}" "ZLM SRT 宿主机监听端口，0 表示关闭。" "ZLM_SRT_PORT" "${ZLM_SRT_PORT}"
  write_env_entry "${env_file}" "ZLM Shell 宿主机监听端口，0 表示关闭。" "ZLM_SHELL_PORT" "${ZLM_SHELL_PORT}"
  write_env_entry "${env_file}" "ZLM ONVIF 宿主机监听端口，0 表示关闭。" "ZLM_ONVIF_PORT" "${ZLM_ONVIF_PORT}"
  write_env_entry "${env_file}" "工作节点访问本机流媒体服务接口使用的主机名或 IP。" "ZLM_API_HOST" "${ZLM_API_HOST}"
  write_env_entry "${env_file}" "ZLM Hook 与 API 共用密钥。" "HOOK_SHARED_SECRET" "${HOOK_SHARED_SECRET}"
  write_env_entry "${env_file}" "允许访问 Hook 接口的源 IP 白名单，多个值用逗号分隔，留空表示不限制。" "HOOK_SOURCE_ALLOWLIST" "${HOOK_SOURCE_ALLOWLIST}"
  write_env_entry "${env_file}" "当前主机对外可访问的主机名或 IP。" "PUBLIC_HOST" "${PUBLIC_HOST}"
  write_env_entry "${env_file}" "当前节点 UUID。" "NODE_ID" "${NODE_ID}"
  write_env_entry "${env_file}" "当前节点名称。" "AGENT_NODE_NAME" "${AGENT_NODE_NAME}"
  write_env_entry "${env_file}" "主网卡名称。" "AGENT_PRIMARY_INTERFACE_NAME" "${PRIMARY_INTERFACE_NAME}"
  write_env_entry "${env_file}" "主网卡 IP。" "AGENT_PRIMARY_INTERFACE_IP" "${PRIMARY_INTERFACE_IP}"
  write_env_entry "${env_file}" "服务挂载源宿主机目录，用于在线播放临时文件，建议本机磁盘。" "ZLM_WWW_MOUNT_HOST_DIR" "${ZLM_WWW_MOUNT_HOST_DIR}"
  write_env_entry "${env_file}" "服务挂载源宿主机目录，用于录制和转码产物，可挂载网络存储。" "ZLM_OUTPUT_MOUNT_HOST_DIR" "${ZLM_OUTPUT_MOUNT_HOST_DIR}"
  write_env_entry "${env_file}" "组播网卡名称。" "AGENT_MULTICAST_INTERFACE_NAME" "${MULTICAST_INTERFACE_NAME}"
  write_env_entry "${env_file}" "组播网卡 IP。" "AGENT_MULTICAST_INTERFACE_IP" "${MULTICAST_INTERFACE_IP}"
  write_env_entry "${env_file}" "当前节点网络模式，固定 host。" "AGENT_NETWORK_MODE" "${AGENT_NETWORK_MODE}"
  write_env_entry "${env_file}" "当前节点算力模式，由安装角色决定。" "AGENT_ACCELERATION_MODE" "${acceleration_mode}"
  write_env_entry "${env_file}" "节点标签，固定包含算力标签 cpu/gpu，额外标签用英文逗号分隔。" "AGENT_LABELS" "${agent_labels}"
  write_env_entry "${env_file}" "最大同时任务数，0 表示自动估算。" "AGENT_MAX_RUNTIME_SLOTS" "${AGENT_MAX_RUNTIME_SLOTS}"
  write_env_entry "${env_file}" "工作节点托管 HLS 录制默认分片秒数，可选 30/60；任务接口显式传值时优先。" "AGENT_HLS_RECORD_SEGMENT_SEC" "${AGENT_HLS_RECORD_SEGMENT_SEC}"
  write_env_entry "${env_file}" "是否开启产物清理。" "AGENT_ARTIFACT_CLEANUP_ENABLED" "${AGENT_ARTIFACT_CLEANUP_ENABLED}"
  write_env_entry "${env_file}" "产物清理触发阈值，单位百分比。" "AGENT_ARTIFACT_CLEANUP_THRESHOLD_PERCENT" "${AGENT_ARTIFACT_CLEANUP_THRESHOLD_PERCENT}"
  write_env_entry "${env_file}" "产物清理策略，可选 delete_oldest_then_reject/reject_only。" "AGENT_ARTIFACT_CLEANUP_STRATEGY" "${AGENT_ARTIFACT_CLEANUP_STRATEGY}"
  write_env_entry "${env_file}" "产物清理检查周期，单位秒。" "AGENT_ARTIFACT_CLEANUP_CHECK_INTERVAL_SEC" "${AGENT_ARTIFACT_CLEANUP_CHECK_INTERVAL_SEC}"
  write_env_entry "${env_file}" "工作节点工作目录。" "WORK_ROOT" "${WORK_ROOT}"
  write_env_entry "${env_file}" "手动上传单文件最大字节数。" "UPLOAD_MAX_BYTES" "${UPLOAD_MAX_BYTES}"
  write_env_entry "${env_file}" "允许手动上传的视频扩展名，多个值用逗号分隔。" "UPLOAD_ALLOWED_EXTENSIONS" "${UPLOAD_ALLOWED_EXTENSIONS}"
  write_env_entry "${env_file}" "手动上传后 ffprobe 探测超时，单位秒。" "UPLOAD_PROBE_TIMEOUT_SEC" "${UPLOAD_PROBE_TIMEOUT_SEC}"
  write_env_entry "${env_file}" "上传文件对外访问基址，留空时使用请求 Host 生成。" "PUBLIC_MEDIA_BASE_URL" "${PUBLIC_MEDIA_BASE_URL}"
  write_env_entry "${env_file}" "允许访问宿主机挂载存储的路径白名单，多个值用逗号分隔。" "STORAGE_ALLOWLIST" "${STORAGE_ALLOWLIST}"
  write_env_entry "${env_file}" "鉴权模式，disabled 表示关闭，local_password 表示启用内建用户名密码。" "AUTH_MODE" "${AUTH_MODE}"
  write_env_entry "${env_file}" "是否启用鉴权，true 表示启用。" "AUTH_ENABLED" "${AUTH_ENABLED}"
  write_env_entry "${env_file}" "JWT 公钥内容，留空时由 AUTH_JWT_PUBLIC_KEY_PATH 指向文件。" "JWT_PUBLIC_KEY" "${JWT_PUBLIC_KEY}"
  write_env_entry "${env_file}" "JWT 私钥文件路径。" "AUTH_JWT_PRIVATE_KEY_PATH" "${AUTH_JWT_PRIVATE_KEY_PATH}"
  write_env_entry "${env_file}" "JWT 公钥文件路径。" "AUTH_JWT_PUBLIC_KEY_PATH" "${AUTH_JWT_PUBLIC_KEY_PATH}"
  write_env_entry "${env_file}" "访问令牌有效期，例如 15m。" "AUTH_ACCESS_TOKEN_TTL" "${AUTH_ACCESS_TOKEN_TTL}"
  write_env_entry "${env_file}" "刷新令牌有效期，例如 7d。" "AUTH_REFRESH_TOKEN_TTL" "${AUTH_REFRESH_TOKEN_TTL}"
  write_env_blank_line "${env_file}"
  write_env_example "${env_file}" "HTTPS 默认关闭，当前应用不内置 HTTPS 监听；如需 HTTPS，请在反向代理中终止 TLS 后转发到控制面板 HTTP 端口。" "CORE_GRPC_TLS_CERT_PATH=/certs/self-signed/media-core.pem"
  printf '# CORE_GRPC_TLS_KEY_PATH=/certs/self-signed/media-core.key\n# CORE_GRPC_TLS_CLIENT_CA_PATH=/certs/self-signed/ca.pem\n# AGENT_CORE_ENDPOINT=https://127.0.0.1:%s\n# AGENT_CERT_PATH=/certs/self-signed/media-agent.pem\n# AGENT_KEY_PATH=/certs/self-signed/media-agent.key\n# AGENT_CA_PATH=/certs/self-signed/ca.pem\n# AGENT_TLS_DOMAIN_NAME=streamserver-core.local\n' "${CORE_GRPC_PORT}" >>"${env_file}"
  append_preserved_custom_env_entries "${env_file}" "${PRESERVED_ENV_SOURCE}"
}

copy_zlm_runtime_assets() {
  local install_dir="$1"
  local template_file="${PACKAGE_ROOT}/templates/common/zlm.config.ini.template"
  local render_script="${PACKAGE_ROOT}/templates/common/zlm.render-config.sh"

  [ -f "${template_file}" ] || fail "缺少 ZLM 模板 ${template_file}"
  [ -f "${render_script}" ] || fail "缺少 ZLM 渲染脚本 ${render_script}"
  mkdir -p "${install_dir}/zlm"
  cp "${template_file}" "${install_dir}/zlm/config.ini.template"
  cp "${render_script}" "${install_dir}/zlm/render-config.sh"
  chmod 755 "${install_dir}/zlm/render-config.sh"
}

copy_common_assets() {
  local install_dir="$1"
  cp "${DEPLOY_DOC_SOURCE}" "${install_dir}/docs/"
  [ -d "${CERT_SOURCE_DIR}" ] || fail "缺少证书目录 ${CERT_SOURCE_DIR}"
  mkdir -p "${install_dir}/certs"
  cp -R "${CERT_SOURCE_DIR}/." "${install_dir}/certs/"
}

copy_compose_template() {
  local template_name="$1"
  local install_dir="$2"
  cp "${PACKAGE_ROOT}/templates/${template_name}/compose.yml" "${install_dir}/${COMPOSE_FILE_NAME}"
  ln -sfn "${COMPOSE_FILE_NAME}" "${install_dir}/docker-compose.yml"
}

prepare_control_plane_layout() {
  local install_dir="$1"
  # PostgreSQL 18 re-executes the entrypoint as the postgres user after
  # creating PGDATA=/var/lib/postgresql/18/docker. If the version directory is
  # auto-created with a restrictive umask (for example 750), the second pass
  # cannot traverse it and startup fails with "mkdir ... /var/lib/postgresql/18:
  # Permission denied". Pre-create the versioned layout on the host so the
  # parent directory remains traversable.
  mkdir -p "${install_dir}/data/postgres/18/docker"
  chmod 755 \
    "${install_dir}/data/postgres" \
    "${install_dir}/data/postgres/18" \
    "${install_dir}/data/postgres/18/docker" 2>/dev/null || true
}

host_dir_path() {
  local install_dir="$1"
  local configured_path="$2"

  case "${configured_path}" in
    /*) printf '%s' "${configured_path}" ;;
    ./*) printf '%s/%s' "${install_dir}" "${configured_path#./}" ;;
    *) printf '%s/%s' "${install_dir}" "${configured_path}" ;;
  esac
}

prepare_worker_layout() {
  local install_dir="$1"
  local www_host_dir
  local output_host_dir

  www_host_dir="$(host_dir_path "${install_dir}" "${ZLM_WWW_MOUNT_HOST_DIR:-./data/zlm/www}")"
  output_host_dir="$(host_dir_path "${install_dir}" "${ZLM_OUTPUT_MOUNT_HOST_DIR:-./data/zlm/www/output}")"

  mkdir -p \
    "${install_dir}/data/media/work" \
    "${install_dir}/data/media/logs" \
    "${www_host_dir}" \
    "${output_host_dir}/mp4" \
    "${output_host_dir}/hls"
}

emit_manual_start_hint() {
  local install_dir="$1"
  local install_role="${2:-${INSTALL_ROLE}}"
  log "已写入部署文件。"
  if [ -n "${STACK_SYSTEMD_UNIT_NAME}" ]; then
    log "开机自启动已启用，可手动执行:"
    log "  $(systemctl_cmd_display) start ${STACK_SYSTEMD_UNIT_NAME}"
    log "  $(systemctl_cmd_display) status ${STACK_SYSTEMD_UNIT_NAME}"
  else
    log "稍后可手动执行:"
    log "  cd ${install_dir} && ${COMPOSE_CMD_DISPLAY} -f ${COMPOSE_FILE_NAME} up -d"
  fi
  log "后续如仅更新 控制面板/工作节点服务，可直接替换 ${install_dir}/bin 下对应二进制后再重新拉起相关服务。"
  log_output_storage_notice "${install_role}"
}

show_tls_notice() {
  local install_dir="$1"
  local grpc_host_hint="${2:-<control-plane-host>}"
  local grpc_port_hint="${3:-50051}"

  install_compose_autostart_service "${install_dir}"
  mkdir -p "${install_dir}/certs/custom"
  log "已放置自签名证书到 ${install_dir}/certs/self-signed"
  log "HTTPS 和 mTLS 默认保持关闭。"
  log "如果你已有正式证书，建议在启动前替换 ${install_dir}/certs/self-signed 下的测试证书，或改用 ${install_dir}/certs/custom"

  if prompt_yes_no "是否已有正式证书，需要在启动前先替换自签名证书？" "N"; then
    log "请先把正式证书放到 ${install_dir}/certs/custom，然后再修改 ${install_dir}/.env。"
    log "mTLS 开启示例:"
    log "  CORE_GRPC_TLS_CERT_PATH=/certs/custom/media-core.pem"
    log "  CORE_GRPC_TLS_KEY_PATH=/certs/custom/media-core.key"
    log "  CORE_GRPC_TLS_CLIENT_CA_PATH=/certs/custom/ca.pem"
    log "  AGENT_CORE_ENDPOINT=https://${grpc_host_hint}:${grpc_port_hint}"
    log "  AGENT_CERT_PATH=/certs/custom/media-agent.pem"
    log "  AGENT_KEY_PATH=/certs/custom/media-agent.key"
    log "  AGENT_CA_PATH=/certs/custom/ca.pem"
    log "  AGENT_TLS_DOMAIN_NAME=streamserver-core.local"
    log "HTTPS 开启说明:"
    log "  当前应用无内置 HTTPS 监听，请在反向代理中加载 ${install_dir}/certs/custom/https.pem 和 https.key 后转发到 控制面板 HTTP 端口。"
    emit_manual_start_hint "${install_dir}" "${INSTALL_ROLE}"
    return 1
  fi

  log "如需后续测试 TLS，可直接使用 ${install_dir}/certs/self-signed 下的测试证书。"
  log "mTLS 启用时，建议将 agent 侧地址改为 https://${grpc_host_hint}:${grpc_port_hint} 并设置 AGENT_TLS_DOMAIN_NAME=streamserver-core.local"
  log "HTTPS 如需启用，请在前置 Nginx/Caddy/Traefik 中加载 ${install_dir}/certs/self-signed/https.pem 和 https.key，再转发到 控制面板 HTTP 端口。"
  return 0
}

start_stack_if_requested() {
  local install_dir="$1"
  local install_role="${2:-${INSTALL_ROLE}}"
  if prompt_yes_no "是否立即启动该部署？" "Y"; then
    if [ -n "${STACK_SYSTEMD_UNIT_NAME}" ]; then
      run_systemctl start "${STACK_SYSTEMD_UNIT_NAME}"
    else
      (
        cd "${install_dir}"
        compose_with_file up -d
      )
    fi
    log "已启动，常用命令:"
    if [ -n "${STACK_SYSTEMD_UNIT_NAME}" ]; then
      log "  $(systemctl_cmd_display) status ${STACK_SYSTEMD_UNIT_NAME}"
      log "  $(journalctl_cmd_display) -u ${STACK_SYSTEMD_UNIT_NAME} -f"
    fi
    log "  ${install_dir}/bin/streamserver-compose ps"
    log "  ${install_dir}/bin/streamserver-compose logs -f"
    log_output_storage_notice "${install_role}"
  else
    emit_manual_start_hint "${install_dir}" "${install_role}"
  fi
}

select_role() {
  local answer
  {
    echo "请选择安装角色:"
    echo "  1) control-plane"
    echo "     用途: 只安装中心控制面，包含控制面板和数据库。"
    echo "     适合: 多工作节点部署中的中心节点，或你已经有独立媒体工作节点的情况。"
    echo "     网络特性: 控制面板和数据库都使用 host。"
    echo "     注意: 会直接占用宿主机 5432/8080/50051 端口。"
    echo
    echo "  2) worker-host-cpu"
    echo "     用途: 安装 CPU-only 媒体工作节点，包含工作节点服务和流媒体服务。"
    echo "     适合: 所有工作节点场景，尤其是组播和需要直接绑定宿主机网卡的情况。"
    echo "     网络特性: 工作节点服务和流媒体服务直接使用 host 网络。"
    echo "     注意: 会直接占用宿主机媒体端口，更适合专用媒体节点。"
    echo
    if [ "${BUNDLE_GPU_SUPPORT}" = "true" ]; then
      echo "  3) worker-host-gpu"
      echo "     用途: 安装 GPU-enabled 媒体工作节点。"
      echo "     适合: 以转码、重编码推流为主，希望优先走 CUDA/NVENC 的场景。"
      echo "     前提: 宿主机可执行 nvidia-smi，Docker 已配置 nvidia runtime。"
      echo
      echo "  4) all-in-one-host-cpu"
      echo "     用途: 单机安装完整系统（CPU-only 工作节点），但媒体面直连宿主机网络。"
      echo "     适合: 同一台机器上既跑控制面又跑真实组播验证。"
      echo "     网络特性: 控制面板、工作节点服务、流媒体服务和数据库全部使用 host。"
      echo "     适用前提: 只有在确实需要 host 网络或直连网卡时才值得选择。"
      echo "     注意: 会直接占用宿主机的 5432/8080/50051/8081/80/554/1935 等端口。"
      echo
      echo "  5) all-in-one-host-gpu"
      echo "     用途: 单机安装完整系统，并让工作节点部分支持 NVIDIA GPU。"
      echo "     适合: 单机联调、单机媒体验证，以及本机 GPU 转码链路验证。"
      echo "     前提: 宿主机可执行 nvidia-smi，Docker 已配置 nvidia runtime。"
      echo
      echo "输入方式: 直接输入上面的角色编号 1-5 后回车。"
    else
      echo "  3) all-in-one-host-cpu"
      echo "     用途: 单机安装完整系统（CPU-only 工作节点），但媒体面直连宿主机网络。"
      echo "     适合: 同一台机器上既跑控制面又跑真实组播验证。"
      echo "     网络特性: 控制面板、工作节点服务、流媒体服务和数据库全部使用 host。"
      echo "     适用前提: 只有在确实需要 host 网络或直连网卡时才值得选择。"
      echo "     注意: 会直接占用宿主机的 5432/8080/50051/8081/80/554/1935 等端口。"
      echo
      echo "当前离线包为 CPU-only，不包含 GPU 镜像和 GPU 模板。"
      echo "输入方式: 直接输入上面的角色编号 1-3 后回车。"
    fi
    echo "默认值: 直接回车等同于输入 1（control-plane）。"
    if [ "${BUNDLE_GPU_SUPPORT}" = "true" ]; then
      echo "快速建议: 普通工作节点选 2，需要 GPU 工作节点选 3，单机联调选 4，需要单机 GPU 联调选 5。"
    else
      echo "快速建议: 普通工作节点选 2，需要单机联调选 3。"
    fi
  } >&2
  while true; do
    if [ "${BUNDLE_GPU_SUPPORT}" = "true" ]; then
      answer="$(prompt "输入角色编号（1-5）" "1")"
      case "${answer}" in
        1) printf '%s' "control-plane"; return 0 ;;
        2) printf '%s' "worker-host-cpu"; return 0 ;;
        3) printf '%s' "worker-host-gpu"; return 0 ;;
        4) printf '%s' "all-in-one-host-cpu"; return 0 ;;
        5) printf '%s' "all-in-one-host-gpu"; return 0 ;;
        *) echo "请输入 1 到 5。" >&2 ;;
      esac
    else
      answer="$(prompt "输入角色编号（1-3）" "1")"
      case "${answer}" in
        1) printf '%s' "control-plane"; return 0 ;;
        2) printf '%s' "worker-host-cpu"; return 0 ;;
        3) printf '%s' "all-in-one-host-cpu"; return 0 ;;
        *) echo "请输入 1 到 3。" >&2 ;;
      esac
    fi
  done
}

configure_control_plane() {
  local default_dir="/opt/streamserver/control-plane"
  local default_secret
  local default_password
  local existing_env_file
  local existing_postgres_password
  local existing_hook_secret

  default_secret="$(generate_secret)"
  default_password="$(generate_secret)"
  INSTALL_ROLE="control-plane"
  reset_install_context

  INSTALL_DIR="$(prompt_non_empty "安装目录" "${default_dir}")"
  prepare_existing_install_context "${INSTALL_DIR}" "${INSTALL_ROLE}"
  existing_env_file="${INSTALL_DIR}/.env"
  PROJECT_NAME="$(prompt_non_empty "Compose 项目名" "$(env_value_or_default "${existing_env_file}" "COMPOSE_PROJECT_NAME" "ss-core")")"
  require_upgrade_project_name_unchanged
  POSTGRES_DB="$(prompt_non_empty "PostgreSQL 数据库名" "$(env_value_or_default "${existing_env_file}" "POSTGRES_DB" "streamserver")")"
  POSTGRES_USER="$(prompt_non_empty "PostgreSQL 用户名" "$(env_value_or_default "${existing_env_file}" "POSTGRES_USER" "postgres")")"
  existing_postgres_password="$(env_value_or_default "${existing_env_file}" "POSTGRES_PASSWORD" "")"
  POSTGRES_PASSWORD="$(prompt "PostgreSQL 密码（留空自动生成）" "")"
  existing_hook_secret="$(env_value_or_default "${existing_env_file}" "HOOK_SHARED_SECRET" "")"
  HOOK_SHARED_SECRET="$(prompt "ZLM Hook/API 密钥（留空自动生成）" "")"
  assign_default_local_tcp_port POSTGRES_PORT "${existing_env_file}" "POSTGRES_PORT" "数据库宿主机监听端口" "5432"
  assign_prompt_local_tcp_port CORE_HTTP_PORT "${existing_env_file}" "CORE_HTTP_PORT" "控制面板网页和 HTTP API 端口" "8080"
  assign_prompt_local_tcp_port CORE_GRPC_PORT "${existing_env_file}" "CORE_GRPC_PORT" "控制面板内部通信端口" "50051"
  HOOK_SOURCE_ALLOWLIST="$(prompt "Hook 源 IP 白名单，逗号分隔（可留空）" "$(env_value_or_default "${existing_env_file}" "HOOK_SOURCE_ALLOWLIST" "")")"
  STORAGE_ALLOWLIST="$(env_value_or_default "${existing_env_file}" "STORAGE_ALLOWLIST" "/data/media/work,/data/zlm/www")"
  prompt_local_auth_configuration "${existing_env_file}"

  [ -n "${POSTGRES_PASSWORD}" ] || POSTGRES_PASSWORD="${existing_postgres_password}"
  [ -n "${POSTGRES_PASSWORD}" ] || POSTGRES_PASSWORD="${default_password}"
  [ -n "${HOOK_SHARED_SECRET}" ] || HOOK_SHARED_SECRET="${existing_hook_secret}"
  [ -n "${HOOK_SHARED_SECRET}" ] || HOOK_SHARED_SECRET="${default_secret}"
  validate_port_number "POSTGRES_PORT" "${POSTGRES_PORT}"
  validate_port_number "CORE_HTTP_PORT" "${CORE_HTTP_PORT}"
  validate_port_number "CORE_GRPC_PORT" "${CORE_GRPC_PORT}"

  prepare_install_target "${INSTALL_DIR}"
  copy_common_assets "${INSTALL_DIR}"
  prepare_local_auth_assets "${INSTALL_DIR}"
  copy_compose_template "control-plane" "${INSTALL_DIR}"
  prepare_control_plane_layout "${INSTALL_DIR}"
  install_host_binaries "${INSTALL_DIR}" media-core streamserver-config
  install_host_ui "${INSTALL_DIR}" media-core
  write_control_plane_env "${INSTALL_DIR}/.env"
  run_streamserver_config_tui_if_requested "${INSTALL_DIR}"
  load_runtime_env "${INSTALL_DIR}/.env"
  ensure_local_auth_after_config_tui "${INSTALL_DIR}"
  ensure_images_loaded postgres media-core
  bootstrap_local_admin_if_needed "${INSTALL_DIR}"
  finalize_deployment "${INSTALL_DIR}" "${INSTALL_ROLE}" "streamserver-core.local" "${CORE_GRPC_PORT}"
}

configure_worker_host() {
  local default_dir="/opt/streamserver/worker-host-cpu"
  local default_ip
  local agent_labels
  local existing_env_file
  local existing_hook_secret

  INSTALL_ROLE="worker-host-cpu"
  reset_install_context
  INSTALL_DIR="$(prompt_non_empty "安装目录" "${default_dir}")"
  prepare_existing_install_context "${INSTALL_DIR}" "${INSTALL_ROLE}"
  existing_env_file="${INSTALL_DIR}/.env"
  PROJECT_NAME="$(prompt_non_empty "Compose 项目名" "$(env_value_or_default "${existing_env_file}" "COMPOSE_PROJECT_NAME" "ss-worker-cpu")")"
  require_upgrade_project_name_unchanged
  NODE_ID="$(prompt_non_empty "节点 UUID（留空自动生成）" "$(env_value_or_default "${existing_env_file}" "NODE_ID" "$(generate_uuid)")")"
  AGENT_NODE_NAME="$(prompt_non_empty "节点名称" "$(env_value_or_default "${existing_env_file}" "AGENT_NODE_NAME" "$(hostname -s 2>/dev/null || echo worker-1)")")"
  configure_host_interface_defaults
  PRIMARY_INTERFACE_NAME="$(env_value_or_default "${existing_env_file}" "AGENT_PRIMARY_INTERFACE_NAME" "${PRIMARY_INTERFACE_NAME}")"
  PRIMARY_INTERFACE_IP="$(env_value_or_default "${existing_env_file}" "AGENT_PRIMARY_INTERFACE_IP" "${PRIMARY_INTERFACE_IP}")"
  MULTICAST_INTERFACE_NAME="$(env_value_or_default "${existing_env_file}" "AGENT_MULTICAST_INTERFACE_NAME" "${MULTICAST_INTERFACE_NAME}")"
  MULTICAST_INTERFACE_IP="$(env_value_or_default "${existing_env_file}" "AGENT_MULTICAST_INTERFACE_IP" "${MULTICAST_INTERFACE_IP}")"
  default_ip="${PRIMARY_INTERFACE_IP}"
  ZLM_API_HOST="$(env_value_or_default "${existing_env_file}" "ZLM_API_HOST" "${PRIMARY_INTERFACE_IP}")"
  CORE_HTTP_HOST="$(prompt_non_empty "control-plane HTTP 地址或域名" "$(env_value_or_default "${existing_env_file}" "CORE_HTTP_HOST" "${default_ip}")")"
  CORE_HTTP_PORT="$(prompt_non_empty "control-plane HTTP 端口" "$(env_value_or_default "${existing_env_file}" "CORE_HTTP_PORT" "8080")")"
  CORE_GRPC_HOST="$(prompt_non_empty "control-plane gRPC 地址或域名" "$(env_value_or_default "${existing_env_file}" "CORE_GRPC_HOST" "${CORE_HTTP_HOST}")")"
  CORE_GRPC_PORT="$(prompt_non_empty "control-plane gRPC 端口" "$(env_value_or_default "${existing_env_file}" "CORE_GRPC_PORT" "50051")")"
  PUBLIC_HOST="$(prompt_non_empty "当前工作节点对外可访问的主机名或 IP" "$(env_value_or_default "${existing_env_file}" "PUBLIC_HOST" "${default_ip}")")"
  existing_hook_secret="$(env_value_or_default "${existing_env_file}" "HOOK_SHARED_SECRET" "")"
  HOOK_SHARED_SECRET="$(prompt "ZLM Hook/API 密钥（需与 control-plane 一致）" "")"
  assign_default_local_tcp_port AGENT_HTTP_PORT "${existing_env_file}" "AGENT_HTTP_PORT" "工作节点本地接口端口" "8081"
  set_default_zlm_port_config "${existing_env_file}"
  configure_output_storage_defaults "${existing_env_file}"
  AGENT_NETWORK_MODE="$(env_value_or_default "${existing_env_file}" "AGENT_NETWORK_MODE" "host")"
  AGENT_MAX_RUNTIME_SLOTS="$(env_value_or_default "${existing_env_file}" "AGENT_MAX_RUNTIME_SLOTS" "0")"
  AGENT_HLS_RECORD_SEGMENT_SEC="$(env_value_or_default "${existing_env_file}" "AGENT_HLS_RECORD_SEGMENT_SEC" "60")"
  AGENT_ARTIFACT_CLEANUP_ENABLED="$(env_value_or_default "${existing_env_file}" "AGENT_ARTIFACT_CLEANUP_ENABLED" "true")"
  AGENT_ARTIFACT_CLEANUP_THRESHOLD_PERCENT="$(env_value_or_default "${existing_env_file}" "AGENT_ARTIFACT_CLEANUP_THRESHOLD_PERCENT" "85")"
  AGENT_ARTIFACT_CLEANUP_STRATEGY="$(env_value_or_default "${existing_env_file}" "AGENT_ARTIFACT_CLEANUP_STRATEGY" "delete_oldest_then_reject")"
  AGENT_ARTIFACT_CLEANUP_CHECK_INTERVAL_SEC="$(env_value_or_default "${existing_env_file}" "AGENT_ARTIFACT_CLEANUP_CHECK_INTERVAL_SEC" "30")"
  WORK_ROOT="$(env_value_or_default "${existing_env_file}" "WORK_ROOT" "/data/media/work")"
  UPLOAD_MAX_BYTES="$(env_value_or_default "${existing_env_file}" "UPLOAD_MAX_BYTES" "10737418240")"
  UPLOAD_ALLOWED_EXTENSIONS="$(env_value_or_default "${existing_env_file}" "UPLOAD_ALLOWED_EXTENSIONS" "mp4,mov,m4v,mkv,webm,ts,m2ts,mts,flv")"
  UPLOAD_PROBE_TIMEOUT_SEC="$(env_value_or_default "${existing_env_file}" "UPLOAD_PROBE_TIMEOUT_SEC" "30")"
  PUBLIC_MEDIA_BASE_URL="$(env_value_or_default "${existing_env_file}" "PUBLIC_MEDIA_BASE_URL" "")"
  agent_labels="$(collect_agent_labels "cpu" "$(env_value_or_default "${existing_env_file}" "AGENT_LABELS" "cpu")")"

  [ -n "${HOOK_SHARED_SECRET}" ] || HOOK_SHARED_SECRET="${existing_hook_secret}"
  [ -n "${HOOK_SHARED_SECRET}" ] || fail "worker 角色必须提供与 control-plane 一致的 Hook/API 密钥"
  validate_port_number "CORE_HTTP_PORT" "${CORE_HTTP_PORT}"
  validate_port_number "CORE_GRPC_PORT" "${CORE_GRPC_PORT}"
  validate_port_number "AGENT_HTTP_PORT" "${AGENT_HTTP_PORT}"
  validate_zlm_port_config
  validate_artifact_cleanup_config
  validate_hls_record_segment_config
  validate_upload_config

  prepare_install_target "${INSTALL_DIR}"
  copy_common_assets "${INSTALL_DIR}"
  copy_compose_template "worker-host-cpu" "${INSTALL_DIR}"
  prepare_worker_layout "${INSTALL_DIR}"
  install_host_binaries "${INSTALL_DIR}" media-agent streamserver-config
  copy_zlm_runtime_assets "${INSTALL_DIR}"
  write_worker_host_env "${INSTALL_DIR}/.env" "${MEDIA_AGENT_IMAGE}" "cpu" "${agent_labels}"
  run_streamserver_config_tui_if_requested "${INSTALL_DIR}"
  load_runtime_env "${INSTALL_DIR}/.env"
  prepare_worker_layout "${INSTALL_DIR}"
  ensure_images_loaded media-agent zlmediakit
  finalize_deployment "${INSTALL_DIR}" "${INSTALL_ROLE}" "${CORE_GRPC_HOST}" "${CORE_GRPC_PORT}"
}

configure_worker_host_gpu() {
  local default_dir="/opt/streamserver/worker-host-gpu"
  local default_ip
  local agent_labels
  local existing_env_file
  local existing_hook_secret

  [ "${BUNDLE_GPU_SUPPORT}" = "true" ] || fail "当前离线包为 CPU-only，不支持 GPU 工作节点模板"
  ensure_nvidia_runtime_ready
  INSTALL_ROLE="worker-host-gpu"
  reset_install_context

  INSTALL_DIR="$(prompt_non_empty "安装目录" "${default_dir}")"
  prepare_existing_install_context "${INSTALL_DIR}" "${INSTALL_ROLE}"
  existing_env_file="${INSTALL_DIR}/.env"
  PROJECT_NAME="$(prompt_non_empty "Compose 项目名" "$(env_value_or_default "${existing_env_file}" "COMPOSE_PROJECT_NAME" "ss-worker-gpu")")"
  require_upgrade_project_name_unchanged
  NODE_ID="$(prompt_non_empty "节点 UUID（留空自动生成）" "$(env_value_or_default "${existing_env_file}" "NODE_ID" "$(generate_uuid)")")"
  AGENT_NODE_NAME="$(prompt_non_empty "节点名称" "$(env_value_or_default "${existing_env_file}" "AGENT_NODE_NAME" "$(hostname -s 2>/dev/null || echo worker-gpu-1)")")"
  configure_host_interface_defaults
  PRIMARY_INTERFACE_NAME="$(env_value_or_default "${existing_env_file}" "AGENT_PRIMARY_INTERFACE_NAME" "${PRIMARY_INTERFACE_NAME}")"
  PRIMARY_INTERFACE_IP="$(env_value_or_default "${existing_env_file}" "AGENT_PRIMARY_INTERFACE_IP" "${PRIMARY_INTERFACE_IP}")"
  MULTICAST_INTERFACE_NAME="$(env_value_or_default "${existing_env_file}" "AGENT_MULTICAST_INTERFACE_NAME" "${MULTICAST_INTERFACE_NAME}")"
  MULTICAST_INTERFACE_IP="$(env_value_or_default "${existing_env_file}" "AGENT_MULTICAST_INTERFACE_IP" "${MULTICAST_INTERFACE_IP}")"
  default_ip="${PRIMARY_INTERFACE_IP}"
  ZLM_API_HOST="$(env_value_or_default "${existing_env_file}" "ZLM_API_HOST" "${PRIMARY_INTERFACE_IP}")"
  CORE_HTTP_HOST="$(prompt_non_empty "control-plane HTTP 地址或域名" "$(env_value_or_default "${existing_env_file}" "CORE_HTTP_HOST" "${default_ip}")")"
  CORE_HTTP_PORT="$(prompt_non_empty "control-plane HTTP 端口" "$(env_value_or_default "${existing_env_file}" "CORE_HTTP_PORT" "8080")")"
  CORE_GRPC_HOST="$(prompt_non_empty "control-plane gRPC 地址或域名" "$(env_value_or_default "${existing_env_file}" "CORE_GRPC_HOST" "${CORE_HTTP_HOST}")")"
  CORE_GRPC_PORT="$(prompt_non_empty "control-plane gRPC 端口" "$(env_value_or_default "${existing_env_file}" "CORE_GRPC_PORT" "50051")")"
  PUBLIC_HOST="$(prompt_non_empty "当前工作节点对外可访问的主机名或 IP" "$(env_value_or_default "${existing_env_file}" "PUBLIC_HOST" "${default_ip}")")"
  existing_hook_secret="$(env_value_or_default "${existing_env_file}" "HOOK_SHARED_SECRET" "")"
  HOOK_SHARED_SECRET="$(prompt "ZLM Hook/API 密钥（需与 control-plane 一致）" "")"
  assign_default_local_tcp_port AGENT_HTTP_PORT "${existing_env_file}" "AGENT_HTTP_PORT" "工作节点本地接口端口" "8081"
  set_default_zlm_port_config "${existing_env_file}"
  configure_output_storage_defaults "${existing_env_file}"
  AGENT_NETWORK_MODE="$(env_value_or_default "${existing_env_file}" "AGENT_NETWORK_MODE" "host")"
  AGENT_MAX_RUNTIME_SLOTS="$(env_value_or_default "${existing_env_file}" "AGENT_MAX_RUNTIME_SLOTS" "0")"
  AGENT_HLS_RECORD_SEGMENT_SEC="$(env_value_or_default "${existing_env_file}" "AGENT_HLS_RECORD_SEGMENT_SEC" "60")"
  AGENT_ARTIFACT_CLEANUP_ENABLED="$(env_value_or_default "${existing_env_file}" "AGENT_ARTIFACT_CLEANUP_ENABLED" "true")"
  AGENT_ARTIFACT_CLEANUP_THRESHOLD_PERCENT="$(env_value_or_default "${existing_env_file}" "AGENT_ARTIFACT_CLEANUP_THRESHOLD_PERCENT" "85")"
  AGENT_ARTIFACT_CLEANUP_STRATEGY="$(env_value_or_default "${existing_env_file}" "AGENT_ARTIFACT_CLEANUP_STRATEGY" "delete_oldest_then_reject")"
  AGENT_ARTIFACT_CLEANUP_CHECK_INTERVAL_SEC="$(env_value_or_default "${existing_env_file}" "AGENT_ARTIFACT_CLEANUP_CHECK_INTERVAL_SEC" "30")"
  WORK_ROOT="$(env_value_or_default "${existing_env_file}" "WORK_ROOT" "/data/media/work")"
  UPLOAD_MAX_BYTES="$(env_value_or_default "${existing_env_file}" "UPLOAD_MAX_BYTES" "10737418240")"
  UPLOAD_ALLOWED_EXTENSIONS="$(env_value_or_default "${existing_env_file}" "UPLOAD_ALLOWED_EXTENSIONS" "mp4,mov,m4v,mkv,webm,ts,m2ts,mts,flv")"
  UPLOAD_PROBE_TIMEOUT_SEC="$(env_value_or_default "${existing_env_file}" "UPLOAD_PROBE_TIMEOUT_SEC" "30")"
  PUBLIC_MEDIA_BASE_URL="$(env_value_or_default "${existing_env_file}" "PUBLIC_MEDIA_BASE_URL" "")"
  agent_labels="$(collect_agent_labels "gpu" "$(env_value_or_default "${existing_env_file}" "AGENT_LABELS" "gpu")")"

  [ -n "${HOOK_SHARED_SECRET}" ] || HOOK_SHARED_SECRET="${existing_hook_secret}"
  [ -n "${HOOK_SHARED_SECRET}" ] || fail "worker 角色必须提供与 control-plane 一致的 Hook/API 密钥"
  validate_port_number "CORE_HTTP_PORT" "${CORE_HTTP_PORT}"
  validate_port_number "CORE_GRPC_PORT" "${CORE_GRPC_PORT}"
  validate_port_number "AGENT_HTTP_PORT" "${AGENT_HTTP_PORT}"
  validate_zlm_port_config
  validate_artifact_cleanup_config
  validate_hls_record_segment_config
  validate_upload_config

  prepare_install_target "${INSTALL_DIR}"
  copy_common_assets "${INSTALL_DIR}"
  copy_compose_template "worker-host-gpu" "${INSTALL_DIR}"
  prepare_worker_layout "${INSTALL_DIR}"
  install_host_binaries "${INSTALL_DIR}" media-agent streamserver-config
  copy_zlm_runtime_assets "${INSTALL_DIR}"
  write_worker_host_env "${INSTALL_DIR}/.env" "${MEDIA_AGENT_GPU_IMAGE}" "gpu" "${agent_labels}"
  run_streamserver_config_tui_if_requested "${INSTALL_DIR}"
  load_runtime_env "${INSTALL_DIR}/.env"
  prepare_worker_layout "${INSTALL_DIR}"
  ensure_images_loaded media-agent-gpu zlmediakit
  finalize_deployment "${INSTALL_DIR}" "${INSTALL_ROLE}" "${CORE_GRPC_HOST}" "${CORE_GRPC_PORT}"
}

configure_all_in_one_host() {
  local default_dir="/opt/streamserver/all-in-one-host-cpu"
  local default_secret
  local default_password
  local default_ip
  local agent_labels
  local existing_env_file
  local existing_postgres_password
  local existing_hook_secret

  default_secret="$(generate_secret)"
  default_password="$(generate_secret)"
  INSTALL_ROLE="all-in-one-host-cpu"
  reset_install_context

  INSTALL_DIR="$(prompt_non_empty "安装目录" "${default_dir}")"
  prepare_existing_install_context "${INSTALL_DIR}" "${INSTALL_ROLE}"
  existing_env_file="${INSTALL_DIR}/.env"
  PROJECT_NAME="$(prompt_non_empty "Compose 项目名" "$(env_value_or_default "${existing_env_file}" "COMPOSE_PROJECT_NAME" "ss-aio-cpu")")"
  require_upgrade_project_name_unchanged
  NODE_ID="$(prompt_non_empty "节点 UUID（留空自动生成）" "$(env_value_or_default "${existing_env_file}" "NODE_ID" "$(generate_uuid)")")"
  AGENT_NODE_NAME="$(prompt_non_empty "节点名称" "$(env_value_or_default "${existing_env_file}" "AGENT_NODE_NAME" "$(hostname -s 2>/dev/null || echo node-1)")")"
  configure_host_interface_defaults
  PRIMARY_INTERFACE_NAME="$(env_value_or_default "${existing_env_file}" "AGENT_PRIMARY_INTERFACE_NAME" "${PRIMARY_INTERFACE_NAME}")"
  PRIMARY_INTERFACE_IP="$(env_value_or_default "${existing_env_file}" "AGENT_PRIMARY_INTERFACE_IP" "${PRIMARY_INTERFACE_IP}")"
  MULTICAST_INTERFACE_NAME="$(env_value_or_default "${existing_env_file}" "AGENT_MULTICAST_INTERFACE_NAME" "${MULTICAST_INTERFACE_NAME}")"
  MULTICAST_INTERFACE_IP="$(env_value_or_default "${existing_env_file}" "AGENT_MULTICAST_INTERFACE_IP" "${MULTICAST_INTERFACE_IP}")"
  default_ip="${PRIMARY_INTERFACE_IP}"
  ZLM_API_HOST="$(env_value_or_default "${existing_env_file}" "ZLM_API_HOST" "${PRIMARY_INTERFACE_IP}")"
  POSTGRES_DB="$(prompt_non_empty "PostgreSQL 数据库名" "$(env_value_or_default "${existing_env_file}" "POSTGRES_DB" "streamserver")")"
  POSTGRES_USER="$(prompt_non_empty "PostgreSQL 用户名" "$(env_value_or_default "${existing_env_file}" "POSTGRES_USER" "postgres")")"
  existing_postgres_password="$(env_value_or_default "${existing_env_file}" "POSTGRES_PASSWORD" "")"
  POSTGRES_PASSWORD="$(prompt "PostgreSQL 密码（留空自动生成）" "")"
  existing_hook_secret="$(env_value_or_default "${existing_env_file}" "HOOK_SHARED_SECRET" "")"
  HOOK_SHARED_SECRET="$(prompt "ZLM Hook/API 密钥（留空自动生成）" "")"
  PUBLIC_HOST="$(prompt_non_empty "当前主机对外可访问的主机名或 IP" "$(env_value_or_default "${existing_env_file}" "PUBLIC_HOST" "${default_ip}")")"
  assign_default_local_tcp_port POSTGRES_PORT "${existing_env_file}" "POSTGRES_PORT" "数据库宿主机监听端口" "5432"
  assign_default_local_tcp_port CORE_HTTP_PORT "${existing_env_file}" "CORE_HTTP_PORT" "控制面板网页和 HTTP API 端口" "8080"
  assign_default_local_tcp_port CORE_GRPC_PORT "${existing_env_file}" "CORE_GRPC_PORT" "控制面板内部通信端口" "50051"
  assign_default_local_tcp_port AGENT_HTTP_PORT "${existing_env_file}" "AGENT_HTTP_PORT" "工作节点本地接口端口" "8081"
  set_default_zlm_port_config "${existing_env_file}"
  HOOK_SOURCE_ALLOWLIST="$(env_value_or_default "${existing_env_file}" "HOOK_SOURCE_ALLOWLIST" "")"
  configure_output_storage_defaults "${existing_env_file}"
  AGENT_NETWORK_MODE="$(env_value_or_default "${existing_env_file}" "AGENT_NETWORK_MODE" "host")"
  AGENT_MAX_RUNTIME_SLOTS="$(env_value_or_default "${existing_env_file}" "AGENT_MAX_RUNTIME_SLOTS" "0")"
  AGENT_HLS_RECORD_SEGMENT_SEC="$(env_value_or_default "${existing_env_file}" "AGENT_HLS_RECORD_SEGMENT_SEC" "60")"
  AGENT_ARTIFACT_CLEANUP_ENABLED="$(env_value_or_default "${existing_env_file}" "AGENT_ARTIFACT_CLEANUP_ENABLED" "true")"
  AGENT_ARTIFACT_CLEANUP_THRESHOLD_PERCENT="$(env_value_or_default "${existing_env_file}" "AGENT_ARTIFACT_CLEANUP_THRESHOLD_PERCENT" "85")"
  AGENT_ARTIFACT_CLEANUP_STRATEGY="$(env_value_or_default "${existing_env_file}" "AGENT_ARTIFACT_CLEANUP_STRATEGY" "delete_oldest_then_reject")"
  AGENT_ARTIFACT_CLEANUP_CHECK_INTERVAL_SEC="$(env_value_or_default "${existing_env_file}" "AGENT_ARTIFACT_CLEANUP_CHECK_INTERVAL_SEC" "30")"
  STORAGE_ALLOWLIST="$(env_value_or_default "${existing_env_file}" "STORAGE_ALLOWLIST" "/data/media/work,/data/zlm/www")"
  WORK_ROOT="$(env_value_or_default "${existing_env_file}" "WORK_ROOT" "/data/media/work")"
  UPLOAD_MAX_BYTES="$(env_value_or_default "${existing_env_file}" "UPLOAD_MAX_BYTES" "10737418240")"
  UPLOAD_ALLOWED_EXTENSIONS="$(env_value_or_default "${existing_env_file}" "UPLOAD_ALLOWED_EXTENSIONS" "mp4,mov,m4v,mkv,webm,ts,m2ts,mts,flv")"
  UPLOAD_PROBE_TIMEOUT_SEC="$(env_value_or_default "${existing_env_file}" "UPLOAD_PROBE_TIMEOUT_SEC" "30")"
  PUBLIC_MEDIA_BASE_URL="$(env_value_or_default "${existing_env_file}" "PUBLIC_MEDIA_BASE_URL" "")"
  prompt_local_auth_configuration "${existing_env_file}"
  agent_labels="$(collect_agent_labels "cpu" "$(env_value_or_default "${existing_env_file}" "AGENT_LABELS" "cpu")")"

  [ -n "${POSTGRES_PASSWORD}" ] || POSTGRES_PASSWORD="${existing_postgres_password}"
  [ -n "${POSTGRES_PASSWORD}" ] || POSTGRES_PASSWORD="${default_password}"
  [ -n "${HOOK_SHARED_SECRET}" ] || HOOK_SHARED_SECRET="${existing_hook_secret}"
  [ -n "${HOOK_SHARED_SECRET}" ] || HOOK_SHARED_SECRET="${default_secret}"
  validate_port_number "POSTGRES_PORT" "${POSTGRES_PORT}"
  validate_port_number "CORE_HTTP_PORT" "${CORE_HTTP_PORT}"
  validate_port_number "CORE_GRPC_PORT" "${CORE_GRPC_PORT}"
  validate_port_number "AGENT_HTTP_PORT" "${AGENT_HTTP_PORT}"
  validate_zlm_port_config
  validate_artifact_cleanup_config
  validate_hls_record_segment_config
  validate_upload_config

  prepare_install_target "${INSTALL_DIR}"
  copy_common_assets "${INSTALL_DIR}"
  prepare_local_auth_assets "${INSTALL_DIR}"
  copy_compose_template "all-in-one-host-cpu" "${INSTALL_DIR}"
  prepare_control_plane_layout "${INSTALL_DIR}"
  prepare_worker_layout "${INSTALL_DIR}"
  install_host_binaries "${INSTALL_DIR}" media-core media-agent streamserver-config
  install_host_ui "${INSTALL_DIR}" media-core
  copy_zlm_runtime_assets "${INSTALL_DIR}"
  write_all_in_one_host_env "${INSTALL_DIR}/.env" "${MEDIA_AGENT_IMAGE}" "cpu" "${agent_labels}"
  run_streamserver_config_tui_if_requested "${INSTALL_DIR}"
  load_runtime_env "${INSTALL_DIR}/.env"
  ensure_local_auth_after_config_tui "${INSTALL_DIR}"
  load_runtime_env "${INSTALL_DIR}/.env"
  prepare_worker_layout "${INSTALL_DIR}"
  ensure_images_loaded postgres media-core media-agent zlmediakit
  bootstrap_local_admin_if_needed "${INSTALL_DIR}"
  log "all-in-one-host-cpu 说明: 数据库、控制面板、工作节点服务和流媒体服务 会直接占用宿主机端口 ${POSTGRES_PORT}/${CORE_HTTP_PORT}/${CORE_GRPC_PORT}/${AGENT_HTTP_PORT}/${ZLM_HTTP_PORT}/${ZLM_RTMP_PORT}/${ZLM_RTSP_PORT}。"
  log "如果这些端口已被宿主机其他服务占用，请先释放端口，或在配置中调整端口后再启动。"
  finalize_deployment "${INSTALL_DIR}" "${INSTALL_ROLE}" "127.0.0.1" "${CORE_GRPC_PORT}"
}

configure_all_in_one_host_gpu() {
  local default_dir="/opt/streamserver/all-in-one-host-gpu"
  local default_secret
  local default_password
  local default_ip
  local agent_labels
  local existing_env_file
  local existing_postgres_password
  local existing_hook_secret

  [ "${BUNDLE_GPU_SUPPORT}" = "true" ] || fail "当前离线包为 CPU-only，不支持 GPU 一体机模板"
  ensure_nvidia_runtime_ready

  default_secret="$(generate_secret)"
  default_password="$(generate_secret)"
  INSTALL_ROLE="all-in-one-host-gpu"
  reset_install_context

  INSTALL_DIR="$(prompt_non_empty "安装目录" "${default_dir}")"
  prepare_existing_install_context "${INSTALL_DIR}" "${INSTALL_ROLE}"
  existing_env_file="${INSTALL_DIR}/.env"
  PROJECT_NAME="$(prompt_non_empty "Compose 项目名" "$(env_value_or_default "${existing_env_file}" "COMPOSE_PROJECT_NAME" "ss-aio-gpu")")"
  require_upgrade_project_name_unchanged
  NODE_ID="$(prompt_non_empty "节点 UUID（留空自动生成）" "$(env_value_or_default "${existing_env_file}" "NODE_ID" "$(generate_uuid)")")"
  AGENT_NODE_NAME="$(prompt_non_empty "节点名称" "$(env_value_or_default "${existing_env_file}" "AGENT_NODE_NAME" "$(hostname -s 2>/dev/null || echo node-gpu-1)")")"
  configure_host_interface_defaults
  PRIMARY_INTERFACE_NAME="$(env_value_or_default "${existing_env_file}" "AGENT_PRIMARY_INTERFACE_NAME" "${PRIMARY_INTERFACE_NAME}")"
  PRIMARY_INTERFACE_IP="$(env_value_or_default "${existing_env_file}" "AGENT_PRIMARY_INTERFACE_IP" "${PRIMARY_INTERFACE_IP}")"
  MULTICAST_INTERFACE_NAME="$(env_value_or_default "${existing_env_file}" "AGENT_MULTICAST_INTERFACE_NAME" "${MULTICAST_INTERFACE_NAME}")"
  MULTICAST_INTERFACE_IP="$(env_value_or_default "${existing_env_file}" "AGENT_MULTICAST_INTERFACE_IP" "${MULTICAST_INTERFACE_IP}")"
  default_ip="${PRIMARY_INTERFACE_IP}"
  ZLM_API_HOST="$(env_value_or_default "${existing_env_file}" "ZLM_API_HOST" "${PRIMARY_INTERFACE_IP}")"
  POSTGRES_DB="$(prompt_non_empty "PostgreSQL 数据库名" "$(env_value_or_default "${existing_env_file}" "POSTGRES_DB" "streamserver")")"
  POSTGRES_USER="$(prompt_non_empty "PostgreSQL 用户名" "$(env_value_or_default "${existing_env_file}" "POSTGRES_USER" "postgres")")"
  existing_postgres_password="$(env_value_or_default "${existing_env_file}" "POSTGRES_PASSWORD" "")"
  POSTGRES_PASSWORD="$(prompt "PostgreSQL 密码（留空自动生成）" "")"
  existing_hook_secret="$(env_value_or_default "${existing_env_file}" "HOOK_SHARED_SECRET" "")"
  HOOK_SHARED_SECRET="$(prompt "ZLM Hook/API 密钥（留空自动生成）" "")"
  PUBLIC_HOST="$(prompt_non_empty "当前主机对外可访问的主机名或 IP" "$(env_value_or_default "${existing_env_file}" "PUBLIC_HOST" "${default_ip}")")"
  assign_default_local_tcp_port POSTGRES_PORT "${existing_env_file}" "POSTGRES_PORT" "数据库宿主机监听端口" "5432"
  assign_default_local_tcp_port CORE_HTTP_PORT "${existing_env_file}" "CORE_HTTP_PORT" "控制面板网页和 HTTP API 端口" "8080"
  assign_default_local_tcp_port CORE_GRPC_PORT "${existing_env_file}" "CORE_GRPC_PORT" "控制面板内部通信端口" "50051"
  assign_default_local_tcp_port AGENT_HTTP_PORT "${existing_env_file}" "AGENT_HTTP_PORT" "工作节点本地接口端口" "8081"
  set_default_zlm_port_config "${existing_env_file}"
  HOOK_SOURCE_ALLOWLIST="$(env_value_or_default "${existing_env_file}" "HOOK_SOURCE_ALLOWLIST" "")"
  configure_output_storage_defaults "${existing_env_file}"
  AGENT_NETWORK_MODE="$(env_value_or_default "${existing_env_file}" "AGENT_NETWORK_MODE" "host")"
  AGENT_MAX_RUNTIME_SLOTS="$(env_value_or_default "${existing_env_file}" "AGENT_MAX_RUNTIME_SLOTS" "0")"
  AGENT_HLS_RECORD_SEGMENT_SEC="$(env_value_or_default "${existing_env_file}" "AGENT_HLS_RECORD_SEGMENT_SEC" "60")"
  AGENT_ARTIFACT_CLEANUP_ENABLED="$(env_value_or_default "${existing_env_file}" "AGENT_ARTIFACT_CLEANUP_ENABLED" "true")"
  AGENT_ARTIFACT_CLEANUP_THRESHOLD_PERCENT="$(env_value_or_default "${existing_env_file}" "AGENT_ARTIFACT_CLEANUP_THRESHOLD_PERCENT" "85")"
  AGENT_ARTIFACT_CLEANUP_STRATEGY="$(env_value_or_default "${existing_env_file}" "AGENT_ARTIFACT_CLEANUP_STRATEGY" "delete_oldest_then_reject")"
  AGENT_ARTIFACT_CLEANUP_CHECK_INTERVAL_SEC="$(env_value_or_default "${existing_env_file}" "AGENT_ARTIFACT_CLEANUP_CHECK_INTERVAL_SEC" "30")"
  STORAGE_ALLOWLIST="$(env_value_or_default "${existing_env_file}" "STORAGE_ALLOWLIST" "/data/media/work,/data/zlm/www")"
  WORK_ROOT="$(env_value_or_default "${existing_env_file}" "WORK_ROOT" "/data/media/work")"
  UPLOAD_MAX_BYTES="$(env_value_or_default "${existing_env_file}" "UPLOAD_MAX_BYTES" "10737418240")"
  UPLOAD_ALLOWED_EXTENSIONS="$(env_value_or_default "${existing_env_file}" "UPLOAD_ALLOWED_EXTENSIONS" "mp4,mov,m4v,mkv,webm,ts,m2ts,mts,flv")"
  UPLOAD_PROBE_TIMEOUT_SEC="$(env_value_or_default "${existing_env_file}" "UPLOAD_PROBE_TIMEOUT_SEC" "30")"
  PUBLIC_MEDIA_BASE_URL="$(env_value_or_default "${existing_env_file}" "PUBLIC_MEDIA_BASE_URL" "")"
  prompt_local_auth_configuration "${existing_env_file}"
  agent_labels="$(collect_agent_labels "gpu" "$(env_value_or_default "${existing_env_file}" "AGENT_LABELS" "gpu")")"

  [ -n "${POSTGRES_PASSWORD}" ] || POSTGRES_PASSWORD="${existing_postgres_password}"
  [ -n "${POSTGRES_PASSWORD}" ] || POSTGRES_PASSWORD="${default_password}"
  [ -n "${HOOK_SHARED_SECRET}" ] || HOOK_SHARED_SECRET="${existing_hook_secret}"
  [ -n "${HOOK_SHARED_SECRET}" ] || HOOK_SHARED_SECRET="${default_secret}"
  validate_port_number "POSTGRES_PORT" "${POSTGRES_PORT}"
  validate_port_number "CORE_HTTP_PORT" "${CORE_HTTP_PORT}"
  validate_port_number "CORE_GRPC_PORT" "${CORE_GRPC_PORT}"
  validate_port_number "AGENT_HTTP_PORT" "${AGENT_HTTP_PORT}"
  validate_zlm_port_config
  validate_artifact_cleanup_config
  validate_hls_record_segment_config
  validate_upload_config

  prepare_install_target "${INSTALL_DIR}"
  copy_common_assets "${INSTALL_DIR}"
  prepare_local_auth_assets "${INSTALL_DIR}"
  copy_compose_template "all-in-one-host-gpu" "${INSTALL_DIR}"
  prepare_control_plane_layout "${INSTALL_DIR}"
  prepare_worker_layout "${INSTALL_DIR}"
  install_host_binaries "${INSTALL_DIR}" media-core media-agent streamserver-config
  install_host_ui "${INSTALL_DIR}" media-core
  copy_zlm_runtime_assets "${INSTALL_DIR}"
  write_all_in_one_host_env "${INSTALL_DIR}/.env" "${MEDIA_AGENT_GPU_IMAGE}" "gpu" "${agent_labels}"
  run_streamserver_config_tui_if_requested "${INSTALL_DIR}"
  load_runtime_env "${INSTALL_DIR}/.env"
  ensure_local_auth_after_config_tui "${INSTALL_DIR}"
  load_runtime_env "${INSTALL_DIR}/.env"
  prepare_worker_layout "${INSTALL_DIR}"
  ensure_images_loaded postgres media-core media-agent-gpu zlmediakit
  bootstrap_local_admin_if_needed "${INSTALL_DIR}"
  log "all-in-one-host-gpu 说明: 数据库、控制面板、工作节点服务和流媒体服务 会直接占用宿主机端口 ${POSTGRES_PORT}/${CORE_HTTP_PORT}/${CORE_GRPC_PORT}/${AGENT_HTTP_PORT}/${ZLM_HTTP_PORT}/${ZLM_RTMP_PORT}/${ZLM_RTSP_PORT}。"
  log "该模式要求宿主机 NVIDIA 驱动和 Docker nvidia runtime 均已就绪。"
  finalize_deployment "${INSTALL_DIR}" "${INSTALL_ROLE}" "127.0.0.1" "${CORE_GRPC_PORT}"
}

main() {
  local role
  ensure_linux_amd64
  ensure_docker_ready

  log "离线包版本: ${BUNDLE_VERSION}"
  role="$(select_role)"
  case "${role}" in
    control-plane) configure_control_plane ;;
    worker-host-cpu) configure_worker_host ;;
    worker-host-gpu) configure_worker_host_gpu ;;
    all-in-one-host-cpu) configure_all_in_one_host ;;
    all-in-one-host-gpu) configure_all_in_one_host_gpu ;;
    *) fail "未知角色 ${role}" ;;
  esac
}

main "$@"
