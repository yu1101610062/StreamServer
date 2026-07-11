#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
INSTALLER="${REPO_ROOT}/packaging/native/install.sh"
VERIFY_SCRIPT="${REPO_ROOT}/scripts/verify-native-bundle-on-target.sh"
CONFIG_TUI="${REPO_ROOT}/crates/streamserver-config/src/main.rs"
NATIVE_WORKFLOW="${REPO_ROOT}/.github/workflows/server-native-bundles.yml"
TMP_DIR="$(mktemp -d)"
trap 'rm -rf "${TMP_DIR}"' EXIT

FUNCTIONS_FILE="${TMP_DIR}/install-functions.sh"
sed '/^main "\$@"$/d' "${INSTALLER}" >"${FUNCTIONS_FILE}"
# shellcheck disable=SC1090
source "${FUNCTIONS_FILE}"

assert_contains() {
  local haystack="$1"
  local needle="$2"
  printf '%s' "${haystack}" | grep -Fq -- "${needle}" || {
    printf 'expected output to contain %s\nactual output:\n%s\n' "${needle}" "${haystack}" >&2
    exit 1
  }
}

run_preflight() {
  local env_file="$1"
  local core_bin="$2"
  local output
  local status
  set +e
  output="$(security_preflight_env "${env_file}" "${core_bin}" 2>&1)"
  status=$?
  set -e
  PREFLIGHT_OUTPUT="${output}"
  PREFLIGHT_STATUS="${status}"
}

INSECURE_ENV="${TMP_DIR}/insecure.env"
printf '%s\n' \
  'INSTALL_ROLE=control-plane' \
  'AUTH_MODE=disabled' \
  'DATABASE_URL=postgresql://diagnostic-user:must-not-leak@127.0.0.1/streamserver' \
  'CORE_HTTP_ADDR=0.0.0.0:8080' \
  'CORE_HTTP_TLS_CERT_PATH=' \
  'CORE_HTTP_TLS_KEY_PATH=' \
  'CORE_GRPC_ADDR=0.0.0.0:50051' \
  'CORE_GRPC_TLS_CERT_PATH=' \
  'CORE_GRPC_TLS_KEY_PATH=' \
  'CORE_GRPC_TLS_CLIENT_CA_PATH=' >"${INSECURE_ENV}"

FAKE_CORE="${TMP_DIR}/media-core"
printf '%s\n' \
  '#!/usr/bin/env bash' \
  'set -euo pipefail' \
  'case "$*" in' \
  '  "auth check-admin") [ "${FAKE_ADMIN_PRESENT:-0}" = "1" ] ;;' \
  '  "auth check-config")' \
  '    case "${AUTH_MODE:-}" in' \
  '      external_jwt) key="${JWT_PUBLIC_KEY:-}" ;;' \
  '      local_password) key="$(cat "${AUTH_JWT_PUBLIC_KEY_PATH:-/nonexistent}" 2>/dev/null || true)" ;;' \
  '      *) exit 1 ;;' \
  '    esac' \
  '    if printf "%s" "${key}" | openssl rsa -pubin -noout >/dev/null 2>&1; then exit 0; fi' \
  '    printf "%s" "${key}" | openssl pkey -pubin -text_pub -noout 2>/dev/null | grep -q ED25519 ;;' \
  '  *) exit 64 ;;' \
  'esac' >"${FAKE_CORE}"
chmod 755 "${FAKE_CORE}"

run_preflight "${INSECURE_ENV}" "${FAKE_CORE}"
[ "${PREFLIGHT_STATUS}" -ne 0 ] || {
  echo 'insecure production env unexpectedly passed security preflight' >&2
  exit 1
}
assert_contains "${PREFLIGHT_OUTPUT}" '[MISSING] auth/admin'
assert_contains "${PREFLIGHT_OUTPUT}" '[MISSING] HTTP TLS'
assert_contains "${PREFLIGHT_OUTPUT}" '[MISSING] gRPC mTLS'

