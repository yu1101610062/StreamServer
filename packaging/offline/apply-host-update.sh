#!/usr/bin/env bash
set -euo pipefail

PACKAGE_ROOT="$(cd "$(dirname "$0")" && pwd)"
MANIFEST_FILE="${PACKAGE_ROOT}/update-manifest.env"

INSTALL_DIR=""
AUTO_RESTART=1
COMPOSE_CMD=()
COMPOSE_CMD_DISPLAY=""
COMPOSE_FILE_NAME=""
BACKUP_DIR=""

usage() {
  cat <<'EOF'
用法:
  ./apply-host-update.sh --install-dir /path/to/streamserver [--no-restart]

说明:
  将独立更新包里的 media-core / media-agent / 前端文件覆盖到现有宿主机挂载目录，
  并按需重建对应容器。
EOF
}

log() {
  printf '[streamserver-host-update] %s\n' "$*"
}

fail() {
  printf '[streamserver-host-update] ERROR: %s\n' "$*" >&2
  exit 1
}

require_cmd() {
  command -v "$1" >/dev/null 2>&1 || fail "缺少命令: $1"
}

ensure_linux() {
  [ "$(uname -s)" = "Linux" ] || fail "更新脚本只能在 Linux 上运行"
}

parse_args() {
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --install-dir)
        [ "$#" -ge 2 ] || fail "--install-dir 需要参数"
        INSTALL_DIR="$2"
        shift 2
        ;;
      --no-restart)
        AUTO_RESTART=0
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

detect_compose_file() {
  if [ -f "${INSTALL_DIR}/compose.yml" ]; then
    COMPOSE_FILE_NAME="compose.yml"
    return 0
  fi
  if [ -f "${INSTALL_DIR}/docker-compose.yml" ]; then
    COMPOSE_FILE_NAME="docker-compose.yml"
    return 0
  fi
  return 1
}

compose_with_file() {
  [ "${#COMPOSE_CMD[@]}" -gt 0 ] || fail "Compose 命令尚未初始化"
  [ -n "${COMPOSE_FILE_NAME}" ] || fail "Compose 文件尚未初始化"
  (
    cd "${INSTALL_DIR}"
    "${COMPOSE_CMD[@]}" -f "${COMPOSE_FILE_NAME}" "$@"
  )
}

ensure_backup_dir() {
  if [ -n "${BACKUP_DIR}" ]; then
    return 0
  fi
  BACKUP_DIR="${INSTALL_DIR}/backup/host-update-$(date '+%Y%m%d-%H%M%S')"
  mkdir -p "${BACKUP_DIR}"
}

load_manifest() {
  [ -f "${MANIFEST_FILE}" ] || fail "缺少 ${MANIFEST_FILE}"
  # shellcheck disable=SC1090
  . "${MANIFEST_FILE}"

  PACKAGE_COMPONENTS="${PACKAGE_COMPONENTS:-}"
  MEDIA_CORE_BINARY_PATH="${MEDIA_CORE_BINARY_PATH:-}"
  MEDIA_AGENT_BINARY_PATH="${MEDIA_AGENT_BINARY_PATH:-}"
  MEDIA_CORE_UI_PATH="${MEDIA_CORE_UI_PATH:-}"

  [ -n "${PACKAGE_COMPONENTS}" ] || fail "更新包缺少 PACKAGE_COMPONENTS"
}

component_enabled() {
  case ",${PACKAGE_COMPONENTS}," in
    *,"$1",*) return 0 ;;
    *) return 1 ;;
  esac
}

install_binary() {
  local src="$1"
  local dst="$2"
  local backup_name="$3"
  local tmp_dst="${dst}.tmp"

  [ -f "${src}" ] || fail "缺少更新文件: ${src}"

  ensure_backup_dir
  mkdir -p "$(dirname "${dst}")" "${BACKUP_DIR}/bin"

  if [ -f "${dst}" ]; then
    cp -p "${dst}" "${BACKUP_DIR}/bin/${backup_name}"
  fi

  install -m 0755 "${src}" "${tmp_dst}"
  mv "${tmp_dst}" "${dst}"
  log "已更新 ${dst}"
}

install_ui_dir() {
  local src_dir="$1"
  local dst_dir="$2"

  [ -d "${src_dir}" ] || fail "缺少更新目录: ${src_dir}"

  ensure_backup_dir

  if [ -e "${dst_dir}" ]; then
    mv "${dst_dir}" "${BACKUP_DIR}/ui"
  fi

  cp -a "${src_dir}" "${dst_dir}"
  log "已更新 ${dst_dir}"
}

restart_services_if_needed() {
  local services=()

  if [ "${AUTO_RESTART}" -ne 1 ]; then
    log "已跳过容器重建"
    return 0
  fi

  require_cmd docker
  docker info >/dev/null 2>&1 || fail "Docker 不可用，请先启动 Docker Engine"
  detect_compose_cmd

  if ! detect_compose_file; then
    log "未发现 compose.yml 或 docker-compose.yml，已跳过容器重建"
    return 0
  fi

  if component_enabled "media-core"; then
    services+=("media-core")
  fi
  if component_enabled "media-agent"; then
    services+=("media-agent")
  fi

  if [ "${#services[@]}" -eq 0 ]; then
    log "本次仅更新前端静态文件，无需重建容器"
    return 0
  fi

  log "使用 ${COMPOSE_CMD_DISPLAY} 重建服务: ${services[*]}"
  compose_with_file up -d --force-recreate "${services[@]}"
}

main() {
  parse_args "$@"
  ensure_linux
  load_manifest

  [ -n "${INSTALL_DIR}" ] || fail "请通过 --install-dir 指定现有安装目录"
  [ -d "${INSTALL_DIR}" ] || fail "安装目录不存在: ${INSTALL_DIR}"

  if component_enabled "media-core"; then
    install_binary "${PACKAGE_ROOT}/${MEDIA_CORE_BINARY_PATH}" "${INSTALL_DIR}/bin/media-core" "media-core"
  fi

  if component_enabled "media-agent"; then
    install_binary "${PACKAGE_ROOT}/${MEDIA_AGENT_BINARY_PATH}" "${INSTALL_DIR}/bin/media-agent" "media-agent"
  fi

  if component_enabled "ui"; then
    install_ui_dir "${PACKAGE_ROOT}/${MEDIA_CORE_UI_PATH}" "${INSTALL_DIR}/ui"
  fi

  restart_services_if_needed

  if [ -n "${BACKUP_DIR}" ]; then
    log "旧文件备份目录: ${BACKUP_DIR}"
  fi
}

main "$@"
