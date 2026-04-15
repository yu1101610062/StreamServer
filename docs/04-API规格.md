# 04. API 规格

## 1. 文档目标

本文件定义 `media-core` 对外提供的 REST API。所有北向调用、前端页面和联调测试都以本文件为唯一契约。

## 2. 通用规则

### 2.1 基础约束

- Base URL: `/api/v1`
- Content-Type: `application/json`
- 写接口统一要求 `Idempotency-Key` 请求头
- 鉴权方式：`Authorization: Bearer <token>`
- 若部署配置 `core.auth_enabled = false`，则北向 API 关闭 JWT 鉴权，默认按平台管理员权限放行
- 时间统一返回 RFC 3339

### 2.2 幂等规则

- `Idempotency-Key` 对应一次逻辑操作。
- 同一租户、同一路径、同一请求体哈希重复提交时，返回第一次成功或失败的结果。
- 同一 `Idempotency-Key` 但请求体哈希不同，返回 `409 CONFLICT`。
- 对 `POST /tasks`，请求头中的 `Idempotency-Key` 会同时写入 `tasks.idempotency_key`，作为任务创建幂等键。

### 2.3 分页规则

列表接口统一支持：

- `page`，默认 `1`
- `page_size`，默认 `20`，最大 `100`
- `sort_by`
- `sort_order=asc|desc`

统一返回：

```json
{
  "items": [],
  "page": 1,
  "page_size": 20,
  "total": 0
}
```

### 2.4 错误模型

```json
{
  "code": "TASK_INVALID_STATE",
  "message": "task cannot be stopped from CREATED",
  "request_id": "01JXXXX",
  "details": {
    "task_id": "0195..."
  }
}
```

错误码前缀：

- `AUTH_*`
- `TASK_*`
- `TEMPLATE_*`
- `NODE_*`
- `ZLM_*`
- `FFMPEG_*`
- `VALIDATION_*`
- `CONFLICT_*`

## 3. 任务接口

### 3.1 `POST /tasks`

创建任务。默认行为是创建后立即进入校验与调度；若 `schedule.start_mode = "manual"`，则停留在 `CREATED`。

定时说明：

- `schedule.start_mode = "at"`：Task 本身停留在 `VALIDATING`，由调度器在 `schedule.start_at` 到点后下发。
- `schedule.start_mode = "cron"`：Task 本身作为计划定义存在；每次命中 Cron 表达式时，调度器会派生一个新的 `immediate` 子任务并立即进入调度。

调度规则：

- 若任务输入可解析出源流地址，Core 优先用该地址匹配在线节点的已上报网卡；节点存在任一同网段网卡即可优先。
- `input.interface_name` / `publish.interface_name` 用于任务层按网卡名覆盖节点本地绑定策略；运行时在节点本地解析成当前 IPv4。
- `input.interface_ip` 只用于节点本地收发绑定，不参与源流亲和调度。
- `multicast_bridge` 的组播输入/输出若未显式指定 `interface_name/interface_ip`，默认使用工作节点安装时配置的组播网卡。
- 同网段优先不是强约束；若没有命中节点，则回落到其他在线节点。
- 同网段优先级之后，再按节点实时负载排序，先比较 `slot_usage`，再比较 `running_tasks`。
- 当前只对字面量 IP 生效；若输入 URL 使用域名，则退化为纯负载调度。

请求体：

```json
{
  "name": "relay-camera-01",
  "type": "stream_ingest",
  "priority": 50,
  "common": {
    "tenant_id": "default",
    "created_by": "alice"
  },
  "input": {
    "kind": "rtsp",
    "source_mode": "live",
    "loop_enabled": false,
    "url": "rtsp://camera.example/live"
  },
  "stream": {
    "app": "live",
    "name": "relay-camera-01"
  },
  "expose": {
    "enable_rtsp": true,
    "enable_rtmp": true
  },
  "record": {
    "enabled": false
  },
  "schedule": {
    "start_mode": "immediate"
  }
}
```

能力边界说明：

