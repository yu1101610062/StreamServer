# RuntimeManager P03：调度与 session_epoch 搬进 manager

## 任务目标

把 runtime command scheduling、并发限制和 control-plane `session_epoch` 校验从 `AgentController` 搬进 `RuntimeManager`。完成后，`AgentController` 专注协议和转发，`RuntimeManager` 负责 runtime job 排队、限流和 stale session 清理。

## 前置条件

- P02 façade actor 已进入系统。
- 控制面 runtime 命令已 job 化，已有并发限制语义可迁移。

## 实施清单

- 扩展 `RuntimeCommand`：
  - `BeginSession { session_epoch }`
  - `EndSession { session_epoch }`
- RuntimeManager state 新增：
  - `active_session_epoch`
  - start/stop/record/adopt queue
  - active op counters
  - per-command concurrency limits
- `AgentController` 连接建立后调用 `begin_session(session_epoch)`。
- 连接断开后调用 `end_session(session_epoch)`。
- StartTask 进入 actor 时检查 request.session_epoch 是否等于 active session。
- 长 start 完成后再次检查 session：
  - 如果 stale，立即调度 internal stop，reason 为 `stale_session_replaced`。
- stop/record/adopt 也由 manager 控制 semaphore/queue。
- 删除 `AgentController` 中的 `start_task_permits` 字段和对应逻辑。

## 验收标准

- start/stop/record/adopt 并发限制由 RuntimeManager 管理。
- stale session start 不泄漏 runtime。
- `AgentController` 不再负责 runtime command semaphore。
- 控制面心跳、日志、runtime notification 仍不被长命令阻塞。

## 测试场景

- 模拟长 start，期间 session end；start 完成后 runtime 被自动 stop。
- 多个 start 超过并发限制时排队执行。
- adopt 同时最多运行一个。
- stop/record 长操作期间 heartbeat/log forwarding 继续工作。

## 依赖和风险

- 本任务仍可以委托旧 executor；不要接管状态提交。
- stale session cleanup 的 stop 不能依赖旧 sender 发送成功。
- 排队策略要保持简单 FIFO，不在本 PR 引入优先级调度。
