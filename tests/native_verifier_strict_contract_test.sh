#!/usr/bin/env bash
set -euo pipefail

if [ "$(uname -s)" != Linux ]; then
  echo 'SKIP: native verifier strict contract requires Linux' >&2
  exit 77
fi

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VERIFY_SCRIPT="${REPO_ROOT}/scripts/verify-native-bundle-on-target.sh"
BUNDLE_BUILDER="${REPO_ROOT}/scripts/build-native-bundle.sh"
NATIVE_WORKFLOW="${REPO_ROOT}/.github/workflows/server-native-bundles.yml"
CONTRACT_TMP="$(mktemp -d)"
cleanup_contract_tmp() {
  if [ "${KEEP_NATIVE_VERIFIER_CONTRACT_TMP:-0}" = 1 ]; then
    printf 'kept native verifier contract tmp: %s\n' "${CONTRACT_TMP}" >&2
  else
    rm -rf -- "${CONTRACT_TMP}"
  fi
}
trap cleanup_contract_tmp EXIT

FAILURES=0
contract_failure() {
  printf '%s\n' "$*" >&2
  FAILURES=$((FAILURES + 1))
}

extract_remote_verifier() {
  awk '
    !capturing && index($0, "REMOTE_SCRIPT_LOCAL") && index($0, "<<") {
      capturing = 1
      next
    }
    capturing && $0 == "REMOTE" { exit }
    capturing { print }
  ' "${VERIFY_SCRIPT}"
}

write_minimal_manifest() {
  local root="$1"
  printf '%s\n' \
    'BUNDLE_VERSION=v0.1.0' \
    'BUNDLE_VARIANT=control-plane-minimal' \
    'BUNDLE_GPU_SUPPORT=false' \
    'BUNDLE_WORKER_SUPPORT=false' \
    'BUNDLE_POSTGRES_RUNTIME=false' \
    'DEPLOY_MODE=native' \
    'MEDIA_CORE_BINARY_PATH=binaries/media-core-linux-amd64' \
    'MEDIA_AGENT_BINARY_PATH=binaries/media-agent-linux-amd64' \
    'MEDIA_GATEWAY_BINARY_PATH=binaries/media-gateway-linux-amd64' \
    'STREAMSERVER_CONFIG_BINARY_PATH=binaries/streamserver-config-linux-amd64' \
    'MEDIA_CORE_UI_PATH=ui/media-core' \
    'FFMPEG_CPU_BINARY_PATH=runtime/ffmpeg/cpu/bin/ffmpeg' \
    'FFPROBE_CPU_BINARY_PATH=runtime/ffmpeg/cpu/bin/ffprobe' \
    'FFMPEG_CPU_LIB_PATH=runtime/ffmpeg/cpu/lib' \
    'FFMPEG_GPU_BINARY_PATH=runtime/ffmpeg/gpu/bin/ffmpeg' \
    'FFPROBE_GPU_BINARY_PATH=runtime/ffmpeg/gpu/bin/ffprobe' \
    'FFMPEG_GPU_LIB_PATH=runtime/ffmpeg/gpu/lib' \
    'ZLM_BINARY_PATH=runtime/zlm/MediaServer' \
    'ZLM_DEFAULT_PEM_PATH=runtime/zlm/default.pem' \
    'ZLM_LIB_PATH=runtime/zlm/lib' \
    'POSTGRES_RUNTIME_PATH=runtime/postgres' \
    'POSTGRES_BIN_PATH=runtime/postgres/bin' \
    'POSTGRES_LIB_PATH=runtime/postgres/lib' \
    'POSTGRES_EXTENSION_MANIFEST_PATH=runtime/postgres/postgres-extension-manifest.tsv' \
    >"${root}/package-manifest.env"
}

write_checksums() {
  local root="$1"
  find "${root}" \( -type f -o -type d \) \
    -exec chmod go-w,u-s,g-s,o-t {} +
  (
    cd "${root}"
    find . -type f ! -name SHA256SUMS -print \
      | LC_ALL=C sort \
      | while IFS= read -r file; do
          sha256sum "${file#./}"
        done >SHA256SUMS
  )
  chmod 644 "${root}/SHA256SUMS"
}

write_build_info() {
  local root="$1"
  local variant="${2:-control-plane-minimal}"
  printf '%s\n' \
    "bundle_name=$(basename "${root}")" \
    'version=0.1.0' \
    'built_at=2026-01-01T00:00:00Z' \
    'builder_os=Linux' \
    'builder_arch=x86_64' \
    'git_commit=contract' \
    "bundle_variant=${variant}" \
    'target_runtime=docker-free' \
    'verification_recommended_location=target-server' \
    >"${root}/build-info.txt"
}

run_fixture() {
  local archive="$1"
  local name="$2"
  set +e
  STREAMSERVER_VERIFY_BUNDLE="${archive}" \
  STREAMSERVER_VERIFY_DIR="${CONTRACT_TMP}/${name}-work" \
  STREAMSERVER_VERIFY_REPORT="${CONTRACT_TMP}/${name}.report" \
    bash "${REMOTE_BODY}" >/dev/null 2>&1
  FIXTURE_STATUS=$?
  set -e
  [ "${FIXTURE_STATUS}" -ne 0 ] || \
    contract_failure "unsafe or synthetic fixture unexpectedly passed: ${name}"
}

REMOTE_BODY="${CONTRACT_TMP}/remote-verifier.sh"
extract_remote_verifier >"${REMOTE_BODY}"
bash -n "${REMOTE_BODY}" || {
  echo 'generated native target verifier body has invalid shell syntax' >&2
  exit 1
}
grep -Fq 'section "Package Shape"' "${REMOTE_BODY}" || {
  echo 'could not extract native target verifier body' >&2
  exit 1
}

STAGE="${CONTRACT_TMP}/stage"
ROOT="${STAGE}/streamserver-native-v0.1.0-linux-amd64-control-plane-minimal-20260101"
mkdir -p "${ROOT}/binaries" "${ROOT}/ui/media-core"
write_minimal_manifest "${ROOT}"
write_build_info "${ROOT}"
printf '%s\n' '<!doctype html><title>contract</title>' \
  >"${ROOT}/ui/media-core/index.html"
for binary in media-core media-agent media-gateway streamserver-config; do
  printf '%s\n' '#!/usr/bin/env sh' 'exit 0' \
    >"${ROOT}/binaries/${binary}-linux-amd64"
  chmod 755 "${ROOT}/binaries/${binary}-linux-amd64"
done
write_checksums "${ROOT}"

printf '%s\n' \
  '#!/usr/bin/env sh' \
  "touch '${CONTRACT_TMP}/gate-one-executed'" \
  'exit 0' \
  >"${ROOT}/binaries/media-core-linux-amd64"
chmod 755 "${ROOT}/binaries/media-core-linux-amd64"
write_checksums "${ROOT}"
printf '%s\n' tampered >>"${ROOT}/ui/media-core/index.html"
tar -czf "${CONTRACT_TMP}/gate-one.tar.gz" \
  -C "${STAGE}" "$(basename "${ROOT}")"
