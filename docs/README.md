# StreamServer Documentation

[中文](#中文) | [English](#english)

## 中文

StreamServer 文档分为三层：

- 根目录 `README.md` 负责项目展示、快速入口和文档导航。
- `docs/README.md` 负责告诉读者应该读哪篇文档。
- `docs/zh`、`docs/en` 和 `docs/adr` 负责详细说明架构、规格、部署、测试和关键决策。

### 快速了解

| 文档 | 说明 |
| --- | --- |
| [项目总览](./zh/00-overview.md) | 项目目标、组件边界、技术选型和整体方案 |
| [架构与部署拓扑](./zh/01-architecture.md) | Core、Agent、PostgreSQL、FFmpeg、ZLM 的关系 |
| [Native 部署](./zh/08-native-deployment.md) | native 离线包构建、安装、systemd 和目标机验收 |
| [测试与质量](./zh/09-testing.md) | 测试层次、质量门禁和 release/native 验证 |

### 核心设计

| 文档 | 说明 |
| --- | --- |
| [领域模型与状态机](./zh/02-domain-model-and-state-machine.md) | Task、Attempt、Lease、状态迁移和终态保护 |
| [API 规格](./zh/03-api.md) | HTTP API、幂等规则、任务操作和返回模型 |
| [Agent-Core RPC](./zh/04-agent-core-rpc.md) | 双向 gRPC 控制流、消息模型和 lease fencing |
| [FFmpeg ExecutionPlan](./zh/05-media-execution-plan.md) | 媒体执行计划、编码封装策略和 FFmpeg 参数生成 |
| [ZLM 集成](./zh/06-zlm-integration.md) | ZLMediaKit API、Hook、录制和流代理边界 |
| [数据库设计](./zh/07-database-schema.md) | PostgreSQL 表结构、索引、约束和迁移 |

### 工程与交付

| 文档 | 说明 |
| --- | --- |
| [运维、安全与风险](./zh/10-operations-and-security.md) | 风险清单、上线复核和安全运维关注点 |
| [研发流程](./zh/11-development-workflow.md) | 模块边界、协作规范和变更规则 |
| [AI 辅助开发说明](./zh/12-ai-assisted-development.md) | 对外说明项目使用 AI 工具辅助开发 |
| [路线图](./zh/13-roadmap.md) | 当前里程碑、已完成项和后续增强方向 |
| [产品边界与术语](./zh/14-product-scope-and-terminology.md) | 目标、非目标、角色和术语 |
| [Web 管理台规格](./zh/15-frontend-console.md) | 页面、字段、状态和前端交互 |
| [环境与依赖基线](./zh/16-environment-and-dependencies.md) | 本地开发、运行依赖和版本策略 |
| [部署架构图](./zh/17-deployment-diagrams.md) | 多视角部署图和网络路径 |
| [桌面客户端](../clients/streamserver-desktop/README.md) | Flutter Desktop + Rust native module 客户端工程 |

### 专项优化报告

历史专项问题、优化方案和 PR 任务拆分已统一归档到 [专项优化报告状态索引](./zh/optimization-reports/README.md)。

这些报告保留写作时的问题证据和执行计划，不再作为当前实现契约。读取单篇报告前，先看状态索引；当前 API、运行时和部署行为以核心设计文档与代码为准。

### ADR

关键架构决策记录：

- [ADR-0001: Native runtime instead of Docker runtime](./adr/0001-native-runtime-no-docker.md)
- [ADR-0002: Core-Agent control plane](./adr/0002-core-agent-control-plane.md)
- [ADR-0003: Task attempt and lease fencing](./adr/0003-task-attempt-lease-fencing.md)
- [ADR-0004: FFmpeg and ZLMediaKit boundary](./adr/0004-ffmpeg-zlm-boundary.md)
- [ADR-0005: PostgreSQL as source of truth](./adr/0005-postgresql-source-of-truth.md)

### 开源许可证与第三方项目

StreamServer 自有代码采用 **GNU General Public License v3.0 (GPL-3.0-only)**。本项目使用 [FFmpeg](https://ffmpeg.org/) 和 [ZLMediaKit](https://github.com/ZLMediaKit/ZLMediaKit) 作为第三方 runtime；发布源码或 native 离线包时，应保留并声明这些项目及其依赖的许可证、版权和源码/构建信息。具体说明见根目录 [README.md](../README.md#开源许可证与第三方项目)。

## English

The English documentation is intentionally smaller than the Chinese source docs. It focuses on the documents most useful for first-time readers and technical reviewers.

| Document | Description |
| --- | --- |
| [Overview](./en/00-overview.md) | What StreamServer is and what problem it solves |
| [Architecture](./en/01-architecture.md) | Core-Agent architecture and component boundaries |
| [Media Execution Plan](./en/05-media-execution-plan.md) | ffprobe-driven FFmpeg planning and media policy |
| [Native Deployment](./en/08-native-deployment.md) | Docker-free runtime delivery and systemd installation |
| [Testing](./en/09-testing.md) | Quality gates and verification strategy |

Architecture decisions are available under [ADR](./adr/).

### License and Third-party Notices

StreamServer-owned code is licensed under the **GNU General Public License v3.0 (GPL-3.0-only)**. StreamServer uses [FFmpeg](https://ffmpeg.org/) and [ZLMediaKit](https://github.com/ZLMediaKit/ZLMediaKit) as third-party runtime projects; source releases and native bundles should keep and declare their licenses, copyright notices, source/build information, and dependency notices. See the root [README.md](../README.md#license-and-third-party-notices).
