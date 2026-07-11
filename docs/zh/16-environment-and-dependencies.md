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
- `CORE_INSECURE_DEV`
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
- `AGENT_CERT_PATH`
- `AGENT_KEY_PATH`
- `FFMPEG_BIN`
- `FFPROBE_BIN`
- `WORK_ROOT`
- `UPLOAD_MAX_BYTES`
- `UPLOAD_ALLOWED_EXTENSIONS`
- `UPLOAD_PROBE_TIMEOUT_SEC`
- `PUBLIC_MEDIA_BASE_URL`

`STREAMSERVER_ENV=production` 时，`AUTH_MODE=disabled` 会被拒绝；gRPC 必须配置完整的服务端证书、私钥和客户端 CA。Core HTTP 未配置证书时只能监听 loopback，监听非 loopback 必须同时配置 HTTP 证书和私钥。仅本地开发可使用 `media-core --insecure-dev`，且 HTTP/gRPC 必须同时监听 loopback。

Native fresh install 选择 `local_password` 时必须在交互终端执行。安装器使用安全随机源生成一次性管理员初始密码，只在全部安装步骤成功后直接向终端显示一次，不写入 `.env` 或安装日志；首次登录响应会要求立即改密，改密同时撤销此前的 refresh 会话。升级和安全预检不会重新生成或重置管理员密码。

手动上传文件由 Agent 写入 `WORK_ROOT/uploads/<node_id>/YYYY/MM/DD/`；Core 代理上传请求并维护上传产物台账，后续 `input.kind=file` 任务按路径内的 `<node_id>` 做节点亲和调度。Agent 注册时会上报 `agent_http_base_url`，Core 使用该地址转发上传请求，不需要额外配置上传地址模板。Agent 心跳会上报 `WORK_ROOT` 所在磁盘的总量、剩余空间和使用率，Core 自动选择上传节点时优先选择上传盘剩余空间更大的节点。上传后的 `ffprobe` 时长探测使用较大的探测窗口；探测失败或超时时上传仍成功，`durationSec` 返回默认值 `0`。

### 7.3 ZLM

- `api.secret`
- `hook.enable`
- `hook.url`
- `http.allow_ip_range`
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

- 为 `media-core` 和 `media-agent` 生成内部 mTLS 证书。
- 为 JWT 准备公钥或 JWK 配置。
- ZLM Hook 采用 shared secret，不直接使用匿名回调。

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
- ZLM API 与 Hook 联通。
- FFmpeg/ffprobe 路径有效，能力探测能成功返回。
- 组播测试地址已确认可收发。
- 媒体样本和目录挂载均已准备。
- 本地或测试环境的 JWT 与 mTLS 证书已可用。
