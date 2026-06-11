export interface ApiParam {
  name: string;
  location: string;
  type: string;
  required: string | boolean;
  description: string;
  example?: unknown;
  enumValues?: string[];
}

export interface ApiField {
  path: string;
  type: string;
  description: string;
  example?: unknown;
  enumValues?: string[];
  required?: string | boolean;
}

export interface ExternalApiDoc {
  category: string;
  method: string;
  path: string;
  title: string;
  summary: string;
  description: string;
  successStatus: string;
  params: ApiParam[];
  requestExample: unknown;
  responseExample: unknown;
  notes?: string[];
  requestFields?: ApiField[];
  responseFields?: ApiField[];
  implementationOwner?: "streamserver" | "business_system";
  direction?: string;
}

const authHeaderParam = (required: string | boolean = "按环境"): ApiParam => ({
  name: "Authorization",
  location: "Header",
  type: "string",
  required,
  description: "Bearer 访问令牌。部署启用鉴权时必须携带。",
  example: "Bearer eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.demo.signature",
});

const idempotencyHeaderParam = (): ApiParam => ({
  name: "Idempotency-Key",
  location: "Header",
  type: "string",
  required: true,
  description: "写接口幂等键；同一路径和同一请求体会复用第一次结果。",
  example: "task-create-relay-camera-01-20260412",
});

const taskIdPathParam = (): ApiParam => ({
  name: "id",
  location: "Path",
  type: "string",
  required: true,
  description: "任务 ID。",
  example: "019d77d3-a942-7c91-8e82-ff963ccf1222",
});

const nodeIdPathParam = (): ApiParam => ({
  name: "id",
  location: "Path",
  type: "string",
  required: true,
  description: "节点 ID。",
  example: "f8996fe7-6a7e-4aa0-8b13-2e8a5f9fdc31",
});

const nodeIdQueryParam = (): ApiParam => ({
  name: "node_id",
  location: "Query",
  type: "string",
  required: true,
  description: "目标工作节点 ID，用于把调试调用路由到指定节点。",
  example: "f8996fe7-6a7e-4aa0-8b13-2e8a5f9fdc31",
});

const callbackEventHeaderParam = (eventType: "task.status" | "task.completed"): ApiParam => ({
  name: "X-StreamServer-Event",
  location: "Header",
  type: "string",
  required: true,
  description: "回调事件类型。业务系统可用它快速分流不同回调处理逻辑。",
  example: eventType,
  enumValues: [eventType],
});

const callbackEventIdHeaderParam = (): ApiParam => ({
  name: "X-StreamServer-Event-Id",
  location: "Header",
  type: "string",
  required: true,
  description: "这次回调的全局事件 ID，可用于幂等去重和排障。",
  example: "019d811a-0af8-7072-8abb-2ff5dd190f40",
});

const callbackTaskIdHeaderParam = (): ApiParam => ({
  name: "X-StreamServer-Task-Id",
  location: "Header",
  type: "string",
  required: true,
  description: "关联任务 ID。",
  example: "019d77d3-a942-7c91-8e82-ff963ccf1222",
});

const callbackAttemptNoHeaderParam = (): ApiParam => ({
  name: "X-StreamServer-Attempt-No",
  location: "Header",
  type: "integer",
  required: true,
  description: "关联 Attempt 编号。",
  example: 1,
});

const callbackSignatureHeaderParam = (): ApiParam => ({
  name: "X-StreamServer-Signature",
  location: "Header",
  type: "string",
  required: "按环境",
  description: "若平台配置了 CALLBACK_SHARED_SECRET，会携带 `sha256=<hex>` 形式的 HMAC 签名。业务系统应按原始请求体校验。",
  example: "sha256=9a12ef3e8d2b0c7a9fa1d8c4b17ec2d5f00b21e5a5fd3d8a8a73e0fbcb8f0d44",
});

const TASK_TYPE_ENUM = ["stream_ingest", "stream_bridge", "file_transcode"];
const TASK_STATUS_ENUM = [
  "CREATED",
  "VALIDATING",
  "QUEUED",
  "DISPATCHING",
  "STARTING",
  "RUNNING",
  "STOPPING",
  "RECOVERING",
  "SUCCEEDED",
  "FAILED",
  "CANCELED",
  "LOST",
];

const ATTEMPT_STATUS_ENUM = ["STARTING", "RUNNING", "STOPPING", "SUCCEEDED", "FAILED"];
const WORKER_KIND_ENUM = ["zlm_proxy", "ffmpeg", "zlm_rtp_server", "hybrid"];
const INPUT_KIND_ENUM = [
  "rtsp",
  "rtmp",
  "hls",
  "ftp",
  "http_mp4",
  "http_flv",
  "http_ts",
  "file",
  "udp_mpegts_multicast",
  "rtp_multicast",
  "gb_rtp",
];
const SOURCE_MODE_ENUM = ["live", "vod"];
const PROCESS_MODE_ENUM = ["passthrough", "copy_or_transcode", "force_transcode"];
const TASK_TRANSCODE_MODE_ENUM = ["none", "adaptive", "forced"];
const PUBLISH_KIND_ENUM = ["file", "udp_mpegts_multicast", "rtp_multicast", "rtmp_push"];
const RECORD_FORMAT_ENUM = ["mp4", "hls", "both"];
const RECOVERY_POLICY_ENUM = ["auto", "never"];
const FILE_ARTIFACT_KIND_ENUM = ["transcode_output", "bridge_output", "stream_ingest_record"];
const START_MODE_ENUM = ["immediate", "manual", "at", "cron"];
const LOG_STREAM_ENUM = ["stdout", "stderr", "merged"];
const SORT_ORDER_ENUM = ["asc", "desc"];
const EVENT_SOURCE_ENUM = ["core", "agent", "hook", "scheduler"];
const EVENT_LEVEL_ENUM = ["debug", "info", "warning", "error"];
const CALLBACK_EVENT_ENUM = ["task.status", "task.completed"];
const CALLBACK_REASON_ENUM = ["running", "terminal_state", "artifact_update"];
const CALLBACK_STATUS_ENUM = ["pending", "retrying", "delivered", "failed", "dead"];
const STREAM_SCHEMA_ENUM = ["rtsp", "rtmp", "http_ts", "http_fmp4", "hls"];
const AUTH_MODE_ENUM = ["disabled", "external_jwt", "local_password"];
const ROLE_ENUM = ["admin", "operator", "viewer"];
const NETWORK_MODE_ENUM = ["host", "bridge"];
const COMMON_PUBLISH_FORMAT_ENUM = ["系统默认", "mp4", "flv", "mpegts", "rtp_mpegts", "matroska", "mkv", "mov", "hls"];

interface FieldMeta {
  description: string;
  enumValues?: string[];
  required?: string | boolean;
}

