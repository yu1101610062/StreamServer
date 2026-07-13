# 04. Agent 与 Core RPC 规格

## 1. 文档目标

本文件定义 `media-core` 与 `media-agent` 的控制链路。该链路承担节点注册、心跳、命令下发、日志和事件回传、能力探测、恢复接管。

## 2. 通信模型

采用单一长连接双向 gRPC 流：

- 服务端：`media-core`
- 客户端：`media-agent`
- 协议：gRPC over HTTP/2
- 安全：mTLS

Agent 在启动后主动连接 `media-core`，建立一条持续存在的双向流；所有控制命令和运行时事件都通过该流传递。

## 3. 服务定义

```proto
service ControlPlane {
  rpc StreamConnect(stream AgentEnvelope) returns (stream CoreEnvelope);
}
```

约束：

- 一个 Agent 进程只允许存在一条活动 `StreamConnect` 流。
- 连接建立后的第一条 `AgentEnvelope` 必须是 `register`。
- production 节点身份来自 mTLS leaf 的 URI SAN：`spiffe://streamserver/agent/<node_id>`；首包 `node_id` 只做一致性校验，`node_name` 仅用于展示和运维。
- Agent 首次部署必须使用 10 分钟、一次性的 enrollment token，在本地生成两把私钥和 CSR；私钥不得离开 Agent。Core 分别签发 control client 与 management server 证书，并把三套独立信任根及 capability 公钥返回给 Agent。
- 健康旧会话存在时，同节点的新连接必须拒绝，不允许自报 `node_id` 抢占。只有旧会话租约超时，或新 leaf 属于已授权且未过期的一次性轮换窗口时，Core 才允许 takeover。
- 若 30 秒未收到对端消息，连接视为失活并重连。

## 4. Agent -> Core 消息

### 4.1 `register`

首次注册或重连后的身份声明。

字段：

- `node_id`
- `node_name`
- `agent_version`
- `hostname`
- `labels[]`
- `interfaces[]`
- `management_port`
- `management_upload_max_bytes`
- FFmpeg/ZLM 能力摘要

说明：

- `interfaces[]` 应优先上报 `网卡名|IP/前缀`，例如 `eth0|192.168.10.12/24`，用于调度阶段的同网段匹配。
- 生产 Hook control 消息不得携带自报身份；Agent 必须从 JSON object 的任意嵌套层递归剥离 `secret`、`mediaServerId`、`media_server_id`、`server_id` 和 `serverId`，Core 只使用证书绑定 session 的 `node_id`。
- `zlm_api_base`、`zlm_api_secret`、`agent_http_base_url` 仅为兼容保留字段，Agent 必须发送空值，Core 必须忽略。Core 的 management 目标只能由认证 peer IP、证书 DNS SAN 和 `management_port` 构造。

### 4.2 `heartbeat`

间隔固定 10 秒。

字段：

- `node_time`
- `cpu_percent`
- `mem_percent`
- `disk_percent`
- `running_tasks`
- `starting_tasks`
- `stopping_tasks`
- `orphaned_tasks`
- `runtime_slot_loads[]`：按 `source_mode=live/vod` 分桶上报 `max_runtime_slots`、各状态任务数和 `slot_usage`
- `zlm_alive`
- `ffmpeg_alive`

连续 3 个心跳窗口未收到心跳，节点状态置为 `UNHEALTHY`。

### 4.3 `capability_snapshot`

Agent 启动探测或接到探测命令后上报。

字段：

- `ffmpeg_protocols[]`
- `ffmpeg_formats[]`
- `ffmpeg_encoders[]`
- `ffmpeg_decoders[]`
- `zlm_version`
- `zlm_api_list[]`
- `gpu[]`

### 4.4 `task_event`

字段：

- `task_id`
- `attempt_no`
- `lease_token`
- `event_type`
- `event_level`
- `message`
- `payload`

### 4.5 `task_log_batch`

日志按批发送，单批上限 64 行或 512KB。Core 只做当前 attempt 归属校验和实时消费，不把完整日志行落库；终态取证以 `attempt_diagnostics` 事件中的摘要、tail 和 Agent 本地日志路径为准。

字段：

- `task_id`
- `attempt_no`
- `lease_token`
- `stream`
- `lines[]`

### 4.6 `task_progress`

用于 FFmpeg 进度。

字段：

- `task_id`
- `attempt_no`
- `lease_token`
- `frame`
- `fps`
- `bitrate_kbps`
- `speed`
- `out_time_ms`
- `dup_frames`
- `drop_frames`