run_fixture "${CONTRACT_TMP}/gate-one.tar.gz" gate-one
[ ! -e "${CONTRACT_TMP}/gate-one-executed" ] || \
  contract_failure 'package code executed after the structure/checksum gate failed'

rm -f -- "${CONTRACT_TMP}/gate-one-executed"
printf '%s\n' '<!doctype html><title>contract</title>' \
  >"${ROOT}/ui/media-core/index.html"
printf '%s\n' \
  '#!/usr/bin/env sh' \
  "touch '${CONTRACT_TMP}/gate-two-executed'" \
  'exit 0' \
  >"${ROOT}/binaries/media-core-linux-amd64"
write_checksums "${ROOT}"
tar -czf "${CONTRACT_TMP}/gate-two.tar.gz" \
  -C "${STAGE}" "$(basename "${ROOT}")"
run_fixture "${CONTRACT_TMP}/gate-two.tar.gz" gate-two
[ ! -e "${CONTRACT_TMP}/gate-two-executed" ] || \
  contract_failure 'package code executed after binary/runtime inspection failed'

rm -f -- "${CONTRACT_TMP}/gate-two-executed"
LARGE_INSPECT_BIN="${CONTRACT_TMP}/large-inspect-bin"
mkdir -p "${LARGE_INSPECT_BIN}"
cat >"${LARGE_INSPECT_BIN}/file" <<'SH'
#!/usr/bin/env sh
printf '%s\n' 'ELF 64-bit LSB executable, x86-64, statically linked, stripped'
SH
cat >"${LARGE_INSPECT_BIN}/readelf" <<'SH'
#!/usr/bin/env bash
case " $* " in
  *' -h '*)
    printf '%s\n' \
      '  Class:                             ELF64' \
      "  Data:                              2's complement, little endian" \
      '  Machine:                           Advanced Micro Devices X86-64'
    python3 -c 'import sys; sys.stdout.write("x" * (2 * 1024 * 1024))'
    ;;
  *' -l '*) printf '%s\n' 'There are no program headers.' ;;
  *' -d '*) printf '%s\n' 'There is no dynamic section.' ;;
esac
SH
chmod 755 "${LARGE_INSPECT_BIN}/file" "${LARGE_INSPECT_BIN}/readelf"
tar -czf "${CONTRACT_TMP}/large-inspect.tar.gz" \
  -C "${STAGE}" "$(basename "${ROOT}")"
set +e
PATH="${LARGE_INSPECT_BIN}:${PATH}" \
STREAMSERVER_VERIFY_BUNDLE="${CONTRACT_TMP}/large-inspect.tar.gz" \
STREAMSERVER_VERIFY_DIR="${CONTRACT_TMP}/large-inspect-work" \
STREAMSERVER_VERIFY_REPORT="${CONTRACT_TMP}/large-inspect.report" \
  bash "${REMOTE_BODY}" >/dev/null 2>&1
LARGE_INSPECT_STATUS=$?
set -e
[ "${LARGE_INSPECT_STATUS}" -ne 0 ] || \
  contract_failure 'oversized readelf output unexpectedly passed the second hard gate'
grep -Fq 'readelf inspection failed before execution gate' \
  "${CONTRACT_TMP}/large-inspect.report" || \
  contract_failure 'oversized readelf output was not reported as a gate failure'
[ ! -e "${CONTRACT_TMP}/gate-two-executed" ] || \
  contract_failure 'oversized readelf output allowed package code execution'

printf '%s\n' '#!/usr/bin/env sh' 'exit 0' \
  >"${ROOT}/binaries/media-core-linux-amd64"
write_checksums "${ROOT}"

python3 - "${ROOT}" "${CONTRACT_TMP}/duplicate-member.tar.gz" <<'PY'
import pathlib
import sys
import tarfile

root = pathlib.Path(sys.argv[1])
archive = pathlib.Path(sys.argv[2])
with tarfile.open(archive, "w:gz") as bundle:
    bundle.add(root, arcname=root.name)
    bundle.add(root / "package-manifest.env", arcname=f"{root.name}/package-manifest.env")
PY
run_fixture "${CONTRACT_TMP}/duplicate-member.tar.gz" duplicate-member
grep -Fq 'archive contains duplicate member path' \
  "${CONTRACT_TMP}/duplicate-member.report" || \
  contract_failure 'duplicate tar member was not rejected before extraction'

python3 - "${ROOT}" "${CONTRACT_TMP}" <<'PY'
import io
import pathlib
import sys
import tarfile

root = pathlib.Path(sys.argv[1])
output = pathlib.Path(sys.argv[2])


def regular(name: str, content: bytes = b"contract") -> tarfile.TarInfo:
    member = tarfile.TarInfo(name)
    member.size = len(content)
    return member


with tarfile.open(output / "multi-top.tar.gz", "w:gz") as bundle:
    bundle.add(root, arcname=root.name)
    content = b"second top"
    bundle.addfile(regular("other-top.txt", content), io.BytesIO(content))

for archive_name, member_name in (
    ("parent-path", "../escaped.txt"),
    ("absolute-path", "/tmp/escaped.txt"),
):
    with tarfile.open(output / f"{archive_name}.tar.gz", "w:gz") as bundle:
        bundle.add(root, arcname=root.name)
        content = b"escape"
        bundle.addfile(regular(member_name, content), io.BytesIO(content))

with tarfile.open(output / "special-member.tar.gz", "w:gz") as bundle:
    bundle.add(root, arcname=root.name)
    special = tarfile.TarInfo(f"{root.name}/pipe")
    special.type = tarfile.FIFOTYPE
    bundle.addfile(special)

for archive_name, link_type, link_name in (
    ("escaping-symlink", tarfile.SYMTYPE, "../../outside"),
    ("escaping-hardlink", tarfile.LNKTYPE, "outside/file"),
):
    with tarfile.open(output / f"{archive_name}.tar.gz", "w:gz") as bundle:
        bundle.add(root, arcname=root.name)
        link = tarfile.TarInfo(f"{root.name}/{archive_name}")
        link.type = link_type
        link.linkname = link_name
        bundle.addfile(link)
PY
while IFS='|' read -r fixture expected_message; do
  run_fixture "${CONTRACT_TMP}/${fixture}.tar.gz" "${fixture}"
  grep -Fq "${expected_message}" "${CONTRACT_TMP}/${fixture}.report" || \
    contract_failure \
      "unsafe tar fixture was not rejected (${fixture}): ${expected_message}"
done <<'EOF'
multi-top|archive must contain exactly one top-level directory
parent-path|archive contains parent traversal member path
absolute-path|archive contains absolute or empty member path
special-member|archive contains unsupported special member
escaping-symlink|archive link target escapes root
escaping-hardlink|archive link target escapes root
EOF

python3 - "${ROOT}" "${CONTRACT_TMP}/invalid-top-name.tar.gz" <<'PY'
import pathlib
import sys
import tarfile

root = pathlib.Path(sys.argv[1])
archive = pathlib.Path(sys.argv[2])
with tarfile.open(archive, "w:gz") as bundle:
    bundle.add(root, arcname="streamserver-native-contract")
PY
run_fixture "${CONTRACT_TMP}/invalid-top-name.tar.gz" invalid-top-name
grep -Fq 'archive top-level directory name does not match native builder contract' \
  "${CONTRACT_TMP}/invalid-top-name.report" || \
  contract_failure 'archive top-level directory was not bound to the native builder naming contract'

