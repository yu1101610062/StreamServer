#!/usr/bin/env bash
set -Eeuo pipefail

INCLUDE_DB_DUMP=0
SEND_WITH_SZ=0

usage() {
  cat <<'EOF'
Usage:
  bash collect-docker-native-migration-info.sh [--include-db-dump] [--sz]

Collect read-only Docker deployment facts for migrating StreamServer to native.

Output defaults to /home/bh/桌面 when available, otherwise /home/bh/Desktop or /tmp.

Notes:
  - Raw environment/config files are saved under secrets/ and may contain passwords.
  - Redacted copies are saved under redacted/.
  - --include-db-dump adds a full custom-format pg_dump. It can be large.
EOF
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --include-db-dump)
      INCLUDE_DB_DUMP=1
      shift
      ;;
    --sz)
      SEND_WITH_SZ=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "unknown argument: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

if [ -d /home/bh/桌面 ]; then
  OUT_BASE="${OUT_BASE:-/home/bh/桌面}"
elif [ -d /home/bh/Desktop ]; then
  OUT_BASE="${OUT_BASE:-/home/bh/Desktop}"
else
  OUT_BASE="${OUT_BASE:-/tmp}"
fi

timestamp="$(date +%Y%m%d_%H%M%S)"
host="$(hostname 2>/dev/null || echo unknown)"
host_safe="$(printf '%s' "${host}" | tr -c 'A-Za-z0-9_.-' '_')"
OUT="${OUT_BASE}/ss_docker_native_migration_${host_safe}_${timestamp}"
ARCHIVE="${OUT}.tar.gz"

mkdir -p \
  "${OUT}/system" \
  "${OUT}/docker" \
  "${OUT}/docker/inspect" \
  "${OUT}/docker/logs" \
  "${OUT}/docker/container_files" \
  "${OUT}/docker/networks" \
  "${OUT}/docker/volumes" \
  "${OUT}/mounts" \
  "${OUT}/db" \
  "${OUT}/health" \
  "${OUT}/native" \
  "${OUT}/secrets/container_env" \
  "${OUT}/secrets/compose" \
  "${OUT}/secrets/docker_inspect" \
  "${OUT}/redacted/container_env" \
  "${OUT}/redacted/compose" \
  "${OUT}/redacted/docker_inspect"
chmod 700 "${OUT}" "${OUT}/secrets" "${OUT}/secrets/container_env" "${OUT}/secrets/compose" "${OUT}/secrets/docker_inspect" 2>/dev/null || true

log() {
  printf '[collect] %s\n' "$*"
}

safe_name() {
  printf '%s' "$1" | sed 's#[^A-Za-z0-9_.-]#_#g'
}

run_cmd() {
  local file="$1"
  shift
  {
    printf '$'
    printf ' %q' "$@"
    printf '\n\n'
    "$@"
  } >"${file}" 2>&1 || true
}

run_shell() {
  local file="$1"
  shift
  {
    printf '$ %s\n\n' "$*"
    bash -lc "$*"
  } >"${file}" 2>&1 || true
}

redact_stream() {
  sed -E \
    -e 's#((^|[^A-Za-z0-9_])(POSTGRES_PASSWORD|DATABASE_URL|JWT_PUBLIC_KEY|AUTH_JWT_PRIVATE_KEY_PATH|AUTH_JWT_PUBLIC_KEY_PATH|HOOK_SHARED_SECRET|ZLM_API_SECRET|ZLM_HOOK_SHARED_SECRET|CALLBACK_SHARED_SECRET|ADMIN_PASSWORD|PASSWORD|PASS|SECRET|TOKEN|KEY)[A-Za-z0-9_]*[[:space:]]*=[[:space:]]*)[^[:space:]]+#\1<redacted>#Ig' \
    -e 's#(postgres(ql)?://[^:/@]+:)[^@/]+#\1<redacted>#Ig' \
    -e 's#("?(POSTGRES_PASSWORD|DATABASE_URL|HOOK_SHARED_SECRET|ZLM_API_SECRET|ZLM_HOOK_SHARED_SECRET|CALLBACK_SHARED_SECRET|ADMIN_PASSWORD|PASSWORD|PASS|SECRET|TOKEN|KEY)[A-Za-z0-9_]*"?[[:space:]]*:[[:space:]]*")[^"]*"#\1<redacted>"#Ig'
}

