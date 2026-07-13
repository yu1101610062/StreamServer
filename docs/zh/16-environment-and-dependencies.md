# 16. 环境准备与依赖基线

## 1. 文档目标

本文件定义开发、测试和部署前必须准备的基础环境、依赖版本策略、目录挂载、网络条件和媒体样本。

## 2. 基础依赖基线

| 组件 | 基线 |
| --- | --- |
| 操作系统 | Linux，优先 Ubuntu 22.04 LTS 或 24.04 LTS |
| Rust | stable，MSRV `1.85+`，edition `2024` |
| PostgreSQL | 16 为最低兼容基线；CI 固定覆盖 16 和 18.3；native bundle 默认 18.3 |
| FFmpeg | 随包 runtime 固定 `8.1` 系列；外部自带 FFmpeg 至少 `6.1+`，必须启用所需协议和编码器 |
| ZLMediaKit | 随包 runtime 固定版本；不允许现场使用 `latest` |

## 3. 版本策略

- 开发环境允许使用次新版，但 CI 和联调环境必须固定版本。
- 所有第三方 runtime 必须锁定版本和来源。
- PostgreSQL 16 是最低兼容基线；CI 同时覆盖 16 和 18.3；native bundle 默认携带 18.3。
- FFmpeg build 来源必须记录到节点能力表中；GPU 节点必须实际通过 `h264_nvenc` 和 `hevc_nvenc` 编码 smoke test。

## 4. 必备系统资源

| 角色 | 最低建议 |
| --- | --- |
| 本地开发机 | 4 核 CPU / 8GB RAM / 20GB 可用磁盘 |
| 集成测试机 | 8 核 CPU / 16GB RAM / 100GB 可用磁盘 |
| 组播节点 | 双网卡或已确认可用的组播网卡 |

## 5. 网络准备

- 普通联调直接使用宿主机端口。
- 组播联调必须准备可用宿主机网卡、路由和防火墙策略。
- 提前确认交换机开启 IGMP Snooping 或按网络方案配置。
- 主机防火墙必须放行所需 UDP 端口范围。

## 6. 目录准备

必须准备以下目录并授予运行用户读写权限：

- `/data/zlm/www`
- `/data/media/work`
- `/data/media/logs`
- `/data/postgres`

## 7. 配置项基线

### 7.1 `media-core`

- `CORE_HTTP_ADDR`
- `CORE_HTTP_TLS_CERT_PATH`
- `CORE_HTTP_TLS_KEY_PATH`
- `CORE_GRPC_ADDR`
- `CORE_GRPC_TLS_CERT_PATH`
- `CORE_GRPC_TLS_KEY_PATH`
- `CORE_GRPC_TLS_CLIENT_CA_PATH`
- `CORE_GRPC_TLS_SERVER_CA_PATH`
- `CORE_AGENT_CA_CERT_PATH`
- `CORE_AGENT_CA_KEY_PATH`
- `CORE_AGENT_CAPABILITY_JWT_PRIVATE_KEY_PATH`
- `CORE_AGENT_CAPABILITY_JWT_PUBLIC_KEY_PATH`
- `CORE_AGENT_CAPABILITY_TTL_SEC`
- `CORE_INSTANCE_ID`
- `CORE_AGENT_MANAGEMENT_CLIENT_CERT_PATH`
- `CORE_AGENT_MANAGEMENT_CLIENT_KEY_PATH`
- `CORE_AGENT_MANAGEMENT_CA_PATH`
- `DATABASE_URL`
- `AUTH_MODE`
- `JWT_PUBLIC_KEY`
- `AUTH_JWT_PRIVATE_KEY_PATH`
- `AUTH_JWT_PUBLIC_KEY_PATH`
- `AUTH_ACCESS_TOKEN_TTL`
- `AUTH_REFRESH_TOKEN_TTL`
- `HOOK_SHARED_SECRET`
- `STORAGE_ALLOWLIST`

### 7.2 `media-agent`

