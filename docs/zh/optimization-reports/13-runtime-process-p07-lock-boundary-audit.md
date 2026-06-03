# Runtime Process P07：锁边界审计

## 任务目标

审计 runtime 相关 `std::sync::RwLock` 使用边界，确保不会持有同步锁 guard 跨 `.await`。本任务不要求机械替换成 `tokio::sync::RwLock`；目标是明确现有锁的安全使用约束，并补测试或注释防止后续引入隐性阻塞。

## 当前证据

当前 runtime 代码大量使用：

- `LocalRuntimeRegistry` 内部同步锁。
- `Arc<RwLock<HashMap<Uuid, ManagedRuntime>>>`。
- `stop_intents`、ZLM capability hints 等短临界区锁。

多数路径是 read/clone/drop 后再 await，这种用法可以接受。但 P01-P03 async 化后，调用链会增加 await 点，需要重新确认所有 guard 生命周期。

## 实施清单

- 搜索 runtime 相关锁使用：
  - `runtimes.read`
  - `runtimes.write`
  - `registry.update`
  - `stop_intents.write`
  - `.expect("... lock poisoned")`
- 对所有 async 函数中的锁使用逐个确认：
  - guard 不跨 `.await`
  - 大块文件 I/O 或 ZLM API 前已释放 guard
  - clone snapshot 后再做慢操作
- 对发现的风险点做局部重排：
  - 用局部 block 限定 guard 生命周期。
  - 提前 clone 必要数据。
  - await 后再重新获取锁提交状态。
- 可考虑把高频短锁换成 `parking_lot::RwLock`，但只有在收益明确时做；不要作为本任务默认动作。
- 在复杂函数中添加少量注释，说明锁释放点和 await 顺序。

## 验收标准

- runtime async 函数中没有持有 `std::sync::RwLock` guard 跨 await 的代码。
- `cargo clippy` 不出现明显的 await-holding-lock 相关问题。
- 代码审计清单中每个 runtime lock 热点都有结论。
- 不因为锁类型替换引入大范围无关 churn。

## 测试场景

- 跑 `cargo test -p media-agent runtime`。
- 跑 `cargo clippy -p media-agent --all-targets`，如果 workspace 当前 clippy 有既有问题，需要记录与本任务无关的失败。
- 人工复核 P01-P03 变更后的 async 函数。

## 实施记录

本轮审计后保留 `std::sync::RwLock`/`Mutex`，不做锁类型替换。结论是当前 runtime 状态仍然由短临界区同步锁保护，所有 async 路径必须先 clone/snapshot 或完成同步更新，再进入 `.await`、ZLM API、文件 I/O、进程信号和事件投递。

已固化的锁边界：

- `LocalRuntimeRegistry`：内部 `RwLock` 不向外泄漏；公开方法只做短同步更新，或返回 cloned `RuntimeHandle`/列表快照。
- `runtimes: Arc<RwLock<HashMap<Uuid, ManagedRuntime>>>`：启动、收养、停止、monitor 和强杀调度路径统一用局部 block 获取 cloned runtime 或完成 insert/remove/update。
- `stop_intents`：stop 写入和 start 前置检查均限定在短 block 内，后续 stale cleanup、slot acquire 和任务启动不持有 guard。
- ZLM capability hints：读取 server id/enhanced flag 时先复制 `Option`，再组装 start/recovery context。
- `RecordingControlGuard`：只保留逻辑 in-flight marker；内部 `StdMutexGuard` 在 `acquire/drop` 内立即释放，guard 本身允许跨 await。

已检查的慢操作边界：

- ZLM API：`start_stream_recording`、`close_live_relay`、`close_rtp_receive`、live relay cleanup、RTP server reopen/close、ZLM start rollback。
- 进程等待/信号：process exit monitor、companion monitor、stale attempt cleanup、record duration stop、startup timeout stop。
- 持久化和事件：`persist_runtime_state`、runtime snapshot/event 发送前均不持有 runtime map 或 registry guard。

验收命令：

- `cargo clippy -p media-agent --all-targets -- -W clippy::await_holding_lock`
- `cargo test -p media-agent runtime`
- `cargo test -p media-agent control_plane`
- `cargo test -p media-agent artifact_cleanup`

## 依赖和风险

- 建议在 P01-P03 后执行，因为 async 化会改变锁边界。
- 不要把 actor 迁移提前塞进本任务；长期最终方案是 RuntimeManager actor 单点拥有状态。
- 如果发现必须跨 await 维护状态，应该拆成 snapshot + await + update，而不是简单换 async lock。
