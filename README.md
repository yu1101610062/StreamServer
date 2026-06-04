# StreamServer

Native Rust Media Control Plane and Edge Agent System

[中文](#中文) | [English](#english)

---

## 中文

**StreamServer 是一个 Rust native 音视频任务控制面与边缘 Agent 系统。**

它由 `media-core`、`media-agent`、PostgreSQL、FFmpeg、ZLMediaKit 和 Web Console 组成，用于编排文件转码、文件转直播、拉流转发、录制、HLS/MP4/MPEGTS/RTMP/RTSP 产物和现场 native 离线部署。`media-core` 负责 API、状态机、调度、幂等、事件、审计和控制面协调；`media-agent` 通过双向 gRPC 流连接 Core，在本机管理 FFmpeg/ZLM runtime，并根据输入媒体 profile、目标封装格式和发布协议生成执行计划。

项目运行时走 native 路径：目标 Linux AMD64 主机不需要 Docker。Docker 只用于构建阶段提取 FFmpeg、ZLMediaKit、PostgreSQL 等 runtime 资产，最终交付为可离线安装的 tar 包，并由 systemd 托管服务。

### 核心亮点

- **Native runtime**: 目标机运行时不依赖 Docker/Compose，支持 Linux AMD64 离线包和 systemd 安装。
- **Rust Core-Agent 架构**: Core 统一管理任务、调度、状态、事件和 API，Agent 负责节点侧媒体 runtime。
- **双向 gRPC 控制面**: Agent 通过长连接注册、心跳、上报能力、日志、进度和事件，Core 通过同一条流下发任务控制命令。
- **任务状态机**: 基于 `TaskAttempt`、`TaskLease`、`attempt_no` 和 `lease_token` 防止旧 Agent 消息污染新任务状态。
- **幂等与事务一致性**: 任务创建、派发和状态迁移使用 PostgreSQL 事务、行锁、operation request 和事件表。
- **动态媒体执行计划**: Agent 基于 ffprobe profile 选择 copy、转码、muxer、bitstream filter、发布协议和 fallback 策略。
- **FFmpeg + ZLMediaKit 编排**: 支持文件处理、直播化、拉流代理、录制、HLS、MP4、MPEGTS、RTMP、Enhanced RTMP、RTSP 等路径。
- **Web 管理台**: Vue/Vite 控制台覆盖任务、节点、上传、流、录像、安全和调试页面。
- **Native 离线交付**: 包含业务二进制、UI、FFmpeg/ZLM/PostgreSQL runtime、安装器、卸载器、配置 TUI 和目标机验证脚本。
- **自动化测试覆盖**: 覆盖领域状态机、Repository、控制面、Agent runtime、FFmpeg plan、前端共享逻辑和 native bundle 布局。

### 架构总览

```text
Web Console / Desktop Client / External API
          |
          | HTTP API
          v
    media-core
  +--------------------------------------+
  | REST API / Web UI                    |
  | Task State Machine                   |
  | Scheduler and Dispatch               |
  | Idempotency and Lease Fencing        |
  | Events / Audit / Callback Outbox     |
  | PostgreSQL Repository                |
  +--------------------------------------+
          |
          | Bidirectional gRPC stream
          v
    media-agent
  +--------------------------------------+
  | Runtime Registry                     |
  | FFmpeg ExecutionPlan                 |
  | ffprobe Capability Probe             |
  | ZLMediaKit Adapter                   |
  | Recording / Progress / Logs          |
  | Native Process Management            |
  +--------------------------------------+
          |
          v
 FFmpeg / ZLMediaKit / Local Media Runtime
```

### 功能矩阵

| 能力 | 说明 | 状态 |
| --- | --- | --- |
| Core-Agent control plane | 双向 gRPC 控制流，支持注册、心跳、任务下发、事件回传 | 已实现 |
| Task state machine | 任务生命周期、状态迁移白名单、终态保护 | 已实现 |
| Attempt / lease fencing | 通过 `attempt_no` 和 `lease_token` 防止 stale message 污染状态 | 已实现 |
| Idempotency | 使用 operation request 处理重复请求、冲突和 replay | 已实现 |
| PostgreSQL persistence | 任务、节点、事件、审计、录像、Hook 和 callback outbox 持久化 | 已实现 |
| FFmpeg execution plan | 根据输入 profile 和目标格式生成 FFmpeg 参数 | 已实现 |
| Dynamic media policy | 编码、封装、发布协议和 fallback 动态选择 | 已实现 |
| ZLMediaKit integration | ZLM API、Hook、RTMP/RTSP/HLS/录制相关适配 | 已实现 |
| Native bundle | Linux AMD64 native 离线包，目标机无 Docker 运行时依赖 | 已实现 |
| systemd install | `install.sh`、`uninstall.sh`、service unit 和 `streamserverctl` | 已实现 |
| Web console | Vue/Vite 管理台 | 已实现 |
| GPU runtime package | GPU FFmpeg runtime 包变体和能力基础 | 已实现 |
| GPU scheduling closure | GPU 调度闭环、容量模型和生产策略仍需增强 | 增强中 |
| Production hardening | 安全、可观测性、升级回滚和现场运维继续增强 | 增强中 |

### 媒体处理策略

Agent 不直接拼接用户传入的 FFmpeg 字符串。它先通过 ffprobe 获取输入媒体 profile，再根据目标封装格式、发布协议、ZLM 能力和节点能力生成 `ExecutionPlan`。

| 场景 | 策略 |
| --- | --- |
| H.264 + AAC -> RTMP | copy video/audio，使用 FLV/RTMP |
| HEVC / AV1 / VP9 -> Live | 优先 Enhanced RTMP；不可用时 fallback 到 RTSP |
| MPEGTS/HLS AAC -> MP4/FLV | 自动追加 `aac_adtstoasc` |
| 多音轨输入 | 选择目标容器可 copy 的优先音轨 |
| 不兼容音频 | 按目标格式转 AAC/Opus 或拒绝 |
| HLS 输出 | 生成 m3u8 与 segment template |
| Native recording | 支持 MP4、HLS 或双输出录制策略 |
| WebM | 保留为上传输入能力，当前不作为输出目标格式暴露 |

### Native 部署

StreamServer 的运行时不依赖 Docker。Docker 只用于构建阶段：

- 构建 Rust musl binary。
- 提取 FFmpeg runtime。
- 提取 ZLMediaKit runtime。
- 提取 PostgreSQL runtime 和工具。
- 生成 Linux AMD64 离线安装包。

构建 native 包：

```bash
./scripts/build-native-bundle.sh --without-gpu
./scripts/build-native-bundle.sh --with-gpu
./scripts/build-native-bundle.sh --control-plane-minimal
```

目标机安装：

```bash
tar -xzf streamserver-native-*.tar.gz
cd streamserver-native-*
./install.sh --check-only
sudo ./install.sh
/opt/streamserver/<role>/bin/streamserverctl status
/opt/streamserver/<role>/bin/streamserverctl health
```

目标机验收：

```bash
./scripts/verify-native-bundle-on-target.sh \
  --bundle dist/streamserver-native-v0.1.0-linux-amd64-cpu-only-<date>.tar.gz \
  --host <target-host>
```

完整说明见 [Native 部署](docs/zh/08-native-deployment.md)。

### 快速开始

运行测试：

```bash
cargo test --workspace --all-targets
```

建议质量门禁：

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
```

本地运行 Core 需要可访问的 PostgreSQL：

```powershell
$env:DATABASE_URL="postgres://postgres:postgres@127.0.0.1:5432/streamserver"
cargo run -p media-core
```

本地运行 Agent 需要 FFmpeg/ffprobe 可用，并指向 Core gRPC 地址：

```powershell
$env:AGENT_CORE_ENDPOINT="http://127.0.0.1:50051"
cargo run -p media-agent
```

前端开发：

```bash
cd crates/media-core/frontend
npm install
npm run dev
```

### 测试与质量

本项目测试覆盖重点包括：

- `TaskSpec` 校验和状态机迁移。
- 幂等请求、任务派发和 attempt/lease fencing。
- stale Agent message 防护。
- Repository、callback outbox、ZLM Hook 和鉴权逻辑。
- Agent runtime registry、进程生命周期、录制控制和产物清理。
- FFmpeg plan、codec/muxer policy、HLS/MP4/RTMP/RTSP 组合。
- Web console 共享逻辑和媒体链接展示。
- native bundle 布局、目标机验证和 codec smoke matrix。

release/native 验证建议额外执行：

```bash
./scripts/smoke-codec-matrix.sh
./scripts/verify-native-bundle-on-target.sh --bundle <bundle> --host <target-host>
```

### 文档导航

文档入口见 [docs/README.md](docs/README.md)。

高价值文档：

- [项目总览](docs/zh/00-overview.md)
- [架构与部署拓扑](docs/zh/01-architecture.md)
- [领域模型与状态机](docs/zh/02-domain-model-and-state-machine.md)
- [Agent-Core RPC](docs/zh/04-agent-core-rpc.md)
- [FFmpeg ExecutionPlan 与媒体策略](docs/zh/05-media-execution-plan.md)
- [Native 部署](docs/zh/08-native-deployment.md)
- [测试计划与质量门禁](docs/zh/09-testing.md)
- [ADR: Native runtime no Docker](docs/adr/0001-native-runtime-no-docker.md)

### 项目状态

| Area | Status |
| --- | --- |
| Core API | 已实现 |
| Agent runtime | 已实现 |
| Core-Agent gRPC | 已实现 |
| PostgreSQL persistence | 已实现 |
| Web console | 已实现 |
| Native bundle | 已实现 |
| Automated tests | 已实现 |
| WebM output policy | 当前保守关闭输出目标 |
| GPU scheduling closure | 增强中 |
| Production observability | 增强中 |
| Upgrade / rollback | 增强中 |

### 开发说明

StreamServer 使用 AI 辅助开发流程。项目维护者负责需求、架构、代码审查、测试、部署验证和持续重构。详细说明见 [AI 辅助开发说明](docs/zh/12-ai-assisted-development.md)。

### 开源许可证与第三方项目

本项目自有代码采用 **GNU General Public License v3.0 (GPL-3.0-only)** 发布。

StreamServer 运行和 native 离线包构建使用以下第三方项目。本仓库许可证不覆盖这些项目及其运行时二进制：

- [FFmpeg](https://ffmpeg.org/): FFmpeg 默认使用 LGPLv2.1+；如果构建或分发的 FFmpeg 启用了 GPL 组件，则对应 FFmpeg 二进制适用 GPLv2+。发布包含 FFmpeg runtime 的包时，应保留 FFmpeg 的许可证、版权、源码获取方式和构建配置说明。实际构建不得引入与 GPLv3 不兼容或不可再分发的组件。
- [ZLMediaKit](https://github.com/ZLMediaKit/ZLMediaKit): ZLMediaKit 自有代码使用 MIT License。使用或分发 ZLMediaKit runtime 时，应保留其版权信息，并同时声明其依赖的第三方库许可证。

发布源码或 native 离线包前，应以实际打包的 FFmpeg/ZLMediaKit 构建参数和依赖清单为准复核许可证声明。

---

## English

**StreamServer is a native Rust media control-plane and edge-agent system.**

It combines `media-core`, `media-agent`, PostgreSQL, FFmpeg, ZLMediaKit, and a Vue/Vite web console into an orchestration platform for media workloads. The Core service handles APIs, task state machines, scheduling, idempotency, events, auditing, and control-plane coordination. The Agent connects to Core through a bidirectional gRPC stream, manages local FFmpeg/ZLM runtimes, and builds execution plans from input media profiles, target containers, and publishing protocols.

The runtime path is native. Target Linux AMD64 hosts do not require Docker. Docker is only used during the build phase to extract runtime assets. The final deliverable is an offline tarball installed as systemd services.

### Highlights

- **Native runtime**: no Docker/Compose dependency on target hosts.
- **Rust Core-Agent architecture**: Core owns APIs, state, scheduling, events, and persistence; Agent owns local media runtime execution.
- **Bidirectional gRPC control plane**: registration, heartbeat, capability snapshots, task dispatching, progress, logs, and events use one long-lived stream.
- **Task state machine**: `TaskAttempt`, `TaskLease`, `attempt_no`, and `lease_token` protect state from stale Agent messages.
- **Idempotency and consistency**: task creation, dispatching, and transitions use PostgreSQL transactions, row locks, operation requests, and event storage.
- **Dynamic media execution planning**: ffprobe-based profile analysis drives copy/transcode, muxer, bitstream filter, protocol, and fallback decisions.
- **FFmpeg + ZLMediaKit orchestration**: file transcoding, file-to-live, stream relay, recording, HLS, MP4, MPEGTS, RTMP, Enhanced RTMP, and RTSP paths.
- **Web console**: a Vue/Vite management UI for tasks, nodes, uploads, streams, recordings, security, and debugging.
- **Offline native delivery**: business binaries, UI assets, FFmpeg/ZLM/PostgreSQL runtime, installer, uninstaller, config TUI, and verification scripts.
- **Automated tests**: domain, repository, control plane, Agent runtime, FFmpeg plans, frontend shared logic, and native bundle layout.

### Architecture

```text
Web Console / Desktop Client / External API
          |
          | HTTP API
          v
    media-core
          |
          | Bidirectional gRPC stream
          v
    media-agent
          |
          v
 FFmpeg / ZLMediaKit / Local Media Runtime
```

### Feature Matrix

| Capability | Description | Status |
| --- | --- | --- |
| Core-Agent control plane | Bidirectional gRPC stream for registration, heartbeat, dispatch, and events | Implemented |
| Task state machine | Lifecycle transitions and terminal-state protection | Implemented |
| Attempt / lease fencing | `attempt_no` and `lease_token` reject stale Agent messages | Implemented |
| Idempotency | Operation requests handle duplicate calls, conflicts, and replay | Implemented |
| PostgreSQL persistence | Tasks, nodes, events, audit, recordings, hooks, and callback outbox | Implemented |
| FFmpeg execution plan | FFmpeg arguments generated from profiles and target formats | Implemented |
| Dynamic media policy | Codec, container, publishing protocol, and fallback decisions | Implemented |
| ZLMediaKit integration | ZLM API, Hook, RTMP/RTSP/HLS, and recording adapters | Implemented |
| Native bundle | Linux AMD64 offline bundle without Docker runtime dependency | Implemented |
| systemd install | Installer, uninstaller, service units, and `streamserverctl` | Implemented |
| Web console | Vue/Vite management UI | Implemented |
| GPU scheduling closure | GPU capacity and scheduling loop still need hardening | In progress |
| Production hardening | Security, observability, upgrade, and rollback continue to evolve | In progress |

### Media Processing Strategy

The Agent does not blindly concatenate user-provided FFmpeg strings. It probes inputs with ffprobe and builds an `ExecutionPlan` from the media profile, target container, publishing protocol, ZLM capabilities, and node capabilities.

| Scenario | Decision |
| --- | --- |
| H.264 + AAC -> RTMP | copy video/audio and use FLV/RTMP |
| HEVC / AV1 / VP9 -> Live | prefer Enhanced RTMP and fall back to RTSP |
| MPEGTS/HLS AAC -> MP4/FLV | add `aac_adtstoasc` automatically |
| Multi-audio input | select the best copy-safe audio stream |
| Incompatible audio | transcode to AAC/Opus or reject depending on target |
| HLS output | generate m3u8 and segment templates |
| Native recording | support MP4, HLS, or dual-output recording |
| WebM | accepted as upload input, not exposed as an output target |

### Native Deployment

Build native bundles:

```bash
./scripts/build-native-bundle.sh --without-gpu
./scripts/build-native-bundle.sh --with-gpu
./scripts/build-native-bundle.sh --control-plane-minimal
```

Install on a target host:

```bash
tar -xzf streamserver-native-*.tar.gz
cd streamserver-native-*
./install.sh --check-only
sudo ./install.sh
/opt/streamserver/<role>/bin/streamserverctl status
/opt/streamserver/<role>/bin/streamserverctl health
```

See [Native Deployment](docs/en/08-native-deployment.md).

### Quick Start

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
```

Run Core with PostgreSQL:

```bash
DATABASE_URL=postgres://postgres:postgres@127.0.0.1:5432/streamserver cargo run -p media-core
```

Run Agent:

```bash
AGENT_CORE_ENDPOINT=http://127.0.0.1:50051 cargo run -p media-agent
```

Run the web console:

```bash
cd crates/media-core/frontend
npm install
npm run dev
```

### Documentation

Start with [docs/README.md](docs/README.md). Key English docs:

- [Overview](docs/en/00-overview.md)
- [Architecture](docs/en/01-architecture.md)
- [Media Execution Plan](docs/en/05-media-execution-plan.md)
- [Native Deployment](docs/en/08-native-deployment.md)
- [Testing](docs/en/09-testing.md)

### Project Status

The core control loop, Agent runtime, gRPC control plane, PostgreSQL persistence, web console, native packaging, and automated tests are implemented. The current hardening focus is GPU scheduling closure, production observability, security posture, upgrade/rollback, and broader real FFmpeg smoke coverage.

### Development Note

StreamServer is developed with AI-assisted engineering workflows. Project maintainers remain responsible for requirements, architecture, code review, testing, deployment validation, and iterative refactoring.

### License and Third-party Notices

StreamServer-owned code is licensed under the **GNU General Public License v3.0 (GPL-3.0-only)**.

StreamServer uses the following third-party projects at runtime and during native bundle assembly. The repository license does not cover those projects or their runtime binaries:

- [FFmpeg](https://ffmpeg.org/): FFmpeg is licensed under LGPLv2.1+ by default. If the FFmpeg build enables GPL components, the corresponding FFmpeg binary is covered by GPLv2+. When distributing bundles that include FFmpeg runtime assets, keep the FFmpeg license, copyright notices, source availability, and build configuration notes. The actual build must not include components that are incompatible with GPLv3 or not redistributable.
- [ZLMediaKit](https://github.com/ZLMediaKit/ZLMediaKit): ZLMediaKit's self-owned code is licensed under the MIT License. When using or distributing ZLMediaKit runtime assets, keep its copyright notices and declare the licenses of its third-party dependencies.

Before publishing source releases or native bundles, verify the license notices against the actual FFmpeg/ZLMediaKit build options and dependency manifests included in the distributed artifacts.
