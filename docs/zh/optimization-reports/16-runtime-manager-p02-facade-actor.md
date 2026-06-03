# RuntimeManager P02：façade actor 外壳

## 任务目标

新增 `RuntimeManager` façade actor，让它实现 `LocalExecutor`，但内部仍委托现有 `ManagedProcessExecutor`。本任务只把 command channel 和 reply channel 引入系统，不接管 runtime 状态。

## 前置条件

- P01 `RuntimeReadModel` 已完成。
- Runtime Process 第一组硬化已完成，`LocalExecutor` 已 async 化。

## 实施清单

- 新增模块目录：
  - `crates/media-agent/src/runtime_manager/mod.rs`
  - `handle.rs`
  - `command.rs`
  - `actor.rs`
- 定义 `RuntimeCommand`：
  - `StartTask`
  - `StopTask`
  - `SetTaskRecording`
  - `AdoptOrphans`
  - `SetZlmServerId`
  - `SetZlmRtmpEnhancedEnabled`
  - `Shutdown`
- 定义 `RuntimeManagerHandle { tx: mpsc::Sender<RuntimeCommand> }`。
- `RuntimeManagerHandle` 实现 async `LocalExecutor`，每个请求通过 oneshot reply 返回。
- `RuntimeManager` actor 收到命令后 spawn worker，委托旧 executor。
- `AgentController` 使用 `RuntimeManagerHandle` 作为 executor。
- 旧 `ManagedProcessExecutor` 仍持有 registry/runtimes 并提交状态。

## 验收标准

- RuntimeManager actor 进入系统，生产控制入口可以使用 `RuntimeManagerHandle`。
- 底层状态仍由 `ManagedProcessExecutor`/`LocalRuntimeRegistry` 管理。
- façade 不改变 start/stop/record/adopt 事件语义。
- actor loop 不在命令分支中执行长 await。

## 测试场景

- contract tests 同时覆盖：
  - 直接 `ManagedProcessExecutor`
  - `RuntimeManagerHandle` façade
- actor channel 关闭时，handle 返回明确 `ExecutorError`。
- `SetZlmServerId` 和 capability hint 能被转发到底层 executor。

## 依赖和风险

- 不要在本 PR 搬迁 registry/runtimes 写路径。
- façade actor 的 worker spawn 需要保留 P02 runtime job 化后的并发控制，或明确仍由上层控制。
- oneshot reply 必须处理 receiver dropped 的情况。
