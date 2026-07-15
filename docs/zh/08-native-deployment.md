# 08. Native 无 Docker 运行时部署

本文说明 native 离线包的构建、安装和目标服务器验收流程。项目运行、安装和现场操作统一走 native 路径；目标机不需要 Docker。

## 1. 构建

构建机可以使用 Docker builder 和 Docker 镜像提取 Linux AMD64 运行时资产：

```bash
./scripts/build-native-bundle.sh
./scripts/build-native-bundle.sh --without-gpu
./scripts/build-native-bundle.sh --with-gpu
./scripts/build-native-bundle.sh --control-plane-minimal
```

未指定 `--without-gpu`、`--with-gpu` 或 `--control-plane-minimal` 时，脚本会在交互终端中询问要构建的包变体。非交互环境必须显式指定包变体。

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

包内不得包含 `images/*.tar`、`compose.yml`、`streamserver-compose` 或 `tools/docker/`。业务程序 `media-core`、`media-agent`、`media-gateway`、`streamserver-config` 以 `x86_64-unknown-linux-musl` 二进制交付；FFmpeg、ZLMediaKit、PostgreSQL 以随包 runtime 交付。

默认 FFmpeg runtime 固定为 `8.1` 系列：CPU 包使用 `jrottenberg/ffmpeg:8.1-ubuntu2404`，GPU 包使用 `jrottenberg/ffmpeg:8.1-nvidia2404`。GPU 节点要求 NVIDIA 驱动满足 FFmpeg/NVIDIA Video Codec SDK 13 系列运行时要求，生产基线按 `570+` 驱动准备；Linux 4.x 内核上的 T4/P4 等老卡优先锁定经过现场验证的 R580/R595 生产分支驱动。

`media-gateway` 对普通 MP4/TS 点播执行 HTTP 下载；HLS 点播即使没有时间参数也会通过 FFmpeg 将播放列表和分片完整物化到共享目录。当 Core 传入 `input.start_offset_sec` 或 `record.duration_sec` 时，Gateway 使用 FFmpeg 输入侧 seek、`-t` 和 `-c copy` 生成共享存储时间片。该过程不转码，编码、分辨率、帧率和音频参数保持不变，但容器索引、时间戳和 HLS 分片边界会重新生成，起点精度受关键帧约束。

Gateway 主机通过 `MEDIA_GATEWAY_FFMPEG_BIN` 指定 FFmpeg；未设置时依次回退到 `FFMPEG_BIN` 和 PATH 中的 `ffmpeg`。`MEDIA_GATEWAY_FFPROBE_BIN` 用于校验原子发布前的本地输出，未设置时默认使用 FFmpeg 同目录下的 `ffprobe`。worker/all-in-one 可以复用 Native 安装器生成的运行时，独立 Gateway 或 core-only 主机必须显式提供可执行文件。源站不支持 Range、HLS 分片定位或容器快速 seek 时，Gateway 不会在共享存储落完整源文件，但网络侧仍可能读取偏移量之前的数据。

大批量现场任务建议显式使用以下 Gateway 边界。下载与 FFmpeg 各自使用 FIFO 队列，排队任务不建立上游连接或启动子进程；`0` 的排队超时表示不限排队时长：

```ini
MEDIA_GATEWAY_MAX_QUEUED_PREFETCHES=4096
MEDIA_GATEWAY_MAX_ACTIVE_DOWNLOADS=4
MEDIA_GATEWAY_MAX_ACTIVE_FFMPEG=2
MEDIA_GATEWAY_FFPROBE_BIN=/opt/streamserver/runtime/ffmpeg/bin/ffprobe
MEDIA_GATEWAY_PREFETCH_QUEUE_TIMEOUT_MS=0
MEDIA_GATEWAY_PREFETCH_EXECUTION_TIMEOUT_MS=21600000
MEDIA_GATEWAY_SOURCE_CONNECT_TIMEOUT_MS=10000
MEDIA_GATEWAY_SOURCE_READ_IDLE_TIMEOUT_MS=60000
MEDIA_GATEWAY_MAX_PREFETCH_RECORDS=8192
MEDIA_GATEWAY_PREFETCH_TERMINAL_RETENTION_SEC=3600
MEDIA_GATEWAY_RELAY_CANCEL_WAIT_MS=5000
MEDIA_GATEWAY_PREFETCH_CANCEL_WAIT_MS=30000
MEDIA_GATEWAY_CANCEL_TOMBSTONE_TTL_SEC=3600
MEDIA_GATEWAY_MAX_ACTIVE_RELAYS=32
MEDIA_GATEWAY_MAX_RELAY_REGISTRATIONS=256
MEDIA_GATEWAY_RELAY_RECONNECT_GRACE_SEC=600
MEDIA_GATEWAY_RELAY_UNOPENED_TTL_SEC=86400
```