python3 - "${ROOT}" "${CONTRACT_TMP}" <<'PY'
import io
import pathlib
import sys
import tarfile

root = pathlib.Path(sys.argv[1])
output = pathlib.Path(sys.argv[2])

with tarfile.open(output / "dangerous-mode.tar.gz", "w:gz") as bundle:
    bundle.add(root, arcname=root.name)
    payload = b"dangerous"
    member = tarfile.TarInfo(f"{root.name}/dangerous-mode")
    member.mode = 0o4755
    member.size = len(payload)
    bundle.addfile(member, io.BytesIO(payload))

with tarfile.open(output / "dangerous-hardlink-mode.tar.gz", "w:gz") as bundle:
    bundle.add(root, arcname=root.name)
    payload = b"hardlink-mode-contract"
    target_name = f"{root.name}/hardlink-mode-target"
    target = tarfile.TarInfo(target_name)
    target.mode = 0o644
    target.size = len(payload)
    bundle.addfile(target, io.BytesIO(payload))
    link = tarfile.TarInfo(f"{root.name}/hardlink-mode-alias")
    link.type = tarfile.LNKTYPE
    link.linkname = target_name
    link.mode = 0o4777
    bundle.addfile(link)

with tarfile.open(output / "mismatched-hardlink-mode.tar.gz", "w:gz") as bundle:
    bundle.add(root, arcname=root.name)
    payload = b"hardlink-mode-contract"
    target_name = f"{root.name}/hardlink-mismatch-target"
    target = tarfile.TarInfo(target_name)
    target.mode = 0o644
    target.size = len(payload)
    bundle.addfile(target, io.BytesIO(payload))
    link = tarfile.TarInfo(f"{root.name}/hardlink-mismatch-alias")
    link.type = tarfile.LNKTYPE
    link.linkname = target_name
    link.mode = 0o600
    bundle.addfile(link)

with tarfile.open(output / "consistent-hardlink-mode.tar.gz", "w:gz") as bundle:
    bundle.add(root, arcname=root.name)
    payload = b"hardlink-mode-contract"
    target_name = f"{root.name}/hardlink-consistent-target"
    target = tarfile.TarInfo(target_name)
    target.mode = 0o644
    target.size = len(payload)
    bundle.addfile(target, io.BytesIO(payload))
    link = tarfile.TarInfo(f"{root.name}/hardlink-consistent-alias")
    link.type = tarfile.LNKTYPE
    link.linkname = target_name
    link.mode = 0o644
    bundle.addfile(link)

with tarfile.open(output / "reversed-dangerous-hardlink-mode.tar.gz", "w:gz") as bundle:
    bundle.add(root, arcname=root.name)
    payload = b"hardlink-mode-contract"
    target_name = f"{root.name}/hardlink-reversed-target"
    link = tarfile.TarInfo(f"{root.name}/hardlink-reversed-alias")
    link.type = tarfile.LNKTYPE
    link.linkname = target_name
    link.mode = 0o4777
    bundle.addfile(link)
    target = tarfile.TarInfo(target_name)
    target.mode = 0o644
    target.size = len(payload)
    bundle.addfile(target, io.BytesIO(payload))

with tarfile.open(output / "reversed-hardlink-mode-mismatch.tar.gz", "w:gz") as bundle:
    bundle.add(root, arcname=root.name)
    payload = b"hardlink-mode-contract"
    target_name = f"{root.name}/hardlink-reversed-mismatch-target"
    link = tarfile.TarInfo(f"{root.name}/hardlink-reversed-mismatch-alias")
    link.type = tarfile.LNKTYPE
    link.linkname = target_name
    link.mode = 0o600
    bundle.addfile(link)
    target = tarfile.TarInfo(target_name)
    target.mode = 0o644
    target.size = len(payload)
    bundle.addfile(target, io.BytesIO(payload))

with tarfile.open(output / "hardlink-chain-mismatch.tar.gz", "w:gz") as bundle:
    bundle.add(root, arcname=root.name)
    payload = b"hardlink-mode-contract"
    target_name = f"{root.name}/hardlink-chain-target"
    first_name = f"{root.name}/hardlink-chain-first"
    target = tarfile.TarInfo(target_name)
    target.mode = 0o644
    target.size = len(payload)
    bundle.addfile(target, io.BytesIO(payload))
    first = tarfile.TarInfo(first_name)
    first.type = tarfile.LNKTYPE
    first.linkname = target_name
    first.mode = 0o644
    bundle.addfile(first)
    second = tarfile.TarInfo(f"{root.name}/hardlink-chain-second")
    second.type = tarfile.LNKTYPE
    second.linkname = first_name
    second.mode = 0o600
    bundle.addfile(second)

with tarfile.open(output / "consistent-hardlink-chain.tar.gz", "w:gz") as bundle:
    bundle.add(root, arcname=root.name)
    payload = b"hardlink-mode-contract"
    target_name = f"{root.name}/hardlink-safe-chain-target"
    first_name = f"{root.name}/hardlink-safe-chain-first"
    target = tarfile.TarInfo(target_name)
    target.mode = 0o644
    target.size = len(payload)
    bundle.addfile(target, io.BytesIO(payload))
    first = tarfile.TarInfo(first_name)
    first.type = tarfile.LNKTYPE
    first.linkname = target_name
    first.mode = 0o644
    bundle.addfile(first)
    second = tarfile.TarInfo(f"{root.name}/hardlink-safe-chain-second")
    second.type = tarfile.LNKTYPE
    second.linkname = first_name
    second.mode = 0o644
    bundle.addfile(second)

with tarfile.open(output / "consistent-hardlink-multilink.tar.gz", "w:gz") as bundle:
    bundle.add(root, arcname=root.name)
    payload = b"hardlink-mode-contract"
    target_name = f"{root.name}/hardlink-multi-target"
    target = tarfile.TarInfo(target_name)
    target.mode = 0o644
    target.size = len(payload)
    bundle.addfile(target, io.BytesIO(payload))
    for suffix in ("first", "second"):
        link = tarfile.TarInfo(f"{root.name}/hardlink-multi-{suffix}")
        link.type = tarfile.LNKTYPE
        link.linkname = target_name
        link.mode = 0o644
        bundle.addfile(link)

with tarfile.open(output / "long-path.tar.gz", "w:gz") as bundle:
    name = f"{root.name}/" + "/".join(["segment" * 20] * 8)
    payload = b"long"
    member = tarfile.TarInfo(name)
    member.size = len(payload)
    bundle.addfile(member, io.BytesIO(payload))

with tarfile.open(output / "long-component.tar.gz", "w:gz") as bundle:
    name = f"{root.name}/" + ("x" * 256)
    payload = b"component"
    member = tarfile.TarInfo(name)
    member.size = len(payload)
    bundle.addfile(member, io.BytesIO(payload))

with tarfile.open(output / "compression-bomb.tar.gz", "w:gz") as bundle:
    payload = b"\0" * (20 * 1024 * 1024)
    member = tarfile.TarInfo(f"{root.name}/highly-compressible")
    member.size = len(payload)
    bundle.addfile(member, io.BytesIO(payload))