- `input.kind` 表示任务直接接收的输入源类型。
- `input.kind=file` 时，`input.url` 必须填写相对 `/data/media/work` 的文件路径；如果误写成 `/demo.mp4`，系统会自动按 `demo.mp4` 处理。
- `input.kind=ftp` 时，`input.url` 必须填写 `ftp://` 地址；当前不支持 `ftps://`。
- `input.source_mode` 用于显式区分 `hls/http_ts` 是实时源还是离线源；`ftp` 固定为 `vod`，其他输入类型按规则自动推断。
- `input.loop_enabled` 仅支持 `stream_ingest + source_mode=vod`，适用于 `file`、`http_mp4`、`hls(vod)`、`http_ts(vod)`；开启后输入读到 EOF 会从头循环。若同时关闭全部播放协议并启用录制，任务会进入快录分支，此时必须填写 `record.duration_sec` 作为快录终点。
- `stream.*` 表示内部流标识，只对 `stream_ingest` 生效。
- `expose.*` 只控制内部流在节点 ZLM 上额外暴露哪些播放协议，不会新增一个独立发布目标。对 `stream_ingest + source_mode=vod + record.enabled=true`，只要任一播放协议开启，任务就保持实时录制；全部关闭则切到快录且不再提供实时播放地址。
- `publish.kind` 表示任务直接写出的外部目标类型；当前支持 `file`、`udp_mpegts_multicast`、`rtp_multicast`、`rtmp_push`。
- `record.duration_sec` 表示总录制时长：`stream_ingest + source_mode=vod` 在实时分支按媒体时间截取、在快录分支作为离线处理的终点；`stream_ingest + source_mode=live` 按现实时间计时；到点后任务整体成功结束。

当前能力矩阵：

| 任务类型 | 支持的 `input.kind` | 支持的 `publish.kind` | 支持的内部流协议暴露 |
| --- | --- | --- | --- |
| `stream_ingest` | `rtsp` `rtmp` `hls` `http_flv` `http_ts` `http_mp4` `ftp(vod)` `file` `udp_mpegts_multicast` `rtp_multicast` `gb_rtp` | 不允许设置 | `expose.enable_rtsp` `enable_rtmp` `enable_http_ts` `enable_http_fmp4` `enable_hls` |
| `stream_bridge` | `rtsp` `rtmp` `hls` `http_flv` `http_ts` `http_mp4` `ftp(vod)` `file` `udp_mpegts_multicast` `rtp_multicast` | `file` `udp_mpegts_multicast` `rtp_multicast` `rtmp_push` | 不适用 |
| `file_transcode` | `file` `ftp(vod)` `http_mp4` `hls(vod)` `http_ts(vod)` | `file` | 不适用 |

循环 VOD 输入示例：

```json
{
  "name": "promo-loop-01",
  "type": "stream_ingest",
  "common": {
    "created_by": "alice"
  },
  "input": {
    "kind": "http_mp4",
    "source_mode": "vod",
    "loop_enabled": true,
    "url": "http://vod.example.com/promo.mp4"
  },
  "stream": {
    "app": "live",
    "name": "promo-loop-01"
  },
  "expose": {
    "enable_rtsp": false,
    "enable_rtmp": false,
    "enable_http_ts": false,
    "enable_http_fmp4": false,
    "enable_hls": false
  },
  "record": {
    "enabled": true,
    "format": "mp4",
    "duration_sec": 180
  },
  "schedule": {
    "start_mode": "immediate"
  }
}
```

`stream_bridge` 输出约束：

- `publish.kind=file`：
  - 输出路径由平台托管生成，不能通过 `publish.url` 指定目录或文件名
  - `stream_bridge(file)` 产物落到 `/data/zlm/www/output/mp4/node-<node-ip>-mp4/<task-id>/HHMMSS[-NN].ext`
- `publish.kind=rtmp_push`：
  - `publish.url` 必填，且必须是完整的 `rtmp://` 或 `rtmps://` 目标地址
  - `publish.format` 留空或填 `flv`；其他格式不允许
  - `vod` 输入会自动按实时节奏推送，避免把外部 RTMP 目标瞬间灌满

