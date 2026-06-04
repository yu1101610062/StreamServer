# RuntimeManager P11：删除 LocalRuntimeRegistry 写路径

状态：历史任务，已由 RuntimeManager backend store 收口。

## 任务目标

在 RuntimeManager 已成为权威状态源后，删除生产路径中的 `LocalRuntimeRegistry` 写依赖和共享 `runtimes` map 写依赖。外部只能通过 `RuntimeManagerHandle` 控制 runtime，通过 read model 查询 runtime。

当前实现中，生产路径不再持有 `LocalRuntimeRegistry`。本地进程句柄、slot permit、stop flag 和 companion process 信息由 RuntimeManager 内部的 backend store 管理；worker 只返回 start/adopt outcome 或 monitor commit，状态和 backend delta 由 actor 统一提交。

## 前置条件

- P10 adopt/recovery actor 化完成。
- start/stop/record/adopt/monitor/recovery 的生产状态提交都已由 actor 完成。

## 实施清单

- 搜索并清理生产路径：
  - `LocalRuntimeRegistry::track`
  - `LocalRuntimeRegistry::update`
  - `LocalRuntimeRegistry::remove`
  - `runtimes.write`
  - `remove_managed_runtime`
  - `slot_limiter.attach_existing`
  - `slot_limiter.try_acquire`
- 将 `LocalRuntimeRegistry` 降级为测试工具，或删除生产导出。
- `AgentController` 只持有：
  - `RuntimeManagerHandle`
  - `RuntimeReadHandle`
- `ArtifactCleanupManager` 只持有：
  - `RuntimeReadHandle`
  - runtime control handle
- monitor context 中不得再出现 registry/runtimes。
- runtime worker 不得直接提交全局状态。
- 更新 `runtime.rs` 导出。

## 验收标准

- production code 中没有 registry track/update/remove 写调用。
- production start/stop/adopt/monitor/recovery 路径中没有 `LocalRuntimeRegistry` 依赖。
- `runtimes.write` 只允许出现在 RuntimeManager backend store 内部实现中。
- read model 是外部唯一同步查询入口。
- RuntimeManagerState 是唯一权威 runtime 状态。
- RuntimeManagerHandle 是外部唯一 runtime 控制入口。
- 所有 contract tests 通过。

## 测试场景

- 全量 media-agent tests。
- contract tests 覆盖 start/stop/adopt/restart/recovery。
- 搜索验证生产路径无 registry 写调用。
- artifact cleanup 超阈值 stop 仍通过 manager 执行。

## 依赖和风险

- 这是清理 PR，不应新增行为。
- 如果仍有必要保留 registry 类型，必须明确只用于测试兼容或 read model projection，不可生产写入。
- 删除导出会影响测试和外部模块，需要集中修复。