function fieldMeta(path: string): FieldMeta | null {
  const rules: Array<{ test: (path: string) => boolean; meta: FieldMeta }> = [
    { test: (value) => value === "auth_enabled", meta: { description: "是否启用鉴权。" } },
    {
      test: (value) => value === "auth_mode",
      meta: { description: "当前实例的鉴权模式。", enumValues: AUTH_MODE_ENUM },
    },
    {
      test: (value) => value === "role",
      meta: { description: "当前账号角色。", enumValues: ROLE_ENUM },
    },
    { test: (value) => value === "must_change_password", meta: { description: "是否要求当前账号尽快改密。" } },
    { test: (value) => value === "permissions[]", meta: { description: "当前账号持有的权限编码列表。" } },
    { test: (value) => value === "environment", meta: { description: "当前系统运行环境标识。" } },
    { test: (value) => value === "subject", meta: { description: "当前登录主体或令牌对应的用户名。" } },
    { test: (value) => value === "username", meta: { description: "登录用户名。", required: true } },
    { test: (value) => value === "password", meta: { description: "登录密码。", required: true } },
    { test: (value) => value === "current_password", meta: { description: "当前密码。", required: true } },
    { test: (value) => value === "new_password", meta: { description: "新密码，通常要求至少 8 位。", required: true } },
    { test: (value) => value === "refresh_token", meta: { description: "刷新令牌。", required: true } },
    { test: (value) => value === "access_token", meta: { description: "访问令牌。" } },
    { test: (value) => value === "id", meta: { description: "对象主键 ID。" } },
    { test: (value) => value === "name" || value === "task.name" || value === "items[].name", meta: { description: "业务名称或显示名称。" } },
    {
      test: (value) => value === "type" || value.endsWith(".type"),
      meta: { description: "任务类型。", enumValues: TASK_TYPE_ENUM, required: "任务创建时必填" },
    },
    {
      test: (value) => /^status$|^task\.status$|^items\[\]\.status$/.test(value),
      meta: { description: "任务状态。", enumValues: TASK_STATUS_ENUM },
    },
    {
      test: (value) => /current_attempt\.status$/.test(value),
      meta: { description: "当前 Attempt 状态。", enumValues: ATTEMPT_STATUS_ENUM },
    },
    { test: (value) => value === "priority" || value.endsWith(".priority"), meta: { description: "调度优先级，数值越大越优先。" } },
    { test: (value) => value === "created_by" || value.endsWith(".created_by"), meta: { description: "任务创建者或发起方标识。" } },
    { test: (value) => value === "assigned_node_id" || value.endsWith(".assigned_node_id"), meta: { description: "当前分配到的工作节点 ID。" } },
    { test: (value) => value === "current_attempt_no" || value.endsWith(".current_attempt_no"), meta: { description: "当前 Attempt 编号，从 1 开始。" } },
    { test: (value) => /(^|\.)(created_at|updated_at|started_at|finished_at|delivered_at|start_time|node_time|received_at|capability_captured_at)$/.test(value), meta: { description: "RFC3339 时间戳。" } },
    { test: (value) => value === "requested_spec", meta: { description: "用户原始提交的任务规格。" } },
    { test: (value) => value === "resolved_spec", meta: { description: "系统补默认值、合并规则后的最终执行规格。" } },
    { test: (value) => value === "task", meta: { description: "任务主信息对象。" } },
    { test: (value) => value === "current_attempt", meta: { description: "当前 Attempt 对象。" } },
    { test: (value) => value === "recent_events[]", meta: { description: "最近事件列表。" } },
    { test: (value) => value === "callback_delivery", meta: { description: "最近一次回调投递状态。" } },
    { test: (value) => /attempt_no$/.test(value), meta: { description: "Attempt 编号。" } },
    {
      test: (value) => /worker_kind$/.test(value),
      meta: { description: "执行器类型。", enumValues: WORKER_KIND_ENUM },
    },
    { test: (value) => /node_id$/.test(value) && value !== "node_id", meta: { description: "关联节点 ID。" } },
    { test: (value) => /pid$/.test(value), meta: { description: "本地进程号。" } },
    { test: (value) => /exit_code$/.test(value), meta: { description: "进程退出码。" } },
    { test: (value) => /failure_code$/.test(value), meta: { description: "结构化失败码。" } },
    { test: (value) => /failure_reason$/.test(value), meta: { description: "失败原因说明。" } },
    {
      test: (value) => value === "recent_events[].source",
      meta: { description: "事件来源。", enumValues: EVENT_SOURCE_ENUM },
    },
    { test: (value) => /event_type$/.test(value), meta: { description: "事件类型编码。" } },
    {
      test: (value) => /event_level$/.test(value),
      meta: { description: "事件级别。", enumValues: EVENT_LEVEL_ENUM },
    },
    { test: (value) => /payload$/.test(value), meta: { description: "结构化事件载荷。" } },
    {
      test: (value) => value === "next_cursor",
      meta: { description: "下一次增量读取日志时使用的游标。" },
    },
    { test: (value) => value === "lines[]", meta: { description: "日志行数组。" } },
    { test: (value) => value.endsWith(".ts"), meta: { description: "日志行时间戳。" } },
    {
      test: (value) => value.endsWith(".stream"),
      meta: { description: "日志流类型。", enumValues: LOG_STREAM_ENUM },
    },
    { test: (value) => value.endsWith(".line"), meta: { description: "单行日志文本。" } },
    { test: (value) => value === "common", meta: { description: "任务通用信息。" } },
    { test: (value) => value === "common.created_by", meta: { description: "任务创建方标识。", required: false } },
    { test: (value) => value === "common.callback_url", meta: { description: "状态/完成回调地址。", required: false } },
    { test: (value) => value === "common.labels[]", meta: { description: "业务标签列表。", required: false } },
    { test: (value) => value === "input", meta: { description: "输入源定义。" } },
    {
      test: (value) => value === "input.kind",
      meta: { description: "输入源类型。", enumValues: INPUT_KIND_ENUM, required: true },
    },
    {
      test: (value) => value === "input.source_mode",
      meta: { description: "输入源语义，显式区分实时源和离线源。", enumValues: SOURCE_MODE_ENUM, required: "hls/http_ts 时必填；ftp 固定为 vod" },
    },
    {
      test: (value) => value === "input.loop_enabled",
      meta: {
        description:
          "是否在离线输入读到 EOF 后从头循环读取。仅 `stream_ingest + source_mode=vod` 支持，对 `file`、`http_mp4`、`hls(vod)`、`http_ts(vod)` 生效；若同时关闭全部播放协议并启用录制，会进入快录模式，此时 `loop_enabled=true` 需要配合 `record.duration_sec`。",
      },
    },
    {
      test: (value) => value === "input.url",
      meta: {
        description: "输入 URL；当 input.kind=file 时，这里填写相对 /data/media/work 的文件路径，前导 / 会被自动忽略；当 input.kind=ftp 时，只支持 ftp://，不支持 ftps://。",
        required: "URL/文件输入时必填",
      },
    },
    { test: (value) => value === "input.group", meta: { description: "组播地址。", required: "组播输入时必填" } },
    { test: (value) => value === "input.port", meta: { description: "输入端口。", required: "组播/GB RTP 输入时必填" } },
    { test: (value) => value === "input.interface_name", meta: { description: "绑定网卡名称。" } },
    { test: (value) => value === "input.interface_ip", meta: { description: "绑定网卡 IP。" } },
    { test: (value) => value === "input.ttl", meta: { description: "多播 TTL。" } },
    { test: (value) => value === "input.reuse", meta: { description: "是否开启地址复用。" } },
    { test: (value) => value === "input.probe_timeout_ms", meta: { description: "输入探测超时，单位毫秒。" } },
    { test: (value) => value === "input.tcp_mode", meta: { description: "GB RTP TCP 模式。" } },
    { test: (value) => value === "input.ssrc", meta: { description: "GB RTP 期望 SSRC。" } },
    { test: (value) => value === "process", meta: { description: "处理方式定义。" } },
    {
      test: (value) => value === "process.mode",
      meta: { description: "处理策略。", enumValues: PROCESS_MODE_ENUM },
    },
    { test: (value) => value === "process.bitrate", meta: { description: "目标码率，单位 kbps。" } },
    { test: (value) => value === "process.fps", meta: { description: "目标帧率。" } },
    { test: (value) => value === "process.gop", meta: { description: "关键帧间隔。" } },
    { test: (value) => value === "stream", meta: { description: "内部流命名规则。" } },
    { test: (value) => value === "stream.app", meta: { description: "内部应用名。", required: "stream_ingest 时建议填写" } },
    { test: (value) => value === "stream.name", meta: { description: "内部流名。", required: "stream_ingest 时建议填写" } },
    { test: (value) => value === "stream.vhost", meta: { description: "内部流所属 vhost。" } },
    { test: (value) => value === "expose", meta: { description: "内部流对外播放协议开关。对 `stream_ingest + source_mode=live`，若外部显式关闭全部播放协议，`resolved_spec` 会自动开启 `enable_http_fmp4=true` 作为最小兜底；对 `stream_ingest + source_mode=vod + record.enabled=true`，任一协议开启都会保持实时录制，全部关闭则切到快录且不再提供实时流播放地址。" } },
    { test: (value) => /^expose\./.test(value), meta: { description: "布尔开关，控制是否暴露对应协议。" } },
    { test: (value) => value === "publish", meta: { description: "显式输出目标定义。" } },
    {
      test: (value) => value === "publish.kind",
      meta: { description: "输出目标类型。", enumValues: PUBLISH_KIND_ENUM, required: "stream_bridge / file_transcode 时必填" },
    },
    {
      test: (value) => value === "publish.url",
      meta: { description: "输出目标 URL。仅 `publish.kind=rtmp_push` 时填写，且必须以 `rtmp://` 或 `rtmps://` 开头；文件输出禁止传入。", required: "rtmp_push 时必填" },
    },
    { test: (value) => value === "publish.group", meta: { description: "输出组播地址。", required: "组播输出时必填" } },
    { test: (value) => value === "publish.port", meta: { description: "输出端口。", required: "组播输出时必填" } },
    { test: (value) => value === "publish.interface_name", meta: { description: "发送组播时绑定的网卡名称；`rtmp_push` 不支持。" } },
    { test: (value) => value === "publish.interface_ip", meta: { description: "发送组播时绑定的网卡 IP；`rtmp_push` 不支持。" } },
    { test: (value) => value === "publish.ttl", meta: { description: "多播 TTL；`rtmp_push` 不支持。" } },
    {
      test: (value) => value === "publish.format",
      meta: { description: "输出封装格式；文件输出留空时默认 MP4，`mkv` 与 `matroska` 等价且输出扩展名为 `.mkv`，`webm` 暂不作为输出目标开放；组播输出按目标自动选择合适封装格式，`rtmp_push` 固定使用 FLV。", enumValues: COMMON_PUBLISH_FORMAT_ENUM },
    },
    { test: (value) => value === "record", meta: { description: "录制设置。" } },
    { test: (value) => value === "record.enabled", meta: { description: "是否启用录制。对 `stream_ingest` 的 VOD 输入，录制模式会由 expose 自动判定。" } },
    {
      test: (value) => value === "record.format",
      meta: { description: "录制输出格式。", enumValues: RECORD_FORMAT_ENUM },
    },
    { test: (value) => value === "record.duration_sec", meta: { description: "录制时长上限，单位秒。VOD 快录分支下如果同时开启 `input.loop_enabled=true`，这里必须填写。" } },
    { test: (value) => value === "record.segment_sec", meta: { description: "录制分段时长，单位秒。MP4 不填时使用节点 AGENT_MP4_RECORD_SEGMENT_SEC（默认 7200）；Agent 托管 HLS 不填时使用节点 AGENT_HLS_RECORD_SEGMENT_SEC（默认 60，可配置 30/60）。" } },
    {
      test: (value) => value === "record.save_path",
      meta: { description: "已忽略。stream_ingest 录制目录由系统托管生成，仅为兼容旧请求保留。" },
    },
    { test: (value) => value === "record.as_player", meta: { description: "是否按播放器视角维持录制保活。" } },
    { test: (value) => value === "recovery", meta: { description: "失败恢复策略。" } },
    {
      test: (value) => value === "recovery.policy",
      meta: { description: "失败恢复策略。auto 会让连续型流接入在断源后持续等待恢复；never 会禁用自动恢复。", enumValues: RECOVERY_POLICY_ENUM },
    },
    { test: (value) => value === "recovery.resume_mode", meta: { description: "恢复时的高级恢复模式，当前为保留字段。" } },
    { test: (value) => value === "recovery.max_consecutive_failures", meta: { description: "最大连续启动失败次数；不限制连续型流接入的断链等待。" } },
    { test: (value) => value === "schedule", meta: { description: "任务首次启动方式。" } },
    {
      test: (value) => value === "schedule.start_mode",
      meta: { description: "首次启动模式。", enumValues: START_MODE_ENUM },
    },
    { test: (value) => value === "schedule.start_at", meta: { description: "定时启动时间，RFC3339 格式。" } },
    { test: (value) => value === "schedule.cron", meta: { description: "Cron 表达式。" } },
    { test: (value) => value === "resource", meta: { description: "调度资源约束。" } },
    {
      test: (value) => value === "resource.required_labels[]",
      meta: { description: "节点必需标签。只有同时具备全部这些标签的在线节点才会进入调度候选集；如果当前没有任何匹配标签的在线节点，任务会直接失败。" },
    },
    {
      test: (value) => value === "transcode_mode",
      meta: { description: "基于 resolved_spec 推导出的转码摘要。`none` 表示不转码，`adaptive` 表示拷贝优先且必要时转码，`forced` 表示按配置会走转码路径。", enumValues: TASK_TRANSCODE_MODE_ENUM, required: false },
    },
    { test: (value) => value === "items[]", meta: { description: "列表结果项。" } },
    { test: (value) => value === "page", meta: { description: "当前页码，从 1 开始。" } },
    { test: (value) => value === "page_size", meta: { description: "每页条数。" } },
    { test: (value) => value === "total", meta: { description: "总条数。" } },
    { test: (value) => value === "callback_url" || value.endsWith(".callback_url"), meta: { description: "回调地址。" } },
    { test: (value) => value === "event_id" || value.endsWith(".event_id"), meta: { description: "回调事件 ID。" } },
    { test: (value) => value === "event_time" || value.endsWith(".event_time"), meta: { description: "回调发送时间，RFC3339 时间戳。" } },
    {
      test: (value) => value === "event_type",
      meta: { description: "回调事件类型。", enumValues: CALLBACK_EVENT_ENUM },
    },
    {
      test: (value) => value === "reason",
      meta: { description: "回调触发原因。", enumValues: CALLBACK_REASON_ENUM },
    },
    {
      test: (value) => value === "callback_delivery.event_type",
      meta: { description: "最近一次回调的事件类型。", enumValues: CALLBACK_EVENT_ENUM },
    },
    {
      test: (value) => value === "callback_delivery.reason",
      meta: { description: "最近一次回调触发原因。", enumValues: CALLBACK_REASON_ENUM },
    },
    {
      test: (value) => value === "callback_delivery.status",
      meta: { description: "最近一次回调投递状态。", enumValues: CALLBACK_STATUS_ENUM },
    },
    { test: (value) => value === "callback_delivery.delivery_attempts", meta: { description: "累计投递尝试次数。" } },
    { test: (value) => value === "callback_delivery.last_http_status", meta: { description: "最近一次回调的 HTTP 状态码。" } },
    { test: (value) => value === "callback_delivery.last_error", meta: { description: "最近一次回调错误信息。" } },
    { test: (value) => value === "schema" || value.endsWith(".schema"), meta: { description: "流协议。", enumValues: STREAM_SCHEMA_ENUM } },
    { test: (value) => value === "vhost" || value.endsWith(".vhost"), meta: { description: "内部流 vhost。" } },
    { test: (value) => value === "app" || value.endsWith(".app"), meta: { description: "内部应用名。" } },
    { test: (value) => value === "stream" || value.endsWith(".stream"), meta: { description: "流名。" } },
    { test: (value) => value === "task_id" || value.endsWith(".task_id"), meta: { description: "关联任务 ID。" } },
    { test: (value) => value === "attempt_id" || value.endsWith(".attempt_id"), meta: { description: "关联 Attempt ID。" } },
    { test: (value) => value === "task_name" || value.endsWith(".task_name"), meta: { description: "任务名称。" } },
    { test: (value) => value.endsWith(".play_urls[]"), meta: { description: "可直接播放的 URL 列表。" } },
    { test: (value) => /viewer_count$/.test(value), meta: { description: "当前观众数量。" } },
    { test: (value) => /has_viewer$/.test(value), meta: { description: "当前是否存在观众。" } },
    { test: (value) => /bitrate_kbps$/.test(value), meta: { description: "当前码率，单位 kbps。" } },
    { test: (value) => /file_path$/.test(value), meta: { description: "文件在节点或共享存储上的完整路径。" } },
    { test: (value) => /http_url$/.test(value), meta: { description: "通过 HTTP 下载或访问该文件的地址。" } },
    { test: (value) => /file_size$/.test(value), meta: { description: "文件大小，单位字节。" } },
    { test: (value) => /time_len$/.test(value), meta: { description: "媒体时长，单位秒。" } },
    { test: (value) => /file_name$/.test(value), meta: { description: "产物文件名。" } },
    { test: (value) => value === "artifact_kind" || value.endsWith(".artifact_kind"), meta: { description: "文件产物来源类型。", enumValues: FILE_ARTIFACT_KIND_ENUM } },
    { test: (value) => value === "streams[]", meta: { description: "本次任务关联的内部流列表。" } },
    { test: (value) => value === "records[]", meta: { description: "本次任务关联的录像文件列表。" } },
    { test: (value) => value === "file_artifacts[]", meta: { description: "本次任务产生的文件产物列表，包含转码输出、桥接输出和流接入快录输出。" } },
    { test: (value) => value === "latest_event", meta: { description: "最近一条与业务相关的事件摘要。" } },
    { test: (value) => value === "latest_event.message", meta: { description: "最近事件的人类可读说明。" } },
    { test: (value) => value === "source" || value.endsWith(".source"), meta: { description: "来源类型或来源标识。" } },
    { test: (value) => value === "entries[]", meta: { description: "机器 API 白名单条目数组。" } },
    { test: (value) => /cidr$/.test(value), meta: { description: "CIDR 形式的来源 IP 范围。" } },
    { test: (value) => /description$/.test(value), meta: { description: "补充说明文本。" } },
    { test: (value) => /node_name$/.test(value), meta: { description: "节点显示名。" } },
    { test: (value) => /hostname$/.test(value), meta: { description: "节点主机名。" } },
    { test: (value) => /labels\[\]$/.test(value), meta: { description: "标签列表。" } },
    { test: (value) => /network_mode$/.test(value), meta: { description: "节点网络模式。", enumValues: NETWORK_MODE_ENUM } },
    { test: (value) => /interfaces\[\]$/.test(value), meta: { description: "节点网卡列表。" } },
    { test: (value) => /healthy$/.test(value), meta: { description: "节点健康状态。" } },
    { test: (value) => /ffmpeg_protocols\[\]$/.test(value), meta: { description: "节点已探测到的 FFmpeg 协议。" } },
    { test: (value) => /ffmpeg_formats\[\]$/.test(value), meta: { description: "节点已探测到的 FFmpeg 格式列表。" } },
    { test: (value) => /ffmpeg_encoders\[\]$/.test(value), meta: { description: "节点已探测到的 FFmpeg 编码器。" } },
    { test: (value) => /ffmpeg_decoders\[\]$/.test(value), meta: { description: "节点已探测到的 FFmpeg 解码器。" } },
    { test: (value) => /zlm_api_list\[\]$/.test(value), meta: { description: "节点支持的 ZLM API 名称列表。" } },
    { test: (value) => /gpu\[\]$/.test(value), meta: { description: "GPU 标识列表。" } },
    { test: (value) => /gpu_devices\[\]/.test(value), meta: { description: "GPU 设备原始能力信息。" } },
    { test: (value) => /slot_usage$/.test(value), meta: { description: "当前资源槽位占用数。" } },
    { test: (value) => /running_tasks$/.test(value), meta: { description: "当前运行任务数。" } },
    { test: (value) => /connected$/.test(value), meta: { description: "控制面连接状态。" } },
    { test: (value) => /cpu_percent$/.test(value), meta: { description: "CPU 使用率，百分比。" } },
    { test: (value) => /mem_percent$/.test(value), meta: { description: "内存使用率，百分比。" } },
    { test: (value) => /disk_percent$/.test(value), meta: { description: "磁盘使用率，百分比。" } },
    { test: (value) => /zlm_alive$/.test(value), meta: { description: "ZLM 存活状态。" } },
    { test: (value) => /ffmpeg_alive$/.test(value), meta: { description: "FFmpeg 可用状态。" } },
    { test: (value) => /gpu_runtime\[\]/.test(value), meta: { description: "GPU 运行时统计数组。" } },
    { test: (value) => /utilization_gpu$/.test(value), meta: { description: "GPU 核心利用率，百分比。" } },
    { test: (value) => /utilization_memory$/.test(value), meta: { description: "GPU 显存利用率，百分比。" } },
    { test: (value) => /memory_total_mb$/.test(value), meta: { description: "GPU 总显存，单位 MB。" } },
    { test: (value) => /memory_used_mb$/.test(value), meta: { description: "GPU 已用显存，单位 MB。" } },
    { test: (value) => value === "entries", meta: { description: "白名单包装对象。" } },
    { test: (value) => value === "data", meta: { description: "调试接口透传的主数据对象。" } },
    { test: (value) => value === "data[]", meta: { description: "调试接口透传的主数据数组。" } },
    { test: (value) => /peer_ip$/.test(value), meta: { description: "对端 IP。" } },
    { test: (value) => /local_port$/.test(value), meta: { description: "本地端口。" } },
    { test: (value) => /peer_port$/.test(value), meta: { description: "对端端口。" } },
    { test: (value) => /typeid$/.test(value), meta: { description: "ZLM 会话类型标识。" } },
    { test: (value) => /client_ip$/.test(value), meta: { description: "播放器客户端 IP。" } },
    { test: (value) => /originType$/.test(value), meta: { description: "ZLM 媒体源类型编号。" } },
    { test: (value) => /totalReaderCount$/.test(value), meta: { description: "总观众数。" } },
    { test: (value) => /bytesSpeed$/.test(value), meta: { description: "字节速率，单位 B/s。" } },
    { test: (value) => /player$/.test(value), meta: { description: "播放器数量。" } },
    { test: (value) => /media$/.test(value) && value !== "task.media", meta: { description: "媒体对象数量。" } },
    { test: (value) => /session$/.test(value) && value !== "task.session", meta: { description: "会话对象数量。" } },
    { test: (value) => /delay$/.test(value), meta: { description: "线程延迟。" } },
    { test: (value) => /load\[\]$/.test(value), meta: { description: "线程负载列表。" } },
    { test: (value) => value === "session_id", meta: { description: "要踢掉的会话 ID。", required: true } },
    { test: (value) => value === "local_port", meta: { description: "按本地端口批量筛选会话。" } },
    { test: (value) => value === "peer_ip", meta: { description: "按对端 IP 批量筛选会话。" } },
    { test: (value) => value === "force", meta: { description: "是否强制关闭流。" } },
    { test: (value) => value === "content_type", meta: { description: "抓图返回内容类型。" } },
    { test: (value) => value === "data_url", meta: { description: "Base64 data URL 形式的截图内容。" } },
    { test: (value) => /server_id$/.test(value), meta: { description: "上报该 Hook 的节点服务标识。" } },
    { test: (value) => /hook_name$/.test(value), meta: { description: "Hook 名称。" } },
    { test: (value) => /dedup_key$/.test(value), meta: { description: "用于去重的业务键。" } },
  ];

  for (const rule of rules) {
    if (rule.test(path)) {
      return rule.meta;
    }
  }
  return null;
}