Core 通过以下配置启用 Source Gateway：

```ini
SOURCE_GATEWAY_BASE_URL=https://gateway.example/bohui/media/
SOURCE_GATEWAY_TLS_INSECURE_SKIP_VERIFY=false
SOURCE_GATEWAY_PREFETCH_POLL_MS=5000
SOURCE_GATEWAY_PREFETCH_TIMEOUT_MS=0
```

`SOURCE_GATEWAY_BASE_URL` 为空时整个 Gateway 改写链路关闭。启用时必须使用 HTTPS；客户端会保留基准地址中的路径前缀、禁用系统代理并拒绝跟随 HTTP 重定向。Core 提交点播后不在创建接口内等待，排队阶段遵循 Gateway 返回的 30 秒提示、运行阶段遵循 5 秒提示；`SOURCE_GATEWAY_PREFETCH_TIMEOUT_MS=0` 表示 Core 不设置总等待时限。`SOURCE_GATEWAY_TLS_INSECURE_SKIP_VERIFY` 默认 `false`，且只接受 `true`/`false`；显式设为 `true` 时，仅 Core 的 Source Gateway 专用客户端跳过证书链、有效期和主机名验证。该开关不影响 Core/Agent mTLS、Agent 管理接口、其他 HTTP 客户端或 FFmpeg/FFprobe。开启后传输仍加密，但对端身份不再可信，应同时使用固定入口地址和网络访问控制，并在 Core 启动日志中核对一次性风险警告。

## 2. 目标服务器验收

构建完成后推荐在实际目标服务器上验证：

```bash
./scripts/verify-native-bundle-on-target.sh \
  --bundle dist/streamserver-native-v0.1.0-linux-amd64-cpu-only-20260602.tar.gz \
  --host <target-host>
```

验证内容包括：

- `sha256sum -c SHA256SUMS`
- 包结构确认无 Docker/Compose 运行时资产
- `media-core`、`media-agent`、`media-gateway`、`streamserver-config` 的 `file` 和 `ldd` 静态链接检查
- Source Gateway 配置的 HTTPS、严格布尔解析和默认 fail-closed 检查
- `ffmpeg`、`ffprobe`、`MediaServer`、`default.pem` 的可执行和动态依赖检查
- 随包 PostgreSQL 模式下 `postgres`、`initdb`、`pg_ctl`、`pg_isready`、`psql` 检查
- 随包 PostgreSQL 模式下扩展 manifest、`pg_available_extensions` 清单、扩展 `.so` 依赖解析和全量 `CREATE EXTENSION` 检查，确保和构建来源 runtime 的扩展能力一致
- 随包 PostgreSQL 模式下 36 个官方工具的 wrapper/version 检查，以及 `pg_dump`/`pg_restore`/`pg_dumpall`、管理工具、`pg_controldata`/`pg_checksums`/`pg_resetwal`、SSL 客户端证书认证、复杂 `pg_hba.conf`、WAL/PITR、`pg_basebackup`、物理复制、逻辑复制、`pg_recvlogical`、业务迁移 smoke；不执行压力读写或基准负载
- FFmpeg、ZLMediaKit、PostgreSQL smoke test；GPU 包还会在有 GPU runtime 时执行 `h264_nvenc` 与 `hevc_nvenc` 实际编码 smoke test
- Agent Hook 端口只监听 `127.0.0.1`，无/错 secret 返回 401，非 object 返回 400，超过 256 KiB 返回 413，队列满或断连返回 503，4 秒内超时返回 504
- 9 个固定 ZLM Hook 经 mTLS ControlPlane 完成 request/response 往返，`on_server_started` 的 mINI 正文不会跨节点，Agent readiness 包含 hook listener 状态

release/native 验证还应在目标 runtime 上执行 codec matrix，覆盖 MP4、HLS、Matroska/MKV、FLV、双输出和 WebM 拒绝路径：

