# RuntimeManager P01：RuntimeReadModel 抽象

## 任务目标

先把外部组件对 `LocalRuntimeRegistry` 的读依赖抽象出来，为后续 RuntimeManager actor 接管权威状态铺路。本任务不引入 actor，不改变 runtime 行为，只把“外部同步读取 runtime 状态”的接口稳定下来。

## 前置条件

- Runtime Process P01-P06 已完成并通过测试。
- 当前系统仍由 `ManagedProcessExecutor`、`LocalRuntimeRegistry` 和 `runtimes` map 管理状态。

## 当前证据

当前 `AgentController` 和 `ArtifactCleanupManager` 直接持有 `LocalRuntimeRegistry`：

- heartbeat 使用 `runtime_registry.state_counts()`。
- stop 成功后使用 `runtime_registry.find_by_task_attempt(...)` 发送 snapshot。
- artifact cleanup 使用 registry 获取 active handles。
- terminal replay 仍从 registry 参与过滤。

## 实施清单

- 新增 `RuntimeReadModel` trait，包含：
  - `state_counts() -> RuntimeStateCounts`
  - `active_handles() -> Vec<RuntimeHandle>`
  - `find_by_task_attempt(task_id, attempt_no) -> Option<RuntimeHandle>`
  - `snapshots(filter: &AdoptFilter) -> Vec<RuntimeHandle>`
- 为 `LocalRuntimeRegistry` 实现 `RuntimeReadModel`。
- 将 `AgentController` 中只读用途改为 `Arc<dyn RuntimeReadModel>`。
- 将 `ArtifactCleanupManager` 中只读用途改为 `Arc<dyn RuntimeReadModel>`。
- 保留本地 `LocalRuntimeRegistry` 传给旧 executor 和 terminal replay 兼容路径，直到后续 PR 迁移。
- 不改变 registry 的写路径。

## 验收标准

- `AgentController` 的心跳和 stop snapshot 读取不依赖具体 `LocalRuntimeRegistry` 类型。
- `ArtifactCleanupManager` 读取 active handles 时依赖 `RuntimeReadModel`。
- `LocalRuntimeRegistry` 仍是实际权威状态。
- 所有现有行为保持不变。

## 测试场景

- 新增 `RuntimeReadModel for LocalRuntimeRegistry` 单元测试。
- heartbeat state counts 与原 registry counts 一致。
- artifact cleanup active task 跳过逻辑仍通过。
- stop 成功后 snapshot 仍能发送。

## 依赖和风险

- 这是 RuntimeManager actor 迁移的低风险铺路 PR。
- 不要在本任务中新增 command channel 或 actor。
- trait 必须保持同步读取，供 heartbeat 和 artifact cleanup 快速采样。