redact_file() {
  local src="$1"
  local dst="$2"
  if [ -f "${src}" ]; then
    mkdir -p "$(dirname "${dst}")"
    redact_stream <"${src}" >"${dst}" || true
  fi
}

copy_secret_file() {
  local src="$1"
  local dst_dir="$2"
  [ -f "${src}" ] || return 0
  local dst="${dst_dir}/$(safe_name "${src}")"
  cp -a "${src}" "${dst}" 2>/dev/null || cp "${src}" "${dst}" 2>/dev/null || true
  chmod 600 "${dst}" 2>/dev/null || true
}

log "output directory: ${OUT}"

cat >"${OUT}/README.txt" <<EOF
StreamServer Docker to native migration diagnostics
host=${host}
timestamp=${timestamp}

secrets/ contains raw environment/config files and may include passwords.
redacted/ contains best-effort redacted copies.

Run this script on every current StreamServer Docker host, for example .9 and .10.
EOF

log "collecting system facts"
run_cmd "${OUT}/system/date.txt" date -Is
run_cmd "${OUT}/system/hostname.txt" hostname -f
run_cmd "${OUT}/system/uname.txt" uname -a
run_shell "${OUT}/system/os-release.txt" 'cat /etc/os-release 2>/dev/null || true'
run_shell "${OUT}/system/ip_addr.txt" 'ip addr show 2>/dev/null || ifconfig -a 2>/dev/null || true'
run_shell "${OUT}/system/ip_route.txt" 'ip route show 2>/dev/null || route -n 2>/dev/null || true'
run_shell "${OUT}/system/listen_ports.txt" 'ss -lntup 2>/dev/null || netstat -lntup 2>/dev/null || true'
run_shell "${OUT}/system/df.txt" 'df -hT 2>/dev/null || df -h'
run_shell "${OUT}/system/free.txt" 'free -h 2>/dev/null || true'
run_shell "${OUT}/system/lscpu.txt" 'lscpu 2>/dev/null || true'
run_shell "${OUT}/system/firewall.txt" 'firewall-cmd --state 2>/dev/null; firewall-cmd --list-all 2>/dev/null; iptables-save 2>/dev/null | sed -n "1,200p"; true'
run_shell "${OUT}/system/selinux.txt" 'getenforce 2>/dev/null || true'
run_shell "${OUT}/native/systemd_streamserver.txt" 'systemctl list-unit-files "*streamserver*" 2>/dev/null; systemctl list-units "*streamserver*" 2>/dev/null; true'

if ! command -v docker >/dev/null 2>&1; then
  echo "docker command not found" >"${OUT}/docker/docker_missing.txt"
  tar -C "$(dirname "${OUT}")" -czf "${ARCHIVE}" "$(basename "${OUT}")"
  log "archive: ${ARCHIVE}"
  exit 0
fi

log "collecting docker facts"
run_cmd "${OUT}/docker/version.txt" docker version
run_cmd "${OUT}/docker/info.txt" docker info
run_cmd "${OUT}/docker/ps.txt" docker ps
run_cmd "${OUT}/docker/ps_all.txt" docker ps -a
run_cmd "${OUT}/docker/images.txt" docker images
run_cmd "${OUT}/docker/stats_no_stream.txt" docker stats --no-stream
run_cmd "${OUT}/docker/volume_ls.txt" docker volume ls
run_cmd "${OUT}/docker/network_ls.txt" docker network ls

docker ps -a --format '{{.Names}}' \
  | grep -E '(^ss-|streamserver|media-core|media-agent|zlmedia|zlm|postgres)' \
  | sort -u >"${OUT}/docker/containers.txt" || true

if [ ! -s "${OUT}/docker/containers.txt" ]; then
  docker ps -a --format '{{.Names}}' | sort -u >"${OUT}/docker/containers.txt" || true