```bash
FFMPEG_BIN=/opt/streamserver/<role>/runtime/ffmpeg/bin/ffmpeg \
FFPROBE_BIN=/opt/streamserver/<role>/runtime/ffmpeg/bin/ffprobe \
./scripts/smoke-codec-matrix.sh
```

脚本会生成并拉回：

```text
dist/native-verification-target-<timestamp>.md
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

首次安装的 `--install-dir` 必须不存在或为空；检测到已有部署时必须显式使用 `--upgrade`，不能通过 fresh 流程覆盖。安装器会先规范化并解析安装目录，拒绝 symlink 边界，然后同时按逻辑实例名和安装目录 canonical path 获取 root-only、非阻塞锁。control-plane、worker 和 all-in-one 均遵守这两把锁；同一实例不能并发写入不同目录，同一目录也不能被不同实例并发占用，后启动的进程立即失败。锁由安装器外层进程持有并覆盖整个进程组、回滚和 durable terminal 决策，TERM/HUP/INT 不会在回滚完成前提前释放锁。

升级必须同时给出与现有 root-managed systemd 拓扑一致的角色和实例名：

```bash
sudo ./install.sh --upgrade \
  --install-dir /opt/streamserver/<role> \
  --role <existing-role> \
  --instance-name <existing-instance>
```

取得双锁后，安装器先把旧 `.env` 和 control tree 规范化为 root-owned、不可由升级前遗留 writable FD 修改的新 inode，并验证角色、实例和数据库拓扑；这一步建立不可回退的 sealed security baseline。随后才在 root-only installer state 中以 `building`→`armed` 两阶段发布事务快照，再执行 ZLM 等语义迁移和服务 quiesce。快照保留 sealed baseline 的 `.env`、程序、UI、runtime、ZLM、文档、证书、内部与外部 systemd unit、卸载器、管理员交付 marker、各项 absent/present 语义和 unit enablement。

后续任一步失败时：quiesce 前失败只还原事务内的语义迁移，不停止运行中的旧服务；quiesce 后失败会先停止可能启动的新服务，再还原文件、unit 和 enablement，执行 `daemon-reload`，并恢复升级前精确的 target 及各 service active/inactive 集合。Core、Agent、ZLM 和包内 PostgreSQL 仅对升级前 active 的组件逐项探测，所有 `systemctl`、临时 `systemd-run` 和 readiness 操作共享绝对截止时间；每轮必须在同一轮内全部成功，返回前还会再次核对所有 `ActiveState`，不会把不同轮次的成功结果拼接成健康状态。ZLM readiness 还要求受限长度的 JSON 响应中恰好一个数值 `code: 0`。回滚失败会保留 armed 快照并显式报错，原安装失败仍返回非零；成功或已恢复事务先 durable 写入 `committed`/`restored` terminal 决策，再清理 fixed transaction、tombstone 和 decision marker。下次升级只回收可严格验证且已 terminal 的残留，任何 `armed` fixed transaction 都会阻断继续写入。

安装器会写入 systemd unit：

```text
ss-<instance>.target
ss-<instance>-postgres.service
ss-<instance>-core.service
ss-<instance>-zlm.service
ss-<instance>-agent.service
```

worker/all-in-one 角色会把工作目录、ZLM HTTP 根目录和产物目录统一写到安装目录下的 `data/media/work` 与 `data/zlm/www`。如需沿用原有网络挂载或历史数据路径，安装时选择相同的 `--install-dir`，相关路径会随安装目录保持一致。

`PUBLIC_HOST` 和 `AGENT_STREAM_ADDR` 描述客户端可访问的媒体数据面。Core 不再读取或直连 ZLM 管理地址：worker/all-in-one 的 `ZLM_API_BASE` 固定由 `ZLM_HTTP_PORT` 派生为 `http://127.0.0.1:<port>`，仅供同机 Agent 转发已认证的调试命令；纯 control-plane 环境不写入 ZLM 管理地址。这里的 loopback 约束是 Agent 客户端的目标约束，不等于 ZLM HTTP listener 只监听 loopback；ZLM 在同一个 HTTP listener 上提供 API、HLS 和静态文件，`http.allow_ip_range` 因而继续包含 loopback 与 RFC1918 媒体网段，否则远端媒体播放也会返回 401。R1 依靠独立、只保存在 worker 的 `ZLM_API_SECRET` 隔离管理凭据，关闭共享 listener 的网络旁路留到 R2 统一出口/数据面改造。配置工具会把旧版非 loopback `ZLM_API_BASE` 迁移到本机地址并提示保存；`--check-only`/`--security-preflight` 对未迁移配置严格返回非零，升级流程则在停止服务前原子迁移并输出警告。

