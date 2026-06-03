# RuntimeManager P06：ZLM start outcome 化

## 任务目标

把 ZLM proxy 和 ZLM RTP server start 拆成 worker outcome + actor commit，使 ZLM 外部资源创建成功后仍由 rollback guard 保护，直到 actor 成功提交状态。

## 前置条件

- P05 ManagedProcess start outcome 化完成。
- ZLM start rollback 已完成。

## 实施清单

- `start_live_relay_task` 改为返回 `RuntimeStartOutcome`。
- `start_rtp_receive_task` 改为返回 `RuntimeStartOutcome`。
- ZLM worker 负责：
  - build plan
  - call ZLM API
  - create rollback guard
  - build handle/backend/monitor plan
  - persist runtime state
- ZLM worker 不再直接 `registry.track` 或 `runtimes.insert`。
- actor commit：
  - 插入 ZLM backend entry
  - 发布 read model
  - 启动 live relay 或 RTP monitor
  - disarm rollback
  - reply handle
- commit 失败时执行 close stream/proxy 或 close RTP server。

## 验收标准

- ZLM start 状态提交由 actor 完成。
- worker 成功但 actor commit 失败时，不泄漏 ZLM proxy/RTP server。
- live relay monitor 和 RTP monitor 从 actor commit 后启动。
- ZLM start/adopt/stop 相关旧测试保持通过。

## 测试场景

- ZLM proxy start 成功后 read model 有 runtime。
- actor commit 失败触发 close stream/proxy。
- RTP server start 成功后 metadata 保留 local port。
- ZLM API 成功但 persistence 失败时 rollback 仍关闭外部资源。

## 依赖和风险

- 不要在本任务中迁移 stop 终态提交；ZLM stop actor 化在后续 P08。
- ZLM rollback 和 stop cleanup 要避免 double close。
- ZLM metadata wire shape 不应改变。