PY
while IFS='|' read -r fixture expected_message; do
  run_fixture "${CONTRACT_TMP}/${fixture}.tar.gz" "${fixture}"
  grep -Fq "${expected_message}" "${CONTRACT_TMP}/${fixture}.report" || \
    contract_failure \
      "archive resource/mode fixture was not rejected (${fixture}): ${expected_message}"
done <<'EOF'
dangerous-mode|archive contains dangerous regular/directory mode
dangerous-hardlink-mode|archive contains dangerous hardlink mode
mismatched-hardlink-mode|archive hardlink inode has inconsistent header modes
reversed-dangerous-hardlink-mode|archive contains dangerous hardlink mode
reversed-hardlink-mode-mismatch|archive hardlink inode has inconsistent header modes
hardlink-chain-mismatch|archive hardlink inode has inconsistent header modes
long-path|archive member path exceeds 1024 bytes
long-component|archive member path component exceeds 255 bytes
compression-bomb|archive compression ratio exceeds safe limit
EOF

run_fixture \
  "${CONTRACT_TMP}/consistent-hardlink-mode.tar.gz" \
  consistent-hardlink-mode
grep -Fq '[OK] secure archive validation and extraction' \
  "${CONTRACT_TMP}/consistent-hardlink-mode.report" || \
  contract_failure 'safe mode-consistent hardlink was rejected during archive extraction'
for fixture in consistent-hardlink-chain consistent-hardlink-multilink; do
  run_fixture "${CONTRACT_TMP}/${fixture}.tar.gz" "${fixture}"
  grep -Fq '[OK] secure archive validation and extraction' \
    "${CONTRACT_TMP}/${fixture}.report" || \
    contract_failure \
      "safe mode-consistent hardlink topology was rejected: ${fixture}"
done

for source_contract in \
  'MAX_ARCHIVE_MEMBERS = 250_000' \
  'MAX_ARCHIVE_FILE_SIZE = 16 * 1024**3' \
  'MAX_ARCHIVE_TOTAL_SIZE = 48 * 1024**3' \
  'archive extraction inventory mismatch' \
  'archive extraction produced dangerous final mode'; do
  grep -Fq "${source_contract}" "${VERIFY_SCRIPT}" || \
    contract_failure "archive verifier is missing resource/inventory contract: ${source_contract}"
done
normalize_definition_line="$(grep -n '^normalize_bundle_permissions() {' \
  "${BUNDLE_BUILDER}" | cut -d: -f1)"
normalize_call_line="$(grep -n 'normalize_bundle_permissions "${bundle_root}"' \
  "${BUNDLE_BUILDER}" | tail -1 | cut -d: -f1)"
checksum_call_line="$(grep -n 'write_checksums "${bundle_root}"' \
  "${BUNDLE_BUILDER}" | tail -1 | cut -d: -f1)"
[ -n "${normalize_definition_line}" ] \
  && [ -n "${normalize_call_line}" ] \
  && [ -n "${checksum_call_line}" ] \
  && [ "${normalize_call_line}" -lt "${checksum_call_line}" ] || \
  contract_failure 'native builder does not normalize safe modes before SHA/archive creation'

sed -i \
  's#^MEDIA_CORE_BINARY_PATH=.*#MEDIA_CORE_BINARY_PATH=../../outside/media-core#' \
  "${ROOT}/package-manifest.env"
write_checksums "${ROOT}"
tar -czf "${CONTRACT_TMP}/manifest-path.tar.gz" \
  -C "${STAGE}" "$(basename "${ROOT}")"
run_fixture "${CONTRACT_TMP}/manifest-path.tar.gz" manifest-path
grep -Fq \
  'package manifest MEDIA_CORE_BINARY_PATH must equal binaries/media-core-linux-amd64' \
  "${CONTRACT_TMP}/manifest-path.report" || \
  contract_failure 'manifest path was not bound to the canonical checked path'

write_minimal_manifest "${ROOT}"
sed -i \
  -e 's/^BUNDLE_VERSION=.*/BUNDLE_VERSION=not-semver/' \
  -e 's/^DEPLOY_MODE=.*/DEPLOY_MODE=container/' \
  -e '/^ZLM_LIB_PATH=/d' \
  "${ROOT}/package-manifest.env"
printf '%s\n' \
  "UNEXPECTED_COMMAND=\$(touch '${CONTRACT_TMP}/manifest-executed')" \
  >>"${ROOT}/package-manifest.env"
write_checksums "${ROOT}"
tar -czf "${CONTRACT_TMP}/manifest-schema.tar.gz" \
  -C "${STAGE}" "$(basename "${ROOT}")"
run_fixture "${CONTRACT_TMP}/manifest-schema.tar.gz" manifest-schema
for expected_message in \
  'package manifest contains unknown key: UNEXPECTED_COMMAND' \
  'package manifest BUNDLE_VERSION must be a canonical v-prefixed semantic version' \
  'package manifest DEPLOY_MODE must equal native' \
  'package manifest must define ZLM_LIB_PATH exactly once'; do
  grep -Fq "${expected_message}" "${CONTRACT_TMP}/manifest-schema.report" || \
    contract_failure "complete manifest schema failure was not reported: ${expected_message}"
done
[ ! -e "${CONTRACT_TMP}/manifest-executed" ] || \
  contract_failure 'target verifier executed package manifest contents'

write_minimal_manifest "${ROOT}"
write_checksums "${ROOT}"
printf '%s\n' 'not listed by SHA256SUMS' >"${ROOT}/unlisted.txt"
chmod 644 "${ROOT}/unlisted.txt"
tar -czf "${CONTRACT_TMP}/unlisted-file.tar.gz" \
  -C "${STAGE}" "$(basename "${ROOT}")"
run_fixture "${CONTRACT_TMP}/unlisted-file.tar.gz" unlisted-file
grep -Fq 'SHA256SUMS does not cover exactly all regular files' \
  "${CONTRACT_TMP}/unlisted-file.report" || \
  contract_failure 'unlisted regular file was not rejected by checksum coverage'

rm -f -- "${ROOT}/unlisted.txt"
write_minimal_manifest "${ROOT}"
write_checksums "${ROOT}"
tar -czf "${CONTRACT_TMP}/local-mode.tar.gz" \
  -C "${STAGE}" "$(basename "${ROOT}")"
mkdir -p "${CONTRACT_TMP}/local-mode-output"
set +e
bash "${VERIFY_SCRIPT}" \
  --local \
  --bundle "${CONTRACT_TMP}/local-mode.tar.gz" \
  --output-dir "${CONTRACT_TMP}/local-mode-output" \
  >/dev/null 2>&1
LOCAL_MODE_STATUS=$?
set -e
LOCAL_MODE_REPORT_COUNT="$(find "${CONTRACT_TMP}/local-mode-output" \
  -type f -name 'native-verification-target-*.md' | wc -l)"
[ "${LOCAL_MODE_STATUS}" -ne 0 ] && [ "${LOCAL_MODE_REPORT_COUNT}" -eq 1 ] || \
  contract_failure 'explicit local mode did not execute and publish the failing target report'