- `AGENT_NODE_NAME`
- `AGENT_CORE_ENDPOINT`
- `AGENT_IDENTITY_DIR`
- `AGENT_TLS_DOMAIN_NAME`
- `AGENT_PUBLIC_MEDIA_ADDR`
- `AGENT_PUBLIC_MEDIA_EXPOSE`
- `AGENT_MANAGEMENT_ADDR`
- `AGENT_MANAGEMENT_PORT`（安装器/配置工具管理的端口键）
- `AGENT_MANAGEMENT_MAX_CONCURRENCY`
- `AGENT_MANAGEMENT_CHUNK_IDLE_TIMEOUT_SEC`
- `AGENT_ZLM_HOOK_ADDR`（Agent 实际监听地址）
- `AGENT_ZLM_HOOK_PORT`（安装器/配置工具管理的端口键）
- `AGENT_ZLM_HOOK_QUEUE_CAPACITY`
- `AGENT_ZLM_HOOK_TIMEOUT_SEC`
- `ZLM_HOOK_SHARED_SECRET`
- `ZLM_HOOK_BASE`（供本地 ZLM 配置渲染）
- `FFMPEG_BIN`
- `FFPROBE_BIN`
- `ZLM_HTTP_PORT`
- `ZLM_API_BASE`（固定为 `http://127.0.0.1:<ZLM_HTTP_PORT>`）
- `ZLM_API_SECRET`（worker 本地独立强随机值，不得复用 Core/Agent Hook secret）
- `ZLM_API_ALLOW_IP_RANGE`（ZLM API 与媒体共享 HTTP listener，native 当前保留 loopback 与 RFC1918 媒体网段）
- `WORK_ROOT`
- `UPLOAD_MAX_BYTES`
- `UPLOAD_ALLOWED_EXTENSIONS`
- `UPLOAD_PROBE_TIMEOUT_SEC`
- `PUBLIC_MEDIA_BASE_URL`

`STREAMSERVER_ENV=production` 时，`AUTH_MODE=disabled` 会被拒绝；gRPC 必须配置完整的服务端证书、私钥和客户端 CA。Core HTTP 未配置证书时只能监听 loopback，监听非 loopback 必须同时配置 HTTP 证书和私钥。仅本地开发可使用 `media-core --insecure-dev`，且 HTTP/gRPC 必须同时监听 loopback。

Native fresh install 选择 `local_password` 时必须在交互终端执行。安装器使用安全随机源生成一次性管理员初始密码，在严格安全预检通过后直接向终端显示一次，不写入 `.env`、安装日志或交付状态文件；首次登录响应会要求立即改密，改密同时撤销此前的 refresh 会话。

独立 worker 首次安装需要管理员预先在 Core 创建绑定精确 `node_id` 的 10 分钟一次性 enrollment token；安装器只在所有非秘密参数、CA 与目录权限校验通过后读取 token，并立即通过 stdin 消费。fresh all-in-one 由安装器调用受保护的本地 Core CLI 创建 token，使用短生命周期的临时 Core 完成同一 HTTPS enrollment 流程，随后销毁 token 并停止临时 Core；任何角色都不会把 token 写入 argv、环境文件或日志。

管理员初始密码交付使用安装目录外的 root-only durable 状态目录 `/var/lib/streamserver-native-installer/<install-dir-sha256>/`。`pending` 标记只保存规范化用户名、随机 handoff ID、安装目录指纹和 JWT 公钥指纹；数据库同时保存该 handoff ID、单调版本和完成时间。安装器以实例锁串行执行，并通过 handoff ID + expected version 的事务 CAS 只创建初始管理员或恢复仍属于同一交付的强制改密账号；并发恢复恰好一个成功，普通管理员重置不属于该交付。`pending` 或 `delivered` 存在期间安装器跳过高级配置 TUI，避免在数据库 reconcile 前后更换 JWT key/path；安装成功后可单独运行配置工具并重启服务。

`pending` 存在时 systemd 会拒绝启动 Core/target；密码显示成功后标记原子切换为 `delivered`，即使后续 `systemctl start` 失败，交互式重跑也只继续安装，不会再次重置或显示密码。若账号已经完成改密，重跑只确认完成状态，不覆盖长期密码；账号冲突、handoff 版本过期、JWT 指纹变化、状态目录不可读或权限不安全都会中止。登录、刷新和改密会在数据库事务内锁定账号并校验密码版本，不能在 recovery 提交后凭旧密码签发新会话或覆盖恢复密码。