UNKNOWN_ROLE_ENV="${TMP_DIR}/unknown-role.env"
printf '%s\n' 'INSTALL_ROLE=unknown' >"${UNKNOWN_ROLE_ENV}"
run_preflight "${UNKNOWN_ROLE_ENV}" "${FAKE_CORE}"
[ "${PREFLIGHT_STATUS}" -ne 0 ]
assert_contains "${PREFLIGHT_OUTPUT}" '[MISSING] configuration'

CA_KEY_FILE="${TMP_DIR}/ca.key"
CA_FILE="${TMP_DIR}/client-ca.pem"
CERT_FILE="${TMP_DIR}/server.pem"
KEY_FILE="${TMP_DIR}/server.key"
CLIENT_CERT_FILE="${TMP_DIR}/client.pem"
CLIENT_KEY_FILE="${TMP_DIR}/client.key"
JWT_PRIVATE_KEY_FILE="${TMP_DIR}/jwt-ed25519-private.pem"
JWT_PUBLIC_KEY_FILE="${TMP_DIR}/jwt-ed25519-public.pem"
EC_PRIVATE_KEY_FILE="${TMP_DIR}/jwt-ec-private.pem"
EC_PUBLIC_KEY_FILE="${TMP_DIR}/jwt-ec-public.pem"

export MSYS2_ARG_CONV_EXCL='/CN='
openssl req -x509 -newkey rsa:2048 -nodes -days 2 -subj '/CN=StreamServer Test CA' \
  -addext 'basicConstraints=critical,CA:TRUE' \
  -keyout "${CA_KEY_FILE}" -out "${CA_FILE}" >/dev/null 2>&1
openssl req -newkey rsa:2048 -nodes -subj '/CN=localhost' \
  -keyout "${KEY_FILE}" -out "${TMP_DIR}/server.csr" >/dev/null 2>&1
openssl x509 -req -days 2 -in "${TMP_DIR}/server.csr" \
  -CA "${CA_FILE}" -CAkey "${CA_KEY_FILE}" -CAcreateserial \
  -out "${CERT_FILE}" >/dev/null 2>&1
openssl req -newkey rsa:2048 -nodes -subj '/CN=streamserver-test-agent' \
  -keyout "${CLIENT_KEY_FILE}" -out "${TMP_DIR}/client.csr" >/dev/null 2>&1
openssl x509 -req -days 2 -in "${TMP_DIR}/client.csr" \
  -CA "${CA_FILE}" -CAkey "${CA_KEY_FILE}" -CAcreateserial \
  -out "${CLIENT_CERT_FILE}" >/dev/null 2>&1
openssl genpkey -algorithm Ed25519 -out "${JWT_PRIVATE_KEY_FILE}" >/dev/null 2>&1
openssl pkey -in "${JWT_PRIVATE_KEY_FILE}" -pubout \
  -out "${JWT_PUBLIC_KEY_FILE}" >/dev/null 2>&1
openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 \
  -out "${EC_PRIVATE_KEY_FILE}" >/dev/null 2>&1
openssl pkey -in "${EC_PRIVATE_KEY_FILE}" -pubout \
  -out "${EC_PUBLIC_KEY_FILE}" >/dev/null 2>&1

SECURE_ENV="${TMP_DIR}/secure.env"
printf '%s\n' \
  'INSTALL_ROLE=control-plane' \
  'AUTH_MODE=local_password' \
  'DATABASE_URL=postgresql://diagnostic-user:super-secret@127.0.0.1/streamserver' \
  "AUTH_JWT_PRIVATE_KEY_PATH=${JWT_PRIVATE_KEY_FILE}" \
  "AUTH_JWT_PUBLIC_KEY_PATH=${JWT_PUBLIC_KEY_FILE}" \
  'CORE_HTTP_ADDR=127.0.0.1:8080' \
  'CORE_HTTP_TLS_CERT_PATH=' \
  'CORE_HTTP_TLS_KEY_PATH=' \
  'CORE_GRPC_ADDR=127.0.0.1:50051' \
  "CORE_GRPC_TLS_CERT_PATH=${CERT_FILE}" \
  "CORE_GRPC_TLS_KEY_PATH=${KEY_FILE}" \
  "CORE_GRPC_TLS_CLIENT_CA_PATH=${CA_FILE}" >"${SECURE_ENV}"

