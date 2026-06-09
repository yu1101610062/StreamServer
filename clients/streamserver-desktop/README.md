# StreamServer Desktop Client

Flutter Desktop + Rust native module client for StreamServer.

## Scope

This client is an additional desktop delivery surface. It does not replace the
existing `media-core` Web console and it does not change the Core-Agent runtime
architecture.

- Flutter owns UI, navigation, forms, dense management tables and user state.
- Rust owns native API calls, multipart upload, platform secure storage,
  diagnostics, media URL validation and media-player process control.
- The client talks only to `media-core` `/api/v1` plus health endpoints.

## Current Implementation

- `native/`: Rust cdylib/rlib with a JSON C ABI bridge:
  - `auth.*`
  - `api.request`
  - `upload.media`
  - `secure_store.*`
  - `diagnostics.probe`
  - `media_player.*`
  - `server_discovery.*`
- `lib/`: Flutter desktop UI with full management-console page coverage:
  overview, tasks, task create, task detail, streams, multicast, records,
  artifacts, media upload, nodes, security and debug.
- `openapi/streamserver.v1.yaml`: first desktop-client API contract draft.

The current app has moved past a bare scaffold:

- Multiple server profiles are persisted, the last server is restored, and a
  refresh token is used to recover the session when possible.
- Task create uses guided forms for common ingest, bridge, recording and file
  transcode scenarios, while still keeping expert JSON input.
- Task list and detail pages support lifecycle operations, filtering,
  pagination, confirmations, events, logs, records, artifacts and online stream
  associations.
- Streams, records, file artifacts, uploads and nodes expose the main filters,
  pagination, copy/open actions, refresh controls and destructive-operation
  confirmations.
- Security supports password change and machine allowlist replacement.
- Debug supports Core diagnostics, ZLM media/session/player/statistic/thread
  probes, snapshots, session kick operations and local player stop/snapshot
  commands.
- Playback is embedded in Flutter through `media_kit`/libmpv. Stream, record,
  artifact and upload URLs open in a shared player panel with screenshot and
  external-player fallback.
- Login supports LAN discovery. The native module scans active private IPv4
  `/24` networks on Core ports `8080` and `80`; when discovery misses a host,
  the user can manually probe `http` or `https` with host and port.
- Rust secure storage uses macOS Keychain, Windows Credential Manager or Linux
  Secret Service/libsecret by default. The file fallback is retained only for
  tests and explicit `STREAMSERVER_DESKTOP_STORE_DIR` use.

The bridge is deliberately narrow: Dart sends one JSON command to Rust and Rust
returns one JSON envelope. This keeps the first implementation buildable without
codegen. A later `flutter_rust_bridge` migration can generate strongly typed
Dart/Rust bindings over the same operation set.

## Bootstrap

This repository checkout currently may not have Flutter installed. After
installing Flutter, initialize platform folders from this directory:

```bash
cd clients/streamserver-desktop
./scripts/bootstrap_flutter_platforms.sh
```

Build the Rust library first:

```bash
cd clients/streamserver-desktop
./scripts/build_native.sh
```

Windows hosts can use the PowerShell entrypoint:

```powershell
cd clients\streamserver-desktop
.\scripts\build_native.ps1
```

Then run the Flutter desktop app:

```bash
flutter pub get
flutter run -d macos
```

Use `-d windows` or `-d linux` on those host platforms.

macOS packaging requires a full Xcode installation, not only Command Line Tools.
If `flutter build macos` reports that `xcodebuild` is missing, install Xcode and
then run:

```bash
sudo xcode-select --switch /Applications/Xcode.app/Contents/Developer
sudo xcodebuild -runFirstLaunch
```

## Native Library Naming

The Dart FFI loader expects:

- macOS: `libstreamserver_desktop.dylib`
- Linux: `libstreamserver_desktop.so`
- Windows: `streamserver_desktop.dll`

`scripts/build_native.sh` and `scripts/build_native.ps1` copy the built library
into `build/native/`. `scripts/package_offline.sh` and
`scripts/package_offline.ps1` copy that file into the final app. macOS packages
place the dylib under `Contents/Frameworks` and sign the dylib before signing
the app bundle. During development, Dart FFI also searches `build/native/`, the
current working directory, the executable directory, macOS `Contents/Frameworks`
and `STREAMSERVER_DESKTOP_NATIVE_LIB`.

## Security Note

`secure_store.*` uses the native credential backend by default:

- macOS Keychain
- Windows Credential Manager
- Linux Secret Service/libsecret

Set `STREAMSERVER_DESKTOP_STORE_DIR` only for tests or temporary diagnostics; in
that mode the client writes a private-permission, machine-obfuscated local file.
Linux packages must include or declare the DBus/Secret Service runtime baseline
expected by the `keyring` backend.

## Playback Note

The primary player is Flutter `media_kit` with bundled libmpv video libraries.
The Rust player backend validates media URLs and still provides an external
fallback that prefers `mpv` or `vlc` if present, then the system handler.

Local player stop works for externally spawned `mpv`/`vlc` sessions tracked by
the native module. Screenshots in the embedded player are handled by `media_kit`
and are saved to the user-provided path or the app temporary cache.

## 196 E2E Verification

The 196 verification script reads credentials only from environment variables.
It creates test data with a `desktop-e2e-*` prefix and cleans up its own tasks
and upload asset at the end.

```bash
cd clients/streamserver-desktop
STREAMSERVER_196_PASSWORD='<password>' ./scripts/verify_196_e2e.py
```

Optional overrides:

- `STREAMSERVER_196_CORE`, default `http://172.17.13.196:8080`
- `STREAMSERVER_196_AGENT`, default `http://172.17.13.196:8081`
- `STREAMSERVER_196_ZLM`, default `http://172.17.13.196:80`
- `STREAMSERVER_196_USERNAME`, default `admin`
- `STREAMSERVER_E2E_SAMPLE_MP4`, optional local MP4 sample path when local
  `ffmpeg` is unavailable

## Remaining v1 Work

- Produce offline packages on each host platform. macOS requires full Xcode;
  Windows and Linux packages must be built and verified on their respective
  hosts.
- Generate SHA256, version metadata, runtime dependency notes and license
  notices for release artifacts.