### 4.7 `task_snapshot`

用于 orphan 接管和运行时探针。

字段：

- `task_id`
- `attempt_no`
- `lease_token`
- `worker_kind`
- `pid`
- `state`
- `command_line`
- `outputs[]`

### 4.8 `certificate_rotation_request` / `certificate_rotation_activated`

- control 与 management 证书任一剩余不超过 30 天时，Agent 生成新的双私钥/双 CSR，并在旧认证会话中发送带稳定 `rotation_id` 的请求。
- Core 只在当前会话和旧证书均有效时签发 5 分钟的一次性轮换 bundle；重复的同一请求幂等返回，CSR 或节点不一致则拒绝。
- 新 control 证书完成一次性 takeover、Core 对新 management leaf 做精确指纹探测后，Core 下发激活命令；Agent 原子完成 generation 切换并返回可重放的激活 ACK。
- 轮换、takeover 与激活审计包含旧新指纹、peer IP、旧新 session ID 和原因，不记录私钥或 capability token。

### 4.9 `zlm_debug_response`

ZLM 调试只允许协议枚举中的固定操作。Agent 在本机附加 ZLM secret、执行请求并返回有界响应；任意 API path、任意参数 JSON 和 secret 都不得进入控制协议。

### 4.10 `zlm_hook_request`

- ZLM 只向 Agent 的 `127.0.0.1` hook ingress 发起 HTTP 请求；shared secret 只作为这段 loopback 请求的 query 凭据。该 router 不挂载会记录完整 URI 的默认 trace layer；Agent/Core 不把 secret 复制到 control stream、ingress/control 日志、事件或数据库。锁定 ZLM 自身在 Hook 失败时仍可能记录完整 URL/body，该上游日志风险留待 R2 处理。
- Agent 只接受 `on_publish`、`on_rtp_server_timeout`、`on_record_mp4`、`on_record_ts`、`on_record_hls`、`on_stream_none_reader`、`on_stream_not_found`、`on_server_keepalive` 和 `on_server_started`。请求体上限为 256 KiB 且必须是 JSON object；所有边界身份字段按任意嵌套深度递归剥离。
- 锁定的 ZLMediaKit 会在 `on_server_started` 正文中附带完整 mINI，其中可能包含 API secret 和带 query secret 的 hook URL；Agent 必须把该 hook 的 `body_json` 严格归一化为 `{}`，只转发启动事件本身。
- 消息仅包含 canonical `request_id`、固定白名单内的 `hook_name` 和规范化 `body_json`，协议中不存在远端可控的 server identity 或 secret 字段。Agent 的入口队列和等待响应表均有界；请求取消、控制流断开和超时会幂等清理。
- Core 始终以该 control session 证书绑定的 `node_id` 作为 `server_id`。单 session 最多并行处理 4 个请求，全局最多 256 个；同一 request ID 同内容幂等重放，不同内容返回冲突。
- 非法请求、业务错误、panic 或 3 秒 Core 处理超时只返回该请求的错误响应，不得终止 control stream，也不得阻塞 heartbeat、rotation、task event 等后续消息。Agent 等待 Core 的总超时固定不超过 4 秒，必须严格小于 ZLM 的 5 秒 hook 超时。

## 5. Core -> Agent 消息

### 5.1 `start_task`

字段：

- `task_id`
- `attempt_no`
- `task_type`
- `resolved_spec`
- `execution_mode`
- `lease_token`
- `trace_context`

行为：

- Agent 收到后先做协议和本地能力校验，再把命令交给 `RuntimeManager` 按 session 和并发限制排队。
- Agent 必须先回发 `task_event{event_type="accepted"}`，再异步执行底层启动。
- 底层启动失败时回发 `task_event{event_type="start_rejected"}`；不得复用旧 Attempt 直接重启。

### 5.2 `stop_task`

字段：

- `task_id`
- `attempt_no`
- `lease_token`
- `reason`
- `grace_period_sec`
- `force_after_sec`

行为：

- Agent 收到停止命令后由 `RuntimeManager` 校验当前 runtime、`lease_token` 和 generation，先提交 `Stopping` 快照，再让 stop worker 执行进程信号或 ZLM/RTP close。
- 若 runtime 尚未注册，不得返回通用失败；应保留 stop intent，等待后台启动路径短路。
- 无法执行停止时回发 `task_event{event_type="stop_rejected"}`，Core 保持 `STOPPING` 并走超时/接管收敛。

### 5.3 `probe_capabilities`

