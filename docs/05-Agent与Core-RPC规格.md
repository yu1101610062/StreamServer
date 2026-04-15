# 05. Agent 与 Core RPC 规格

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
  rpc Connect(stream AgentEnvelope) returns (stream CoreEnvelope);
}
```

约束：

- 一个 Agent 进程只允许存在一条活动 `Connect` 流。
- 连接建立后的第一条 `AgentEnvelope` 必须是 `register`。
- 节点身份只由 `node_id` 决定；`node_name` 仅用于展示和运维。
- 若 Agent 未配置 `node_id`，启动时生成一个 UUID 并在该进程生命周期内复用。
- 若 Core 发现已有活动流占用了相同 `node_id`，新连接必须顶掉旧会话；旧会话后续晚到消息一律按 stale 丢弃。
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
- `zlm`
- `ffmpeg`

说明：

- `interfaces[]` 应优先上报 `网卡名|IP/前缀`，例如 `eth0|192.168.10.12/24`，用于调度阶段的同网段匹配。
- Hook 里的 `server_id/mediaServerId` 必须等于该节点的 `node_id` 字符串。

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
- `slot_usage`
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

日志按批发送，单批上限 128 行或 32KB。

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

- Agent 收到后先校验 `lease_token` 和本地能力。
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

- Agent 收到停止命令后必须先记录 stop intent，再去匹配或停止本地 runtime。
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
