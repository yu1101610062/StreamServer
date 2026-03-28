# 06. ZLM 适配与 Hook 规格

## 1. 文档目标

本文件定义 `media-core` 如何调用 ZLMediaKit、如何接收和标准化 ZLM Hook，以及如何把 ZLM 现场状态并入任务状态机。

## 2. 适配原则

- ZLM 仅作为媒体数据面，不暴露给业务方。
- 所有 ZLM 调用必须通过 `ZlmAdapter` 完成，不允许散落在业务逻辑中。
- 所有 ZLM Hook 统一进入 `HookReceiver`，再转为内部事件。

## 3. 节点配置模型

每个节点保存以下 ZLM 配置：

- `api_base`
- `secret`
- `hook_base`
- `server_id`
- `default_vhost`
- `record_root`

`server_id` 由平台分配，用于区分不同节点上报的 Hook 源。

约束：

- `server_id` 必须等于节点 `node_id` 的 UUID 字符串。
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

内部仅暴露一个接收入口：

- `POST /internal/hooks/zlm/{server_id}`

处理流程：

1. 校验源 IP 白名单。
2. 校验 shared secret。
3. 校验 `server_id` 对应节点。
4. 生成 `event_dedup_key`。
5. 落库原始 payload。
6. 转换为内部事件。
7. 推送给恢复引擎和状态机。

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
- 生产不得启用 `hook.admin_params` 绕过鉴权。
- 调试接口必须经过平台管理员权限校验。