if ! python3 - "${NATIVE_WORKFLOW}" <<'PY'
import pathlib
import sys
import yaml

workflow = yaml.safe_load(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
if workflow["permissions"] != {"contents": "read"}:
    raise SystemExit("global permissions must be contents: read")
jobs = workflow["jobs"]
if jobs["release"]["permissions"] != {"contents": "write"}:
    raise SystemExit("release permissions must be contents: write")
for name, job in jobs.items():
    if name != "release" and "write" in (job.get("permissions") or {}).values():
        raise SystemExit(f"unexpected write permission in {name}")
triggers = workflow.get("on", workflow.get(True))
for event in ("push", "pull_request"):
    paths = triggers[event]["paths"]
    if ".gitattributes" not in paths:
        raise SystemExit(f"{event} lacks .gitattributes")
    if "docs/zh/08-native-deployment.md" not in paths:
        raise SystemExit(f"{event} lacks native deployment doc")
PY
then
  contract_failure 'native workflow permission/path contract is not structurally valid YAML'
fi
grep -Fq 'github.com/rhysd/actionlint/cmd/actionlint@v1.7.7' \
  "${NATIVE_WORKFLOW}" || \
  contract_failure 'native workflow does not execute a pinned actionlint gate'
grep -Fq 'scripts/verify-native-bundle-on-target.sh' "${NATIVE_WORKFLOW}" \
  && grep -Fq -- '--local' "${NATIVE_WORKFLOW}" || \
  contract_failure 'built native archives are not executed through local verifier mode'
grep -Fq -- '--gpu-hardware skip' "${NATIVE_WORKFLOW}" || \
  contract_failure 'GPU runner verification does not explicitly separate hardware smokes'
grep -Fq 'mktemp -d' "${VERIFY_SCRIPT}" || \
  contract_failure 'native verifier does not allocate a unique remote run directory'
grep -Fq 'ConnectTimeout=' "${VERIFY_SCRIPT}" || \
  contract_failure 'SSH connections do not set ConnectTimeout'
grep -Fq 'ServerAliveInterval=' "${VERIFY_SCRIPT}" || \
  contract_failure 'SSH connections do not set a server-alive deadline'
if grep -Fq 'set timeout -1' "${VERIFY_SCRIPT}"; then
  contract_failure 'expect-based SSH or SCP still has an infinite timeout'
fi

if grep -Fq "'\${ROOT}/" "${VERIFY_SCRIPT}"; then
  contract_failure 'run_shell interpolates the extracted ROOT into outer shell source'
fi
grep -Fq 'env VERIFY_ROOT="${ROOT:-}"' "${VERIFY_SCRIPT}" || \
  contract_failure 'run_shell does not pass extracted ROOT through a dedicated environment value'
PREEXEC_BLOCK="$(sed -n \
  '/section "Pre-execution Binary Inspection"/,/abort_gate_if_failed "binary and runtime inspection gate"/p' \
  "${VERIFY_SCRIPT}")"
printf '%s\n' "${PREEXEC_BLOCK}" | grep -Fq 'inspect_elf_preexec' || \
  contract_failure 'second hard gate does not use pure host-side ELF inspection'
if printf '%s\n' "${PREEXEC_BLOCK}" \
    | grep -Eq '(^|[^[:alpha:]_])(ldd|runtime_list_deps|runtime_exec)([^[:alpha:]_]|$)'; then
  contract_failure 'second hard gate executes ldd or a bundled runtime loader before passing'
fi
grep -Fq 'run_shell "readelf present"' "${VERIFY_SCRIPT}" || \
  contract_failure 'host readelf is not checked before pure ELF inspection'
for timeout_contract in \
  'DEADLINE_KILL_AFTER_SEC=120' \
  'CAPTURE_KILL_AFTER_SEC=30' \
  '--kill-after="${DEADLINE_KILL_AFTER_SEC}s"' \
  '"${CAPTURE_KILL_AFTER_SEC}" "${timeout_seconds}" "$@"' \
  '--kill-after=135s' \
  '--kill-after=150s' \
  'REMOTE_VERIFY_TIMEOUT_SEC + 180'; do
  grep -Fq -- "${timeout_contract}" "${VERIFY_SCRIPT}" || \
    contract_failure "cleanup timeout hierarchy is missing: ${timeout_contract}"
done

CHECKSUM_FUNCTION="$(sed -n \
  '/^write_checksums() {$/,/^}$/p' "${BUNDLE_BUILDER}")"
printf '%s\n' "${CHECKSUM_FUNCTION}" \
  | grep -Fq 'chmod 0644 "${bundle_root}/SHA256SUMS"' || \
  contract_failure 'bundle checksum manifest mode is not normalized in write_checksums'
NO_DOCKER_FUNCTION="$(sed -n \
  '/^assert_no_docker_runtime_assets() {$/,/^}$/p' "${BUNDLE_BUILDER}")"
printf '%s\n' "${NO_DOCKER_FUNCTION}" | grep -Fq -- '-print -quit' || \
  contract_failure 'builder Docker inventory search does not stop without a SIGPIPE pipeline'
if printf '%s\n' "${NO_DOCKER_FUNCTION}" | grep -Fq '| grep'; then
  contract_failure 'builder Docker inventory search can false-green after find SIGPIPE'
fi
PG_DEPENDENCY_BLOCK="$(sed -n \
  '/^inspect_shared_object() {$/,/^}$/p' \
  "${VERIFY_SCRIPT}")"
printf '%s\n' "${PG_DEPENDENCY_BLOCK}" \
  | grep -Fq 'dependency_status=$?' || \
  contract_failure 'PostgreSQL shared-object loader status is not captured'
printf '%s\n' "${PG_DEPENDENCY_BLOCK}" \
  | grep -Fq 'runtime_bounded_capture' || \
  contract_failure 'PostgreSQL shared-object loader output is not write-time bounded'
if printf '%s\n' "${PG_DEPENDENCY_BLOCK}" | grep -Fq '|| true'; then
  contract_failure 'PostgreSQL shared-object loader failure is still swallowed'
fi

grep -Fq 'BUNDLE_SHA256=' "${VERIFY_SCRIPT}" \
  && grep -Fq 'sha256sum -c' "${VERIFY_SCRIPT}" || \
  contract_failure 'HTTP transfer is not bound to a locally computed fixed SHA256 digest'
grep -Fq 'scp_upload "${REMOTE_SCRIPT_LOCAL}" "${remote_script}"' \
  "${VERIFY_SCRIPT}" || \
  contract_failure 'verifier helper is not always transferred through SCP'
http_digest_line="$(grep -n 'sha256sum -c -' "${VERIFY_SCRIPT}" \
  | head -1 | cut -d: -f1)"
http_stop_line="$(grep -n '^[[:space:]]*stop_http_server$' "${VERIFY_SCRIPT}" \
  | tail -1 | cut -d: -f1)"
[ -n "${http_digest_line}" ] && [ -n "${http_stop_line}" ] \
  && [ "${http_stop_line}" -gt "${http_digest_line}" ] \
  && [ $((http_stop_line - http_digest_line)) -le 3 ] || \
  contract_failure 'HTTP server is not stopped immediately after digest-bound archive download'
grep -Fq 'REMOTE_RUN_DIR=' "${VERIFY_SCRIPT}" || \
  contract_failure 'outer verifier does not retain the unique remote run directory for cleanup'
grep -Fq 'cleanup_remote_verifier() {' "${VERIFY_SCRIPT}" || \
  contract_failure 'outer verifier has no bounded idempotent remote cleanup path'
for signal_contract in \
  'trap cleanup_remote_run EXIT' \
  "trap 'handle_remote_signal 129' HUP" \
  "trap 'handle_remote_signal 130' INT" \
  "trap 'handle_remote_signal 143' TERM"; do
  grep -Fq "${signal_contract}" "${VERIFY_SCRIPT}" || \
    contract_failure \
      "remote verifier wrapper lacks explicit signal cleanup: ${signal_contract}"
done
if grep -Fq 'ssh_run "cat $(shell_quote "${remote_report}")"' "${VERIFY_SCRIPT}"; then
  contract_failure 'report is fetched through a second SSH connection instead of the verifier connection'
fi
if grep -Fq 'ssh_run "STREAMSERVER_VERIFY_BUNDLE=' "${VERIFY_SCRIPT}"; then
  contract_failure 'legacy direct remote verifier invocation is still present'
fi
[ "$(grep -Fxc '    ssh_run "${remote_command}" "${remote_outer_timeout}" \' \
  "${VERIFY_SCRIPT}")" -eq 1 ] || \
  contract_failure 'remote verifier wrapper must be started exactly once'

DEADLINE_DRIVER="${CONTRACT_TMP}/deadline-driver.sh"
{
  printf '%s\n' '#!/usr/bin/env bash' 'set -euo pipefail'
  sed -n '/^deadline_exec() {$/,/^}$/p' "${REMOTE_BODY}"
  sed -n '/^process_starttime() {$/,/^}$/p' "${REMOTE_BODY}"
  sed -n '/^process_is_live_non_zombie() {$/,/^}$/p' "${REMOTE_BODY}"
  sed -n '/^process_pgid() {$/,/^}$/p' "${REMOTE_BODY}"
  sed -n '/^process_group_has_live_members() {$/,/^}$/p' "${REMOTE_BODY}"
  sed -n '/^bounded_stream_to_file() {$/,/^}$/p' "${REMOTE_BODY}"
  sed -n '/^terminate_bounded_group() {$/,/^}$/p' "${REMOTE_BODY}"
  sed -n '/^fifo_holder_pids() {$/,/^}$/p' "${REMOTE_BODY}"
  sed -n '/^terminate_fifo_holders() {$/,/^}$/p' "${REMOTE_BODY}"
  sed -n '/^run_bounded_capture() {$/,/^}$/p' "${REMOTE_BODY}"
  sed -n '/^mark_report_truncated() {$/,/^}$/p' "${REMOTE_BODY}"
  sed -n '/^append_capped_output() {$/,/^}$/p' "${REMOTE_BODY}"
  sed -n '/^run_shell() {$/,/^}$/p' "${REMOTE_BODY}"
  cat <<'DRIVER'
REPORT="${DEADLINE_REPORT}"
RUN_WORK="${DEADLINE_RUN_WORK}"
COMMAND_OUTPUT_LIMIT_BYTES=1048576
REPORT_LIMIT_BYTES=16777216
REPORT_TRUNCATED=0
REPORT_FINALIZING=0
DEADLINE_KILL_AFTER_SEC=2
CAPTURE_KILL_AFTER_SEC=2
FAILURES=0
SMOKE_SHELL_TIMEOUT_SEC=1
mkdir -p "${RUN_WORK}"
append() { :; }
enforce_report_limit() { :; }
record_ok() { :; }
record_failure() { FAILURES=$((FAILURES + 1)); }
run_shell deadline-contract \
  "printf '%s\\n' \"\$\$\" >'${DEADLINE_PID_FILE}'; trap '' TERM; while :; do sleep 1; done"
run_shell bounded-output-contract \
  "python3 -c 'import sys; sys.stdout.write(\"x\" * (2 * 1024 * 1024))'"
run_shell infinite-output-contract \
  "printf '%s\\n' \"\${BASHPID}\" >'${INFINITE_OUTPUT_PID_FILE}'; trap '' PIPE TERM; while :; do printf '%65536s' '' || true; done"
run_shell escaped-fifo-holder-contract \
  "setsid bash --noprofile --norc -c 'trap \"\" HUP INT TERM; printf \"%s\\n\" \"\${BASHPID}\" >\"${ESCAPED_HOLDER_PID_FILE}\"; while :; do sleep 1; done' & for _ in \$(seq 1 100); do [ -s \"${ESCAPED_HOLDER_PID_FILE}\" ] && break; sleep 0.01; done; [ -s \"${ESCAPED_HOLDER_PID_FILE}\" ]; exit 0"
[ "${FAILURES}" -eq 4 ]
grep -Fq '[TRUNCATED: command output' "${REPORT}"
[ "$(wc -c <"${REPORT}")" -le 2200000 ]
[ -s "${DEADLINE_PID_FILE}" ]
deadline_pid="$(cat "${DEADLINE_PID_FILE}")"
if process_is_live_non_zombie "${deadline_pid}"; then
  kill -KILL "${deadline_pid}" >/dev/null 2>&1 || true
  exit 1
fi
[ -s "${INFINITE_OUTPUT_PID_FILE}" ]
pid="$(cat "${INFINITE_OUTPUT_PID_FILE}")"
if [ -r "/proc/${pid}/stat" ]; then
  state="$(sed 's/^.*) //' "/proc/${pid}/stat" | awk '{print $1}')"
  [ "${state}" = Z ]
fi
[ -s "${ESCAPED_HOLDER_PID_FILE}" ]
escaped_pid="$(cat "${ESCAPED_HOLDER_PID_FILE}")"
if process_is_live_non_zombie "${escaped_pid}"; then
  kill -KILL "${escaped_pid}" >/dev/null 2>&1 || true
  exit 1
fi
DRIVER
} >"${DEADLINE_DRIVER}"
chmod 755 "${DEADLINE_DRIVER}"
: >"${CONTRACT_TMP}/deadline.report"
set +e
DEADLINE_REPORT="${CONTRACT_TMP}/deadline.report" \
DEADLINE_PID_FILE="${CONTRACT_TMP}/deadline.pid" \
DEADLINE_RUN_WORK="${CONTRACT_TMP}/deadline-work" \
INFINITE_OUTPUT_PID_FILE="${CONTRACT_TMP}/infinite-output.pid" \
ESCAPED_HOLDER_PID_FILE="${CONTRACT_TMP}/escaped-holder.pid" \
  timeout --signal=TERM --kill-after=2s 45s bash "${DEADLINE_DRIVER}"
DEADLINE_STATUS=$?
set -e
if [ -s "${CONTRACT_TMP}/deadline.pid" ]; then
  DEADLINE_PID="$(cat "${CONTRACT_TMP}/deadline.pid")"
  case "${DEADLINE_PID}" in
    ''|*[!0-9]*) ;;
    *) kill -KILL "${DEADLINE_PID}" >/dev/null 2>&1 || true ;;
  esac