返回：

- `201 CREATED`
- 响应体为完整 Task 摘要

`stream_ingest` 中的 `gb_rtp` 请求约束：

- `input.kind` 必须为 `gb_rtp`
- `input.port` 必须提供，允许为 `0` 以便由节点动态分配端口
- `input.tcp_mode` 可选，`0=udp`、`1=tcp_passive`、`2=tcp_active`，默认 `0`
- `input.reuse` 可选，对应 ZLM `re_use_port`
- `input.ssrc` 可选，对应 ZLM `ssrc`
- `publish.kind` 不允许设置；内部流协议暴露统一走 `expose.*`

### 3.2 `GET /tasks/{id}`

返回任务主信息、当前 Attempt 摘要、最近事件摘要，以及最近一次任务回调状态摘要。

新增字段：

- `callback_delivery`：最近一次任务回调状态；未配置 `common.callback_url` 时为 `null`

### 3.3 任务回调

当任务配置了 `common.callback_url` 时，`media-core` 会异步向该地址发起回调。

回调事件：

- `task.status`：任务某个 Attempt 首次进入 `RUNNING` 时立即发送，`reason=running`
- `task.completed`：任务进入终态 `SUCCEEDED`、`FAILED`、`CANCELED`、`LOST` 时发送，`reason=terminal_state`

回调规则：

- 方法固定 `POST`
- `Content-Type: application/json`
- `task.status` 默认即时发送
- `task.completed` 默认在任务终态后延迟 `8s` 发送，给录像和转码产物留出入库窗口
- 若录像文件或转码产物在首次 `task.completed(reason=terminal_state)` 回调之后才入库，会自动补发一次 `task.completed(reason=artifact_update)` 的刷新回调
- 网络错误、超时、`429` 和 `5xx` 会自动重试
- 其他 `4xx` 不重试

固定请求头：

- `X-StreamServer-Event`：`task.status` 或 `task.completed`
- `X-StreamServer-Event-Id`
- `X-StreamServer-Task-Id`
- `X-StreamServer-Attempt-No`
- 若配置 `CALLBACK_SHARED_SECRET`，额外携带 `X-StreamServer-Signature: sha256=<hex>`

`task.status` 回调体：

```json
{
  "event_id": "019d....",
  "event_type": "task.status",
  "reason": "running",
  "event_time": "2026-04-12T10:21:33Z",
  "status": "RUNNING",
  "task": {
    "id": "019d....",
    "name": "relay-camera-01",
    "type": "file_to_live",
    "status": "RUNNING"
  },
  "attempt": {
    "id": "019d....",
    "no": 1,
    "status": "RUNNING",
    "worker_kind": "hybrid"
  },
  "latest_event": {
    "event_type": "running",
    "event_level": "info",
    "message": "task is running"
  }
}
```

`task.completed` 回调体：

```json
{
  "event_id": "019d....",
  "event_type": "task.completed",
  "reason": "terminal_state",
  "event_time": "2026-04-12T10:21:45Z",
  "task": {
    "id": "019d....",
    "name": "relay-camera-01",
    "type": "file_to_live",
    "status": "SUCCEEDED"
  },
  "attempt": {
    "id": "019d....",
    "no": 1,
    "status": "SUCCEEDED",
    "worker_kind": "hybrid"
  },
  "streams": [
    {
      "schema": "rtsp",
      "app": "live",
      "stream": "camera01",
      "play_urls": ["rtsp://192.168.6.10/live/camera01"]
    }
  ],
  "records": [
    {
      "id": "019d....",
      "file_path": "/node-192_168_6_10-mp4/019d....../clip.mp4",
      "http_url": "http://192.168.6.10/output/mp4/node-192_168_6_10-mp4/019d....../clip.mp4"
    }
  ],
  "file_artifacts": [
    {
      "artifact_kind": "transcode_output",
      "id": "019d....",
      "file_name": "output.mp4",
      "file_path": "/node-192_168_6_10-mp4/019d....../output.mp4",
      "http_url": "http://192.168.6.10/output/mp4/node-192_168_6_10-mp4/019d....../output.mp4"
    }
  ],
  "latest_event": {
    "event_type": "succeeded",
    "event_level": "info",
    "message": "task finished"
  }
}
```

