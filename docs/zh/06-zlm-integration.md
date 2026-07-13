# 06. ZLM 集成与 Hook 规格

## 1. 文档目标

本文件定义 `media-core` 如何调用 ZLMediaKit、如何接收和标准化 ZLM Hook，以及如何把 ZLM 现场状态并入任务状态机。

## 2. 适配原则

- ZLM 仅作为媒体数据面，不暴露给业务方。
- 所有 ZLM 调用必须通过 `ZlmAdapter` 完成，不允许散落在业务逻辑中。
- 所有 ZLM Hook 统一进入 `HookReceiver`，再转为内部事件。
- 生产 Hook 的网络入口位于 Agent 本机，Agent 与 Core 之间只复用已认证的 mTLS control stream；Core 不接受 ZLM 远程直连。

## 3. 节点配置模型

每个节点在 Agent 本地保存并校验以下边界配置：

- Agent runtime：`AGENT_ZLM_HOOK_ADDR`、`ZLM_HOOK_SHARED_SECRET`、`AGENT_ZLM_HOOK_QUEUE_CAPACITY`、`AGENT_ZLM_HOOK_TIMEOUT_SEC`。
- Native 端口协调与渲染：`AGENT_ZLM_HOOK_PORT`、`ZLM_HOOK_BASE`。
- Agent→ZLM 本地管理：`ZLM_API_BASE`、`ZLM_API_SECRET`、`ZLM_SERVER_ID`。

`ZLM_HOOK_SHARED_SECRET` 是独立的 Agent-local ingress secret，不与 Core 的兼容 hook secret 或 ZLM API secret 共享同一信任边界。它允许出现在 ZLM 到 Agent loopback ingress 的 query 中，但 Agent/Core 不把它复制到注册消息、Hook 转发消息、Core URL、ingress/control 日志或数据库事件。production 的 `AGENT_ZLM_HOOK_ADDR` 必须是 loopback；native 规范值为 `127.0.0.1:<AGENT_ZLM_HOOK_PORT>`，`ZLM_HOOK_BASE` 必须是 `http://127.0.0.1:<port>/internal/zlm-hooks`。Core 不信任 ZLM payload 自报的 `server_id`，而是从 mTLS control session 证书身份派生。

约束：

- Agent 可以把本地 ZLM 的 `server_id` 配置为节点 UUID 便于现场诊断，但 Hook payload 中任何形式的 server identity 都会被剥离，不能参与 Core 鉴权或节点查找。
- `node_name` 不能再作为 Hook 源身份或节点查找键使用。

## 4. 调用映射

| 任务场景 | ZLM 接口 | 成功判定 |
| --- | --- | --- |
| `live_relay` 启动 | `addStreamProxy` | 返回成功且内部流在线 |
| `live_relay` 录制开启 | `startRecord` | API 成功或录制 Hook 到达 |
| `live_relay` 停止 | `stopRecord` + `close_streams` | API 成功且流下线 |
| 主动外推 | `addStreamPusherProxy` | 推流器创建成功 |
| `rtp_receive` 启动 | `openRtpServer` | 返回端口成功 |
| `rtp_receive` 停止 | `closeRtpServer` | API 成功 |
| 调试会话 | `getAllSession`, `kick_session` | API 成功 |
| 调试流信息 | `getMediaList`, `getMediaPlayerList` | API 成功 |

## 5. Hook 接收端点

生产处理流程：

1. ZLM 仅调用 `http://127.0.0.1:<AGENT_ZLM_HOOK_PORT>/internal/zlm-hooks/<hook>?secret=...`；Agent 入口 router 不记录完整 URI，也不把 query secret 继续转发，请求不得经公网或管理网直接到达 Core。锁定 ZLM 自身在请求失败时仍可能记录完整 URL/body，此上游日志风险不属于本轮入口日志保证。
2. Agent 只接受固定的 9 个 hook 和最大 256 KiB 的 JSON object，并从对象和数组的任意嵌套层递归移除 `secret`、`mediaServerId`、`media_server_id`、`server_id` 与 `serverId`。
   `on_server_started` 是例外：锁定 ZLM 会把完整 mINI 放入正文，Agent 必须将正文归一化为 `{}`，防止 `api.secret`、`hook.on_*` 等 dotted 配置键跨越信任边界。
3. Agent 通过 mTLS control stream 发送 `zlm_hook_request`；Core 从认证 session 取得 `node_id` 并将其作为唯一 `server_id`。
4. Core 以 request ID 做单 session 幂等与并发限制，再复用统一 Hook 业务处理器生成响应、落库事件并驱动状态机。
5. Core 经同一 session 返回 `zlm_hook_response`，Agent 将 status/JSON body 回传给原本地请求。

Agent 的入口队列和 pending response map 均有界；本地校验、队列满、控制流断开与超时分别返回结构化 JSON 错误。Agent relay 超时最大 4 秒，严格小于 ZLM `hook.timeoutSec=5`；超时、HTTP 客户端取消、断连及晚到/重复响应只清理当前 request ID，不终止 control stream。

