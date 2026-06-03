# Runtime Process P05：ZLM 启动 rollback

## 任务目标

给 ZLM `addStreamProxy` 和 `openRtpServer` 启动流程增加 async rollback。ZLM 外部资源创建成功后，如果本地持久化、registry/runtimes 注册或 monitor 启动失败，必须关闭已经创建的 ZLM proxy 或 RTP server。

## 当前证据

当前 `crates/media-agent/src/runtime_zlm_start.rs` 中：

- `start_live_relay_task` 调用 ZLM `addStreamProxy` 成功后，直接 `registry.track`、`persist_runtime_state`、写入 `runtimes`、启动 monitor。
- `start_rtp_receive_task` 调用 ZLM `openRtpServer` 成功后，也直接进入本地注册和持久化。
- 如果 ZLM API 成功但本地后续步骤失败，ZLM 资源可能遗留。

## 实施清单

- 为 ZLM proxy 新增 rollback guard：
  - 记录 proxy key、vhost/app/stream、work dir、runtime id。
  - armed 状态下 drop 或显式 cleanup 会调用 close stream/proxy 逻辑。
- 为 ZLM RTP server 新增 rollback guard：
  - 记录 stream id、local port/requested port、runtime id。
  - 失败时调用 close RTP server 逻辑。
- 因 cleanup 需要 await，本 PR 应在 async executor 完成后执行。
- 启动流程调整为：
  - build plan
  - call ZLM API
  - create async rollback guard
  - build handle
  - persist runtime state
  - insert runtimes
  - registry.track
  - spawn monitor
  - disarm guard
- 对 actor 迁移前的现有架构，guard 可以在函数内显式 cleanup；不要依赖 async Drop。
- cleanup 失败要记录 warning，但不能掩盖原始启动失败原因。

## 验收标准

- `addStreamProxy` 成功但 persistence 失败时，会调用 close stream/proxy。
- `openRtpServer` 成功但 persistence 失败时，会调用 close RTP server。
- registry/runtimes 不留下半初始化 runtime。
- slot permit 正确释放。
- 成功路径不会额外关闭刚创建的 ZLM runtime。

## 测试场景

- mock ZLM `addStreamProxy` 返回成功，mock persistence 失败，断言 close API 被调用。
- mock ZLM `openRtpServer` 返回成功，mock local registry/register 失败，断言 close API 被调用。
- close API 失败时，返回的错误仍以原始启动失败为主，并记录 cleanup warning。
- 成功启动 ZLM proxy/RTP server 时 monitor 仍正常启动。

## 依赖和风险

- 依赖 P01 async executor。
- 最好在 P04 后执行，使 process 和 ZLM 两类外部副作用都有一致事务语义。
- 需要避免 double close：stop 路径和 rollback 路径不能同时关闭同一个资源。
