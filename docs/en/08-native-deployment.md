# 08. Native Deployment

StreamServer target hosts do not need Docker. The runtime deliverable is a Linux AMD64 offline bundle installed as systemd services.

Docker may be used on the build host to:

- build Rust musl binaries;
- extract FFmpeg runtime assets;
- extract ZLMediaKit runtime assets;
- extract PostgreSQL runtime and tools;
- generate the offline tarball.

## Build

```bash
./scripts/build-native-bundle.sh --without-gpu
./scripts/build-native-bundle.sh --with-gpu
./scripts/build-native-bundle.sh --control-plane-minimal
```

Package variants:

- `cpu-only`: Core, Agent, CPU FFmpeg runtime, ZLMediaKit, and bundled PostgreSQL runtime.
- `gpu-enabled`: CPU package plus GPU FFmpeg runtime.
- `control-plane-minimal`: Core, config tool, and UI; database is external PostgreSQL.

## Install

```bash
tar -xzf streamserver-native-*.tar.gz
cd streamserver-native-*
./install.sh --check-only
sudo ./install.sh
```

Common commands:

```bash
/opt/streamserver/<role>/bin/streamserverctl status
/opt/streamserver/<role>/bin/streamserverctl health
/opt/streamserver/<role>/bin/streamserverctl logs
```

## Target Verification

```bash
./scripts/verify-native-bundle-on-target.sh \
  --bundle dist/streamserver-native-v0.1.0-linux-amd64-cpu-only-<date>.tar.gz \
  --host <target-host>
```

The verification script checks bundle integrity, absence of Docker/Compose runtime assets, business binaries, FFmpeg/ffprobe, ZLMediaKit, PostgreSQL tools, and smoke tests. It writes a report under:

```text
dist/native-verification-target-<timestamp>.md
```

A native bundle should not be considered accepted without a target-host verification report.
