# Runtime Process P01：LocalExecutor async 化

## 任务目标

把 `LocalExecutor` 的真实异步边界显式化，删除 `ManagedProcessExecutor::run_sync` 以及对 `tokio::task::block_in_place`、`Handle::block_on` 的依赖。完成后，ZLM API、停止任务、录制控制和 orphan adoption 中的异步 I/O 都应直接通过 async 调用链执行。

## 当前证据

当前 `crates/media-agent/src/runtime_executor.rs` 中：

- `LocalExecutor` 仍是同步 trait，`start_task`、`stop_task`、`set_task_recording`、`adopt_orphans` 都接收借用参数并同步返回。
- `stop_task` 和 `set_task_recording` 通过 `run_sync(...)` 包装 async 函数。
- ZLM 启动路径 `start_live_relay_task`、`start_rtp_receive_task` 也通过 `run_sync(...)` 执行。
- `zlm_stream_online_blocking`、`rtp_server_port_blocking` 仍把 async ZLM 探测转成同步闭包。

## 实施清单

- 给项目选择一个现有 async trait 方案；优先使用已经存在于依赖中的 `tonic::async_trait` 或 `async_trait::async_trait`，不要引入重复依赖。
- 将 `LocalExecutor` 改为 async trait：
  - `start_task(request: StartTaskRequest) -> Result<RuntimeHandle, ExecutorError>`
  - `stop_task(request: StopTaskRequest) -> Result<(), ExecutorError>`
  - `set_task_recording(request: TaskRecordingControlRequest) -> Result<RuntimeHandle, ExecutorError>`
  - `adopt_orphans(filter: AdoptFilter) -> Vec<RuntimeHandle>`
- 使用 owned request/filter，避免 async trait 跨 await 持有借用造成生命周期复杂度。
- 更新 `ManagedProcessExecutor` 实现，直接 `.await`：
  - `start_zlm_live_relay_task`
  - `start_zlm_rtp_receive_task`
  - `stop_runtime_task`
  - `set_runtime_task_recording`
- 删除 `run_sync`、`zlm_stream_online_blocking`、`rtp_server_port_blocking`。
- 更新所有调用点：
  - `control_plane.rs`
  - `artifact_cleanup.rs`
  - runtime tests
  - artifact cleanup tests 中的 fake executor
- 保持 `set_zlm_server_id` 和 `set_zlm_rtmp_enhanced_enabled` 为同步轻量方法。

## 验收标准

- 生产代码中搜不到 `ManagedProcessExecutor::run_sync`、`block_in_place`、`Handle::block_on`。
- `LocalExecutor` 的主要运行时控制方法全部为 async。
- ZLM 启动和停止路径不再隐藏 async I/O。
- current-thread Tokio 测试环境中调用 executor 不会因为 `block_in_place` panic。
- 不改变 start/stop/record/adopt 的外部事件语义。

## 测试场景

- 新增 current-thread `tokio::test`：调用 async executor 的 stop/record/adopt mock 路径，确认不 panic。
- 现有 `crates/media-agent/src/tests/runtime.rs` 全部通过。
- 现有 `crates/media-agent/src/tests/artifact_cleanup.rs` 全部通过。
- 至少跑 `cargo test -p media-agent runtime` 和 `cargo test -p media-agent artifact_cleanup`。

## 依赖和风险

- 本任务是后续 job 化、async adopt、ZLM rollback 的前置任务。
- 改 trait 会影响测试 fake 类型和 `Arc<dyn LocalExecutor>` 调用点，必须一次性修完编译错误。
- 不要在这个任务里重构 runtime 状态所有权；状态仍由现有 registry/runtimes 管理。