fi

docker network ls --format '{{.Name}}' | while IFS= read -r network_name; do
  [ -n "${network_name}" ] || continue
  docker network inspect "${network_name}" >"${OUT}/docker/networks/$(safe_name "${network_name}").json" 2>&1 || true
done

docker volume ls --format '{{.Name}}' | while IFS= read -r volume_name; do
  [ -n "${volume_name}" ] || continue
  docker volume inspect "${volume_name}" >"${OUT}/docker/volumes/$(safe_name "${volume_name}").json" 2>&1 || true
done

mount_sources_file="${OUT}/mounts/sources.txt"
: >"${mount_sources_file}"
compose_files_file="${OUT}/docker/compose_file_candidates.txt"
: >"${compose_files_file}"
compose_dirs_file="${OUT}/docker/compose_working_dirs.txt"
: >"${compose_dirs_file}"

while IFS= read -r container; do
  [ -n "${container}" ] || continue
  c_safe="$(safe_name "${container}")"
  log "container: ${container}"

  docker inspect "${container}" >"${OUT}/secrets/docker_inspect/${c_safe}.json" 2>&1 || true
  redact_file "${OUT}/secrets/docker_inspect/${c_safe}.json" "${OUT}/redacted/docker_inspect/${c_safe}.json"
  cp "${OUT}/redacted/docker_inspect/${c_safe}.json" "${OUT}/docker/inspect/${c_safe}.redacted.json" 2>/dev/null || true

  docker logs --timestamps --tail 500 "${container}" >"${OUT}/docker/logs/${c_safe}.log" 2>&1 || true
  docker top "${container}" >"${OUT}/docker/${c_safe}_top.txt" 2>&1 || true
  docker inspect "${container}" \
    --format '{{range .Mounts}}{{println .Type "|" .Source "|" .Destination "|" .RW}}{{end}}' \
    >"${OUT}/docker/${c_safe}_mounts.txt" 2>&1 || true
  awk -F'|' '{gsub(/^ +| +$/, "", $2); if ($2 != "") print $2}' "${OUT}/docker/${c_safe}_mounts.txt" >>"${mount_sources_file}" || true

  docker inspect "${container}" \
    --format 'working_dir={{index .Config.Labels "com.docker.compose.project.working_dir"}}
project={{index .Config.Labels "com.docker.compose.project"}}
config_files={{index .Config.Labels "com.docker.compose.project.config_files"}}
service={{index .Config.Labels "com.docker.compose.service"}}' \
    >"${OUT}/docker/${c_safe}_compose_labels.txt" 2>&1 || true

  docker inspect "${container}" --format '{{index .Config.Labels "com.docker.compose.project.working_dir"}}' 2>/dev/null \
    | sed '/^<no value>$/d;/^$/d' >>"${compose_dirs_file}" || true
  docker inspect "${container}" --format '{{index .Config.Labels "com.docker.compose.project.config_files"}}' 2>/dev/null \
    | tr ',' '\n' | sed '/^<no value>$/d;/^$/d' >>"${compose_files_file}" || true

  docker exec "${container}" sh -lc 'printenv | sort' >"${OUT}/secrets/container_env/${c_safe}.env" 2>&1 || true
  chmod 600 "${OUT}/secrets/container_env/${c_safe}.env" 2>/dev/null || true
  redact_file "${OUT}/secrets/container_env/${c_safe}.env" "${OUT}/redacted/container_env/${c_safe}.env"

  docker exec "${container}" sh -lc '
    echo "--- id ---"; id || true
    echo "--- pwd ---"; pwd || true
    echo "--- root listing ---"; ls -la / 2>/dev/null | sed -n "1,120p" || true
    echo "--- common app dirs ---"
    for d in /opt /opt/streamserver /data /app /workspace /home/streamserver /var/lib/postgresql /var/lib/postgresql/data; do
      [ -e "$d" ] || continue
      echo "### $d"
      ls -la "$d" 2>/dev/null | sed -n "1,120p" || true
    done
  ' >"${OUT}/docker/container_files/${c_safe}_layout.txt" 2>&1 || true