fi
[ "${DEADLINE_STATUS}" -eq 0 ] || \
  contract_failure 'run_shell did not TERM then KILL an ignored-TERM command by its deadline'

REPORT_LIMIT_DRIVER="${CONTRACT_TMP}/report-limit-driver.sh"
{
  printf '%s\n' '#!/usr/bin/env bash' 'set -euo pipefail'
  sed -n '/^append() {$/,/^}$/p' "${REMOTE_BODY}"
  sed -n '/^enforce_report_limit() {$/,/^}$/p' "${REMOTE_BODY}"
  sed -n '/^mark_report_truncated() {$/,/^}$/p' "${REMOTE_BODY}"
  sed -n '/^section() {$/,/^}$/p' "${REMOTE_BODY}"
  sed -n '/^cap_report_for_summary() {$/,/^}$/p' "${REMOTE_BODY}"
  sed -n '/^write_summary() {$/,/^}$/p' "${REMOTE_BODY}"
  cat <<'DRIVER'
REPORT="${REPORT_LIMIT_REPORT}"
RUN_WORK="${REPORT_LIMIT_WORK}"
FAILURES=0
REPORT_LIMIT_BYTES=16777216
REPORT_TRUNCATED=0
REPORT_FINALIZING=0
mkdir -p "${RUN_WORK}"
truncate -s 17000000 "${REPORT}"
write_summary
[ "${FAILURES}" -eq 1 ]
[ "${REPORT_TRUNCATED}" -eq 1 ]
[ "$(grep -c '^## Summary$' "${REPORT}")" -eq 1 ]
[ "$(tail -n 1 "${REPORT}")" = '- result: FAIL' ]
[ "$(wc -c <"${REPORT}")" -le "${REPORT_LIMIT_BYTES}" ]
DRIVER
} >"${REPORT_LIMIT_DRIVER}"
chmod 755 "${REPORT_LIMIT_DRIVER}"
if ! REPORT_LIMIT_REPORT="${CONTRACT_TMP}/oversized.report" \
  REPORT_LIMIT_WORK="${CONTRACT_TMP}/report-limit-work" \
    bash "${REPORT_LIMIT_DRIVER}"; then
  contract_failure 'total report truncation did not force one stable FAIL summary'