`POST /internal/hooks/zlm/{server_id}` 与命名 hook 变体不在 production router 中注册；它们仅在 `development` 环境保留，用于旧配置兼容和本机调试，仍执行来源 IP、shared secret 与 server ID 校验。

## 6. Hook 标准化

### 6.1 标准事件结构

```json
{
  "source": "zlm_hook",
  "event_type": "stream_online",
  "server_id": "0195f0f0-6d2b-7f3d-a72d-8c7c4f5b0b11",
  "schema": "rtsp",
  "vhost": "__defaultVhost__",
  "app": "live",
  "stream": "camera01",
  "payload": {}
}
```

### 6.2 Hook 映射表

| ZLM Hook | 内部事件 | 说明 |
| --- | --- | --- |
| `on_publish` | `stream_publish_requested` | 可用于发布鉴权和元数据登记 |
| `on_record_mp4` | `record_file_created` | 录像入库主来源 |
| `on_record_ts` | `record_file_created` | TS 录像产物入库 |
| `on_record_hls` | `record_file_created` | HLS 录像产物入库 |
| `on_stream_none_reader` | `stream_no_reader` | 用于按需关流或保活决策 |
| `on_stream_not_found` | `stream_lookup_miss` | 用于按需拉流 |
| `on_server_started` | `zlm_restarted` | 触发节点级恢复 |
| `on_server_keepalive` | `zlm_keepalive` | 刷新 ZLM 活性 |
| `on_rtp_server_timeout` | `rtp_server_timeout` | 触发 `rtp_receive` 降级或恢复 |

## 7. 去重与幂等

每个 Hook 统一计算：

```text
event_dedup_key = sha256(server_id + hook_name + canonical_json(payload))
```

规则：

- 同一个 `event_dedup_key` 在 24 小时内只处理一次。
- 原始 payload 仍然保留，用于审计。
- 若内部事件写入失败，保留原始 Hook 记录并重试转换。

## 8. 任务状态联动规则

### 8.1 `live_relay`

- `addStreamProxy` 成功后，必须再用 `getMediaList` 确认内部流存在，才允许任务进入 `RUNNING`。
- 收到 `stream_no_reader` 时，若任务未启用录制且策略为按需关闭，则进入停止流程。

### 8.2 `record`

- `startRecord` 返回成功但未收到 `record_file_created` 时，不立即判失败；由对账任务在 60 秒内确认产物。
- `record_file_created` 必须写入 `record_files` 表。

### 8.3 `rtp_receive`

- `stream_id` 由系统按 Attempt 生成，格式为 `task_id-attempt_no`，用于避免重试时与旧接收端口冲突。
- 北向 `input.reuse` 和 `input.ssrc` 分别映射到 ZLM `openRtpServer.re_use_port` 与 `openRtpServer.ssrc`；未指定时不主动覆盖 ZLM 默认值。
- `on_publish` 命中该 `stream_id` 后，才视为“收到有效媒体”，任务允许进入 `RUNNING`，并写入 `stream_bindings.rtp_stream_id`。
- `openRtpServer` 成功后，若 30 秒内未收到有效媒体或收到 `rtp_server_timeout`，任务进入 `LOST`。

## 9. 对账接口使用规则

| 接口 | 使用时机 |
| --- | --- |
| `getMediaList` | 启动确认、恢复、后台巡检 |
| `listRtpServer` | `rtp_receive` 恢复对账 |
| `getThreadsLoad` / `getWorkThreadsLoad` | 节点健康页和恢复前置检查 |
| `getStatistic` | 节点概览和异常诊断 |

## 10. 失败处理

- ZLM API 调用失败时，记录 `zlm_api_error` 事件。
- 可重试接口采用 `1s/3s/5s` 最多 3 次重试。
- 超过重试上限仍失败时，由任务类型决定进入 `FAILED` 还是 `LOST`。

## 11. 安全规则

- 生产必须启用 ZLM `secret`。
- ZLM `secret` 只允许保存在 Agent 本地配置；它可以附加到 ZLM→Agent 的 loopback hook query 或 Agent→ZLM 的本地 API 请求，但 Agent/Core 不得将其写入跨主机 URL、Core 注册、control-stream payload、ingress/control/event 日志或数据库。锁定 ZLM 的失败日志仍可能包含 URL/body，必须在 R2 单独治理。
- Agent 的 `ZLM_API_BASE` 固定为 loopback，但 ZLM API、HLS 和静态文件共享 HTTP listener；`http.allow_ip_range` 在 R1 仍需保留 loopback 与 RFC1918 媒体网段，不能声称 API 在网络上仅本机可达。R1 的边界是 Core 不持有/直连 ZLM 且 `ZLM_API_SECRET` 与两个 Hook secret 独立，关闭共享 listener 旁路留待 R2。
- production Core 不得注册 `/internal/hooks/zlm/...`；所有生产 Hook 必须经 Agent mTLS control stream，并由证书绑定的 `node_id` 确定来源。
- 生产不得启用 `hook.admin_params` 绕过鉴权。
- 调试接口必须经过平台管理员权限校验。
