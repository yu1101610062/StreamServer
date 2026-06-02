# StreamServer Native 无 Docker 运行时部署

本文说明 native 离线包的构建、安装和 196 验收流程。现有 Docker/Compose 离线包仍保留为 legacy 路径；native 包的目标机不需要 Docker。

## 1. 构建

构建机可以使用 Docker builder 和 Docker 镜像提取 Linux AMD64 运行时资产：

```bash
./scripts/build-native-bundle.sh --without-gpu
./scripts/build-native-bundle.sh --with-gpu
./scripts/build-native-bundle.sh --control-plane-minimal
```

生成的包名形如：

```text
streamserver-native-v0.1.0-linux-amd64-cpu-only-20260602.tar.gz
streamserver-native-v0.1.0-linux-amd64-gpu-enabled-20260602.tar.gz
streamserver-native-v0.1.0-linux-amd64-control-plane-minimal-20260602.tar.gz
```

包内不得包含 `images/*.tar`、`compose.yml`、`streamserver-compose` 或 `tools/docker/`。业务程序 `media-core`、`media-agent`、`streamserver-config` 以 `x86_64-unknown-linux-musl` 二进制交付；FFmpeg、ZLMediaKit、PostgreSQL 以随包 runtime 交付。

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
- 随包 PostgreSQL 模式下扩展 manifest、`pg_available_extensions` 清单、扩展 `.so` 依赖解析和全量 `CREATE EXTENSION` 检查，确保和 Docker 来源镜像的扩展能力一致
- 随包 PostgreSQL 模式下 36 个官方工具的 wrapper/version 检查，以及 `pg_dump`/`pg_restore`/`pg_dumpall`、管理工具、`pg_controldata`/`pg_checksums`/`pg_resetwal`、SSL 客户端证书认证、复杂 `pg_hba.conf`、WAL/PITR、`pg_basebackup`、物理复制、逻辑复制、`pg_recvlogical`、业务迁移 smoke；不执行压力读写或基准负载
- FFmpeg、ZLMediaKit、PostgreSQL smoke test

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

常用命令：

```bash
/opt/streamserver/<role>/bin/streamserverctl status
/opt/streamserver/<role>/bin/streamserverctl health
/opt/streamserver/<role>/bin/streamserverctl logs
```

生产控制面优先使用外部 PostgreSQL；一体机或离线演示可以选择包内 PostgreSQL runtime。旧 Docker PostgreSQL 数据目录不直接复用，迁移使用 `pg_dumpall`/`psql` 或 `pg_dump`/`pg_restore`。