要求 Agent 重新执行 FFmpeg/ZLM 能力探测并上报。

### 5.4 `adopt_orphans`

要求 Agent 只对 Core 明确授权的候选执行 reclaim。

字段：

- `runtimes[].task_id`
- `runtimes[].attempt_no`
- `runtimes[].lease_token`
- `runtimes[].worker_kind`

### 5.5 `certificate_rotation_bundle` / `activate_certificate_rotation`

轮换 bundle 同时携带新 control 和 management 证书、指纹、序列号、有效期、三套既有信任根及 capability 公钥标识。Agent 必须逐项验证与当前信任材料完全一致，并验证证书与本地新私钥匹配后才允许原子落盘。

### 5.6 `zlm_debug_request`

请求使用固定操作枚举与 typed 参数 union。Core 以 `request_id` 关联响应；会话关闭时取消全部待处理请求。快照与 JSON 响应分别执行大小和超时限制。

### 5.7 `zlm_hook_response`

Core 在原认证 session 上返回 `request_id`、HTTP status 与有界 JSON body。Agent 只把匹配本地 pending request 的响应交还 ZLM；旧 session、未知 request ID、超时或重复完成响应必须忽略。Core 的生产 HTTP 服务不注册直连 ZLM hook route；旧 `/internal/hooks/zlm/...` 仅在 `development` 环境作为受限兼容入口存在。

## 6. 连接与重试

- Agent 初次连接失败时，按 `1s/2s/5s/10s/30s` 退避重连。
- 长连接断开后，Agent 必须在 3 秒内尝试重建。
- Core 在连接恢复后可以按需要下发精确的 `adopt_orphans` reclaim 列表，并请求最新 `capability_snapshot`。
- 单条坏消息不得打断整条控制流；只记录节点/任务级错误并继续处理后续消息。

## 7. 任务执行确认语义

| 阶段 | Core 期望 | 超时 |
| --- | --- | --- |
| 接单 | `accepted` 事件 | 5 秒 |
| 启动 | `starting` 事件 | 10 秒 |
| 在线 | `running` 事件或健康探针成功 | 30 秒 |
| 停止 | `stopping` 事件 | 5 秒 |
| 完成 | `succeeded/failed/canceled` 事件 | `grace_period + 30 秒` |

超时处理：

- 接单超时：Task 回到 `QUEUED`，释放租约。
- 启动超时：Task 进入 `LOST`。
- `start_rejected`：结束当前 Attempt，是否重试由恢复策略决定。
- `stop_rejected`：保持 `STOPPING`，升级为强制停止、reclaim-stop 或 `CANCELED/LOST` 收敛。

## 8. 本地执行对象模型

Agent 必须把本地执行对象抽象为统一 `RuntimeHandle`：

- `runtime_id`
- `task_id`
- `attempt_no`
- `lease_token`
- `worker_kind`
- `pid`
- `started_at`
- `last_progress_at`
- `metadata`

补充约束：

- Agent 的本地索引主键必须是 `(task_id, attempt_no)`，`runtime_id` 只作为辅助索引。
- 同 `(task_id, attempt_no, lease_token)` 的重复 `start_task` 必须幂等返回已有 runtime；同 Attempt 不同 `lease_token` 必须拒绝为 stale dispatch。
- Agent 内部 runtime 状态由 `RuntimeManagerState` 维护，外部同步读取统一通过 `RuntimeReadHandle`。
- Heartbeat 的 runtime 计数、artifact cleanup 的 active handle、terminal replay 的 active 过滤、stop 成功后的 snapshot 查询都来自 `RuntimeReadHandle`，不得直接读取 legacy registry。
- 控制流断开后，旧 session 的 start/stop/record/adopt 结果不得向旧 sender 发送任务事件；manager 对 stale session 结果静默收敛。

`worker_kind` 固定值：

- `zlm_proxy`
- `ffmpeg`
- `zlm_rtp_server`
- `hybrid`

## 9. 恢复接管规则

- Agent 在启动后扫描 PID 文件、工作目录和进程列表。
- 能与现有 `task_id + attempt_no` 对上的对象，先上报 `task_snapshot`，等待 Core 决定是否接管。
- 未能映射到任务的本地对象标记为孤儿，不主动销毁，只上报。

## 10. 日志保留规则

- Agent 本地日志至少保留 7 天。
- Core 侧日志索引至少保留 30 天。
- 每行日志必须携带 `task_id`、`attempt_no`、`node_id`、`stream`、`ts`。
