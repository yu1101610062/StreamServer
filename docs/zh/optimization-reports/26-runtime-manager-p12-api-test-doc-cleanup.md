# RuntimeManager P12：API、测试与文档收尾

## 任务目标

完成 RuntimeManager actor 迁移后的收尾：清理 API 导出、删除过渡代码、补齐 contract tests、更新架构文档，并确保 runtime 行为与迁移前合同一致。

## 前置条件

- P11 删除生产 registry 写路径完成。
- RuntimeManagerState 已是唯一权威 runtime 状态。

## 实施清单

- 清理 runtime 模块导出：
  - 保留 `LocalExecutor` 如仍作为接口需要。
  - 导出 `RuntimeManager`、`RuntimeManagerHandle`、`RuntimeReadHandle`。
  - 删除或降级 `ManagedProcessExecutor` 生产导出。
- 删除过渡 façade/mirror 兼容代码。
- 删除不再使用的 registry/runtimes context 字段。
- 补齐 contract tests：
  - 幂等 start。
  - lease mismatch 拒绝。
  - stop 语义。
  - process exit terminal event。
  - stale session cleanup。
  - adopt/recovery。
  - disk cleanup stop。
- 更新中文文档：
  - `docs/zh/01-architecture.md`
  - `docs/zh/04-agent-core-rpc.md`
  - `docs/zh/05-media-execution-plan.md`
  - 如有必要更新 ADR 或新增 ADR。
- 更新测试与运维文档中的 runtime 状态源说明。

## 验收标准

- `RuntimeManagerState` 是唯一权威 runtime 状态。
- `RuntimeReadSnapshot` 是外部唯一同步查询入口。
- actor loop 不执行长 await。
- 所有 internal events 都带 generation。
- contract tests 与原行为一致。
- 文档反映新架构，不再描述旧 registry 权威状态。

## 测试场景

- `cargo test -p media-agent`
- `cargo test --workspace`
- native smoke 如当前 release 流程要求。
- 人工运行 start/stop/adopt/reconnect/recovery 场景。

## 依赖和风险

- 不要在收尾 PR 增加新功能。
- 如果发现 contract tests 与旧行为冲突，优先修实现，不要放宽测试。
- 文档更新要明确 worker 做慢操作、actor 做状态提交这一核心原则。
