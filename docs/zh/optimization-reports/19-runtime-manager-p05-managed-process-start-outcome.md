# RuntimeManager P05：ManagedProcess start outcome 化

## 任务目标

把 managed process start 拆成 worker outcome + actor commit。worker 负责外部副作用和持久化准备，actor 负责提交权威状态、发布 read model、启动 monitor，并向 caller reply。

## 前置条件

- P04 mirror state 已稳定。
- Process start rollback guard 已完成。

## 实施清单

- 拆分 `start_process_task`：
  - worker 执行 plan/build、spawn child、persist、准备 monitor plan。
  - worker 不再直接 `registry.track`。
  - worker 不再直接 `runtimes.insert`。
- 定义 `RuntimeStartOutcome`：
  - `handle`
  - `backend`
  - `work_dir`
  - `success_check`
  - `monitor_plan`
  - `rollback`
- rollback guard 保持 armed，直到 actor commit 成功后 disarm。
- actor 收到 outcome 后：
  - 插入 `RuntimeManagerState`
  - 发布 read model projection
  - 注册 slot/backend
  - 启动 stream readers、startup probe、exit monitor
  - reply `Ok(handle)`
- 成功后旧 registry 写路径对 managed process start 不再使用。

## 验收标准

- ManagedProcess start worker 不直接提交全局 runtime 状态。
- actor commit 成功前，外部资源仍受 rollback guard 保护。
- actor commit 失败或 outcome 被丢弃时，child 被清理。
- ManagedProcess start contract tests 全部通过。

## 测试场景

- start 成功后 read model 可查 runtime。
- actor commit 前模拟失败，断言 child 被 signal，read model 无 runtime。
- 启动后 startup probe/exit monitor 仍工作。
- 无 startup probe 的 process 仍进入 running 并发 snapshot。

## 依赖和风险

- 这是第一个真正权威状态搬迁 PR，风险高。
- 不要同时搬 ZLM start、stop 或 monitor 状态提交。
- monitor plan 中如携带 child/stdout/stderr，必须保证 ownership 不丢失。