export FAKE_ADMIN_PRESENT=1
run_preflight "${SECURE_ENV}" "${FAKE_CORE}"
[ "${PREFLIGHT_STATUS}" -eq 0 ] || {
  printf 'secure production env failed preflight:\n%s\n' "${PREFLIGHT_OUTPUT}" >&2
  exit 1
}
assert_contains "${PREFLIGHT_OUTPUT}" '[OK] auth/admin'
assert_contains "${PREFLIGHT_OUTPUT}" '[OK] HTTP TLS'
assert_contains "${PREFLIGHT_OUTPUT}" '[OK] gRPC mTLS'

UNSUPPORTED_LOCAL_KEY_ENV="${TMP_DIR}/unsupported-local-key.env"
printf '%s\n' \
  'INSTALL_ROLE=control-plane' \
  'AUTH_MODE=local_password' \
  'DATABASE_URL=postgresql://127.0.0.1/unused' \
  "AUTH_JWT_PRIVATE_KEY_PATH=${EC_PRIVATE_KEY_FILE}" \
  "AUTH_JWT_PUBLIC_KEY_PATH=${EC_PUBLIC_KEY_FILE}" \
  'CORE_HTTP_ADDR=127.0.0.1:8080' \
  'CORE_HTTP_TLS_CERT_PATH=' \
  'CORE_HTTP_TLS_KEY_PATH=' \
  'CORE_GRPC_ADDR=127.0.0.1:50051' \
  "CORE_GRPC_TLS_CERT_PATH=${CERT_FILE}" \
  "CORE_GRPC_TLS_KEY_PATH=${KEY_FILE}" \
  "CORE_GRPC_TLS_CLIENT_CA_PATH=${CA_FILE}" >"${UNSUPPORTED_LOCAL_KEY_ENV}"
run_preflight "${UNSUPPORTED_LOCAL_KEY_ENV}" "${FAKE_CORE}"
[ "${PREFLIGHT_STATUS}" -ne 0 ] || {
  echo 'unsupported EC local_password JWT key pair unexpectedly passed preflight' >&2
  exit 1
}
assert_contains "${PREFLIGHT_OUTPUT}" '[INVALID] auth/admin: local_password JWT configuration'

RELATIVE_ENV="${TMP_DIR}/relative.env"
sed "s|${TMP_DIR}/||g" "${SECURE_ENV}" >"${RELATIVE_ENV}"
run_preflight "${RELATIVE_ENV}" "${FAKE_CORE}"
[ "${PREFLIGHT_STATUS}" -eq 0 ] || {
  printf 'relative security paths failed preflight:\n%s\n' "${PREFLIGHT_OUTPUT}" >&2
  exit 1
}
assert_contains "${PREFLIGHT_OUTPUT}" '[OK] auth/admin'
assert_contains "${PREFLIGHT_OUTPUT}" '[OK] gRPC mTLS'

export FAKE_ADMIN_PRESENT=0
run_preflight "${SECURE_ENV}" "${FAKE_CORE}"
[ "${PREFLIGHT_STATUS}" -ne 0 ]
assert_contains "${PREFLIGHT_OUTPUT}" '[MISSING] auth/admin'
if printf '%s' "${PREFLIGHT_OUTPUT}" | grep -Fq 'super-secret'; then
  echo 'security preflight leaked DATABASE_URL credentials' >&2
  exit 1
