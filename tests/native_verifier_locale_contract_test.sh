#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VERIFY_SCRIPT="${REPO_ROOT}/scripts/verify-native-bundle-on-target.sh"

extension_fixture="$(printf '%s\n' \
  $'pgcrypto\t1.4' \
  $'pg_buffercache\t1.6' \
  $'pgrowlocks\t1.2' \
  $'pg_stat_statements\t1.12' \
  $'pgstattuple\t1.5' \
  $'pg_freespacemap\t1.3' \
  | LC_ALL=C sort)"
expected_fixture="$(printf '%s\n' \
  $'pg_buffercache\t1.6' \
  $'pg_freespacemap\t1.3' \
  $'pg_stat_statements\t1.12' \
  $'pgcrypto\t1.4' \
  $'pgrowlocks\t1.2' \
  $'pgstattuple\t1.5')"

[ "${extension_fixture}" = "${expected_fixture}" ] || {
  echo 'C bytewise PostgreSQL extension fixture did not have the expected order' >&2
  exit 1
}

grep -Fq \
  'cut -f1,2 "${extension_manifest}" | LC_ALL=C sort >"${tmp}/expected-extensions.tsv"' \
  "${VERIFY_SCRIPT}" || {
  echo 'native target verifier does not sort the extension manifest with the C locale' >&2
  exit 1
}

grep -Fq 'order by name collate \"C\";' "${VERIFY_SCRIPT}" || {
  echo 'native target verifier does not request C collation from PostgreSQL' >&2
  exit 1
}

echo 'native verifier locale contract tests passed'
