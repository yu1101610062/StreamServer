# Runtime Process P02：控制面 runtime 命令 job 化

## 任务目标

让 `start/stop/record/adopt` 这几类 runtime 命令都以独立 job 运行，避免控制面 `tokio::select!` 主循环等待慢 ZLM API、慢 stop、慢 adoption 或录制控制完成。完成后，控制面 loop 应继续处理心跳、runtime notification、日志批次和后续 Core 命令。

## 当前证据

当前 `crates/media-agent/src/control_plane.rs` 中：

- `handle_start_task` 已经 `tokio::spawn` 并通过 `start_task_permits` 限制并发。
- `handle_stop_task` 仍直接调用 `self.executor.stop_task(...)` 并等待结果。
- `handle_task_recording_control` 仍直接调用 `self.executor.set_task_recording(...)` 并等待结果。
- `handle_adopt_orphans` 仍直接调用 `self.executor.adopt_orphans(...)` 并在当前控制面分支里发送 orphaned/adopted 结果。

## 实施清单

- 保留 start job 的现有 session guard 语义，并把调用切换到 async executor。
- 新增独立并发限制：
  - `START_TASK_CONCURRENCY_LIMIT = 4`
  - `STOP_TASK_CONCURRENCY_LIMIT = 8`
  - `RECORDING_CONTROL_CONCURRENCY_LIMIT = 4`
  - `ADOPT_ORPHANS_CONCURRENCY_LIMIT = 1`
- 在 `AgentController` 中新增 stop、recording、adopt semaphore 字段。
- 将 `handle_stop_task` 改成轻量解析后 spawn job：
  - job 内 await `executor.stop_task(request.clone())`
  - 成功后发送 `stopping` event 和 snapshot
  - 失败后发送 `stop_rejected`
- 将 `handle_task_recording_control` 改成 spawn job：
  - 成功发送 updated snapshot
  - 失败发送 `recording_control_failed`
- 将 `handle_adopt_orphans` 改成 spawn job：
  - 解析 filter 后立即返回控制面 loop
  - job 内执行 adoption
  - 对未 adopted 的 filter 项继续发送 orphaned event
  - 对 adopted handles 发送 snapshot
- 所有 spawned job 都必须检查 `session_epoch`，避免旧连接残留事件进入新会话。

## 验收标准

- `handle_core_envelope` 中 stop/record/adopt 分支不会直接等待 executor 长操作。
- 控制面主循环在慢 stop/record/adopt 期间仍能转发 heartbeat、runtime notification 和 log batch。
- 每类 runtime 命令都有明确 semaphore 限制。
- adopt 同时最多跑 1 个 job。
- 旧 session 的 job 完成后不会向新 session sender 发送事件。

## 测试场景

- 用 fake executor 模拟 stop 延迟 3 秒，期间注入 runtime notification，确认 notification 能被处理。
- 用 fake executor 模拟 recording control 延迟，确认心跳路径不被阻塞。
- 用 fake executor 模拟 adopt 延迟，确认后续 Core 命令仍可进入 handler。
- 覆盖 session 失效后 job 完成不发送旧事件。

## 依赖和风险

- 依赖 P01：`LocalExecutor` 已 async 化。
- spawn 后 sender 生命周期和 session guard 要处理清楚，避免断线时 job 继续向旧 sender 堆积大量消息。
- 不要改变 `accepted`、`starting`、`stopping`、`orphaned` 等事件类型的业务语义。