fi

for smoke_label in \
  'media-core auth check-config smoke' \
  'media-agent liveness/readiness smoke' \
  'streamserver-config non-interactive smoke'; do
  grep -Fq "${smoke_label}" "${VERIFY_SCRIPT}" || \
    contract_failure "required business smoke is missing: ${smoke_label}"
done
zlm_smoke="$(sed -n \
  '/run_shell "ZLM statistic smoke"/,/^  "$/p' "${VERIFY_SCRIPT}")"
printf '%s\n' "${zlm_smoke}" \
  | grep -Fq 'ZLM statistic endpoint is ready while process is alive' || \
  contract_failure 'ZLM readiness response is not fenced by a live-process check'
zlm_identity_checks="$(grep -Fc 'zlm_process_matches' <<<"${zlm_smoke}" || true)"
[ "${zlm_identity_checks}" -ge 4 ] || \
  contract_failure 'ZLM readiness/TERM/KILL cleanup does not consistently fence PID identity'
if grep -Fq 'kill -0 \"\${pid}\"' <<<"${zlm_smoke}"; then
  contract_failure 'ZLM cleanup still acts on a bare reusable PID'
fi
gateway_smoke="$(sed -n \
  '/run_shell "media-gateway startup smoke"/,/^"$/p' "${VERIFY_SCRIPT}")"
printf '%s\n' "${gateway_smoke}" \
  | grep -Fq '/api/healthz' || \
  contract_failure 'media-gateway smoke does not probe its health endpoint'
printf '%s\n' "${gateway_smoke}" \
  | grep -Fq 'kill -0 \"\${pid}\"' || \
  contract_failure 'media-gateway health response is not followed by an alive-process check'
grep -Fq 'media-core readiness endpoint is ready while process is alive' \
  "${VERIFY_SCRIPT}" || \
  contract_failure 'media-core readiness response is not fenced by a live-process check'
grep -Fq 'registered_process_is_live "${expected_pid}"' \
  "${VERIFY_SCRIPT}" || \
  contract_failure 'PostgreSQL readiness is not fenced by registered process identity'
grep -Fq '"${pgwrap}/pg_isready" -h "${socket_dir}"' "${VERIFY_SCRIPT}" || \
  contract_failure 'PostgreSQL readiness is not scoped to its private Unix socket directory'
grep -Fq 'process_group_has_live_members "${expected_pgid}" || return 0' \
  "${VERIFY_SCRIPT}" || \
  contract_failure 'registered cleanup abandons descendants after the group leader exits'
grep -Fq 'setsid setpriv --reuid="${nobody_uid}"' "${VERIFY_SCRIPT}" || \
  contract_failure 'root PostgreSQL smoke is not anchored to the directly execed server PID'
if grep -Fq 'setsid runuser' "${VERIFY_SCRIPT}"; then
  contract_failure 'root PostgreSQL smoke still tracks a forking runuser parent'
fi
grep -Fq -- '-c listen_addresses=127.0.0.1 &' "${VERIFY_SCRIPT}" || \
  contract_failure 'PostgreSQL private readiness and TCP smoke are not bound to one IPv4 instance'
grep -Fq 'POSTGRES_SMOKE_PID_REGISTRY="${POSTGRES_SMOKE_CONTROL_DIR}/registered-processes.tsv"' \
  "${VERIFY_SCRIPT}" || \
  contract_failure 'PostgreSQL PID registry is not isolated in its private control directory'
grep -Fq 'POSTGRES_SMOKE_SOCKET_DIR="$(mktemp -d /tmp/ss-pg.XXXXXXXX)"' \
  "${VERIFY_SCRIPT}" || \
  contract_failure 'PostgreSQL smoke does not use a bounded short Unix socket path'
grep -Fq 'pgwrap="${POSTGRES_SMOKE_TOOL_DIR}/pgwrap"' "${VERIFY_SCRIPT}" || \
  contract_failure 'PostgreSQL wrappers are not isolated from the low-privilege runtime tree'
grep -Fq 'chmod 700 "${POSTGRES_SMOKE_CONTROL_DIR}"' "${VERIFY_SCRIPT}" \
  && grep -Fq 'chmod 600 "${POSTGRES_SMOKE_PID_REGISTRY}"' "${VERIFY_SCRIPT}" \
  && grep -Fq 'chmod 755 "${POSTGRES_SMOKE_TOOL_DIR}"' "${VERIFY_SCRIPT}" || \
  contract_failure 'PostgreSQL control/tool paths do not have explicit safe modes'
grep -Fq 'chmod 711 "${WORK_DIR}" "${RUN_WORK}" "${RUN_WORK}/extract"' \
  "${VERIFY_SCRIPT}" || \
  contract_failure 'root verifier does not grant the dropped PostgreSQL process execute-only traversal'
grep -Fq 'verification work directory must be canonical, real, and owned by the verifier user' \
  "${VERIFY_SCRIPT}" || \
  contract_failure 'root verifier can chmod an untrusted or symlinked work directory'
if grep -Fq 'chown -R nobody "${tmp}"' "${VERIFY_SCRIPT}"; then
  contract_failure 'PostgreSQL runtime ownership still includes root control state'