ZLM Hook 同样不直接访问 Core。worker/all-in-one 默认分配独立的 `AGENT_ZLM_HOOK_PORT=18082`，并持久化 `AGENT_ZLM_HOOK_ADDR=127.0.0.1:<port>`、`ZLM_HOOK_BASE=http://127.0.0.1:<port>/internal/zlm-hooks`、独立强随机 `ZLM_HOOK_SHARED_SECRET`、`AGENT_ZLM_HOOK_QUEUE_CAPACITY=64` 和 `AGENT_ZLM_HOOK_TIMEOUT_SEC=4`。ZLM 模板只把固定 9 个 hook 指向该 loopback 地址，`hook.timeoutSec=5`；因此 Agent 必须在 ZLM 超时前返回。query secret 仅用于同机 ZLM→Agent 请求，Agent/Core 不把它写入 control stream、ingress/control 日志或数据库；锁定 ZLM 自身的 Hook 失败日志仍可能含完整 URL/body，留待 R2 治理。配置工具把 `AGENT_MANAGEMENT_PORT` 与 `AGENT_ZLM_HOOK_PORT` 都作为受管理端口参与冲突检测，并据此重算地址。

升级旧 worker/all-in-one 时，安装器在安全 preflight 之前原子迁移 ZLM 管理和 hook 字段：移除 `ZLM_API_HOST`，把 Agent 的 API 目标改为 loopback，同时保留共享 HTTP listener 所需的 loopback/RFC1918 媒体网段，并选择未与现有端口/范围或宿主监听冲突的 hook 端口。`ZLM_API_SECRET` 与 `ZLM_HOOK_SHARED_SECRET` 只能分别保留已经存在、合规且彼此独立的强值；缺失、弱值或与 Core 的 `HOOK_SHARED_SECRET`/另一节点密钥相等时都会轮换。worker-only 环境会删除遗留的 Core `HOOK_SHARED_SECRET`，all-in-one 才保留 Core 自身兼容值；三个 secret 必须两两不同。纯 control-plane 会移除所有 Agent/ZLM endpoint 和节点 secret 字段。迁移和 preflight 均发生在停止旧服务之前；任何失败都会阻断升级、还原迁移前 `.env`，且不进入 quiesce 或调用会改变服务状态的 systemd 操作。

首次安装 worker 时，先在已经运行的 Core 上为目标节点创建 10 分钟、一次性的 enrollment token：

```bash
sudo systemd-run --quiet --pipe --wait --collect \
  --unit=streamserver-create-enrollment-$(date +%s) \
  --property=User=<core-service-user> \
  --property=Group=<core-service-group> \
  --property=EnvironmentFile=/opt/streamserver/<control-plane>/.env \
  /usr/bin/env STREAMSERVER_ENV=production \
  STREAMSERVER_UI_DIR=/opt/streamserver/<control-plane>/ui \
  /opt/streamserver/<control-plane>/bin/media-core \
  agent create-enrollment \
  --node-id <canonical-node-uuid> \
  --token-stdout
```

`--token-stdout` 是显式的敏感输出开关；标准输出只包含 token 和换行。当前 token wire 固定为 `ssae1.<96 字符 base64url>.<43 字符 base64url>`，总长 146 个 ASCII 字节；旧版 43 字符 token 不再兼容。不要把 token 放入命令行参数、环境文件、shell history 或安装日志。worker 安装器会在校验 Core HTTPS URL、CA、节点 UUID、identity 父目录和服务账户权限之后，最后从交互终端读取 token，并立即经标准输入交给 `media-agent enroll`。Agent 在本机生成 control/management 两把私钥，私钥不会离开节点。

fresh all-in-one 不要求操作员手工复制 token。安装器先完成 Core-only 安全预检和管理员原子 bootstrap，再通过同一受保护配置调用上述本地 CLI，短暂启动受限的 enrollment Core，完成真实 HTTPS enrollment 后立即停止临时 Core；token 不写入 `.env`、文件、argv 或日志。完整 Core/Agent 预检通过并交付一次性管理员密码后，才启动正式 target。

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