done <"${OUT}/docker/containers.txt"

sort -u "${mount_sources_file}" -o "${mount_sources_file}" || true
sort -u "${compose_dirs_file}" -o "${compose_dirs_file}" || true
sort -u "${compose_files_file}" -o "${compose_files_file}" || true

log "collecting compose/env files from host"
while IFS= read -r compose_dir; do
  [ -n "${compose_dir}" ] || continue
  [ -d "${compose_dir}" ] || continue
  copy_secret_file "${compose_dir}/.env" "${OUT}/secrets/compose"
  find "${compose_dir}" -maxdepth 2 -type f \( \
    -name '.env' -o -name '*.env' -o -name 'compose.yml' -o -name 'compose.yaml' \
    -o -name 'docker-compose.yml' -o -name 'docker-compose.yaml' \
    -o -name '*.toml' -o -name '*.ini' -o -name '*.conf' \) -print 2>/dev/null \
    | while IFS= read -r file; do
        copy_secret_file "${file}" "${OUT}/secrets/compose"
      done
done <"${compose_dirs_file}"

while IFS= read -r compose_file; do
  [ -n "${compose_file}" ] || continue
  [ -f "${compose_file}" ] || continue
  copy_secret_file "${compose_file}" "${OUT}/secrets/compose"
done <"${compose_files_file}"

find "${OUT}/secrets/compose" -type f | while IFS= read -r file; do
  rel="${file#${OUT}/secrets/compose/}"
  redact_file "${file}" "${OUT}/redacted/compose/${rel}"
done

log "collecting mount source metadata"
while IFS= read -r source; do
  [ -n "${source}" ] || continue
  src_safe="$(safe_name "${source}")"
  {
    echo "source=${source}"
    echo "--- stat ---"
    stat "${source}" 2>/dev/null || true
    echo "--- df ---"
    df -hT "${source}" 2>/dev/null || df -h "${source}" 2>/dev/null || true
    echo "--- top-level listing ---"
    ls -la "${source}" 2>/dev/null | sed -n '1,200p' || true
    echo "--- directory tree maxdepth 2 sample ---"
    find "${source}" -maxdepth 2 -type d -print 2>/dev/null | sed -n '1,500p' || true
    echo "--- size sample timeout 20s ---"
    timeout 20 du -sh "${source}" 2>/dev/null || true
  } >"${OUT}/mounts/${src_safe}.txt" 2>&1 || true
done <"${mount_sources_file}"

log "collecting database metadata"
grep -Ei '(postgres)' "${OUT}/docker/containers.txt" >"${OUT}/db/postgres_containers.txt" || true
while IFS= read -r pg_container; do
  [ -n "${pg_container}" ] || continue
  pg_safe="$(safe_name "${pg_container}")"
  docker exec "${pg_container}" sh -lc 'printenv | sort' >"${OUT}/secrets/container_env/${pg_safe}.env" 2>&1 || true
  redact_file "${OUT}/secrets/container_env/${pg_safe}.env" "${OUT}/redacted/container_env/${pg_safe}.env"

  pg_user="$(docker exec "${pg_container}" sh -lc 'printf "%s" "${POSTGRES_USER:-postgres}"' 2>/dev/null || printf postgres)"
  pg_db="$(docker exec "${pg_container}" sh -lc 'printf "%s" "${POSTGRES_DB:-streamserver}"' 2>/dev/null || printf streamserver)"

  docker exec "${pg_container}" sh -lc "pg_isready -U '${pg_user}' -d '${pg_db}'" >"${OUT}/db/${pg_safe}_pg_isready.txt" 2>&1 || true
  docker exec "${pg_container}" sh -lc "pg_dump -s -U '${pg_user}' -d '${pg_db}'" >"${OUT}/db/${pg_safe}_schema.sql" 2>&1 || true
  docker exec "${pg_container}" sh -lc "psql -U '${pg_user}' -d '${pg_db}' -v ON_ERROR_STOP=0 -X -P pager=off" >"${OUT}/db/${pg_safe}_metadata.txt" 2>&1 <<'SQL' || true
