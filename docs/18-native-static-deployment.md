# StreamServer Native 无 Docker 运行时部署

本文说明 native 离线包的构建、安装和 196 验收流程。项目运行、安装和现场操作统一走 native 路径；目标机不需要 Docker。

## 1. 构建

构建机可以使用 Docker builder 和 Docker 镜像提取 Linux AMD64 运行时资产：

```bash
./scripts/build-native-bundle.sh --without-gpu
./scripts/build-native-bundle.sh --with-gpu
./scripts/build-native-bundle.sh --control-plane-minimal
```

首次构建会从 runtime 来源镜像提取 FFmpeg、ZLMediaKit、PostgreSQL 等资产，并写入本地缓存目录 `./.build-cache/native-runtime`。后续构建在镜像引用、提取参数和提取器版本一致时会直接复用该缓存，跳过 runtime 镜像拉取和依赖提取步骤。Rust musl builder 也会复用本地 `streamserver-rust-musl-builder:<target>` 镜像。

runtime 缓存可用参数：

```bash
./scripts/build-native-bundle.sh --without-gpu --refresh-runtime-cache
./scripts/build-native-bundle.sh --without-gpu --offline-runtime-cache
./scripts/build-native-bundle.sh --without-gpu --no-runtime-cache
NATIVE_RUNTIME_CACHE_DIR=/data/streamserver/native-runtime-cache ./scripts/build-native-bundle.sh --without-gpu
```

- `--refresh-runtime-cache`：忽略已有 runtime 缓存，重新从来源镜像提取并覆盖缓存。
- `--offline-runtime-cache`：只允许使用已有 runtime 缓存；缺失或校验失败时直接失败，不拉取 runtime 镜像。
- `--no-runtime-cache`：禁用 runtime 缓存，保持每次从镜像提取 runtime 资产的旧行为。

生成的包名形如：

```text
streamserver-native-v0.1.0-linux-amd64-cpu-only-20260602.tar.gz
streamserver-native-v0.1.0-linux-amd64-gpu-enabled-20260602.tar.gz
streamserver-native-v0.1.0-linux-amd64-control-plane-minimal-20260602.tar.gz
```

包内不得包含 `images/*.tar`、`compose.yml`、`streamserver-compose` 或 `tools/docker/`。业务程序 `media-core`、`media-agent`、`streamserver-config` 以 `x86_64-unknown-linux-musl` 二进制交付；FFmpeg、ZLMediaKit、PostgreSQL 以随包 runtime 交付。

默认 FFmpeg runtime 固定为 `8.1` 系列：CPU 包使用 `jrottenberg/ffmpeg:8.1-ubuntu2404`，GPU 包使用 `jrottenberg/ffmpeg:8.1-nvidia2404`。GPU 节点要求 NVIDIA 驱动满足 FFmpeg/NVIDIA Video Codec SDK 13 系列运行时要求，生产基线按 `570+` 驱动准备；Linux 4.x 内核上的 T4/P4 等老卡优先锁定经过现场验证的 R580/R595 生产分支驱动。

## 2. 196 验收

构建完成后必须在 196 上验证：

```bash
./scripts/verify-native-bundle-on-196.sh \
  --bundle dist/streamserver-native-v0.1.0-linux-amd64-cpu-only-20260602.tar.gz \
  --access-file docs/196服务器访问方式
```

验证内容包括：

- `sha256sum -c SHA256SUMS`
- 包结构确认无 Docker/Compose 运行时资产
- `media-core`、`media-agent`、`streamserver-config` 的 `file` 和 `ldd` 静态链接检查
- `ffmpeg`、`ffprobe`、`MediaServer`、`default.pem` 的可执行和动态依赖检查
- 随包 PostgreSQL 模式下 `postgres`、`initdb`、`pg_ctl`、`pg_isready`、`psql` 检查
- 随包 PostgreSQL 模式下扩展 manifest、`pg_available_extensions` 清单、扩展 `.so` 依赖解析和全量 `CREATE EXTENSION` 检查，确保和构建来源 runtime 的扩展能力一致
- 随包 PostgreSQL 模式下 36 个官方工具的 wrapper/version 检查，以及 `pg_dump`/`pg_restore`/`pg_dumpall`、管理工具、`pg_controldata`/`pg_checksums`/`pg_resetwal`、SSL 客户端证书认证、复杂 `pg_hba.conf`、WAL/PITR、`pg_basebackup`、物理复制、逻辑复制、`pg_recvlogical`、业务迁移 smoke；不执行压力读写或基准负载
- FFmpeg、ZLMediaKit、PostgreSQL smoke test；GPU 包还会在有 GPU runtime 时执行 `h264_nvenc` 与 `hevc_nvenc` 实际编码 smoke test

脚本会生成并拉回：

```text
dist/native-verification-196-<timestamp>.md
```

没有该报告，不能认为 native 包通过验收。

## 3. 安装

目标 Linux AMD64 主机解压后执行：

```bash
tar -xzf streamserver-native-*.tar.gz
cd streamserver-native-*
./install.sh --check-only
sudo ./install.sh
```

安装器会写入 systemd unit：

```text
ss-<instance>.target
ss-<instance>-postgres.service
ss-<instance>-core.service
ss-<instance>-zlm.service
ss-<instance>-agent.service
```

worker/all-in-one 角色会把工作目录、ZLM HTTP 根目录和产物目录统一写到安装目录下的 `data/media/work` 与 `data/zlm/www`。如需沿用原有网络挂载或历史数据路径，安装时选择相同的 `--install-dir`，相关路径会随安装目录保持一致。

常用命令：

```bash
/opt/streamserver/<role>/bin/streamserverctl status
/opt/streamserver/<role>/bin/streamserverctl health
/opt/streamserver/<role>/bin/streamserverctl logs
```

卸载命令：

```bash
cd /opt/streamserver/<role>
sudo ./uninstall.sh
```

也可以从任意目录指定安装目录：

```bash
sudo /opt/streamserver/<role>/uninstall.sh --install-dir /opt/streamserver/<role>
```

卸载脚本默认会询问是否删除数据和配置，默认选择保留 `.env`、`data/`、`certs/`，只移除程序、runtime、UI 和 systemd unit。无人值守卸载时，使用 `--keep-data --yes` 保留数据，使用 `--purge --yes` 删除整个安装目录。

生产控制面优先使用外部 PostgreSQL；一体机或离线演示可以选择包内 PostgreSQL runtime。旧部署数据目录不直接复用，迁移使用 `pg_dumpall`/`psql` 或 `pg_dump`/`pg_restore`。