function inferFieldType(value: unknown): string {
  if (Array.isArray(value)) {
    if (!value.length) return "array";
    const first = value[0];
    return typeof first === "object" && first !== null ? "array<object>" : `array<${inferFieldType(first)}>`;
  }
  if (value === null) return "null";
  if (typeof value === "string") return "string";
  if (typeof value === "number") return Number.isInteger(value) ? "integer" : "number";
  if (typeof value === "boolean") return "boolean";
  if (typeof value === "object") return "object";
  return typeof value;
}

function appendField(fields: ApiField[], path: string, value: unknown) {
  if (!path) return;
  const meta = fieldMeta(path);
  fields.push({
    path,
    type: inferFieldType(value),
    description: meta?.description ?? "该字段的语义可参考示例值和上方接口说明。",
    enumValues: meta?.enumValues,
    required: meta?.required,
    example: value,
  });
}

function walkFields(value: unknown, path = "", fields: ApiField[] = []): ApiField[] {
  if (value === null || value === undefined) {
    appendField(fields, path, value);
    return fields;
  }

  if (Array.isArray(value)) {
    if (!path) return fields;
    const arrayPath = `${path}[]`;
    appendField(fields, arrayPath, value);
    if (value.length) {
      const first = value[0];
      if (first && typeof first === "object" && !Array.isArray(first)) {
        Object.entries(first as Record<string, unknown>).forEach(([key, child]) => {
          walkFields(child, `${arrayPath}.${key}`, fields);
        });
      }
    }
    return fields;
  }

  if (typeof value === "object") {
    if (path) {
      appendField(fields, path, value);
    }
    Object.entries(value as Record<string, unknown>).forEach(([key, child]) => {
      walkFields(child, path ? `${path}.${key}` : key, fields);
    });
    return fields;
  }

  appendField(fields, path, value);
  return fields;
}

function buildBodyFields(example: unknown): ApiField[] {
  if (!example || typeof example !== "object" || !("body" in (example as Record<string, unknown>))) {
    return [];
  }
  const body = (example as Record<string, unknown>).body;
  if (!body || (typeof body === "object" && !Array.isArray(body) && Object.keys(body as Record<string, unknown>).length === 0)) {
    return [];
  }
  return walkFields(body);
}

function buildResponseFields(example: unknown): ApiField[] {
  if (example === null || example === undefined) {
    return [];
  }
  if (Array.isArray(example)) {
    return walkFields(example, "items");
  }
  return walkFields(example);
}

const streamIngestExample = {
  name: "relay-camera-01",
  type: "stream_ingest",
  priority: 50,
  common: {
    created_by: "alice",
    callback_url: "https://biz.example.com/callback",
    labels: ["project-a", "night-shift"],
  },
  input: {
    kind: "rtsp",
    source_mode: "live",
    loop_enabled: false,
    url: "rtsp://camera.example/live/camera01",
    probe_timeout_ms: 7000,
  },
  process: {
    mode: "copy_or_transcode",
  },
  stream: {
    app: "live",
    name: "camera01",
    vhost: "__defaultVhost__",
  },
  expose: {
    enable_rtsp: true,
    enable_rtmp: true,
    enable_http_ts: true,
    enable_http_fmp4: true,
    enable_hls: false,
    stop_on_no_reader: false,
  },
  record: {
    enabled: true,
    format: "mp4",
    duration_sec: 300,
  },
  recovery: {
    policy: "auto",
  },
  schedule: {
    start_mode: "immediate",
  },
  resource: {
    required_labels: ["beijing-idc"],
  },
};

const streamIngestLoopExample = {
  name: "promo-loop-01",
  type: "stream_ingest",
  priority: 50,
  common: {
    created_by: "alice",
  },
  input: {
    kind: "http_mp4",
    source_mode: "vod",
    loop_enabled: true,
    url: "http://vod.example.com/promo.mp4",
    probe_timeout_ms: 7000,
  },
  process: {
    mode: "copy_or_transcode",
  },
  stream: {
    app: "live",
    name: "promo-loop-01",
    vhost: "__defaultVhost__",
  },
  expose: {
    enable_rtsp: true,
    enable_rtmp: true,
    enable_http_ts: true,
    enable_http_fmp4: true,
    enable_hls: false,
    stop_on_no_reader: false,
  },
  record: {
    enabled: true,
    format: "mp4",
    duration_sec: 180,
  },
  recovery: {
    policy: "auto",
  },
  schedule: {
    start_mode: "immediate",
  },
};