fi
export FAKE_ADMIN_PRESENT=1

INVALID_CA_FILE="${TMP_DIR}/invalid-ca.pem"
: >"${INVALID_CA_FILE}"
INVALID_TLS_ENV="${TMP_DIR}/invalid-tls.env"
sed "s|CORE_GRPC_TLS_CLIENT_CA_PATH=.*|CORE_GRPC_TLS_CLIENT_CA_PATH=${INVALID_CA_FILE}|" \
  "${SECURE_ENV}" >"${INVALID_TLS_ENV}"
run_preflight "${INVALID_TLS_ENV}" "${FAKE_CORE}"
[ "${PREFLIGHT_STATUS}" -ne 0 ]
assert_contains "${PREFLIGHT_OUTPUT}" '[INVALID] gRPC mTLS'

PARTIAL_HTTP_ENV="${TMP_DIR}/partial-http.env"
sed "s|CORE_HTTP_TLS_CERT_PATH=.*|CORE_HTTP_TLS_CERT_PATH=${CERT_FILE}|" \
  "${SECURE_ENV}" >"${PARTIAL_HTTP_ENV}"
run_preflight "${PARTIAL_HTTP_ENV}" "${FAKE_CORE}"
[ "${PREFLIGHT_STATUS}" -ne 0 ]
assert_contains "${PREFLIGHT_OUTPUT}" '[MISSING] HTTP TLS'

WORKER_ENV="${TMP_DIR}/worker.env"
printf '%s\n' \
  'INSTALL_ROLE=worker-host-cpu' \
  'AGENT_CORE_ENDPOINT=http://core.example.test:50051' \
  "AGENT_CERT_PATH=${CLIENT_CERT_FILE}" \
  "AGENT_KEY_PATH=${CLIENT_KEY_FILE}" \
  "AGENT_CA_PATH=${CA_FILE}" \
  'AGENT_TLS_DOMAIN_NAME=core.example.test' >"${WORKER_ENV}"
run_preflight "${WORKER_ENV}" "${FAKE_CORE}"
[ "${PREFLIGHT_STATUS}" -ne 0 ]
assert_contains "${PREFLIGHT_OUTPUT}" '[MISSING] worker mTLS'

sed 's|AGENT_CORE_ENDPOINT=http://|AGENT_CORE_ENDPOINT=https://|' \
  "${WORKER_ENV}" >"${TMP_DIR}/secure-worker.env"
run_preflight "${TMP_DIR}/secure-worker.env" "${FAKE_CORE}"
[ "${PREFLIGHT_STATUS}" -eq 0 ] || {
  printf 'secure worker env failed preflight:\n%s\n' "${PREFLIGHT_OUTPUT}" >&2
  exit 1
}
assert_contains "${PREFLIGHT_OUTPUT}" '[OK] worker mTLS'

sed "s|${TMP_DIR}/||g" "${TMP_DIR}/secure-worker.env" >"${TMP_DIR}/relative-worker.env"
run_preflight "${TMP_DIR}/relative-worker.env" "${FAKE_CORE}"
[ "${PREFLIGHT_STATUS}" -eq 0 ] || {
  printf 'relative worker TLS paths failed preflight:\n%s\n' "${PREFLIGHT_OUTPUT}" >&2
  exit 1
}
assert_contains "${PREFLIGHT_OUTPUT}" '[OK] worker mTLS'

