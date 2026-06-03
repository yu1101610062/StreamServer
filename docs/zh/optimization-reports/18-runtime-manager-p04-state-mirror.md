# RuntimeManager P04：RuntimeManagerState mirror

## 任务目标

让 RuntimeManager 维护一份 mirror state，用来验证未来 actor 权威状态模型是否正确。本任务中 mirror 不是权威状态，实际生产状态仍来自 `LocalRuntimeRegistry`。

## 前置条件

- P03 调度和 session 已搬进 RuntimeManager。
- `RuntimeReadModel` 可提供 registry 只读 snapshot。

## 实施清单

- 新增基础结构：
  - `RuntimeManagerState`
  - `RuntimeEntry`
  - `RuntimeBackendEntry`
  - `RuntimeOperationId`
- mirror state 至少维护：
  - `by_runtime_id`
  - `by_task_attempt`
  - state counts
  - pending operation ids
- actor 根据这些结果更新 mirror：
  - start 成功返回 handle
  - stop 成功后 read model snapshot
  - adopt 返回 handles
  - runtime notification 中的 task snapshot
- 每次状态变更后，在测试配置下与 `RuntimeReadModel` 做一致性检查。
- 不让 mirror 反向写 registry。

## 验收标准

- mirror state 与 `LocalRuntimeRegistry` 在长时间 start/stop/adopt/restart 流程中保持一致。
- 出现不一致时，测试输出 runtime_id、task_id、attempt_no 和 state 差异。
- 生产行为仍以旧 registry 为准。
- 不改变外部事件顺序。

## 测试场景

- start 后 mirror 和 registry 都能查到 runtime。
- stop 后 mirror state 跟随 snapshot 进入 stopping 或 terminal。
- adopt persisted runtime 后 mirror 有对应 entry。
- stale session cleanup 后 mirror 不保留泄漏 runtime。

## 依赖和风险

- 这是 observer 阶段，不要提前删除 `LocalRuntimeRegistry` 写路径。
- mirror 更新只能基于已有结果和 snapshot，不能猜测 worker 内部状态。
- 一致性检查在生产环境应可关闭，避免额外开销。
