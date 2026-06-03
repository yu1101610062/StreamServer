# RuntimeManager P09：recording control actor 化

## 任务目标

把 task recording control 的状态提交搬进 RuntimeManager actor。worker 只执行 ZLM API 或 companion 控制，actor 校验 lease、处理 command_id 防重、提交 recording metadata 并发布 snapshot。

## 前置条件

- P08 stop actor 化完成。
- actor 已是 start/stop 主要状态提交者。

## 实施清单

- actor 收到 `SetTaskRecording`：
  - 查 runtime entry。
  - 校验 lease_token。
  - 检查 `command_id` 防重。
  - 标记 pending recording control。
  - spawn recording worker。
- recording worker 负责：
  - 调 ZLM startRecord/stopRecord。
  - 或控制 companion recording process。
  - 返回 updated recording metadata。
- actor 收到 worker result：
  - 确认 pending command 仍有效。
  - 更新 handle metadata。
  - persist snapshot。
  - publish read model。
  - reply updated handle。
  - 发送 snapshot notification。
- failure 不修改 handle recording metadata。

## 验收标准

- recording worker 不直接 registry.update。
- 同 command_id 重复请求幂等返回或明确拒绝，行为在文档和测试中固定。
- 失败路径不污染 runtime metadata。
- 录制控制成功后 snapshot 包含最新 metadata。

## 测试场景

- start recording 成功后 metadata.started 为 true。
- stop recording 成功后 metadata.started 为 false。
- ZLM API 失败时 metadata 不变。
- command_id 重复请求行为稳定。
- lease mismatch 返回 recording control failed。

## 依赖和风险

- 录制控制可能与 stop 并发；actor 要定义 stop pending 时是否拒绝 recording control，优先保持现有行为。
- 不要在本任务引入新的 Core RPC 字段。
- companion recording 的 process cleanup 仍要与 P08 stop 行为一致。