(
  INSTALL_DIR="${TMP_DIR}/fresh-install"
  INSTALL_ROLE="control-plane"
  mkdir -p "${INSTALL_DIR}/certs/auth"
  prompt() { printf '%s' "${2:-}"; }
  prompt_non_empty() { printf '%s' "$2"; }
  prompt_local_tcp_port() { printf '%s' "$4"; }
  prompt_password_with_confirmation() { printf '%s' 'contract-test-password'; }

  configure_core_values
  [ "${AUTH_MODE}" = "local_password" ]
  [ "${AUTH_ENABLED}" = "true" ]
  [ "${ADMIN_BOOTSTRAP_REQUIRED}" -eq 1 ]
  [ "${CORE_HTTP_ADDR}" = "127.0.0.1:8080" ]
  [ "${CORE_GRPC_ADDR}" = "127.0.0.1:50051" ]
  validate_private_public_key_pair \
    "${AUTH_JWT_PRIVATE_KEY_PATH}" "${AUTH_JWT_PUBLIC_KEY_PATH}"
)

UPGRADE_DIR="${TMP_DIR}/upgrade-install"
mkdir -p "${UPGRADE_DIR}/certs/auth"
cp "${JWT_PRIVATE_KEY_FILE}" "${UPGRADE_DIR}/certs/auth/jwt-private.pem"
cp "${JWT_PUBLIC_KEY_FILE}" "${UPGRADE_DIR}/certs/auth/jwt-public.pem"
printf '%s\n' \
  'AUTH_MODE=local_password' \
  'AUTH_JWT_PRIVATE_KEY_PATH=certs/auth/jwt-private.pem' \
  'AUTH_JWT_PUBLIC_KEY_PATH=certs/auth/jwt-public.pem' \
  'CORE_HTTP_ADDR=127.0.0.1:18080' \
  'CORE_HTTP_PORT=18080' \
  'CORE_HTTP_TLS_CERT_PATH=certs/http.pem' \
  'CORE_HTTP_TLS_KEY_PATH=certs/http.key' \
  'CORE_GRPC_ADDR=127.0.0.1:15051' \
  'CORE_GRPC_PORT=15051' \
  'CORE_GRPC_TLS_CERT_PATH=certs/grpc.pem' \
  'CORE_GRPC_TLS_KEY_PATH=certs/grpc.key' \
  'CORE_GRPC_TLS_CLIENT_CA_PATH=certs/client-ca.pem' >"${UPGRADE_DIR}/.env"
UPGRADE_KEY_HASH="$(sha256sum "${UPGRADE_DIR}/certs/auth/jwt-private.pem" | awk '{print $1}')"
(
  INSTALL_DIR="${UPGRADE_DIR}"
  INSTALL_ROLE="control-plane"
  prompt() { printf '%s' "${2:-}"; }
  prompt_non_empty() { printf '%s' "$2"; }
  prompt_local_tcp_port() { printf '%s' "$4"; }

  configure_core_values
  [ "${AUTH_MODE}" = "local_password" ]
  [ "${ADMIN_BOOTSTRAP_REQUIRED}" -eq 0 ]
  [ "${AUTH_JWT_PRIVATE_KEY_PATH}" = "certs/auth/jwt-private.pem" ]
  [ "${CORE_HTTP_TLS_CERT_PATH}" = "certs/http.pem" ]
  [ "${CORE_GRPC_TLS_CLIENT_CA_PATH}" = "certs/client-ca.pem" ]
)
[ "${UPGRADE_KEY_HASH}" = "$(sha256sum "${UPGRADE_DIR}/certs/auth/jwt-private.pem" | awk '{print $1}')" ]