### 3.4 `GET /tasks`

查询参数：

- `status`
- `type`
- `tenant_id`
- `assigned_node_id`
- `keyword`
- `created_from`
- `created_to`

### 3.5 `GET /tasks/{id}/events`

查询参数：

- `attempt_no`
- `source`
- `event_type`
- `page`
- `page_size`

### 3.6 `GET /tasks/{id}/logs`

查询参数：

- `attempt_no`，默认当前 Attempt
- `stream=stdout|stderr|merged`
- `cursor`，用于增量拉取
- `limit`，默认 `200`

返回：

```json
{
  "attempt_no": 1,
  "next_cursor": "1710000000.123",
  "lines": [
    {
      "ts": "2026-03-28T10:00:00Z",
      "stream": "stderr",
      "line": "frame=10 fps=25 q=-1.0"
    }
  ]
}
```

### 3.6 `GET /tasks/{id}/resolved-spec`

返回冻结后的 `resolved_spec`，用于审计和重放。

### 3.7 `POST /tasks/{id}/start`

- 允许状态：`CREATED`, `FAILED`, `CANCELED`
- 成功返回 `202 ACCEPTED`

### 3.8 `POST /tasks/{id}/stop`

- 允许状态：`DISPATCHING`, `STARTING`, `RUNNING`, `RECOVERING`, `LOST`
- 成功返回 `202 ACCEPTED`

### 3.9 `POST /tasks/{id}/cancel`

- 允许状态：`CREATED`, `VALIDATING`, `QUEUED`, `DISPATCHING`, `STARTING`, `RUNNING`, `RECOVERING`
- 成功返回 `202 ACCEPTED`

### 3.10 `POST /tasks/{id}/retry`

- 允许状态：`FAILED`, `LOST`
- 创建新 Attempt，Task ID 不变
- 返回新 Attempt 摘要

### 3.11 `POST /tasks/{id}/clone`

- 允许状态：`SUCCEEDED`, `FAILED`, `CANCELED`, `LOST`
- 生成新 Task
- 支持可选请求体覆盖少量字段：`name`、`priority`、`common.created_by`、`schedule.start_mode`

示例：

```json
{
  "name": "relay-camera-01-copy",
  "priority": 15,
  "common": {
    "created_by": "bob"
  },
  "schedule": {
    "start_mode": "manual"
  }
}
```

## 4. 运行时接口

### 4.1 `GET /streams`

支持字段：

- `schema`
- `app`
- `stream`
- `task_id`
- `node_id`
- `has_viewer`

返回字段补充：

- `viewer_count`：从节点 ZLM `getMediaList.totalReaderCount` 富化得到的精确 viewer 数
- `bitrate_kbps`：从节点 ZLM `getMediaList.bytesSpeed` 换算得到的实时码率
- `play_urls`：ControlPlane 根据节点 `agent_stream_addr` 和当前在线 schema 生成的播放地址列表

### 4.2 `GET /records`

支持字段：

- `task_id`
- `stream`
- `date_from`
- `date_to`
- `page`
- `page_size`

返回字段补充：

- `http_url`：录像文件的 HTTP 访问地址。若 ZLM Hook 未上报 URL，则允许为空。

说明：

- `file_path` 返回平台受管挂载根下的相对路径，而不是容器内绝对路径。
- 相对路径按“节点自己的网络挂载前缀”裁剪；常见表现为 `/node-<node-ip>-<type>/<task-id>/...`。

