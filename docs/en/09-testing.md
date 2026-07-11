# 09. Testing

## Linux CI parity

Server release gates run on Linux AMD64 in `.github/workflows/server-ci.yml`.
Use Rust 1.85.0, Node.js 20, PostgreSQL 16 and 18.3, and the following commands from
the repository root. The PostgreSQL account must be able to create and drop
temporary databases. This copy-pasteable example uses an ephemeral test-only
container for each database version.

```bash
set -euo pipefail
test "$(uname -s)" = "Linux"
test "$(uname -m)" = "x86_64"
sudo apt-get update
sudo apt-get install -y --no-install-recommends \
  libdbus-1-dev pkg-config postgresql-client protobuf-compiler
rustup toolchain install 1.85.0 \
  --profile minimal --component rustfmt --component clippy
rustup default 1.85.0
python3 tests/ci_workflow_contract_test.py
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cleanup_postgres() {
  docker rm -f streamserver-ci-postgres >/dev/null 2>&1 || true
}
trap cleanup_postgres EXIT
for POSTGRES_VERSION in 16 18.3; do
  cleanup_postgres
  docker run --rm --detach --name streamserver-ci-postgres \
    --env POSTGRES_PASSWORD=test --publish 5432:5432 "postgres:${POSTGRES_VERSION}"
  export TEST_DATABASE_URL=postgresql://postgres:test@127.0.0.1:5432/postgres
  timeout 60 bash -c 'until psql "${TEST_DATABASE_URL}" -c "select 1" >/dev/null 2>&1; do sleep 1; done'
  cargo test --workspace --all-targets
  cleanup_postgres
done
trap - EXIT
(
  cd crates/media-core/frontend
  npm ci
  npm run typecheck
  npm run test
)
```

The Rust workspace tests run once against PostgreSQL 16 and once against 18.3.
These are server gates for the Linux AMD64 target. A whole-workspace failure
seen only on Windows, including a `media-agent` compile failure, is not a server regression.
Reproduce it on Linux AMD64 before classifying it as one. Desktop packaging
remains in `desktop-client.yml`.

The native bundle build is intentionally separate in
`server-native-bundles.yml`; it is not part of the fast server quality gate.

Native/release checks:

```bash
./scripts/build-native-bundle.sh --without-gpu
./scripts/smoke-codec-matrix.sh
./scripts/verify-native-bundle-on-target.sh --bundle <bundle> --host <target-host>
```

## Coverage Focus

- TaskSpec validation.
- Task state-machine transitions.
- Idempotent requests.
- Attempt and lease fencing.
- Stale Agent message protection.
- Repository behavior and PostgreSQL migrations.
- Control-plane dispatch and callbacks.
- Agent runtime registry and process lifecycle.
- FFmpeg execution planning and media policy.
- Recording, HLS, MP4, RTMP, RTSP, and artifact cleanup paths.
- Frontend shared logic and media-link behavior.
- Native bundle layout and target-host verification.