EXTERNAL_DIR="${TMP_DIR}/external-upgrade"
mkdir -p "${EXTERNAL_DIR}"
: >"${EXTERNAL_DIR}/.env"
EXPECTED_EXTERNAL_KEY="$(tr -d '\r' <"${JWT_PUBLIC_KEY_FILE}")"
write_env_entry "${EXTERNAL_DIR}/.env" INSTALL_ROLE control-plane
write_env_entry "${EXTERNAL_DIR}/.env" AUTH_MODE external_jwt
write_env_entry "${EXTERNAL_DIR}/.env" JWT_PUBLIC_KEY "${EXPECTED_EXTERNAL_KEY}"
write_env_entry "${EXTERNAL_DIR}/.env" DATABASE_URL postgresql://127.0.0.1/unused
write_env_entry "${EXTERNAL_DIR}/.env" CORE_HTTP_ADDR 127.0.0.1:8080
write_env_entry "${EXTERNAL_DIR}/.env" CORE_HTTP_TLS_CERT_PATH ''
write_env_entry "${EXTERNAL_DIR}/.env" CORE_HTTP_TLS_KEY_PATH ''
write_env_entry "${EXTERNAL_DIR}/.env" CORE_GRPC_ADDR 127.0.0.1:50051
write_env_entry "${EXTERNAL_DIR}/.env" CORE_GRPC_TLS_CERT_PATH "${CERT_FILE}"
write_env_entry "${EXTERNAL_DIR}/.env" CORE_GRPC_TLS_KEY_PATH "${KEY_FILE}"
write_env_entry "${EXTERNAL_DIR}/.env" CORE_GRPC_TLS_CLIENT_CA_PATH "${CA_FILE}"
STORED_EXTERNAL_KEY="$(existing_env_value "${EXTERNAL_DIR}/.env" JWT_PUBLIC_KEY)"
[ "${STORED_EXTERNAL_KEY}" = "${EXPECTED_EXTERNAL_KEY}" ] || {
  printf 'multiline external JWT public key did not round-trip through EnvironmentFile (stored=%s bytes, expected=%s bytes)\n' \
    "${#STORED_EXTERNAL_KEY}" "${#EXPECTED_EXTERNAL_KEY}" >&2
  exit 1
}
(
  INSTALL_DIR="${EXTERNAL_DIR}"
  INSTALL_ROLE="control-plane"
  prompt() { printf '%s' "${2:-}"; }
  prompt_non_empty() { printf '%s' "$2"; }
  prompt_local_tcp_port() { printf '%s' "$4"; }

  configure_core_values
  [ "${AUTH_MODE}" = "external_jwt" ]
  [ "${AUTH_ENABLED}" = "true" ]
  [ "${JWT_PUBLIC_KEY}" = "${EXPECTED_EXTERNAL_KEY}" ]
  [ "${ADMIN_BOOTSTRAP_REQUIRED}" -eq 0 ]
)
run_preflight "${EXTERNAL_DIR}/.env" "${FAKE_CORE}"
[ "${PREFLIGHT_STATUS}" -eq 0 ] || {
  printf 'valid multiline external JWT key failed preflight:\n%s\n' "${PREFLIGHT_OUTPUT}" >&2
  exit 1
}
assert_contains "${PREFLIGHT_OUTPUT}" '[OK] auth/admin: external_jwt public key verified'

MALFORMED_EXTERNAL_ENV="${TMP_DIR}/malformed-external.env"
: >"${MALFORMED_EXTERNAL_ENV}"
write_env_entry "${MALFORMED_EXTERNAL_ENV}" INSTALL_ROLE control-plane
write_env_entry "${MALFORMED_EXTERNAL_ENV}" AUTH_MODE external_jwt
write_env_entry "${MALFORMED_EXTERNAL_ENV}" JWT_PUBLIC_KEY not-a-public-key
write_env_entry "${MALFORMED_EXTERNAL_ENV}" DATABASE_URL postgresql://127.0.0.1/unused
write_env_entry "${MALFORMED_EXTERNAL_ENV}" CORE_HTTP_ADDR 127.0.0.1:8080
write_env_entry "${MALFORMED_EXTERNAL_ENV}" CORE_HTTP_TLS_CERT_PATH ''
write_env_entry "${MALFORMED_EXTERNAL_ENV}" CORE_HTTP_TLS_KEY_PATH ''
write_env_entry "${MALFORMED_EXTERNAL_ENV}" CORE_GRPC_ADDR 127.0.0.1:50051
write_env_entry "${MALFORMED_EXTERNAL_ENV}" CORE_GRPC_TLS_CERT_PATH "${CERT_FILE}"
write_env_entry "${MALFORMED_EXTERNAL_ENV}" CORE_GRPC_TLS_KEY_PATH "${KEY_FILE}"
write_env_entry "${MALFORMED_EXTERNAL_ENV}" CORE_GRPC_TLS_CLIENT_CA_PATH "${CA_FILE}"
run_preflight "${MALFORMED_EXTERNAL_ENV}" "${FAKE_CORE}"
[ "${PREFLIGHT_STATUS}" -ne 0 ] || {
  echo 'malformed external JWT public key unexpectedly passed preflight' >&2
  exit 1
}
assert_contains "${PREFLIGHT_OUTPUT}" '[INVALID] auth/admin: external_jwt public key'