`local_password` access JWT 使用 `urn:streamserver:credential_version` 和 `urn:streamserver:must_change_password` 私有 claim 绑定数据库凭据状态；每次本地用户鉴权都会重新校验账号启用状态、凭据版本和强制改密状态。恢复、改密或重置密码都会单调递增凭据版本，使此前签发的 access JWT 立即失效。强制改密期间业务权限全部拒绝，仍允许读取 `/me`、改密和退出；控制台会将该会话锁定在只显示改密面板的安全页，不读取或展示机器白名单。`external_jwt` 不解释这些本地私有 claim，也不会为此查询本地用户数据库。

`--check-only` 和 `--security-preflight` 对该状态只读：发现 `pending` 或无法遍历 root-only 状态目录时返回非零，不生成密码、不修改数据库。没有 handoff 标记的普通升级不会创建或重置管理员密码。

`--upgrade` 会先取得实例锁并原子封存现有环境文件。在安全 preflight 和停止旧服务之前，旧 worker/all-in-one 会原子移除 `ZLM_API_HOST`，把 Agent 使用的 `ZLM_API_BASE` 重写为本机 loopback，并把共享 ZLM HTTP listener 的 `allow_ip_range` 恢复为 loopback 加 RFC1918 媒体网段；仅设 loopback 会同时破坏远端 HLS/静态媒体。安装器还会补齐 Agent hook ingress：保留无冲突的既有端口，否则从 `18082` 起避开已配置端口、范围与宿主监听选择新端口；`ZLM_API_SECRET` 和 `ZLM_HOOK_SHARED_SECRET` 仅在各自是 URL-safe 32–256 字节且与 Core/另一节点 secret 独立时保留，否则轮换。worker-only 删除遗留 `HOOK_SHARED_SECRET`，all-in-one 保留 Core 自身的兼容 hook secret，三者必须两两不同。随后写入精确 loopback hook addr/base、queue `64` 和 timeout `4`。纯 control-plane 会删除所有遗留 Agent/ZLM endpoint 与节点 secret 字段。迁移完成后才使用新包内 `media-core` 做安全预检；迁移或预检失败不会停止服务，也不会调用 systemd 进入 quiesce。预检通过后才记录当前 active 的 Core/Agent/ZLM 主进程 PID，并停止这些非数据库服务和实例 target；内置 PostgreSQL 保持运行以完成迁移。升级成功时只恢复升级前 active 的 unit 与 target 状态，原本 inactive 的 unit 不会被意外启动，并验证原 active unit 的 MainPID 已变化；升级中任一步失败时也会尽力恢复同一状态集合。选择 `--no-start` 明确表示成功升级后保持这些服务停止。重新激活或健康检查失败时安装器返回非零并保留尚未完成清理的密码交付标记，以便安全重跑。

手动上传文件由 Agent 写入 `WORK_ROOT/uploads/<node_id>/YYYY/MM/DD/`；Core 代理上传请求并维护上传产物台账，后续 `input.kind=file` 任务按路径内的 `<node_id>` 做节点亲和调度。Agent 的公开媒体 listener 与 mTLS management listener 相互独立：公开 listener 默认只绑定 loopback 且不提供写接口；Core 根据已认证控制会话的 peer IP、证书 DNS SAN 和注册端口访问 management listener，不信任 Agent 自报 URL。每次上传或删除还必须携带由 Core 临时签发并绑定节点、操作、路径、字节上限和有效期的 capability JWT。Agent 心跳会上报 `WORK_ROOT` 所在磁盘的总量、剩余空间和使用率，Core 自动选择上传节点时优先选择上传盘剩余空间更大的节点。上传后的 `ffprobe` 时长探测使用较大的探测窗口；探测失败或超时时上传仍成功，`durationSec` 返回默认值 `0`。

Agent 对 ZLM 的管理调用目标限定在本机：`ZLM_API_BASE` 必须精确等于由 `ZLM_HTTP_PORT` 派生的 loopback URL。`PUBLIC_HOST` 只用于媒体播放数据面，不能改变管理目标。Core 不读取或请求节点的 ZLM 地址；调试请求经已认证控制流发送给 Agent，再由 Agent 本地调用 ZLM。需要注意，ZLM 的 API、HLS 和静态文件仍共享 HTTP listener，`http.allow_ip_range` 不能在 R1 收窄为 loopback；同网段请求在获知 `ZLM_API_SECRET` 时仍可到达 API，这一网络旁路由 R2 的统一出口/数据面边界关闭。

