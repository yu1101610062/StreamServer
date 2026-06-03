# Runtime Process P03：adopt_orphans async 化与有界并发探测

## 任务目标

把 `adopt_orphans` 改成真正 async 的恢复流程，并对 persisted ZLM runtime 探测做有界并发。完成后，重连恢复多个 ZLM proxy/RTP runtime 时，不再按每个 ZLM 查询串行等待。

## 当前证据

当前 `crates/media-agent/src/runtime_adoption.rs` 中：

- `adopt_orphan_runtimes` 是同步函数。
- 通过闭包 `zlm_stream_online` 和 `rtp_server_port` 做探测。
- `runtime_executor.rs` 中这些闭包来自 `zlm_stream_online_blocking`、`rtp_server_port_blocking`，内部仍使用 `run_sync`。
- persisted runtime 扫描后逐个处理，ZLM 查询失败或慢响应会拉长整个 adoption。

## 实施清单

- 将 `adopt_orphan_runtimes` 改为 async 函数。
- `RuntimeAdoptionContext` 保持携带 settings/http client/registry/runtimes/slot limiter/events，但不要持有锁跨 await。
- 先同步处理当前 registry 中已经存在的 matching runtime，保持 reattach 语义。
- 对 persisted runtime 做两阶段处理：
  - 本地 process pid 探测可继续同步快速处理。
  - ZLM proxy/RTP server 探测进入 async bounded concurrency。
- 使用 `tokio::task::JoinSet` 或 `FuturesUnordered` 加 `Semaphore`，并发默认 8。
- 单个 ZLM 探测失败只影响对应 runtime，不影响其他 runtime adoption。
- restart fallback 也改为 async 调用 `start_task(request).await`。
- 删除阻塞探测闭包接口。

## 验收标准

- `adopt_orphans` 不再调用 blocking ZLM helper。
- 多个 persisted ZLM runtime 会以有界并发探测。
- registry 中已存在的 runtime 仍优先返回 snapshot，不重复占 slot。
- persisted runtime 探测失败不会污染 registry/runtimes。
- adoption 返回 handles 的语义保持不变。

## 测试场景

- 构造 10 个 persisted ZLM runtime，mock 每个 ZLM 查询延迟，确认总耗时接近 `ceil(10 / concurrency) * delay`，不是 10 倍 delay。
- 一个 ZLM runtime 探测失败，其余 runtime 仍能 adopted。
- registry 中已有 runtime 时，不再扫描结果中重复 track。
- ZLM RTP server 端口刷新后 metadata 中 `local_port` 更新。

## 依赖和风险

- 依赖 P01 async executor，建议在 P02 job 化后执行。
- 并发任务中不能直接共享借用上下文；需要 clone `Client`、`AgentSettings` 和必要 metadata。
- 不要为了并发而并发修改 registry；先产生 adoption decision，再集中提交状态更稳。