grep -Fq -- '--upgrade' "${INSTALLER}"
grep -Fq -- '--security-preflight' "${INSTALLER}"
grep -Fq 'security_preflight_env "${INSTALL_DIR}/.env"' "${INSTALLER}"
grep -Fq 'env_value_or_default "${existing_env_file}" "AUTH_MODE" "local_password"' "${INSTALLER}"
grep -Fq 'write_env_entry "${env_file}" CORE_HTTP_TLS_CERT_PATH' "${INSTALLER}"
grep -Fq 'write_env_entry "${env_file}" CORE_HTTP_TLS_KEY_PATH' "${INSTALLER}"
grep -Fq 'write_env_entry "${env_file}" CORE_GRPC_TLS_CERT_PATH' "${INSTALLER}"
grep -Fq 'write_env_entry "${env_file}" CORE_GRPC_TLS_KEY_PATH' "${INSTALLER}"
grep -Fq 'write_env_entry "${env_file}" CORE_GRPC_TLS_CLIENT_CA_PATH' "${INSTALLER}"
grep -Fq 'write_env_entry "${env_file}" AGENT_CERT_PATH' "${INSTALLER}"
grep -Fq 'write_env_entry "${env_file}" AGENT_KEY_PATH' "${INSTALLER}"
grep -Fq 'write_env_entry "${env_file}" AGENT_CA_PATH' "${INSTALLER}"
grep -Fq 'write_env_entry "${env_file}" AGENT_TLS_DOMAIN_NAME' "${INSTALLER}"
grep -Fq 'write_env_entry "${env_file}" AGENT_CORE_ENDPOINT "https://' "${INSTALLER}"
if grep -Fq 'write_env_entry "${env_file}" CORE_HTTP_ADDR "0.0.0.0:' "${INSTALLER}"; then
  echo 'native installer still hard-codes a public plaintext HTTP bind' >&2
  exit 1
fi
if grep -Fq 'write_env_entry "${env_file}" CORE_GRPC_ADDR "0.0.0.0:' "${INSTALLER}"; then
  echo 'native installer still hard-codes a public gRPC bind' >&2
  exit 1
fi
grep -Fq 'default_if_missing(values, "AUTH_MODE", "local_password");' "${CONFIG_TUI}"
grep -Fq '"${ROOT}/binaries/media-core-linux-amd64" --insecure-dev' "${VERIFY_SCRIPT}"
grep -Fq 'tests/native_security_contract_test.sh' "${NATIVE_WORKFLOW}"
grep -Fq 'bash -n packaging/native/install.sh' "${NATIVE_WORKFLOW}"

tui_line="$(grep -n '^  run_streamserver_config_tui_if_requested$' "${INSTALLER}" | cut -d: -f1)"
preflight_line="$(grep -n '^  prepare_production_security_state$' "${INSTALLER}" | cut -d: -f1)"
[ -n "${tui_line}" ] && [ -n "${preflight_line}" ] && [ "${preflight_line}" -gt "${tui_line}" ] || {
  echo 'production security preflight must run after the optional TUI save' >&2
  exit 1
}

echo 'native security contract tests passed'