ZLM Hook 只调用 `AGENT_ZLM_HOOK_ADDR=127.0.0.1:<AGENT_ZLM_HOOK_PORT>`。本地 query secret 使用常量时间校验，入口只接受固定 9 个 hook、最大 256 KiB 的 JSON object，并在进入 mTLS control stream 前递归清除 `secret` 与 4 种 server identity 拼写；`on_server_started` 会携带完整 mINI，因此 Agent 将其正文严格归一化为空对象，只转发“服务已启动”事件。入口不挂载记录完整 URI 的默认 trace middleware。队列与 pending response map 均有界；relay timeout 为 `1..4` 秒且 native 默认 `4`，严格小于 ZLM 的 `hook.timeoutSec=5`。本地 400/401/413/503/504 错误、取消、断连和晚到响应都只影响当前 Hook，不终止 control stream。该保证只覆盖 Agent/Core；锁定 ZLM 自身在 Hook 失败时仍可能记录完整 URL/body，需在 R2 处理。

### 7.3 ZLM

- `api.secret`
- `hook.enable`
- `hook.on_publish`
- `hook.on_rtp_server_timeout`
- `hook.on_record_mp4`
- `hook.on_record_ts`
- `hook.on_record_hls`
- `hook.on_stream_none_reader`
- `hook.on_stream_not_found`
- `hook.on_server_keepalive`
- `hook.on_server_started`
- `hook.timeoutSec=5`
- `hook.retry=1`
- `http.allow_ip_range`（同时影响 HTTP API 与媒体文件访问；native 当前为 loopback + RFC1918 媒体网段）
- `multicast.addrMin`
- `multicast.addrMax`
- `multicast.udpTTL`

## 8. 媒体样本准备

至少准备以下样本：

- 1 路 RTSP 可稳定访问的摄像头或模拟源
- 1 路 RTMP 样本源
- 1 个时长 1 分钟以上的 MP4 文件
- 1 个带音频和字幕的测试文件
- 1 组可加入的组播地址和端口
- 1 套 RTP/GB28181 测试来源

## 9. 证书与密钥

- Core 内部 PKI 使用三套相互独立的根：Agent control/management leaf issuer、Core gRPC server CA、Core management client CA；不得复用 JWT 或 capability key。
- Agent 使用管理员创建的 10 分钟一次性 enrollment token，本地生成 control 与 management 两把私钥和 CSR。Core 签发 90 天证书；剩余 30 天开始一次性双证书轮换。
- production gRPC 强制 mTLS；Core 从 Agent leaf 的 SPIFFE URI SAN 提取节点身份，首包 `node_id` 只做一致性校验。
- Core 访问 Agent management listener 时也使用 mTLS，并在 CA/DNS 校验后精确 pin 当前或待轮换 management leaf 指纹。
- 为 JWT 准备公钥或 JWK 配置。
- ZLM Hook shared secret 只保护同机 ZLM→Agent loopback 请求；节点身份由 Agent→Core mTLS 会话保护，两者不能互相替代。secret 可以出现在本机 query 中，但不得进入 control stream、完整 URI access log、事件或数据库。

## 10. Native 拓扑建议

本地开发标准服务：

- `media-core`
- `media-agent`
- `zlmediakit`
- `postgres`

组播场景：

- `media-agent` 与 `zlmediakit` 直接使用宿主机网卡
- `media-core` 与 `postgres` 使用本机或内网 TCP 端口

## 11. 开工前检查清单

- PostgreSQL 可正常连接并已执行迁移。
- Core 不持有或直连 ZLM API 地址，Agent 的管理目标为 loopback 且使用独立 `ZLM_API_SECRET`；共享 ZLM HTTP listener 的 RFC1918 网络旁路作为 R2 已知项。Hook listener 只监听 `127.0.0.1`，错误 secret 会被拒绝，真实 Hook 能经 mTLS control stream 往返，Agent readiness 显示 hook listener available。
- FFmpeg/ffprobe 路径有效，能力探测能成功返回。
- 组播测试地址已确认可收发。
- 媒体样本和目录挂载均已准备。
- 本地或测试环境的 JWT 与 mTLS 证书已可用。