fi
grep -Fq 'PostgreSQL root control state is isolated from the runtime user' \
  "${VERIFY_SCRIPT}" || \
  contract_failure 'root PostgreSQL smoke does not dynamically prove control/tool isolation'
grep -Fq '[ "${expected_pgid}" = "${process_pid}" ] || return 1' \
  "${VERIFY_SCRIPT}" || \
  contract_failure 'registered cleanup does not reject a PID/PGID mismatch before signaling'

OUTER_BIN="${CONTRACT_TMP}/outer-bin"
mkdir -p "${OUTER_BIN}"
cat >"${OUTER_BIN}/ssh" <<'SH'
#!/usr/bin/env sh
printf '%s\n' "${CONTRACT_REPORT_MARKER:-none}:$*" >>"${CONTRACT_SSH_LOG}"
case "$*" in
  *"mktemp -d"*)
    printf '/tmp/native-verifier-contract/target-run.%s\n' \
      "${CONTRACT_REPORT_MARKER:-none}"
    ;;
  *"cat "*.md*)
    printf '%s\n' \
      "marker=${CONTRACT_REPORT_MARKER:-none}" \
      '## Summary' \
      '- failures: 0' \
      '- result: PASS'
    ;;
esac
exit 0
SH
cat >"${OUTER_BIN}/scp" <<'SH'
#!/usr/bin/env sh
printf '%s\n' "${CONTRACT_REPORT_MARKER:-none}:$*" >>"${CONTRACT_SCP_LOG}"
exit 0
SH
cat >"${OUTER_BIN}/date" <<'SH'
#!/usr/bin/env sh
printf '%s\n' '20260101-000000'
SH
chmod 755 "${OUTER_BIN}/ssh" "${OUTER_BIN}/scp" "${OUTER_BIN}/date"
: >"${CONTRACT_TMP}/outer-bundle.tar.gz"
: >"${CONTRACT_TMP}/ssh.log"
: >"${CONTRACT_TMP}/scp.log"
printf '%s\n' '备注: documentation mentions http but does not select it' \
  >"${CONTRACT_TMP}/access.txt"
mkdir -p "${CONTRACT_TMP}/access-output"
set +e
CONTRACT_REPORT_MARKER=access \
CONTRACT_SSH_LOG="${CONTRACT_TMP}/ssh.log" \
CONTRACT_SCP_LOG="${CONTRACT_TMP}/scp.log" \
PATH="${OUTER_BIN}:${PATH}" \
  bash "${VERIFY_SCRIPT}" \
    --bundle "${CONTRACT_TMP}/outer-bundle.tar.gz" \
    --ssh-target contract.invalid \
    --access-file "${CONTRACT_TMP}/access.txt" \
    --remote-dir /tmp/native-verifier-contract \
    --output-dir "${CONTRACT_TMP}/access-output" \
    >"${CONTRACT_TMP}/access-run.log" 2>&1
ACCESS_STATUS=$?
set -e
if [ "${ACCESS_STATUS}" -ne 0 ]; then
  cat "${CONTRACT_TMP}/access-run.log" >&2
  contract_failure 'default SCP outer verifier fixture did not complete'
fi
grep -Fq 'access:' "${CONTRACT_TMP}/scp.log" || \
  contract_failure 'an arbitrary http word in access-file changed the default upload method'
grep -Fq 'mktemp -d' "${CONTRACT_TMP}/ssh.log" || \
  contract_failure 'explicit remote base was reused without a unique mktemp child'

mkdir -p "${CONTRACT_TMP}/concurrent-output"
: >"${CONTRACT_TMP}/concurrent-ssh.log"
: >"${CONTRACT_TMP}/concurrent-scp.log"
outer_status_dir="${CONTRACT_TMP}/outer-status"
mkdir -p "${outer_status_dir}"
for marker in alpha beta; do
  (
    set +e
    CONTRACT_REPORT_MARKER="${marker}" \
    CONTRACT_SSH_LOG="${CONTRACT_TMP}/concurrent-ssh.log" \
    CONTRACT_SCP_LOG="${CONTRACT_TMP}/concurrent-scp.log" \
    PATH="${OUTER_BIN}:${PATH}" \
      bash "${VERIFY_SCRIPT}" \
        --bundle "${CONTRACT_TMP}/outer-bundle.tar.gz" \
        --ssh-target contract.invalid \
        --remote-dir /tmp/native-verifier-contract \
        --output-dir "${CONTRACT_TMP}/concurrent-output" \
        >/dev/null 2>&1
    printf '%s\n' "$?" >"${outer_status_dir}/${marker}"
  ) &
done
wait
for marker in alpha beta; do
  [ "$(cat "${outer_status_dir}/${marker}")" -eq 0 ] || \
    contract_failure "concurrent outer verifier failed: ${marker}"
done
CONCURRENT_REPORT_COUNT="$(find "${CONTRACT_TMP}/concurrent-output" \
  -type f -name 'native-verification-target-*.md' | wc -l)"
[ "${CONCURRENT_REPORT_COUNT}" -eq 2 ] || \
  contract_failure 'concurrent verifier runs overwrote or shared one local report'

HTTP_FUNCTIONS="${CONTRACT_TMP}/http-functions.sh"
{
  sed -n '/^prepare_http_serve_dir() {$/,/^}$/p' "${VERIFY_SCRIPT}"
} >"${HTTP_FUNCTIONS}"
# shellcheck disable=SC1090
source "${HTTP_FUNCTIONS}"
HTTP_SERVE_DIR=""
HTTP_SERVER_LOG=""
# Consumed by prepare_http_serve_dir sourced dynamically above.
# shellcheck disable=SC2034
BUNDLE_PATH="${CONTRACT_TMP}/outer-bundle.tar.gz"
REMOTE_SCRIPT_LOCAL="${CONTRACT_TMP}/http-helper.sh"
printf '%s\n' '#!/usr/bin/env sh' 'exit 0' >"${REMOTE_SCRIPT_LOCAL}"
prepare_http_serve_dir bundle.tar.gz
HTTP_SERVED_FILES="$(find "${HTTP_SERVE_DIR}" -mindepth 1 -maxdepth 1 \
  -printf '%f\n' | LC_ALL=C sort)"
[ "${HTTP_SERVED_FILES}" = 'bundle.tar.gz' ] || \
  contract_failure 'HTTP upload directory exposes anything other than the archive'
if grep -Fq 'publish_http_helper() {' "${VERIFY_SCRIPT}"; then
  contract_failure 'verifier helper remains publishable over unauthenticated HTTP'
fi
case "${HTTP_SERVER_LOG}" in
  "${HTTP_SERVE_DIR}"/*)
    contract_failure 'HTTP server log is exposed from the upload directory'
    ;;
esac
rm -rf -- "${HTTP_SERVE_DIR}"
rm -f -- "${HTTP_SERVER_LOG}" "${REMOTE_SCRIPT_LOCAL}"

[ "${FAILURES}" -eq 0 ] || {
  printf 'native verifier strict contract failures: %s\n' "${FAILURES}" >&2
  exit 1
}

echo 'native verifier strict contract tests passed'