- 该接口只覆盖实时录制产生的录像。
- `record.format=hls` 时，列表按播放列表 `m3u8` 展示逻辑录像条目，不展开底层 `ts` segment。
- 仅因 `expose.enable_hls=true` 产生的实时播放 HLS 文件不会进入该接口。
- `stream_ingest + source_mode=vod + record.enabled=true` 且 expose 全关闭时，会进入快录分支；这类输出不会进入 `/records`，而是进入 `/file-artifacts`。

### 5.3 `GET /file-artifacts`

支持字段：

- `artifact_kind`
- `task_id`
- `date_from`
- `date_to`
- `page`
- `page_size`

返回字段：

- `id`
- `artifact_kind`
- `task_id`
- `attempt_id`
- `node_id`
- `file_name`
- `file_path`
- `http_url`
- `file_size`
- `created_at`

说明：

- 同时覆盖 `file_transcode`、`stream_bridge(file)` 与 `stream_ingest(vod 快录)` 的成功产物。
- `artifact_kind` 取值为 `transcode_output`、`bridge_output` 或 `stream_ingest_record`。
- `http_url` 基于工作节点 `agent_stream_addr` 和 `/data/zlm/www` 下的相对路径生成。

### 5.4 `GET /nodes`

返回节点健康、能力摘要、当前负载和最近心跳。

### 5.5 `GET /nodes/{id}/heartbeats`

返回指定节点最近的 heartbeat 历史样本，默认 `24` 条，最大 `200` 条。

## 6. 调试接口

### 6.1 `GET /debug/zlm/media`

按节点透传封装后的 `getMediaList` 结果，仅管理员可用。

### 6.2 `GET /debug/zlm/sessions`

按节点透传封装后的 `getAllSession` 结果，仅管理员可用。

### 6.3 `GET /debug/zlm/players`

按节点透传封装后的 `getMediaPlayerList` 结果，仅管理员可用。

### 6.4 `GET /debug/zlm/statistic`

按节点透传封装后的 `getStatistic` 结果，仅管理员可用。

### 6.5 `GET /debug/zlm/threads-load`

按节点透传封装后的 `getThreadsLoad` 结果，仅管理员可用。

### 6.6 `GET /debug/zlm/work-threads-load`

按节点透传封装后的 `getWorkThreadsLoad` 结果，仅管理员可用。

### 6.7 `POST /debug/zlm/kick-session`

请求体：

```json
{
  "node_id": "0195...",
  "session_id": "123456"
}
```

### 6.8 `POST /debug/zlm/kick-sessions`

请求体：

```json
{
  "node_id": "0195...",
  "local_port": 554,
  "peer_ip": "10.0.0.8"
}
```

`local_port` 和 `peer_ip` 都是可选过滤项，至少提供一个更有意义。

### 6.9 `POST /debug/zlm/close-stream`

请求体：

```json
{
  "node_id": "0195...",
  "schema": "rtsp",
  "vhost": "__defaultVhost__",
  "app": "live",
  "stream": "camera01",
  "force": false
}
```

### 6.10 `GET /debug/zlm/snap`

查询参数：

- `node_id`
- `url`
- `timeout_sec`，默认 `10`
- `expire_sec`，默认 `30`

返回 JSON：

```json
{
  "content_type": "image/jpeg",
  "data_url": "data:image/jpeg;base64,..."
}
```

### 6.11 `GET /debug/hooks`

返回指定节点最近的 Hook 时间线。

查询参数：

- `node_id`
- `hook_name`
- `limit`，默认 `50`

## 7. 权限约束

| 接口组 | 平台管理员 | 业务调用方 | 审计用户 |
| --- | --- | --- | --- |
| 任务增删改查 | 允许 | 允许本租户 | 只读 |
| 节点与调试 | 允许 | 禁止 | 禁止 |
| 录像浏览 | 允许 | 允许本租户 | 只读 |

## 8. 状态与接口冲突规则

- 状态非法时返回 `409 CONFLICT`。
- 参数不合法返回 `422 UNPROCESSABLE ENTITY`。
- 下游依赖不可用返回 `503 SERVICE UNAVAILABLE`。
- 节点能力不满足返回 `412 PRECONDITION FAILED`。