select version();
select current_database(), current_user, now();
select table_schema, table_name
from information_schema.tables
where table_schema = 'public'
order by table_schema, table_name;
select column_name, data_type
from information_schema.columns
where table_schema='public'
  and table_name='node_heartbeats'
  and column_name in ('slot_usage', 'runtime_slot_loads')
order by column_name;
select version, description, success, installed_on
from _sqlx_migrations
order by version;
select 'nodes' as table_name, count(*) from nodes where to_regclass('public.nodes') is not null
union all select 'node_heartbeats', count(*) from node_heartbeats where to_regclass('public.node_heartbeats') is not null
union all select 'tasks', count(*) from tasks where to_regclass('public.tasks') is not null
union all select 'task_attempts', count(*) from task_attempts where to_regclass('public.task_attempts') is not null
union all select 'task_events', count(*) from task_events where to_regclass('public.task_events') is not null;
SQL

  if [ "${INCLUDE_DB_DUMP}" -eq 1 ]; then
    log "including full db dump for ${pg_container}"
    docker exec "${pg_container}" sh -lc "pg_dump -U '${pg_user}' -d '${pg_db}' -Fc" >"${OUT}/secrets/${pg_safe}_${pg_db}_full.dump" 2>"${OUT}/db/${pg_safe}_full_dump.err" || true
    chmod 600 "${OUT}/secrets/${pg_safe}_${pg_db}_full.dump" 2>/dev/null || true
  else
    echo "Full DB dump not included. Re-run with --include-db-dump if needed." >"${OUT}/db/full_dump_not_included.txt"
  fi
done <"${OUT}/db/postgres_containers.txt"

log "collecting health probes"
for url in \
  http://127.0.0.1:8080/health/live \
  http://127.0.0.1:8080/health/ready \
  http://127.0.0.1:8081/health/live \
  http://127.0.0.1:8081/health/ready \
  http://127.0.0.1:80/; do
  url_safe="$(safe_name "${url}")"
  run_shell "${OUT}/health/${url_safe}.txt" "curl -fsS -m 5 -i '${url}'"
done

log "writing summary"
{
  echo "archive_will_contain_raw_secrets=yes"
  echo "host=${host}"
  echo "timestamp=${timestamp}"
  echo
  echo "--- containers ---"
  cat "${OUT}/docker/containers.txt" 2>/dev/null || true
  echo
  echo "--- compose working dirs ---"
  cat "${compose_dirs_file}" 2>/dev/null || true
  echo
  echo "--- compose config files ---"
  cat "${compose_files_file}" 2>/dev/null || true
  echo
  echo "--- mount sources ---"
  cat "${mount_sources_file}" 2>/dev/null || true
  echo
  echo "--- key env names by container (redacted values are in redacted/container_env) ---"
  for f in "${OUT}"/redacted/container_env/*.env; do
    [ -f "$f" ] || continue
    echo "### $(basename "$f")"
    grep -E '^(DEPLOY_MODE|INSTALL_ROLE|DATABASE_URL|POSTGRES_|CORE_|AGENT_|NODE_ID|ZLM_|WORK_ROOT|STREAMSERVER_UI_DIR|AUTH_|HOOK_|STORAGE_ALLOWLIST|PUBLIC_|FFMPEG_BIN|FFPROBE_BIN)=' "$f" || true
  done
} >"${OUT}/summary.txt"

log "creating archive"
tar -C "$(dirname "${OUT}")" -czf "${ARCHIVE}" "$(basename "${OUT}")"
chmod 600 "${ARCHIVE}" 2>/dev/null || true

log "archive created: ${ARCHIVE}"
log "raw secrets are included under secrets/. Keep this archive private."

if [ "${SEND_WITH_SZ}" -eq 1 ]; then
  if command -v sz >/dev/null 2>&1; then
    sz "${ARCHIVE}"
  else
    log "sz command not found; archive remains at ${ARCHIVE}"
  fi
else
  log "to transfer with rz/sz, run: sz '${ARCHIVE}'"
fi
