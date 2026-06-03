# 09. Testing

The default Rust test command is:

```bash
cargo test --workspace --all-targets
```

Recommended quality gates:

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
```

Frontend checks:

```bash
cd crates/media-core/frontend
npm run typecheck
npm run test
```

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