const baseExternalApiDocs: ExternalApiDoc[] = [
  {
    category: "鉴权与会话",
    method: "GET",
    path: "/api/v1/me",
    title: "查询当前会话",
    summary: "返回当前登录主体、角色、权限集合和运行环境。",
    description: "控制台和外部集成方都可以用它判断当前账号是否已登录、是否需要改密，以及能否看到某些页面或接口。",
    successStatus: "200 OK",
    params: [authHeaderParam()],
    requestExample: {
      headers: {
        Authorization: authHeaderParam().example,
      },
    },
    responseExample: {
      auth_enabled: false,
      auth_mode: "disabled",
      subject: "auth_disabled",
      role: "admin",
      must_change_password: false,
      permissions: ["task_read", "task_write", "record_read", "node_read", "debug_read", "security_write"],
      environment: "production",
    },
  },
  {
    category: "鉴权与会话",
    method: "POST",
    path: "/api/v1/auth/login",
    title: "用户名密码登录",
    summary: "用用户名和密码换取访问令牌与刷新令牌。",
    description: "适合控制台登录页或外部管理系统接入；当系统关闭鉴权时通常不会用到这个接口。",
    successStatus: "200 OK",
    params: [],
    requestExample: {
      body: {
        username: "alice",
        password: "correct horse battery staple",
      },
    },
    responseExample: {
      access_token: "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.access.signature",
      refresh_token: "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.refresh.signature",
      subject: "alice",
    },
  },
  {
    category: "鉴权与会话",
    method: "POST",
    path: "/api/v1/auth/refresh",
    title: "刷新访问令牌",
    summary: "使用刷新令牌换取新的访问令牌。",
    description: "控制台会在访问令牌过期后自动调用；外部集成方可用它维持长会话。",
    successStatus: "200 OK",
    params: [],
    requestExample: {
      body: {
        refresh_token: "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.refresh.signature",
      },
    },
    responseExample: {
      access_token: "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.new-access.signature",
      refresh_token: "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.new-refresh.signature",
      subject: "alice",
    },
  },
  {
    category: "鉴权与会话",
    method: "POST",
    path: "/api/v1/auth/logout",
    title: "退出登录",
    summary: "使当前刷新令牌失效。",
    description: "控制台点击退出时会调用；若只想清掉前端本地令牌而不撤销服务端会话，可以不调用。",
    successStatus: "204 No Content",
    params: [authHeaderParam("可选")],
    requestExample: {
      headers: {
        Authorization: authHeaderParam("可选").example,
      },
      body: {
        refresh_token: "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.refresh.signature",
      },
    },
    responseExample: null,
  },
  {
    category: "鉴权与会话",
    method: "POST",
    path: "/api/v1/auth/change-password",
    title: "修改当前密码",
    summary: "修改当前账号密码。",
    description: "适合控制台安全页；提交成功后通常需要重新登录。",
    successStatus: "204 No Content",
    params: [authHeaderParam()],
    requestExample: {
      headers: {
        Authorization: authHeaderParam().example,
      },
      body: {
        current_password: "old-password",
        new_password: "new-password-2026",
      },
    },
    responseExample: null,
  },
  {
    category: "任务管理",
    method: "POST",
    path: "/api/v1/tasks/preview",
    title: "检查并解析任务规格",
    summary: "不落库，只返回 requested_spec 和 resolved_spec。",
    description: "适合新建任务页在正式创建前做校验，也适合外部系统在自动化下发前先做语义确认。",
    successStatus: "200 OK",
    params: [authHeaderParam("按环境"), idempotencyHeaderParam()],
    requestExample: {
      headers: {
        Authorization: authHeaderParam().example,
        "Idempotency-Key": "preview-relay-camera-01-20260412",
      },
      body: streamIngestLoopExample,
    },
    responseExample: {
      requested_spec: streamIngestLoopExample,
      resolved_spec: {
        ...streamIngestLoopExample,
        common: {
          created_by: "alice",
        },
      },
    },
    notes: [
      "这个接口不会创建任务，也不会占用节点资源。",
      "如果要让离线输入持续供流，可在 `stream_ingest + source_mode=vod` 时设置 `input.loop_enabled=true`。",
      "当 `stream_ingest + source_mode=live` 且 expose 全部关闭时，resolved_spec 会自动开启 HTTP-FMP4 兜底；当 `stream_ingest + source_mode=vod + record.enabled=true` 且 expose 全部关闭时，resolved_spec 会走快录语义。",
    ],
  },
  {
    category: "任务管理",
    method: "POST",
    path: "/api/v1/tasks",
    title: "创建任务",
    summary: "创建新的业务任务，支持幂等键。",
    description: "适合外部业务系统发起流接入、桥接、录制和离线转码任务；是否立即启动取决于 schedule.start_mode。",
    successStatus: "201 Created",
    params: [authHeaderParam("按环境"), idempotencyHeaderParam()],
    requestExample: {
      headers: {
        Authorization: authHeaderParam().example,
        "Idempotency-Key": "task-create-relay-camera-01-20260412",
      },
      body: streamIngestExample,
    },
    responseExample: {
      id: "019d77d3-a942-7c91-8e82-ff963ccf1222",
      name: "relay-camera-01",
      type: "stream_ingest",
      status: "RUNNING",
      transcode_mode: "none",
      priority: 50,
      created_by: "alice",
      assigned_node_id: "f8996fe7-6a7e-4aa0-8b13-2e8a5f9fdc31",
      current_attempt_no: 1,
      created_at: "2026-04-12T10:30:00+08:00",
      updated_at: "2026-04-12T10:30:08+08:00",
    },
    notes: [
      "`input.loop_enabled=true` 仅支持 `stream_ingest` 的离线输入；若同时配置 `record.duration_sec`，到时任务仍会自动成功结束。",
      "`stream_ingest` 的直播接入不会最终保持零 expose，外部全关时会默认开启 HTTP-FMP4；VOD 录制不会手动指定实时/快录，而是由 expose 自动判定。",
      "`resource.required_labels[]` 会做节点硬过滤；如果当前没有任何匹配标签的在线节点，任务会直接失败。",
    ],
  },
  {
    category: "任务管理",
    method: "GET",
    path: "/api/v1/tasks",
    title: "查询任务列表",
    summary: "按状态、类型、节点和时间等条件检索任务。",
    description: "适合任务看板、工单回查和批量状态同步。",
    successStatus: "200 OK",
    params: [
      authHeaderParam(),
      {
        name: "status",
        location: "Query",
        type: "string",
        required: false,
        description: "任务状态过滤。",
        example: "RUNNING",
        enumValues: TASK_STATUS_ENUM,
      },
      {
        name: "type",
        location: "Query",
        type: "string",
        required: false,
        description: "任务类型过滤。",
        example: "stream_ingest",
        enumValues: TASK_TYPE_ENUM,
      },
      { name: "assigned_node_id", location: "Query", type: "string", required: false, description: "按工作节点过滤。", example: "f8996fe7-6a7e-4aa0-8b13-2e8a5f9fdc31" },
      { name: "keyword", location: "Query", type: "string", required: false, description: "按任务名或 ID 模糊检索。", example: "camera01" },
      { name: "created_from", location: "Query", type: "RFC3339 datetime", required: false, description: "创建时间下界。", example: "2026-04-12T00:00:00+08:00" },
      { name: "created_to", location: "Query", type: "RFC3339 datetime", required: false, description: "创建时间上界。", example: "2026-04-12T23:59:59+08:00" },
      { name: "page", location: "Query", type: "integer", required: false, description: "页码，从 1 开始。", example: 1 },
      { name: "page_size", location: "Query", type: "integer", required: false, description: "每页条数。", example: 20 },
      { name: "sort_by", location: "Query", type: "string", required: false, description: "排序字段。", example: "created_at" },
      {
        name: "sort_order",
        location: "Query",
        type: "string",
        required: false,
        description: "排序方向。",
        example: "desc",
        enumValues: SORT_ORDER_ENUM,
      },
    ],
    requestExample: {
      headers: {
        Authorization: authHeaderParam().example,
      },
      query: {
        status: "RUNNING",
        type: "stream_ingest",
        keyword: "camera01",
        created_from: "2026-04-12T00:00:00+08:00",
        created_to: "2026-04-12T23:59:59+08:00",
        page: 1,
        page_size: 20,
        sort_by: "created_at",
        sort_order: "desc",
      },
    },
    responseExample: {
      items: [
        {
          id: "019d77d3-a942-7c91-8e82-ff963ccf1222",
          name: "relay-camera-01",
          type: "stream_ingest",
          status: "RUNNING",
          transcode_mode: "none",
          priority: 50,
          created_by: "alice",
          assigned_node_id: "f8996fe7-6a7e-4aa0-8b13-2e8a5f9fdc31",
          current_attempt_no: 1,
          created_at: "2026-04-12T10:30:00+08:00",
          updated_at: "2026-04-12T10:30:08+08:00",
        },
      ],
      page: 1,
      page_size: 20,
      total: 1,
    },
  },
  {
    category: "任务管理",
    method: "GET",
    path: "/api/v1/tasks/{id}",
    title: "查询任务详情",
    summary: "返回任务主信息、当前 Attempt、最近事件和回调状态。",
    description: "适合做任务详情页、异常排障和与外部系统的状态对账。",
    successStatus: "200 OK",
    params: [authHeaderParam(), taskIdPathParam()],
    requestExample: {
      headers: {
        Authorization: authHeaderParam().example,
      },
      path: {
        id: taskIdPathParam().example,
      },
    },
    responseExample: {
      task: {
        id: "019d77d3-a942-7c91-8e82-ff963ccf1222",
        name: "relay-camera-01",
        type: "stream_ingest",
        status: "RUNNING",
        transcode_mode: "none",
        priority: 50,
        created_by: "alice",
        assigned_node_id: "f8996fe7-6a7e-4aa0-8b13-2e8a5f9fdc31",
        current_attempt_no: 1,
        created_at: "2026-04-12T10:30:00+08:00",
        updated_at: "2026-04-12T10:30:08+08:00",
      },
      current_attempt: {
        id: "019d77d4-1e55-7b16-9f8c-f4b0b59f7c0b",
        attempt_no: 1,
        worker_kind: "hybrid",
        status: "RUNNING",
        node_id: "f8996fe7-6a7e-4aa0-8b13-2e8a5f9fdc31",
      },
      recent_events: [
        {
          id: "019d77d4-87db-7c5f-bf9f-8d6d11328791",
          source: "agent",
          event_type: "running",
          event_level: "info",
          payload: {
            message: "task is running",
          },
          created_at: "2026-04-12T10:30:08+08:00",
        },
      ],
      callback_delivery: {
        callback_url: "https://biz.example.com/callback",
        event_type: "task.status",
        reason: "running",
        status: "delivered",
        delivery_attempts: 1,
        delivered_at: "2026-04-12T10:30:09+08:00",
        updated_at: "2026-04-12T10:30:09+08:00",
      },
    },
  },
  {
    category: "任务管理",
    method: "DELETE",
    path: "/api/v1/tasks/{id}",
    title: "删除任务",
    summary: "删除指定任务及其关联的 Attempt、事件、录像和产物索引。",
    description:
      "允许删除 CREATED、VALIDATING、QUEUED、SUCCEEDED、FAILED、CANCELED，以及已经失去当前节点归属和租约的 LOST 任务；运行中任务仍需先停掉。",
    successStatus: "200 OK",
    params: [authHeaderParam(), taskIdPathParam()],
    requestExample: {
      headers: {
        Authorization: authHeaderParam().example,
      },
      path: {
        id: taskIdPathParam().example,
      },
    },
    responseExample: {
      id: "019d77d3-a942-7c91-8e82-ff963ccf1222",
      name: "relay-camera-01",
      type: "stream_ingest",
      status: "FAILED",
      transcode_mode: "none",
      priority: 50,
      created_by: "alice",
      assigned_node_id: null,
      current_attempt_no: 1,
      created_at: "2026-04-12T10:30:00+08:00",
      updated_at: "2026-04-12T10:35:21+08:00",
    },
    notes: [
      "删除成功后返回被删除任务的最后快照，便于前端提示和审计记录。",
      "如果任务仍处于 DISPATCHING、STARTING、RUNNING、STOPPING、RECOVERING，或 LOST 但仍保留当前租约/节点归属，会返回 `TASK_DELETE_FORBIDDEN`。",
    ],
  },
  {
    category: "业务系统回调",
    method: "POST",
    path: "{common.callback_url}",
    title: "接收任务运行回调",
    summary: "当任务某个 Attempt 首次进入 RUNNING 时，平台会向业务系统配置的回调地址发送 `task.status`。",
    description: "这不是 StreamServer 提供给你调用的接口，而是业务系统需要自行实现的 HTTP 接收端。创建任务时填写 `common.callback_url` 后，平台会主动调用这里。",
    successStatus: "2xx Accepted",
    implementationOwner: "business_system",
    direction: "平台 -> 业务系统",
    params: [
      { name: "Content-Type", location: "Header", type: "string", required: true, description: "固定为 `application/json`。", example: "application/json" },
      callbackEventHeaderParam("task.status"),
      callbackEventIdHeaderParam(),
      callbackTaskIdHeaderParam(),
      callbackAttemptNoHeaderParam(),
      callbackSignatureHeaderParam(),
    ],
    requestExample: {
      headers: {
        "Content-Type": "application/json",
        "X-StreamServer-Event": "task.status",
        "X-StreamServer-Event-Id": "019d811a-0af8-7072-8abb-2ff5dd190f40",
        "X-StreamServer-Task-Id": "019d77d3-a942-7c91-8e82-ff963ccf1222",
        "X-StreamServer-Attempt-No": 1,
        "X-StreamServer-Signature": "sha256=9a12ef3e8d2b0c7a9fa1d8c4b17ec2d5f00b21e5a5fd3d8a8a73e0fbcb8f0d44",
      },
      body: {
        event_id: "019d811a-0af8-7072-8abb-2ff5dd190f40",
        event_type: "task.status",
        reason: "running",
        event_time: "2026-04-12T10:21:33Z",
        status: "RUNNING",
        task: {
          id: "019d77d3-a942-7c91-8e82-ff963ccf1222",
          name: "relay-camera-01",
          type: "stream_ingest",
          status: "RUNNING",
          priority: 50,
          created_by: "alice",
          assigned_node_id: "f8996fe7-6a7e-4aa0-8b13-2e8a5f9fdc31",
          created_at: "2026-04-12T10:20:58Z",
          started_at: "2026-04-12T10:21:31Z",
          finished_at: null,
        },
        attempt: {
          id: "019d811a-08e1-72f2-99eb-62abf78cb33a",
          no: 1,
          status: "RUNNING",
          node_id: "f8996fe7-6a7e-4aa0-8b13-2e8a5f9fdc31",
          worker_kind: "hybrid",
          started_at: "2026-04-12T10:21:31Z",
          ended_at: null,
          failure_code: null,
          failure_reason: null,
        },
        latest_event: {
          event_type: "running",
          event_level: "info",
          message: "task is running",
          created_at: "2026-04-12T10:21:33Z",
        },
      },
    },
    responseExample: {
      accepted: true,
      message: "received",
    },
    notes: [
      "业务系统需要自己实现这个 POST 接口；只要返回任意 2xx，平台就认为回调接收成功。",
      "推荐使用 `X-StreamServer-Event-Id` 做幂等去重，避免网络重试导致重复入库。",
      "如果配置了 `CALLBACK_SHARED_SECRET`，请对原始请求体校验 `X-StreamServer-Signature`。",
      "`task.status` 只会在同一个 Attempt 首次进入 RUNNING 时发送一次。",
    ],
  },
  {
    category: "业务系统回调",
    method: "POST",
    path: "{common.callback_url}",
    title: "接收任务完成回调",
    summary: "当任务进入终态后，平台会优先等待预期产物入库，再向业务系统发送 `task.completed`。",
    description: "这同样是业务系统需要自行承接的接口。适合把任务完成、录像落盘和转码产物入库同步到你的业务系统；如果等待窗口内没有等到预期产物，平台会先发送终态回调，后续再用产物刷新回调补齐。",
    successStatus: "2xx Accepted",
    implementationOwner: "business_system",
    direction: "平台 -> 业务系统",
    params: [
      { name: "Content-Type", location: "Header", type: "string", required: true, description: "固定为 `application/json`。", example: "application/json" },
      callbackEventHeaderParam("task.completed"),
      callbackEventIdHeaderParam(),
      callbackTaskIdHeaderParam(),
      callbackAttemptNoHeaderParam(),
      callbackSignatureHeaderParam(),
    ],
    requestExample: {
      headers: {
        "Content-Type": "application/json",
        "X-StreamServer-Event": "task.completed",
        "X-StreamServer-Event-Id": "019d811a-14be-7fe9-a344-cfecb90246cc",
        "X-StreamServer-Task-Id": "019d77d3-a942-7c91-8e82-ff963ccf1222",
        "X-StreamServer-Attempt-No": 1,
        "X-StreamServer-Signature": "sha256=08dd7c2f5e5951788eac909d6c42411d0328d1323f5717161f3ee1383b86362f",
      },
      body: {
        event_id: "019d811a-14be-7fe9-a344-cfecb90246cc",
        event_type: "task.completed",
        reason: "terminal_state",
        event_time: "2026-04-12T10:21:45Z",
        task: {
          id: "019d77d3-a942-7c91-8e82-ff963ccf1222",
          name: "relay-camera-01",
          type: "stream_ingest",
          status: "SUCCEEDED",
          priority: 50,
          created_by: "alice",
          assigned_node_id: "f8996fe7-6a7e-4aa0-8b13-2e8a5f9fdc31",
          created_at: "2026-04-12T10:20:58Z",
          started_at: "2026-04-12T10:21:31Z",
          finished_at: "2026-04-12T10:21:45Z",
        },
        attempt: {
          id: "019d811a-08e1-72f2-99eb-62abf78cb33a",
          no: 1,
          status: "SUCCEEDED",
          node_id: "f8996fe7-6a7e-4aa0-8b13-2e8a5f9fdc31",
          worker_kind: "hybrid",
          started_at: "2026-04-12T10:21:31Z",
          ended_at: "2026-04-12T10:21:45Z",
          failure_code: null,
          failure_reason: null,
        },
        streams: [
          {
            schema: "rtsp",
            vhost: "__defaultVhost__",
            app: "live",
            stream: "camera01",
            play_urls: ["rtsp://192.168.6.10/live/camera01"],
            rtp_stream_id: null,
          },
        ],
        records: [
          {
            id: "019d811a-0ef0-714c-8d0d-d76c2ce6b6ca",
            file_path: "/data/zlm/www/record/project-a/camera01-20260412-102133.mp4",
            http_url: "http://192.168.6.10/record/project-a/camera01-20260412-102133.mp4",
            file_size: 52428800,
            time_len: 300,
            start_time: "2026-04-12T10:21:33Z",
            source: "zlm_mp4",
          },
        ],
        file_artifacts: [
          {
            artifact_kind: "transcode_output",
            id: "019d811a-10fc-7f08-bdf0-0566ec5849c1",
            file_name: "camera01-archive.mp4",
            file_path: "/data/zlm/www/artifacts/transcode/camera01-archive.mp4",
            http_url: "http://192.168.6.10/artifacts/transcode/camera01-archive.mp4",
            file_size: 62310412,
            created_at: "2026-04-12T10:21:44Z",
          },
        ],
        latest_event: {
          event_type: "succeeded",
          event_level: "info",
          message: "task completed",
          created_at: "2026-04-12T10:21:45Z",
        },
      },
    },
    responseExample: {
      accepted: true,
      sync_task_id: "biz-sync-20260412-001",
    },
    notes: [
      "这条接口由业务系统实现，平台主动 POST 过来；响应体内容由业务系统自定义，但平台只关心是否返回 2xx。",
      "对有录像或文件产物预期的任务，平台会优先等待产物入库后再发送首次 `reason=terminal_state`；如果超时仍未产出，会先发终态，晚到产物再补一条 `reason=artifact_update` 的刷新回调。",
      "网络错误、超时、HTTP 429 和 5xx 会自动重试；其他 4xx 不重试。",
    ],
  },
  {
    category: "任务管理",
    method: "GET",
    path: "/api/v1/tasks/{id}/events",
    title: "查询任务事件",
    summary: "返回任务事件时间线。",
    description: "适合增量排障、审计和对账；事件包含来源、级别和结构化载荷。",
    successStatus: "200 OK",
    params: [
      authHeaderParam(),
      taskIdPathParam(),
      { name: "attempt_no", location: "Query", type: "integer", required: false, description: "按 Attempt 过滤。", example: 1 },
      {
        name: "source",
        location: "Query",
        type: "string",
        required: false,
        description: "按事件来源过滤。",
        example: "agent",
        enumValues: EVENT_SOURCE_ENUM,
      },
      { name: "event_type", location: "Query", type: "string", required: false, description: "按事件类型过滤。", example: "task_progress" },
      { name: "page", location: "Query", type: "integer", required: false, description: "页码。", example: 1 },
      { name: "page_size", location: "Query", type: "integer", required: false, description: "每页条数。", example: 50 },
    ],
    requestExample: {
      headers: {
        Authorization: authHeaderParam().example,
      },
      path: {
        id: taskIdPathParam().example,
      },
      query: {
        attempt_no: 1,
        page: 1,
        page_size: 50,
      },
    },
    responseExample: {
      items: [
        {
          id: "019d77d4-87db-7c5f-bf9f-8d6d11328791",
          attempt_no: 1,
          source: "agent",
          event_type: "running",
          event_level: "info",
          payload: {
            message: "task is running",
          },
          created_at: "2026-04-12T10:30:08+08:00",
        },
      ],
      page: 1,
      page_size: 50,
      total: 1,
    },
  },
  {
    category: "任务管理",
    method: "GET",
    path: "/api/v1/tasks/{id}/logs",
    title: "查询任务日志",
    summary: "读取任务 stdout / stderr 日志。",
    description: "适合增量拉日志、联调 FFmpeg 命令和排查运行时错误。",
    successStatus: "200 OK",
    params: [
      authHeaderParam(),
      taskIdPathParam(),
      { name: "attempt_no", location: "Query", type: "integer", required: false, description: "默认取当前 Attempt。", example: 1 },
      {
        name: "stream",
        location: "Query",
        type: "string",
        required: false,
        description: "日志流类型。",
        example: "merged",
        enumValues: LOG_STREAM_ENUM,
      },
      { name: "cursor", location: "Query", type: "string", required: false, description: "增量游标。", example: "1710000000.123" },
      { name: "limit", location: "Query", type: "integer", required: false, description: "最大返回行数。", example: 200 },
    ],
    requestExample: {
      headers: {
        Authorization: authHeaderParam().example,
      },
      path: {
        id: taskIdPathParam().example,
      },
      query: {
        attempt_no: 1,
        stream: "merged",
        limit: 200,
      },
    },
    responseExample: {
      attempt_no: 1,
      next_cursor: "1710000000.123",
      lines: [
        {
          ts: "2026-04-12T10:30:07+08:00",
          stream: "stderr",
          line: "frame=10 fps=25 q=-1.0 size=512kB time=00:00:00.40 bitrate=10485.7kbits/s",
        },
      ],
    },
  },
  {
    category: "任务管理",
    method: "GET",
    path: "/api/v1/tasks/{id}/resolved-spec",
    title: "查询冻结后的任务规格",
    summary: "返回实际落库并用于执行的 resolved_spec。",
    description: "适合审计、复盘和把历史任务重新克隆为新任务。",
    successStatus: "200 OK",
    params: [authHeaderParam(), taskIdPathParam()],
    requestExample: {
      headers: {
        Authorization: authHeaderParam().example,
      },
      path: {
        id: taskIdPathParam().example,
      },
    },
    responseExample: streamIngestExample,
  },
  {
    category: "任务管理",
    method: "POST",
    path: "/api/v1/tasks/{id}/start",
    title: "启动任务",
    summary: "启动或重新派发处于 CREATED、VALIDATING、QUEUED、FAILED 或 CANCELED 的任务。",
    description: "适合手动审核后启动、补推进等待中的任务、失败后人工拉起等场景。",
    successStatus: "202 Accepted",
    params: [authHeaderParam(), idempotencyHeaderParam(), taskIdPathParam()],
    requestExample: {
      headers: {
        Authorization: authHeaderParam().example,
        "Idempotency-Key": "task-start-019d77d3",
      },
      path: {
        id: taskIdPathParam().example,
      },
      body: {},
    },
    responseExample: {
      id: "019d77d3-a942-7c91-8e82-ff963ccf1222",
      status: "VALIDATING",
    },
  },
  {
    category: "任务管理",
    method: "POST",
    path: "/api/v1/tasks/{id}/stop",
    title: "停止任务",
    summary: "停止正在运行或恢复中的任务。",
    description: "适合业务系统主动下线转发、录制和桥接任务。",
    successStatus: "202 Accepted",
    params: [authHeaderParam(), idempotencyHeaderParam(), taskIdPathParam()],
    requestExample: {
      headers: {
        Authorization: authHeaderParam().example,
        "Idempotency-Key": "task-stop-019d77d3",
      },
      path: {
        id: taskIdPathParam().example,
      },
      body: {},
    },
    responseExample: {
      id: "019d77d3-a942-7c91-8e82-ff963ccf1222",
      status: "RUNNING",
    },
  },
  {
    category: "任务管理",
    method: "POST",
    path: "/api/v1/tasks/{id}/cancel",
    title: "取消任务",
    summary: "取消尚未完成的任务。",
    description: "适合终止排队、派发和运行中的任务；与 stop 相比，语义更偏向放弃本次执行。",
    successStatus: "202 Accepted",
    params: [authHeaderParam(), idempotencyHeaderParam(), taskIdPathParam()],
    requestExample: {
      headers: {
        Authorization: authHeaderParam().example,
        "Idempotency-Key": "task-cancel-019d77d3",
      },
      path: {
        id: taskIdPathParam().example,
      },
      body: {},
    },
    responseExample: {
      id: "019d77d3-a942-7c91-8e82-ff963ccf1222",
      status: "RUNNING",
    },
  },
  {
    category: "任务管理",
    method: "POST",
    path: "/api/v1/tasks/{id}/retry",
    title: "重试任务",
    summary: "对 FAILED 或 LOST 任务创建新的 Attempt。",
    description: "Task ID 不变，但会创建新的 Attempt 并重新走调度。",
    successStatus: "202 Accepted",
    params: [authHeaderParam(), idempotencyHeaderParam(), taskIdPathParam()],
    requestExample: {
      headers: {
        Authorization: authHeaderParam().example,
        "Idempotency-Key": "task-retry-019d77d3",
      },
      path: {
        id: taskIdPathParam().example,
      },
      body: {},
    },
    responseExample: {
      id: "019d77d3-a942-7c91-8e82-ff963ccf1222",
      status: "VALIDATING",
      current_attempt_no: 2,
    },
  },
  {
    category: "任务管理",
    method: "POST",
    path: "/api/v1/tasks/{id}/recording/start",
    title: "运行中开启录制",
    summary: "对正在运行且已有实时流绑定的流接入任务单独开启录制。",
    description:
      "仅支持实时源 stream_ingest，或已开启播放暴露并已有 ZLM 流绑定的离线流分支。这里的 duration_sec 表示本次录制会话时长，到点只停止录制，不停止任务。",
    successStatus: "202 Accepted",
    params: [authHeaderParam(), taskIdPathParam()],
    requestExample: {
      headers: {
        Authorization: authHeaderParam().example,
      },
      path: {
        id: taskIdPathParam().example,
      },
      body: {
        format: "mp4",
        segment_sec: 300,
        duration_sec: 3600,
        as_player: false,
      },
    },
    responseExample: {
      task_id: "019d77d3-a942-7c91-8e82-ff963ccf1222",
      attempt_no: 1,
      desired_enabled: true,
      recording_state: "requested",
      message: "recording control accepted",
    },
  },
  {
    category: "任务管理",
    method: "POST",
    path: "/api/v1/tasks/{id}/recording/stop",
    title: "运行中关闭录制",
    summary: "对正在运行且已有实时流绑定的流接入任务单独关闭录制，任务继续运行。",
    description: "手动关闭后，断源重连不会自动恢复录制；再次开启需要调用 recording/start。",
    successStatus: "202 Accepted",
    params: [authHeaderParam(), taskIdPathParam()],
    requestExample: {
      headers: {
        Authorization: authHeaderParam().example,
      },
      path: {
        id: taskIdPathParam().example,
      },
      body: {
        reason: "user_requested",
      },
    },
    responseExample: {
      task_id: "019d77d3-a942-7c91-8e82-ff963ccf1222",
      attempt_no: 1,
      desired_enabled: false,
      recording_state: "requested",
      message: "recording control accepted",
    },
  },
  {
    category: "任务管理",
    method: "POST",
    path: "/api/v1/tasks/{id}/clone",
    title: "克隆任务",
    summary: "基于历史任务复制出新任务。",
    description: "适合从稳定配置快速复制新任务，只覆盖少量字段即可。",
    successStatus: "201 Created",
    params: [authHeaderParam(), idempotencyHeaderParam(), taskIdPathParam()],
    requestExample: {
      headers: {
        Authorization: authHeaderParam().example,
        "Idempotency-Key": "task-clone-019d77d3",
      },
      path: {
        id: taskIdPathParam().example,
      },
      body: {
        name: "relay-camera-01-copy",
        priority: 15,
        common: {
          created_by: "bob",
        },
        schedule: {
          start_mode: "manual",
        },
      },
    },
    responseExample: {
      id: "019d77f5-5ff5-7e73-bb1d-0de5fa5f4a2e",
      name: "relay-camera-01-copy",
      type: "stream_ingest",
      status: "CREATED",
      transcode_mode: "none",
      priority: 15,
      created_by: "bob",
      current_attempt_no: 0,
      created_at: "2026-04-12T10:40:00+08:00",
      updated_at: "2026-04-12T10:40:00+08:00",
    },
  },
  {
    category: "运行态查询",
    method: "GET",
    path: "/api/v1/streams",
    title: "查询在线内部流",
    summary: "返回当前在线流、播放地址、观众数和关联任务。",
    description: "适合做流看板、播放器集成和排障。",
    successStatus: "200 OK",
    params: [
      authHeaderParam(),
      {
        name: "schema",
        location: "Query",
        type: "string",
        required: false,
        description: "按流协议过滤。",
        example: "rtsp",
        enumValues: STREAM_SCHEMA_ENUM,
      },
      { name: "app", location: "Query", type: "string", required: false, description: "按内部应用名过滤。", example: "live" },
      { name: "stream", location: "Query", type: "string", required: false, description: "按流名过滤。", example: "camera01" },
      { name: "task_id", location: "Query", type: "string", required: false, description: "按任务 ID 过滤。", example: "019d77d3-a942-7c91-8e82-ff963ccf1222" },
      { name: "node_id", location: "Query", type: "string", required: false, description: "按节点 ID 过滤。", example: "f8996fe7-6a7e-4aa0-8b13-2e8a5f9fdc31" },
      { name: "has_viewer", location: "Query", type: "boolean", required: false, description: "是否有观众。", example: true },
    ],
    requestExample: {
      headers: {
        Authorization: authHeaderParam().example,
      },
      query: {
        app: "live",
        stream: "camera01",
        has_viewer: true,
      },
    },
    responseExample: [
      {
        id: "019d7804-2a3b-70d1-b1cb-2f6c835d4463",
        task_id: "019d77d3-a942-7c91-8e82-ff963ccf1222",
        attempt_id: "019d77d4-1e55-7b16-9f8c-f4b0b59f7c0b",
        attempt_no: 1,
        task_name: "relay-camera-01",
        node_id: "f8996fe7-6a7e-4aa0-8b13-2e8a5f9fdc31",
        schema: "rtsp",
        vhost: "__defaultVhost__",
        app: "live",
        stream: "camera01",
        has_viewer: true,
        viewer_count: 3,
        bitrate_kbps: 2488,
        started_at: "2026-04-12T10:30:08+08:00",
        updated_at: "2026-04-12T10:31:01+08:00",
        play_urls: [
          "rtsp://127.0.0.1/live/camera01",
          "rtmp://127.0.0.1/live/camera01",
          "http://127.0.0.1/live/camera01.live.flv",
          "http://127.0.0.1/live/camera01.live.mp4",
        ],
      },
    ],
  },
  {
    category: "运行态查询",
    method: "GET",
    path: "/api/v1/records",
    title: "查询录像记录",
    summary: "返回录像索引、文件大小、时长和 HTTP 地址。",
    description: "适合检索实时录制产生的录像、做回看和路径回传。HLS 录制按 `m3u8` 播放列表展示，不展开底层 `ts` segment；仅用于实时播放的 HLS 文件不会落在这里。VOD 快录输出不会进入该接口，而是进入文件产物接口。",
    successStatus: "200 OK",
    params: [
      authHeaderParam(),
      { name: "task_id", location: "Query", type: "string", required: false, description: "按任务过滤。", example: "019d77d3-a942-7c91-8e82-ff963ccf1222" },
      { name: "stream", location: "Query", type: "string", required: false, description: "按流名过滤。", example: "camera01" },
      { name: "date_from", location: "Query", type: "RFC3339 datetime", required: false, description: "起始时间。", example: "2026-04-12T00:00:00+08:00" },
      { name: "date_to", location: "Query", type: "RFC3339 datetime", required: false, description: "结束时间。", example: "2026-04-12T23:59:59+08:00" },
      { name: "page", location: "Query", type: "integer", required: false, description: "页码。", example: 1 },
      { name: "page_size", location: "Query", type: "integer", required: false, description: "每页条数。", example: 20 },
    ],
    requestExample: {
      headers: {
        Authorization: authHeaderParam().example,
      },
      query: {
        task_id: "019d77d3-a942-7c91-8e82-ff963ccf1222",
        stream: "camera01",
        date_from: "2026-04-12T00:00:00+08:00",
        date_to: "2026-04-12T23:59:59+08:00",
      },
    },
    responseExample: {
      items: [
        {
          id: "019d7810-38a9-7489-8949-b7db868f39fd",
          task_id: "019d77d3-a942-7c91-8e82-ff963ccf1222",
          attempt_id: "019d77d4-1e55-7b16-9f8c-f4b0b59f7c0b",
          vhost: "__defaultVhost__",
          app: "live",
          stream: "camera01",
          file_path: "/data/zlm/www/record/project-a/camera01-001.mp4",
          http_url: "http://127.0.0.1/record/project-a/camera01-001.mp4",
          file_size: 45812931,
          time_len: 300,
          start_time: "2026-04-12T10:30:09+08:00",
          source: "zlm_mp4",
          created_at: "2026-04-12T10:35:09+08:00",
        },
      ],
      page: 1,
      page_size: 20,
      total: 1,
    },
  },
  {
    category: "运行态查询",
    method: "GET",
    path: "/api/v1/file-artifacts",
    title: "查询文件产物",
    summary: "返回桥接输出、转码输出和流接入快录的托管文件产物。",
    description: "适合统一查看平台托管的文件输出、HTTP 地址和节点落盘路径。VOD 快录不会进入录像中心，而是落在这里。",
    successStatus: "200 OK",
    params: [
      authHeaderParam(),
      { name: "artifact_kind", location: "Query", type: "string", required: false, description: "按文件产物类型过滤。", example: "bridge_output", enumValues: FILE_ARTIFACT_KIND_ENUM },
      { name: "task_id", location: "Query", type: "string", required: false, description: "按任务过滤。", example: "019d7904-7e1b-7a0e-9b64-2dc0e50ae8d1" },
      { name: "date_from", location: "Query", type: "RFC3339 datetime", required: false, description: "起始时间。", example: "2026-04-12T00:00:00+08:00" },
      { name: "date_to", location: "Query", type: "RFC3339 datetime", required: false, description: "结束时间。", example: "2026-04-12T23:59:59+08:00" },
      { name: "page", location: "Query", type: "integer", required: false, description: "页码。", example: 1 },
      { name: "page_size", location: "Query", type: "integer", required: false, description: "每页条数。", example: 20 },
    ],
    requestExample: {
      headers: {
        Authorization: authHeaderParam().example,
      },
      query: {
        artifact_kind: "bridge_output",
        task_id: "019d7904-7e1b-7a0e-9b64-2dc0e50ae8d1",
        date_from: "2026-04-12T00:00:00+08:00",
        date_to: "2026-04-12T23:59:59+08:00",
      },
    },
    responseExample: {
      items: [
        {
          id: "019d7908-fd70-7fe9-a9c3-8815dcc5ec83",
          artifact_kind: "bridge_output",
          task_id: "019d7904-7e1b-7a0e-9b64-2dc0e50ae8d1",
          attempt_id: "019d7904-8db6-78f0-903b-3ec2d1f2c7e6",
          node_id: "f8996fe7-6a7e-4aa0-8b13-2e8a5f9fdc31",
          file_name: "120000.mp4",
          file_path: "/data/zlm/www/artifacts/bridge/2026/04/12/120000.mp4",
          http_url: "http://127.0.0.1/artifacts/bridge/2026/04/12/120000.mp4",
          file_size: 245812931,
          created_at: "2026-04-12T12:00:00+08:00",
        },
      ],
      page: 1,
      page_size: 20,
      total: 1,
    },
    notes: [
      "文件输出路径由平台托管生成，不再支持业务系统通过 publish.url 自定义目录或文件名。",
    ],
  },
  {
    category: "任务管理",
    method: "POST",
    path: "/api/v1/uploads/media",
    title: "上传点播媒资",
    summary: "上传单个视频文件，由 Core 选择 Agent 落盘并返回 file 输入路径。",
    description: "适合业务系统或控制台手动上传点播媒资。文件由 Agent 保存到上传根目录，返回的 sourceUrl 可作为后续任务 input.kind=file 的 input.url。",
    successStatus: "200 OK",
    params: [
      authHeaderParam(),
      { name: "Content-Type", location: "Header", type: "multipart/form-data", required: true, description: "必须使用 multipart/form-data。", example: "multipart/form-data; boundary=..." },
      { name: "node_id", location: "Query", type: "string", required: false, description: "指定上传落盘节点 UUID。", example: "cc74d485-3ff7-41fa-bd58-bdce7b42e81c" },
      { name: "required_labels", location: "Query", type: "string", required: false, description: "上传节点必须具备的标签，多个标签用英文逗号分隔。", example: "objective,room-a" },
    ],
    requestFields: [
      { path: "file", type: "file", required: true, description: "单个视频文件。", example: "origin.mp4" },
    ],
    responseFields: [
      { path: "id", type: "string", description: "上传 ID。", example: "019dd784-e69c-7db1-869a-9ee97226a427" },
      { path: "fileName", type: "string", description: "原始文件名。", example: "origin.mp4" },
      { path: "sourceUrl", type: "string", description: "相对文件路径，固定包含落盘节点 ID。", example: "uploads/cc74d485-3ff7-41fa-bd58-bdce7b42e81c/2026/04/29/019dd784-e69c-7db1-869a-9ee97226a427.mp4" },
      { path: "httpUrl", type: "string", description: "用于预览、下载和排查的 HTTP 地址。", example: "http://172.17.13.196:8081/media/uploads/cc74d485-3ff7-41fa-bd58-bdce7b42e81c/2026/04/29/019dd784-e69c-7db1-869a-9ee97226a427.mp4" },
      { path: "durationSec", type: "integer", description: "Agent 探测到的时长秒数；探测失败返回 0。", example: 604 },
      { path: "fileSize", type: "integer", description: "文件大小，单位字节。", example: 328172988 },
      { path: "sha256", type: "string", description: "文件 SHA-256。", example: "a2b7b0f58d3e558bbd2d4a43494d9728b059f7305af6731380fc0450b1f6cd79" },
      { path: "contentType", type: "string", description: "内容类型。", example: "video/mp2t" },
      { path: "createdAt", type: "integer", description: "创建时间戳，单位毫秒。", example: 1777437304476 },
    ],
    requestExample: {
      headers: {
        Authorization: authHeaderParam().example,
        "Content-Type": "multipart/form-data",
      },
      formData: {
        file: "@origin.mp4",
      },
    },
    responseExample: {
      id: "019dd784-e69c-7db1-869a-9ee97226a427",
      fileName: "origin.mp4",
      sourceUrl: "uploads/cc74d485-3ff7-41fa-bd58-bdce7b42e81c/2026/04/29/019dd784-e69c-7db1-869a-9ee97226a427.mp4",
      httpUrl: "http://172.17.13.196:8081/media/uploads/cc74d485-3ff7-41fa-bd58-bdce7b42e81c/2026/04/29/019dd784-e69c-7db1-869a-9ee97226a427.mp4",
      durationSec: 604,
      fileSize: 328172988,
      sha256: "a2b7b0f58d3e558bbd2d4a43494d9728b059f7305af6731380fc0450b1f6cd79",
      contentType: "video/mp2t",
      createdAt: 1777437304476,
    },
    notes: [
      "Core 使用节点注册上报的 agent_http_base_url 代理到 Agent，不再配置上传地址模板。",
      "未指定 node_id 时，Core 会在满足标签和健康条件的节点中优先选择上传盘剩余空间更大的节点。",
      "Agent 的 ffprobe 时长探测为尽力而为；探测失败不阻断上传，durationSec 返回 0。",
      "上传成功后 Core 会写入上传产物台账，可通过 GET /api/v1/uploads/media 查询。",
      "后续任务调度会从 sourceUrl 的 uploads/<node_id>/... 解析落盘节点并做节点亲和。",
    ],
  },
  {
    category: "任务管理",
    method: "GET",
    path: "/api/v1/uploads/media",
    title: "查询上传产物",
    summary: "分页查询手动上传媒资台账。",
    description: "用于控制台选择 file 输入、排查上传结果和执行台账删除。",
    successStatus: "200 OK",
    params: [
      authHeaderParam(),
      { name: "status", location: "Query", type: "string", required: false, description: "状态过滤：active、deleted 或 all，默认 active。", example: "active" },
      { name: "node_id", location: "Query", type: "string", required: false, description: "按落盘节点过滤。", example: "cc74d485-3ff7-41fa-bd58-bdce7b42e81c" },
      { name: "keyword", location: "Query", type: "string", required: false, description: "按文件名、Source URL 或 SHA-256 模糊查询。", example: "origin" },
      { name: "page", location: "Query", type: "integer", required: false, description: "页码，默认 1。", example: 1 },
      { name: "page_size", location: "Query", type: "integer", required: false, description: "每页数量，默认 20。", example: 20 },
    ],
    requestExample: {
      headers: { Authorization: authHeaderParam().example },
      query: { status: "active", page_size: 20 },
    },
    responseExample: {
      items: [
        {
          id: "019dd784-e69c-7db1-869a-9ee97226a427",
          node_id: "cc74d485-3ff7-41fa-bd58-bdce7b42e81c",
          node_name: "node-1",
          file_name: "origin.mp4",
          source_url: "uploads/cc74d485-3ff7-41fa-bd58-bdce7b42e81c/2026/04/29/019dd784-e69c-7db1-869a-9ee97226a427.mp4",
          http_url: "http://172.17.13.196:8081/media/uploads/cc74d485-3ff7-41fa-bd58-bdce7b42e81c/2026/04/29/019dd784-e69c-7db1-869a-9ee97226a427.mp4",
          duration_sec: 604,
          file_size: 328172988,
          sha256: "a2b7b0f58d3e558bbd2d4a43494d9728b059f7305af6731380fc0450b1f6cd79",
          content_type: "video/mp2t",
          status: "active",
          file_deleted: false,
          created_at: "2026-04-29T10:15:04Z",
        },
      ],
      page: 1,
      page_size: 20,
      total: 1,
    },
  },
  {
    category: "任务管理",
    method: "DELETE",
    path: "/api/v1/uploads/media/{id}",
    title: "删除上传产物",
    summary: "删除上传产物台账，可选同步删除 Agent 底层文件。",
    description: "delete_file=true 会删除底层文件，可能影响外部业务系统、历史任务和预览地址。",
    successStatus: "200 OK",
    params: [
      authHeaderParam(),
      { name: "id", location: "Path", type: "string", required: true, description: "上传产物 ID。", example: "019dd784-e69c-7db1-869a-9ee97226a427" },
      { name: "delete_file", location: "Query", type: "boolean", required: false, description: "是否同步删除底层文件，默认 false。", example: false },
    ],
    requestExample: {
      headers: { Authorization: authHeaderParam().example },
      query: { delete_file: false },
    },
    responseExample: {
      id: "019dd784-e69c-7db1-869a-9ee97226a427",
      status: "deleted",
      file_deleted: false,
    },
  },
  {
    category: "节点与安全",
    method: "GET",
    path: "/api/v1/nodes",
    title: "查询节点摘要",
    summary: "返回节点健康、能力摘要、当前负载和 ZLM/FFmpeg 状态。",
    description: "适合做资源调度可视化和运维观测。",
    successStatus: "200 OK",
    params: [authHeaderParam()],
    requestExample: {
      headers: {
        Authorization: authHeaderParam().example,
      },
    },
    responseExample: [
      {
        id: "f8996fe7-6a7e-4aa0-8b13-2e8a5f9fdc31",
        node_name: "node-1",
        hostname: "archlinux-yyzy",
        labels: ["beijing-idc", "archive"],
        network_mode: "host",
        interfaces: ["eth0", "eth1"],
        healthy: true,
        last_seen_at: "2026-04-12T22:57:32+08:00",
        ffmpeg_protocols: ["rtsp", "rtmp", "http", "file"],
        ffmpeg_formats: ["mpegts", "flv", "mp4"],
        ffmpeg_encoders: ["libx264", "aac"],
        ffmpeg_decoders: ["h264", "aac"],
        gpu: [],
        slot_usage: 0,
        running_tasks: 0,
        cpu_percent: 7.7,
        mem_percent: 15.9,
        disk_percent: 22.1,
        upload_disk_total_bytes: 107374182400,
        upload_disk_available_bytes: 85899345920,
        upload_disk_used_percent: 20.0,
        zlm_alive: true,
        ffmpeg_alive: true,
      },
    ],
  },
  {
    category: "节点与安全",
    method: "GET",
    path: "/api/v1/nodes/{id}/heartbeats",
    title: "查询节点心跳历史",
    summary: "返回指定节点最近的心跳样本。",
    description: "适合观察节点负载波动、运行任务数和心跳稳定性。",
    successStatus: "200 OK",
    params: [
      authHeaderParam(),
      nodeIdPathParam(),
      { name: "limit", location: "Query", type: "integer", required: false, description: "默认 24，最大 200。", example: 24 },
    ],
    requestExample: {
      headers: {
        Authorization: authHeaderParam().example,
      },
      path: {
        id: nodeIdPathParam().example,
      },
      query: {
        limit: 24,
      },
    },
    responseExample: [
      {
        node_id: "f8996fe7-6a7e-4aa0-8b13-2e8a5f9fdc31",
        cpu_percent: 7.7,
        mem_percent: 15.9,
        disk_percent: 22.1,
        upload_disk_total_bytes: 107374182400,
        upload_disk_available_bytes: 85899345920,
        upload_disk_used_percent: 20.0,
        running_tasks: 0,
        slot_usage: 0,
        zlm_alive: true,
        ffmpeg_alive: true,
        gpu_runtime: [],
        node_time: "2026-04-12T22:57:32+08:00",
        received_at: "2026-04-12T22:57:32+08:00",
      },
    ],
  },
  {
    category: "节点与安全",
    method: "GET",
    path: "/api/v1/security/machine-allowlist",
    title: "查询机器 API 白名单",
    summary: "返回当前机器 API 的 CIDR 白名单。",
    description: "适合安全页展示和审计。",
    successStatus: "200 OK",
    params: [authHeaderParam()],
    requestExample: {
      headers: {
        Authorization: authHeaderParam().example,
      },
    },
    responseExample: {
      entries: [
        {
          id: "019d7a01-0f87-7d8a-a2f3-58d35b0d5c7b",
          cidr: "10.0.0.0/24",
          description: "内网调度系统",
          created_at: "2026-04-12T09:00:00+08:00",
          updated_at: "2026-04-12T09:00:00+08:00",
        },
      ],
    },
  },
  {
    category: "节点与安全",
    method: "PUT",
    path: "/api/v1/security/machine-allowlist",
    title: "更新机器 API 白名单",
    summary: "整体替换白名单条目。",
    description: "适合控制台安全页保存配置；服务端会按请求体中提供的 entries 作为新的完整白名单。",
    successStatus: "200 OK",
    params: [authHeaderParam(), idempotencyHeaderParam()],
    requestExample: {
      headers: {
        Authorization: authHeaderParam().example,
        "Idempotency-Key": "allowlist-update-20260412",
      },
      body: {
        entries: [
          {
            cidr: "10.0.0.0/24",
            description: "内网调度系统",
          },
          {
            cidr: "192.168.10.5/32",
            description: "单机运维出口",
          },
        ],
      },
    },
    responseExample: {
      entries: [
        {
          id: "019d7a01-0f87-7d8a-a2f3-58d35b0d5c7b",
          cidr: "10.0.0.0/24",
          description: "内网调度系统",
          created_at: "2026-04-12T09:00:00+08:00",
          updated_at: "2026-04-12T09:00:00+08:00",
        },
        {
          id: "019d7a02-b1f8-7821-bd84-dc2c5e3e9f55",
          cidr: "192.168.10.5/32",
          description: "单机运维出口",
          created_at: "2026-04-12T09:05:00+08:00",
          updated_at: "2026-04-12T09:05:00+08:00",
        },
      ],
    },
  },
  {
    category: "管理员调试",
    method: "GET",
    path: "/api/v1/debug/zlm/media",
    title: "查询媒体列表",
    summary: "按节点透传封装后的 ZLM getMediaList 结果。",
    description: "适合管理员排查内部流当前是否真正在线、协议和观众数是否符合预期。",
    successStatus: "200 OK",
    params: [
      authHeaderParam(),
      nodeIdQueryParam(),
      {
        name: "schema",
        location: "Query",
        type: "string",
        required: false,
        description: "协议过滤。",
        example: "rtsp",
        enumValues: STREAM_SCHEMA_ENUM,
      },
      { name: "vhost", location: "Query", type: "string", required: false, description: "vhost 过滤。", example: "__defaultVhost__" },
      { name: "app", location: "Query", type: "string", required: false, description: "应用名过滤。", example: "live" },
      { name: "stream", location: "Query", type: "string", required: false, description: "流名过滤。", example: "camera01" },
    ],
    requestExample: {
      headers: { Authorization: authHeaderParam().example },
      query: {
        node_id: nodeIdQueryParam().example,
        schema: "rtsp",
        vhost: "__defaultVhost__",
        app: "live",
        stream: "camera01",
      },
    },
    responseExample: {
      data: [
        {
          schema: "rtsp",
          vhost: "__defaultVhost__",
          app: "live",
          stream: "camera01",
          originType: 7,
          totalReaderCount: 3,
          bytesSpeed: 311040,
        },
      ],
    },
  },
  {
    category: "管理员调试",
    method: "GET",
    path: "/api/v1/debug/zlm/sessions",
    title: "查询全部会话",
    summary: "按节点透传封装后的 ZLM getAllSession 结果。",
    description: "适合排查连接数、来源 IP 和协议占用情况。",
    successStatus: "200 OK",
    params: [authHeaderParam(), nodeIdQueryParam()],
    requestExample: {
      headers: { Authorization: authHeaderParam().example },
      query: {
        node_id: nodeIdQueryParam().example,
      },
    },
    responseExample: {
      data: [
        {
          id: "123456",
          peer_ip: "10.0.0.8",
          local_port: 554,
          peer_port: 54218,
          typeid: "TcpSession",
        },
      ],
    },
  },
  {
    category: "管理员调试",
    method: "GET",
    path: "/api/v1/debug/zlm/players",
    title: "查询播放器列表",
    summary: "按节点透传封装后的 ZLM getMediaPlayerList 结果。",
    description: "适合确认某条流现在到底有没有播放器、播放器来自哪里。",
    successStatus: "200 OK",
    params: [authHeaderParam(), nodeIdQueryParam()],
    requestExample: {
      headers: { Authorization: authHeaderParam().example },
      query: {
        node_id: nodeIdQueryParam().example,
      },
    },
    responseExample: {
      data: [
        {
          app: "live",
          stream: "camera01",
          schema: "rtsp",
          client_ip: "10.0.0.8",
        },
      ],
    },
  },
  {
    category: "管理员调试",
    method: "GET",
    path: "/api/v1/debug/zlm/statistic",
    title: "查询 ZLM 统计",
    summary: "按节点透传封装后的 ZLM getStatistic 结果。",
    description: "适合判断对象数量、连接数和线程负载。",
    successStatus: "200 OK",
    params: [authHeaderParam(), nodeIdQueryParam()],
    requestExample: {
      headers: { Authorization: authHeaderParam().example },
      query: {
        node_id: nodeIdQueryParam().example,
      },
    },
    responseExample: {
      data: {
        player: 3,
        media: 1,
        session: 6,
      },
    },
  },
  {
    category: "管理员调试",
    method: "GET",
    path: "/api/v1/debug/zlm/threads-load",
    title: "查询前台线程负载",
    summary: "按节点透传封装后的 ZLM getThreadsLoad 结果。",
    description: "适合观察前台线程是否过载。",
    successStatus: "200 OK",
    params: [authHeaderParam(), nodeIdQueryParam()],
    requestExample: {
      headers: { Authorization: authHeaderParam().example },
      query: {
        node_id: nodeIdQueryParam().example,
      },
    },
    responseExample: {
      data: {
        delay: 2,
        load: [0.04, 0.06, 0.05],
      },
    },
  },
  {
    category: "管理员调试",
    method: "GET",
    path: "/api/v1/debug/zlm/work-threads-load",
    title: "查询后台线程负载",
    summary: "按节点透传封装后的 ZLM getWorkThreadsLoad 结果。",
    description: "适合判断后台工作线程是否成为瓶颈。",
    successStatus: "200 OK",
    params: [authHeaderParam(), nodeIdQueryParam()],
    requestExample: {
      headers: { Authorization: authHeaderParam().example },
      query: {
        node_id: nodeIdQueryParam().example,
      },
    },
    responseExample: {
      data: {
        delay: 1,
        load: [0.02, 0.03],
      },
    },
  },
  {
    category: "管理员调试",
    method: "POST",
    path: "/api/v1/debug/zlm/kick-session",
    title: "踢掉单个会话",
    summary: "按 session_id 断开单个 ZLM 会话。",
    description: "适合断开异常播放器或可疑连接。",
    successStatus: "204 No Content",
    params: [authHeaderParam(), idempotencyHeaderParam()],
    requestExample: {
      headers: {
        Authorization: authHeaderParam().example,
        "Idempotency-Key": "kick-session-123456",
      },
      body: {
        node_id: "f8996fe7-6a7e-4aa0-8b13-2e8a5f9fdc31",
        session_id: "123456",
      },
    },
    responseExample: null,
  },
  {
    category: "管理员调试",
    method: "POST",
    path: "/api/v1/debug/zlm/kick-sessions",
    title: "批量踢会话",
    summary: "按本地端口或对端 IP 批量断开会话。",
    description: "适合断开一批异常连接；至少提供一个过滤条件更有意义。",
    successStatus: "204 No Content",
    params: [authHeaderParam(), idempotencyHeaderParam()],
    requestExample: {
      headers: {
        Authorization: authHeaderParam().example,
        "Idempotency-Key": "kick-sessions-554",
      },
      body: {
        node_id: "f8996fe7-6a7e-4aa0-8b13-2e8a5f9fdc31",
        local_port: 554,
        peer_ip: "10.0.0.8",
      },
    },
    responseExample: null,
  },
  {
    category: "管理员调试",
    method: "POST",
    path: "/api/v1/debug/zlm/close-stream",
    title: "关闭内部流",
    summary: "按 schema/vhost/app/stream 关闭指定内部流。",
    description: "适合管理员强制下线一条内部流或做排障清理。",
    successStatus: "204 No Content",
    params: [authHeaderParam(), idempotencyHeaderParam()],
    requestExample: {
      headers: {
        Authorization: authHeaderParam().example,
        "Idempotency-Key": "close-stream-camera01",
      },
      body: {
        node_id: "f8996fe7-6a7e-4aa0-8b13-2e8a5f9fdc31",
        schema: "rtsp",
        vhost: "__defaultVhost__",
        app: "live",
        stream: "camera01",
        force: true,
      },
    },
    responseExample: null,
  },
  {
    category: "管理员调试",
    method: "GET",
    path: "/api/v1/debug/zlm/snap",
    title: "远程抓图",
    summary: "让节点对指定媒体地址执行抓图并返回 data URL。",
    description: "适合确认输入源是否可达，以及截图是否符合预期。",
    successStatus: "200 OK",
    params: [
      authHeaderParam(),
      nodeIdQueryParam(),
      { name: "url", location: "Query", type: "string", required: true, description: "要截图的媒体地址。", example: "rtsp://127.0.0.1/live/camera01" },
      { name: "timeout_sec", location: "Query", type: "integer", required: false, description: "超时时间，默认 10。", example: 10 },
      { name: "expire_sec", location: "Query", type: "integer", required: false, description: "截图缓存保留时间，默认 30。", example: 30 },
    ],
    requestExample: {
      headers: { Authorization: authHeaderParam().example },
      query: {
        node_id: nodeIdQueryParam().example,
        url: "rtsp://127.0.0.1/live/camera01",
        timeout_sec: 10,
        expire_sec: 30,
      },
    },
    responseExample: {
      content_type: "image/jpeg",
      data_url: "data:image/jpeg;base64,/9j/4AAQSkZJRgABAQAAAQABAAD...",
    },
  },
  {
    category: "管理员调试",
    method: "GET",
    path: "/api/v1/debug/hooks",
    title: "查询 Hook 时间线",
    summary: "返回节点最近收到的 Hook 事件。",
    description: "适合排查录像回调、流上线/下线和去重行为。",
    successStatus: "200 OK",
    params: [
      authHeaderParam(),
      { name: "node_id", location: "Query", type: "string", required: false, description: "可选，按节点过滤。", example: nodeIdQueryParam().example },
    ],
    requestExample: {
      headers: { Authorization: authHeaderParam().example },
      query: {
        node_id: nodeIdQueryParam().example,
      },
    },
    responseExample: [
      {
        id: "019d7a8c-f66e-7a5a-90d9-c67c5e31c511",
        server_id: "node-1",
        hook_name: "on_record_mp4",
        dedup_key: "node-1:on_record_mp4:camera01:20260412",
        payload: {
          app: "live",
          stream: "camera01",
          file_path: "/data/zlm/www/record/project-a/camera01-001.mp4",
        },
        created_at: "2026-04-12T10:35:09+08:00",
      },
    ],
  },
];

export const externalApiDocs: ExternalApiDoc[] = baseExternalApiDocs.map((doc) => ({
  ...doc,
  requestFields: buildBodyFields(doc.requestExample),
  responseFields: buildResponseFields(doc.responseExample),
}));
