# RuntimeManager P07：monitor internal event 化

## 任务目标

让所有 runtime monitor 不再直接修改 registry/runtimes，而是向 RuntimeManager 发送 `RuntimeInternalEvent`。actor 根据事件中的 `runtime_id + generation` 判断是否提交状态，防止旧 monitor 污染新 runtime。

## 前置条件

- P05/P06 start outcome 化完成，actor 已能在 start commit 后启动 monitor。

## 实施清单

- 定义 `RuntimeInternalEvent`，至少包含：
  - `ProcessExited`
  - `StartupProbeSucceeded`
  - `StartupProbeFailed`
  - `LiveRelayOffline`
  - `RtpServerMissing`
  - `ProgressObserved`
  - `PersistenceFailed`
- 每个 event 必须携带 `runtime_id` 和 `generation`。
- `RuntimeEntry` 新增 generation，每次 start/adopt/recovery 分配新值。
- 迁移 monitor context：
  - 删除 `LocalRuntimeRegistry`
  - 删除 `Arc<RwLock<HashMap<Uuid, ManagedRuntime>>>`
  - 增加 `manager_tx`
- actor 收到 internal event 后：
  - 找不到 runtime 直接忽略。
  - generation 不匹配直接忽略。
  - 匹配时提交状态、持久化、发布 read model、发外部 notification。
- 逐个迁移 process exit、startup probe、live relay、RTP monitor。

## 验收标准

- monitor 生产路径不再调用 `registry.update/remove`。
- monitor 生产路径不再访问 runtimes map。
- 旧 generation event 不影响当前 runtime。
- process exit/restart/recovery 行为与 contract tests 一致。

## 测试场景

- process exited event generation 匹配时进入 terminal。
- generation 不匹配时事件被丢弃。
- startup probe failed 后状态和事件保持旧语义。
- live relay offline 和 RTP missing 触发正确 terminal 或 recovery。

## 依赖和风险

- 这是 actor 化最容易出错的 PR。
- actor loop 处理 event 时不能执行长 await；慢 I/O 仍要走 worker。
- 迁移时要持续搜索 `registry.update`、`registry.remove`、`runtimes.write` 是否仍在 monitor 中。
