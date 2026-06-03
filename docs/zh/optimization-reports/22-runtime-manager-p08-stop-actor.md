# RuntimeManager P08：stop_task actor 化

## 任务目标

让 `stop_task` 的状态提交集中到 RuntimeManager actor。actor 负责校验、记录 stop intent、先提交 Stopping；stop worker 只执行 signal、ZLM close、recording stop 等慢副作用。

## 前置条件

- P07 monitor internal event 化完成。
- Managed process terminal 状态已能通过 `ProcessExited` internal event 提交。

## 实施清单

- actor 收到 StopTask：
  - 查 runtime entry。
  - 校验 lease_token。
  - 记录 stop intent。
  - 更新 handle state 为 `Stopping`。
  - 写 stop metadata。
  - publish read model。
  - spawn stop worker。
- stop worker 负责：
  - signal process group/pid。
  - close live relay/ZLM proxy。
  - close RTP server。
  - stop recording。
  - schedule force kill 或请求 actor 调度。
  - 返回 `RuntimeStopOutcome`。
- actor 根据 outcome：
  - ManagedProcess 等待 `ProcessExited`。
  - ZLM-only runtime 可直接提交 terminal。
  - AlreadyGone 按现有语义处理。
- `stop_task` reply 语义保持为 stop request accepted，不等待 FFmpeg 真正退出。

## 验收标准

- stop worker 不直接 registry/runtimes 写状态。
- actor 在 stop 副作用前先发布 Stopping。
- lease_token mismatch 行为保持不变。
- `disk_threshold_exceeded` terminal event 语义保持不变。
- ZLM RTP stop 后 terminal snapshot 仍发送。

## 测试场景

- stop 成功立即看到 Stopping snapshot。
- managed process stop 后，terminal 由 ProcessExited event 提交。
- ZLM proxy/RTP stop 关闭外部资源并提交 terminal。
- lease_token mismatch 返回 stop rejected。
- disk threshold stop 产生 failed/error 语义。

## 依赖和风险

- 不要让 actor 在 loop 中直接 await ZLM close 或 process wait。
- stop intent 与 stale attempt cleanup 的交互要保留。
- force kill timer 应携带 generation，避免误杀新 runtime。
