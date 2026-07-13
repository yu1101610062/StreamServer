#!/usr/bin/env bash
set -Eeuo pipefail

readonly SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
readonly REPO_ROOT="$(cd -- "${SCRIPT_DIR}/.." && pwd)"
readonly MIGRATIONS_DIR="${REPO_ROOT}/migrations"

: "${TEST_DATABASE_URL:?TEST_DATABASE_URL must point to a disposable PostgreSQL admin database}"
command -v psql >/dev/null 2>&1 || {
  echo "psql is required" >&2
  exit 1
}

readonly NODE_ID="00000000-0000-0000-0000-000000000014"
readonly TEST_SCHEMA="streamserver_m0014_${BASHPID}_$(date +%s%N)"
[[ "${TEST_SCHEMA}" =~ ^[a-z0-9_]+$ ]] || {
  echo "generated unsafe test schema name" >&2
  exit 1
}

psql_admin() {
  PGOPTIONS="-c client_min_messages=warning" \
    psql "${TEST_DATABASE_URL}" --no-psqlrc --set ON_ERROR_STOP=1 "$@"
}

psql_test() {
  PGOPTIONS="-c client_min_messages=warning -c search_path=${TEST_SCHEMA}" \
    psql "${TEST_DATABASE_URL}" --no-psqlrc --set ON_ERROR_STOP=1 "$@"
}

cleanup() {
  local status=$?
  trap - EXIT
  if ! psql_admin --quiet --command "drop schema if exists \"${TEST_SCHEMA}\" cascade" >/dev/null; then
    echo "failed to clean migration test schema ${TEST_SCHEMA}" >&2
    status=1
  fi
  exit "${status}"
}
trap cleanup EXIT

psql_admin --quiet --command "create schema \"${TEST_SCHEMA}\""
active_schema="$({ psql_test --tuples-only --no-align --command 'select current_schema()'; } | tr -d '\r')"
[[ "${active_schema}" == "${TEST_SCHEMA}" ]] || {
  echo "migration test escaped its isolated schema: ${active_schema}" >&2
  exit 1
}

for version in $(seq 1 13); do
  printf -v migration_pattern '%s/%04d_*.sql' "${MIGRATIONS_DIR}" "${version}"
  mapfile -t migrations < <(compgen -G "${migration_pattern}" | sort)
  if [[ ${#migrations[@]} -ne 1 ]]; then
    echo "expected exactly one migration for version ${version}, found ${#migrations[@]}" >&2
    exit 1
  fi
  psql_test --quiet --file "${migrations[0]}" >/dev/null
done

psql_test --quiet <<SQL >/dev/null
insert into media_nodes (
  id,
  node_name,
  hostname,
  zlm_api_base,
  agent_stream_addr,
  network_mode,
  healthy,
  control_connected,
  last_seen_at,
  control_last_seen_at,
  media_last_seen_at
) values (
  '${NODE_ID}',
  'legacy-online-node',
  'legacy-online-node',
  'http://127.0.0.1:8080',
  '127.0.0.1:50051',
  'host',
  true,
  true,
  clock_timestamp(),
  clock_timestamp(),
  clock_timestamp()
);
SQL

before_state="$({ psql_test --tuples-only --no-align --command \
  "select healthy::text || '|' || control_connected::text from media_nodes where id = '${NODE_ID}'"; } | tr -d '\r')"
[[ "${before_state}" == "true|true" ]] || {
  echo "invalid legacy fixture state: ${before_state}" >&2
  exit 1
}

psql_test --quiet --file "${MIGRATIONS_DIR}/0014_agent_identity.sql" >/dev/null

after_state="$({ psql_test --tuples-only --no-align --command \
  "select i.status || '|' || n.healthy::text || '|' || n.control_connected::text || '|' || (n.last_seen_at is not null)::text || '|' || (n.control_last_seen_at is not null)::text || '|' || (n.media_last_seen_at is not null)::text from media_nodes n join agent_identities i on i.node_id = n.id where n.id = '${NODE_ID}'"; } | tr -d '\r')"
[[ "${after_state}" == "pending_enrollment|false|false|true|true|true" ]] || {
  echo "0014 left legacy node in unsafe state: ${after_state}" >&2
  exit 1
}

printf '0014 legacy online node migration: PASS (%s)\n' "${after_state}"
