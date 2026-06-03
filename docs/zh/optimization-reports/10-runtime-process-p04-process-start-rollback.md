# Runtime Process P04：ManagedProcess 启动 rollback guard

## 任务目标

给 FFmpeg managed process 和 companion recording process 的启动流程增加 rollback guard。只要 child spawn 后的持久化、registry/runtimes 注册、monitor 启动等步骤失败，就必须清理已经创建的进程和半初始化状态。

## 当前证据

当前 `crates/media-agent/src/runtime_process_start.rs` 中：

- 先 spawn child，再构造 handle。
- 随后执行 `registry.track(handle.clone())`。
- 然后 `persist_runtime_state(...) ?`。
- 再向 `runtimes.write().insert(...)` 写入 `ManagedRuntime`。
- 最后启动 stream reader、companion、startup probe、exit monitor。

如果 `persist_runtime_state` 或后续步骤失败，child 可能已经存在，但 runtime map/registry/monitor 状态不完整。

## 实施清单

- 新增 `RuntimeStartRollback` 或同等 guard，至少记录：
  - `runtime_id`
  - 主进程 pid
  - companion pids
  - registry handle
  - runtimes map handle
  - armed/disarmed 状态
- child spawn 成功后立即创建 rollback guard。
- 调整启动顺序，优先保证失败可清理：
  - prepare plan/work dir
  - spawn child
  - create rollback guard
  - build handle
  - persist runtime state
  - insert runtimes
  - registry.track
  - start readers/monitors/companion
  - disarm rollback
- 如果现有顺序必须保留，也要保证 guard 的 Drop 能 remove registry、remove runtimes 并 signal pid。
- companion recording process spawn 成功后也要注册到 guard；后续任何失败都清理 companion。
- rollback signal 优先 `SIGTERM`，随后调度短延迟 force kill。
- 如果需要测试失败路径，先把 `persist_runtime_state` 或 persistence writer 抽象成可注入函数。

## 验收标准

- child spawn 后任一步失败都不会留下 registry/runtimes 半初始化状态。
- slot permit 会随 `ManagedRuntime` 或 rollback guard drop 正确释放。
- companion child 在失败路径中也会被 signal。
- 成功路径 disarm 后不触发 rollback。
- 正常 start 事件和 snapshot 语义不变。

## 测试场景

- 模拟 `persist_runtime_state` 失败，断言 child 被 signal，registry 查不到 task attempt。
- 模拟 runtimes insert 后 monitor 启动前失败，断言 map 被清理。
- 模拟 companion spawn 后持久化失败，断言主进程和 companion 都被清理。
- 成功 start 后确认 guard disarmed，不误杀进程。

## 依赖和风险

- 建议在 P01 之后执行；不强依赖 P02/P03。
- Drop 中不能执行 async cleanup；process rollback 只做同步 signal 和后台 force kill 调度。
- 不要在这个 PR 同时做 actor outcome 化；这里只加现有架构下的事务保护。
