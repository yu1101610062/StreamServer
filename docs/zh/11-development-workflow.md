# 11. 研发流程与协作规范

## 1. 文档目标

本文件定义代码组织、分支策略、配置约定、日志规范和协作要求，避免开发过程中出现风格和边界漂移。

## 2. 代码目录建议

建议尽快演进为 Cargo workspace：

```text
.
├── crates/
│   ├── media-core/
│   ├── media-agent/
│   ├── media-domain/
│   ├── media-api/
│   ├── media-adapters/
│   └── media-proto/
├── docs/
├── migrations/
└── deploy/
```

职责约束：

- `media-domain` 只放领域模型、状态机、校验。
- `media-api` 只放 HTTP API 请求/响应模型。
- `media-adapters` 放 ZLM、FFmpeg、存储适配层。
- `media-proto` 放 gRPC/proto 定义。

## 3. 分支策略

- `master` 为可发布分支，不直接推送日常开发提交。
- `DEV` 为日常集成分支，功能、缺陷和文档工作均从 `DEV` 创建分支。
- 功能开发使用 `feature/<topic>`，缺陷修复使用 `fix/<topic>`，文档补充使用 `docs/<topic>`。
- 开发分支完成后向 `DEV` 提交 PR，并通过服务端 CI 后合并。
- 发布时从 `DEV` 向 `master` 提交发布 PR；禁止用绕过 PR 的方式同步发布分支。

## 4. 提交与评审规则

- 一个 PR 只做一类事情：功能、重构、文档、迁移不得混杂。
- 任何接口、状态、数据库变更都必须同步更新 `docs/`。
- 迁移文件必须和使用它的代码一并提交。

## 5. 配置规范

- 配置按 `base + env override + env vars` 三层加载。
- 不允许在代码中硬编码路径、端口、密钥。
- 敏感信息必须来自环境变量或密钥管理系统。

## 6. 日志规范

所有结构化日志至少包含：

- `ts`
- `level`
- `request_id`
- `task_id`
- `attempt_no`
- `node_id`
- `component`
- `event`

日志风格：

- 面向检索，禁止输出无上下文的纯文本错误。
- 用户操作必须写审计日志。

## 7. 错误码规范

错误码格式：

```text
<DOMAIN>_<ACTION>_<DETAIL>
```

例如：

- `TASK_START_INVALID_STATE`
- `NODE_CAPABILITY_MISSING_CODEC`
- `ZLM_ADD_STREAM_PROXY_FAILED`
- `FFMPEG_OUTPUT_BACKPRESSURE`

## 8. 文档先行规则

以下变更必须先改文档再改代码：

- 新任务类型
- 新状态
- 新外部接口
- 新表或字段
- 新节点能力字段

## 9. 测试与 CI 规则

- 单元测试、格式检查、静态检查必须进入 CI。
- 数据库迁移必须在 CI 中执行一次干净初始化。
- 端到端测试可先拆为 smoke job 和 nightly job。

## 10. 代码审查重点

- 是否破坏状态机约束。
- 是否绕过统一适配层直接访问第三方系统。
- 是否引入未文档化的默认值。
- 是否让 `requested_spec` 或 `resolved_spec` 语义漂移。
