const REFRESH_TOKEN_STORAGE_KEY = "streamserver.console.refresh_token";
const THEME_STORAGE_KEY = "streamserver.console.theme";
const AUTO_REFRESH_MS = 10000;
const THEME_OPTIONS = ["system", "light", "dark"];
const DEFAULT_PAGE_SIZE = "20";

const NAV_ITEMS = [
  {
    path: "/overview",
    label: "系统总览",
    note: "系统介绍、整体状态、节点负载",
    permission: null,
  },
  {
    path: "/api-docs",
    label: "外部 API 文档",
    note: "第三方业务系统对接说明与示例",
    permission: null,
  },
  {
    path: "/tasks",
    label: "任务中心",
    note: "创建、筛选、派发、重试",
    permission: "task_read",
  },
  {
    path: "/streams",
    label: "流中心",
    note: "在线流、播放地址、关闭流",
    permission: "task_read",
  },
  {
    path: "/multicast",
    label: "组播中心",
    note: "组播任务、网卡、TTL、上下游",
    permission: "task_read",
  },
  {
    path: "/records",
    label: "录像中心",
    note: "录像索引、日期检索、路径复制",
    permission: "record_read",
  },
  {
    path: "/transcode-artifacts",
    label: "转码产物",
    note: "离线转码结果、HTTP 地址、文件路径",
    permission: "record_read",
  },
  {
    path: "/security",
    label: "安全设置",
    note: "修改密码、维护机器 API 白名单",
    permission: "security_write",
  },
  {
    path: "/nodes",
    label: "节点中心",
    note: "节点健康、能力矩阵、当前负载",
    permission: "node_read",
  },
  {
    path: "/debug",
    label: "调试台",
    note: "ZLM 原始调试、会话、踢人、关流",
    permission: "debug_read",
  },
];

const TASK_TYPES = [
  { value: "live_relay", label: "实时拉流转发", note: "拉取实时源并发布为平台流" },
  { value: "file_transcode", label: "文件转码", note: "离线转码并生成目标文件" },
  { value: "file_to_live", label: "文件转直播", note: "把文件实时推送为直播流" },
  { value: "multicast_bridge", label: "组播桥接", note: "组播与平台流互转" },
  { value: "rtp_receive", label: "RTP 接收", note: "接收国标/RTP 并发布为内部流" },
];

const INPUT_KINDS = [
  "rtsp",
  "rtmp",
  "hls",
  "http_mp4",
  "http_flv",
  "http_ts",
  "file",
  "udp_mpegts_multicast",
  "rtp_multicast",
  "gb_rtp",
];

const PUBLISH_KINDS = ["file", "zlm_ingest", "udp_mpegts_multicast", "rtp_multicast"];
const START_MODES = ["immediate", "manual", "cron", "at"];
const RECORD_FORMATS = ["mp4", "hls", "both"];
const RECOVERY_POLICIES = ["never", "on_failure", "always"];
const PROFILE_OPTIONS = [
  "",
  "realtime_compat",
  "archive_quality",
  "multicast_ts",
  "rtmp_hevc_ext",
];
const API_EXAMPLE_BASE_URL = "http://media-core.example.com:8080";
const EXAMPLE_TASK_ID = "019d77d3-a942-7c91-8e82-ff963ccf1222";
const EXAMPLE_NODE_ID = "f8995794-b5af-440b-b1f4-742c6c7f1641";
const EXAMPLE_AUTHORIZATION = "Bearer eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.demo.signature";

function authHeaderParam() {
  return {
    name: "Authorization",
    location: "Header",
    type: "string",
    required: "按环境",
    description: "Bearer 访问令牌。当前环境启用鉴权时必须携带，用于标识调用方身份并校验权限范围。",
    example: EXAMPLE_AUTHORIZATION,
  };
}

function idempotencyHeaderParam() {
  return {
    name: "Idempotency-Key",
    location: "Header",
    type: "string",
    required: true,
    description: "任务创建幂等键。业务系统重复提交同一业务请求时，服务端返回同一任务结果，避免重复建单。",
    example: "task-create-relay-camera-01-20260411",
  };
}

function taskIdPathParam() {
  return {
    name: "id",
    location: "Path",
    type: "string",
    required: true,
    description: "任务 ID。由创建任务接口返回，后续查询详情、启动、停止、重试和克隆都依赖该值。",
    example: EXAMPLE_TASK_ID,
  };
}

function nodeIdPathParam() {
  return {
    name: "id",
    location: "Path",
    type: "string",
    required: true,
    description: "节点 ID。可先通过查询节点列表接口获取，再用于查看该节点的心跳与负载历史。",
    example: EXAMPLE_NODE_ID,
  };
}

const OVERVIEW_FEATURES = [
  {
    title: "任务编排",
    description: "统一管理实时拉流、文件转码、文件转直播、组播桥接和 RTP 接收任务。",
  },
  {
    title: "流媒体分发",
    description: "对外提供 RTSP、RTMP、HTTP-TS、HTTP-FMP4 等多种播放协议。",
  },
  {
    title: "录像索引",
    description: "集中展示录像文件、检索时间范围，并快速回溯关联任务与流。",
  },
  {
    title: "节点观测",
    description: "汇总节点健康、CPU、内存、磁盘、ZLM 与 FFmpeg 运行状态。",
  },
];
const EXTERNAL_API_DOCS = [
  {
    category: "任务管理",
    method: "POST",
    path: "/api/v1/tasks",
    title: "创建任务",
    summary: "创建新的业务任务，支持幂等键。",
    description: "适合第三方业务系统发起实时转发、转码、直播和组播类任务。请求头需要提供 `Idempotency-Key`。",
    params: [
      authHeaderParam(),
      idempotencyHeaderParam(),
    ],
    responseExample: { id: "0195...", status: "RUNNING", assigned_node_id: "f899..." },
  },
  {
    category: "任务管理",
    method: "GET",
    path: "/api/v1/tasks",
    title: "查询任务列表",
    summary: "按状态、类型、节点、时间等条件查询任务。",
    description: "适合做任务看板、工单回查和批量状态同步。",
    params: [
      authHeaderParam(),
      { name: "status", location: "Query", type: "enum", required: false, description: "任务状态筛选。适合只同步运行中、失败或待处理任务。 ", example: "RUNNING" },
      { name: "type", location: "Query", type: "enum", required: false, description: "任务类型筛选。适合按业务场景拆分任务列表。", example: "live_relay" },
      { name: "assigned_node_id", location: "Query", type: "string", required: false, description: "执行节点 ID。用于查看某个工作节点承担的任务。", example: EXAMPLE_NODE_ID },
      { name: "keyword", location: "Query", type: "string", required: false, description: "任务名或任务 ID 关键字，用于模糊检索。", example: "camera-01" },
      { name: "created_from", location: "Query", type: "datetime", required: false, description: "创建时间起点，ISO 8601。用于限定查询窗口。", example: "2026-04-11T00:00:00+08:00" },
      { name: "created_to", location: "Query", type: "datetime", required: false, description: "创建时间终点，ISO 8601。", example: "2026-04-11T23:59:59+08:00" },
      { name: "page", location: "Query", type: "number", required: false, description: "页码，从 1 开始。", example: 1 },
      { name: "page_size", location: "Query", type: "number", required: false, description: "每页条数。", example: 20 },
      { name: "sort_by", location: "Query", type: "enum", required: false, description: "排序字段。", example: "updated_at" },
      { name: "sort_order", location: "Query", type: "enum", required: false, description: "排序方向。", example: "desc" },
    ],
    responseExample: { items: [{ id: "0195...", name: "relay-camera-01", status: "RUNNING" }], total: 1, page: 1, page_size: 20 },
  },
  {
    category: "任务管理",
    method: "GET",
    path: "/api/v1/tasks/{id}",
    title: "查询任务详情",
    summary: "返回任务主信息、当前 attempt、最近事件和规格。",
    description: "适合做任务详情页、异常排障和状态同步。",
    params: [
      authHeaderParam(),
      taskIdPathParam(),
    ],
    responseExample: { task: { id: "0195...", status: "RUNNING", type: "live_relay" }, callback_delivery: { event_type: "task.status", status: "delivered", callback_url: "https://biz.example.com/streamserver/callback" }, recent_events: [] },
  },
  {
    category: "任务管理",
    method: "POST",
    path: "/api/v1/tasks/{id}/start",
    title: "启动任务",
    summary: "启动已创建、失败或已取消的任务。",
    description: "适合人工审核后启动或失败重启场景。",
    params: [
      authHeaderParam(),
      taskIdPathParam(),
    ],
    responseExample: { id: "0195...", status: "RUNNING" },
  },
  {
    category: "任务管理",
    method: "POST",
    path: "/api/v1/tasks/{id}/stop",
    title: "停止任务",
    summary: "停止运行中的任务。",
    description: "适合业务系统主动下线转发、录像或组播桥接任务。",
    params: [
      authHeaderParam(),
      taskIdPathParam(),
    ],
    responseExample: { id: "0195...", status: "STOPPING" },
  },
  {
    category: "任务管理",
    method: "POST",
    path: "/api/v1/tasks/{id}/retry",
    title: "重试任务",
    summary: "为失败或丢失的任务创建新 attempt。",
    description: "任务 ID 不变，便于外部系统维持同一业务单据。",
    params: [
      authHeaderParam(),
      taskIdPathParam(),
    ],
    responseExample: { attempt_no: 2, status: "QUEUED" },
  },
  {
    category: "任务管理",
    method: "POST",
    path: "/api/v1/tasks/{id}/clone",
    title: "克隆任务",
    summary: "复制既有任务并按需覆盖少量字段。",
    description: "适合快速复制既有模板化任务。",
    params: [
      authHeaderParam(),
      taskIdPathParam(),
    ],
    responseExample: { id: "0196...", status: "CREATED", name: "relay-camera-01-copy" },
  },
  {
    category: "运行观察",
    method: "GET",
    path: "/api/v1/streams",
    title: "查询在线流",
    summary: "查看在线流、观众数和播放地址。",
    description: "适合业务系统展示当前在线流状态和播放链接。",
    params: [
      authHeaderParam(),
      { name: "schema", location: "Query", type: "enum", required: false, description: "按输出协议过滤。适合区分 RTSP、RTMP、HLS 等分发结果。", example: "rtsp" },
      { name: "app", location: "Query", type: "string", required: false, description: "按应用名过滤。", example: "live" },
      { name: "stream", location: "Query", type: "string", required: false, description: "按流名过滤。", example: "camera01" },
      { name: "node_id", location: "Query", type: "string", required: false, description: "按节点过滤，用于查看某个工作节点上的在线流。", example: EXAMPLE_NODE_ID },
      { name: "has_viewer", location: "Query", type: "boolean", required: false, description: "按是否存在观众过滤。", example: true },
      { name: "task_id", location: "Query", type: "string", required: false, description: "按任务 ID 过滤。", example: EXAMPLE_TASK_ID },
    ],
    responseExample: [{ task_id: "0195...", app: "live", stream: "camera01", play_urls: ["rtmp://example/live/camera01"] }],
  },
  {
    category: "运行观察",
    method: "GET",
    path: "/api/v1/records",
    title: "查询录像文件",
    summary: "按任务、流名和时间范围检索录像文件。",
    description: "适合业务系统做录像浏览、回放索引和审计留痕。",
    params: [
      authHeaderParam(),
      { name: "task_id", location: "Query", type: "string", required: false, description: "按任务 ID 过滤，用于回看某个业务任务的录像输出。", example: EXAMPLE_TASK_ID },
      { name: "stream", location: "Query", type: "string", required: false, description: "按流名过滤。", example: "camera01" },
      { name: "date_from", location: "Query", type: "datetime", required: false, description: "查询起始时间。", example: "2026-04-11T00:00:00+08:00" },
      { name: "date_to", location: "Query", type: "datetime", required: false, description: "查询结束时间。", example: "2026-04-11T23:59:59+08:00" },
      { name: "page", location: "Query", type: "number", required: false, description: "页码，从 1 开始。", example: 1 },
      { name: "page_size", location: "Query", type: "number", required: false, description: "每页条数。", example: 20 },
    ],
    responseExample: { items: [{ task_id: "0195...", file_path: "/data/zlm/record/live/camera01/2026-04-10.mp4" }], total: 1 },
  },
  {
    category: "节点状态",
    method: "GET",
    path: "/api/v1/nodes",
    title: "查询节点列表",
    summary: "返回节点健康、能力摘要和当前负载。",
    description: "适合运维或外部监控系统汇总工作节点状态。",
    params: [authHeaderParam()],
    responseExample: [{ id: "f899...", node_name: "localhost", healthy: true, cpu_percent: 12.4, running_tasks: 3 }],
  },
  {
    category: "节点状态",
    method: "GET",
    path: "/api/v1/nodes/{id}/heartbeats",
    title: "查询节点心跳历史",
    summary: "查看指定节点最近的负载采样。",
    description: "适合做节点趋势图或故障回溯。",
    params: [
      authHeaderParam(),
      nodeIdPathParam(),
      { name: "limit", location: "Query", type: "number", required: false, description: "返回条数，默认 24。适合控制趋势图窗口长度。", example: 24 },
    ],
    responseExample: [{ received_at: "2026-04-10T14:32:45Z", cpu_percent: 8.4, mem_percent: 32.1 }],
  },
];

function buildApiDocDetails() {
  return {
  "POST /api/v1/tasks": {
    requestSample: {
      note: "该示例覆盖了创建任务接口的主要可选字段。视频编解码器和 GPU/CPU 路径由系统内部自动决定，北向接口不再要求手动指定。",
      headers: {
        Authorization: EXAMPLE_AUTHORIZATION,
        "Idempotency-Key": "task-create-relay-camera-01-20260411",
        "Content-Type": "application/json",
      },
      body: {
        name: "relay-camera-01",
        type: "live_relay",
        template: "default-live-relay",
        profile: "realtime_compat",
        priority: 50,
        common: {
          created_by: "partner-system",
          callback_url: "https://biz.example.com/streamserver/callback",
          labels: ["camera", "vip"],
        },
        input: {
          kind: "rtsp",
          url: "rtsp://camera.example.com/live/01",
          group: "239.10.10.31",
          port: 12361,
          interface_name: "eth1",
          interface_ip: "172.17.13.196",
          ttl: 16,
          reuse: true,
          probe_timeout_ms: 5000,
          tcp_mode: 0,
          ssrc: 0,
        },
        process: {
          mode: "copy_or_transcode",
          bitrate: 4096,
          fps: 25,
          gop: 50,
        },
        publish: {
          kind: "zlm_ingest",
          url: "rtmp://media-core.example.com/live/relay-camera-01",
          group: "239.10.10.31",
          port: 12361,
          interface_name: "eth1",
          interface_ip: "172.17.13.196",
          enable_rtsp: true,
          enable_rtmp: true,
          enable_http_ts: true,
          enable_http_fmp4: true,
          enable_hls: true,
        },
        record: {
          enabled: true,
          format: "both",
          duration_sec: 300,
          segment_sec: 300,
          save_path: "/data/records/relay-camera-01",
        },
        recovery: {
          policy: "on_failure",
        },
        schedule: {
          start_mode: "immediate",
          start_at: "2026-04-11T10:00:00+08:00",
          cron: "0 */2 * * * *",
        },
        resource: {
          required_labels: ["edge"],
          preferred_labels: ["multicast", "ssd"],
        },
      },
    },
    requestFields: [
      { name: "name", type: "string", required: true, description: "任务名称，建议由业务系统保证同类任务内可读且可检索。" },
      { name: "type", type: "enum", required: true, description: "任务类型。决定输入、处理与发布结构。" },
      { name: "template", type: "string", required: false, description: "模板名称。提供任务默认值和约束。" },
      { name: "profile", type: "enum", required: false, description: "任务预设档位。用于实时兼容、归档或组播优化。" },
      { name: "priority", type: "number", required: false, description: "调度优先级，通常 0-100，数字越大越优先。" },
      { name: "common.created_by", type: "string", required: false, description: "任务创建来源，用于审计和回查。" },
      { name: "common.callback_url", type: "string", required: false, description: "任务终态回调地址。任务完成后由 media-core 异步回调，若录像或转码产物稍后入库会自动补发更新回调。" },
      { name: "common.labels[]", type: "string[]", required: false, description: "业务标签，用于筛选和资源偏好。" },
      { name: "input.kind", type: "enum", required: true, description: "输入类型，例如 RTSP、HTTP MP4、文件、组播或国标 RTP。" },
      { name: "input.url", type: "string", required: false, description: "文件或网络源地址。" },
      { name: "input.group", type: "string", required: false, description: "组播组地址。组播输入时使用。" },
      { name: "input.port", type: "number", required: false, description: "输入端口。" },
      { name: "input.interface_name", type: "string", required: false, description: "绑定输入网卡名。组播场景优先使用。" },
      { name: "input.interface_ip", type: "string", required: false, description: "绑定输入本地地址。" },
      { name: "input.ttl", type: "number", required: false, description: "组播 TTL。" },
      { name: "input.reuse", type: "boolean", required: false, description: "是否开启端口重用。" },
      { name: "input.probe_timeout_ms", type: "number", required: false, description: "探测源流超时时间。" },
      { name: "input.tcp_mode", type: "number", required: false, description: "RTP 接收时的 TCP 模式。" },
      { name: "input.ssrc", type: "number", required: false, description: "RTP 接收时的 SSRC。" },
      { name: "process.mode", type: "string", required: false, description: "处理模式，如 copy_or_transcode。具体是否走 GPU、使用何种编解码器由系统自动决定。" },
      { name: "process.bitrate", type: "number", required: false, description: "目标码率 kbps。" },
      { name: "process.fps", type: "number", required: false, description: "目标帧率。" },
      { name: "process.gop", type: "number", required: false, description: "关键帧间隔。" },
      { name: "publish.kind", type: "enum", required: false, description: "发布类型，可输出到内部流、文件或组播。" },
      { name: "publish.url", type: "string", required: false, description: "目标文件路径或推流地址。" },
      { name: "publish.group", type: "string", required: false, description: "输出组播组地址。" },
      { name: "publish.port", type: "number", required: false, description: "输出端口。" },
      { name: "publish.interface_name", type: "string", required: false, description: "绑定输出网卡名。" },
      { name: "publish.interface_ip", type: "string", required: false, description: "绑定输出本地地址。" },
      { name: "publish.enable_rtsp", type: "boolean", required: false, description: "是否让内部流额外暴露 RTSP 播放地址。" },
      { name: "publish.enable_rtmp", type: "boolean", required: false, description: "是否让内部流额外暴露 RTMP 播放地址。" },
      { name: "publish.enable_http_ts", type: "boolean", required: false, description: "是否让内部流额外暴露 HTTP-TS 播放地址。" },
      { name: "publish.enable_http_fmp4", type: "boolean", required: false, description: "是否让内部流额外暴露 HTTP-FMP4 播放地址。" },
      { name: "publish.enable_hls", type: "boolean", required: false, description: "是否让内部流额外暴露 HLS 播放地址。" },
      { name: "record.enabled", type: "boolean", required: false, description: "是否开启录像。" },
      { name: "record.format", type: "enum", required: false, description: "录像格式，支持 MP4、HLS 或同时输出。" },
      { name: "record.duration_sec", type: "number", required: false, description: "总录制时长（秒）。离线源按媒体时长截取，在线源按现实时间计时。" },
      { name: "record.segment_sec", type: "number", required: false, description: "分段时长（秒）。" },
      { name: "record.save_path", type: "string", required: false, description: "录像根路径。" },
      { name: "recovery.policy", type: "enum", required: false, description: "失败后的恢复策略。" },
      { name: "schedule.start_mode", type: "enum", required: false, description: "启动方式，可立即、手动、指定时间或 Cron。" },
      { name: "schedule.start_at", type: "string", required: false, description: "指定启动时间，ISO 8601。" },
      { name: "schedule.cron", type: "string", required: false, description: "Cron 表达式。" },
      { name: "resource.required_labels[]", type: "string[]", required: false, description: "节点必需标签。" },
      { name: "resource.preferred_labels[]", type: "string[]", required: false, description: "节点优选标签。" },
    ],
    responseFields: [
      { name: "id", type: "string", description: "任务 ID。" },
      { name: "name", type: "string", description: "任务名称。" },
      { name: "type", type: "enum", description: "任务类型。" },
      { name: "status", type: "enum", description: "当前任务状态。" },
      { name: "assigned_node_id", type: "string", description: "当前分配的节点 ID。" },
      { name: "current_attempt_no", type: "number", description: "当前尝试号。" },
      { name: "created_at", type: "string", description: "创建时间。" },
      { name: "updated_at", type: "string", description: "更新时间。" },
    ],
    enums: {
      type: TASK_TYPES.map((item) => ({ value: item.value, label: item.label, description: item.note })),
      "input.kind": Object.entries(LABELS.inputKind).map(([value, label]) => ({ value, label })),
      "publish.kind": Object.entries(LABELS.publishKind).map(([value, label]) => ({ value, label })),
      "record.format": Object.entries(LABELS.recordFormat).map(([value, label]) => ({ value, label })),
      "recovery.policy": Object.entries(LABELS.recoveryPolicy).map(([value, label]) => ({ value, label })),
      "schedule.start_mode": Object.entries(LABELS.startMode).map(([value, label]) => ({ value, label })),
      status: Object.entries(LABELS.status).map(([value, label]) => ({ value, label })),
    },
  },
  "GET /api/v1/tasks": {
    requestSample: {
      headers: {
        Authorization: EXAMPLE_AUTHORIZATION,
      },
      query: {
        status: "RUNNING",
        type: "live_relay",
        assigned_node_id: EXAMPLE_NODE_ID,
        keyword: "camera-01",
        created_from: "2026-04-11T00:00:00+08:00",
        created_to: "2026-04-11T23:59:59+08:00",
        page: 1,
        page_size: 20,
        sort_by: "updated_at",
        sort_order: "desc",
      },
    },
    responseFields: [
      { name: "items[].id", type: "string", description: "任务 ID。" },
      { name: "items[].name", type: "string", description: "任务名称。" },
      { name: "items[].type", type: "enum", description: "任务类型。" },
      { name: "items[].status", type: "enum", description: "任务状态。" },
      { name: "items[].assigned_node_id", type: "string", description: "节点 ID。" },
      { name: "items[].priority", type: "number", description: "优先级。" },
      { name: "items[].created_by", type: "string", description: "创建人。" },
      { name: "items[].created_at", type: "string", description: "创建时间。" },
      { name: "items[].updated_at", type: "string", description: "更新时间。" },
      { name: "total", type: "number", description: "总条数。" },
      { name: "page", type: "number", description: "当前页。" },
      { name: "page_size", type: "number", description: "每页条数。" },
    ],
    enums: {
      status: Object.entries(LABELS.status).map(([value, label]) => ({ value, label })),
      type: TASK_TYPES.map((item) => ({ value: item.value, label: item.label })),
    },
  },
  "GET /api/v1/tasks/{id}": {
    requestSample: {
      headers: {
        Authorization: EXAMPLE_AUTHORIZATION,
      },
      pathParams: {
        id: EXAMPLE_TASK_ID,
      },
    },
    responseFields: [
      { name: "task", type: "object", description: "任务主信息。" },
      { name: "current_attempt", type: "object", description: "当前 attempt 状态与节点信息。" },
      { name: "callback_delivery", type: "object", description: "最近一次任务回调的状态摘要。" },
      { name: "recent_events[]", type: "array", description: "最近事件。" },
      { name: "requested_spec", type: "object", description: "原始请求规格。" },
      { name: "resolved_spec", type: "object", description: "服务端补全后的最终规格。" },
    ],
    enums: {
      status: Object.entries(LABELS.status).map(([value, label]) => ({ value, label })),
      source: Object.entries(LABELS.eventSource).map(([value, label]) => ({ value, label })),
      level: Object.entries(LABELS.eventLevel).map(([value, label]) => ({ value, label })),
    },
  },
  "POST /api/v1/tasks/{id}/start": {
    requestSample: {
      headers: {
        Authorization: EXAMPLE_AUTHORIZATION,
      },
      pathParams: {
        id: EXAMPLE_TASK_ID,
      },
    },
    responseFields: [
      { name: "id", type: "string", description: "任务 ID。" },
      { name: "status", type: "enum", description: "启动后的任务状态。" },
      { name: "current_attempt_no", type: "number", description: "最新 attempt 号。" },
    ],
    enums: { status: Object.entries(LABELS.status).map(([value, label]) => ({ value, label })) },
  },
  "POST /api/v1/tasks/{id}/stop": {
    requestSample: {
      headers: {
        Authorization: EXAMPLE_AUTHORIZATION,
      },
      pathParams: {
        id: EXAMPLE_TASK_ID,
      },
    },
    responseFields: [
      { name: "id", type: "string", description: "任务 ID。" },
      { name: "status", type: "enum", description: "停止请求后的任务状态。" },
    ],
    enums: { status: Object.entries(LABELS.status).map(([value, label]) => ({ value, label })) },
  },
  "POST /api/v1/tasks/{id}/retry": {
    requestSample: {
      headers: {
        Authorization: EXAMPLE_AUTHORIZATION,
      },
      pathParams: {
        id: EXAMPLE_TASK_ID,
      },
    },
    responseFields: [
      { name: "attempt_no", type: "number", description: "新建的 attempt 号。" },
      { name: "status", type: "enum", description: "重试后的任务状态。" },
    ],
    enums: { status: Object.entries(LABELS.status).map(([value, label]) => ({ value, label })) },
  },
  "POST /api/v1/tasks/{id}/clone": {
    requestSample: {
      headers: {
        Authorization: EXAMPLE_AUTHORIZATION,
        "Content-Type": "application/json",
      },
      pathParams: {
        id: EXAMPLE_TASK_ID,
      },
      body: {
        name: "relay-camera-01-copy",
        priority: 15,
        schedule: {
          start_mode: "manual",
        },
      },
    },
    requestFields: [
      { name: "name", type: "string", required: true, description: "新任务名称。" },
      { name: "priority", type: "number", required: false, description: "新任务优先级。" },
      { name: "schedule.start_mode", type: "enum", required: false, description: "新任务启动方式。" },
    ],
    responseFields: [
      { name: "id", type: "string", description: "新任务 ID。" },
      { name: "name", type: "string", description: "新任务名称。" },
      { name: "status", type: "enum", description: "通常为已创建。" },
    ],
    enums: {
      "schedule.start_mode": Object.entries(LABELS.startMode).map(([value, label]) => ({ value, label })),
      status: Object.entries(LABELS.status).map(([value, label]) => ({ value, label })),
    },
  },
  "GET /api/v1/streams": {
    requestSample: {
      headers: {
        Authorization: EXAMPLE_AUTHORIZATION,
      },
      query: {
        schema: "rtsp",
        app: "live",
        stream: "camera01",
        node_id: EXAMPLE_NODE_ID,
        has_viewer: true,
        task_id: EXAMPLE_TASK_ID,
      },
    },
    responseFields: [
      { name: "task_id", type: "string", description: "关联任务 ID。" },
      { name: "node_id", type: "string", description: "当前流所在节点。" },
      { name: "schema", type: "enum", description: "输出协议。" },
      { name: "vhost", type: "string", description: "虚拟主机。" },
      { name: "app", type: "string", description: "应用名。" },
      { name: "stream", type: "string", description: "流名。" },
      { name: "viewer_count", type: "number", description: "当前观众数。" },
      { name: "has_viewer", type: "boolean", description: "是否存在观众。" },
      { name: "play_urls[]", type: "string[]", description: "可直接播放的地址集合。" },
    ],
    enums: {
      schema: Object.entries(LABELS.inputKind).map(([value, label]) => ({ value, label })),
      has_viewer: [{ value: "true", label: "是" }, { value: "false", label: "否" }],
    },
  },
  "GET /api/v1/records": {
    requestSample: {
      headers: {
        Authorization: EXAMPLE_AUTHORIZATION,
      },
      query: {
        task_id: EXAMPLE_TASK_ID,
        stream: "camera01",
        date_from: "2026-04-11T00:00:00+08:00",
        date_to: "2026-04-11T23:59:59+08:00",
        page: 1,
        page_size: 20,
      },
    },
    responseFields: [
      { name: "items[].id", type: "string", description: "录像记录 ID。" },
      { name: "items[].task_id", type: "string", description: "关联任务 ID。" },
      { name: "items[].vhost", type: "string", description: "虚拟主机。" },
      { name: "items[].app", type: "string", description: "应用名。" },
      { name: "items[].stream", type: "string", description: "流名。" },
      { name: "items[].file_path", type: "string", description: "文件绝对路径。" },
      { name: "items[].http_url", type: "string", description: "可直接访问的 HTTP 地址；历史记录可能为空。" },
      { name: "items[].file_size", type: "number", description: "文件大小（字节）。" },
      { name: "items[].time_len", type: "number", description: "时长（秒）。" },
      { name: "items[].start_time", type: "string", description: "录像开始时间。" },
      { name: "items[].source", type: "enum", description: "记录来源。" },
      { name: "total", type: "number", description: "总条数。" },
    ],
    enums: {
      source: Object.entries(LABELS.recordSource).map(([value, label]) => ({ value, label })),
    },
  },
  "GET /api/v1/transcode-artifacts": {
    requestSample: {
      headers: {
        Authorization: EXAMPLE_AUTHORIZATION,
      },
      query: {
        task_id: EXAMPLE_TASK_ID,
        date_from: "2026-04-11T00:00:00+08:00",
        date_to: "2026-04-11T23:59:59+08:00",
        page: 1,
        page_size: 20,
      },
    },
    responseFields: [
      { name: "items[].id", type: "string", description: "转码产物记录 ID。" },
      { name: "items[].task_id", type: "string", description: "关联任务 ID。" },
      { name: "items[].attempt_id", type: "string", description: "关联 attempt ID。" },
      { name: "items[].node_id", type: "string", description: "产物所在工作节点 ID。" },
      { name: "items[].file_name", type: "string", description: "文件名。" },
      { name: "items[].file_path", type: "string", description: "工作节点上的绝对路径。" },
      { name: "items[].http_url", type: "string", description: "可直接访问的 HTTP 地址。" },
      { name: "items[].file_size", type: "number", description: "文件大小（字节）。" },
      { name: "items[].created_at", type: "string", description: "产物落库时间。" },
      { name: "total", type: "number", description: "总条数。" },
    ],
  },
  "GET /api/v1/nodes": {
    requestSample: {
      headers: {
        Authorization: EXAMPLE_AUTHORIZATION,
      },
    },
    responseFields: [
      { name: "id", type: "string", description: "节点 ID。" },
      { name: "node_name", type: "string", description: "节点名称。" },
      { name: "hostname", type: "string", description: "宿主机名。" },
      { name: "healthy", type: "boolean", description: "节点健康状态。" },
      { name: "network_mode", type: "enum", description: "节点网络模式。" },
      { name: "cpu_percent", type: "number", description: "CPU 使用率。" },
      { name: "mem_percent", type: "number", description: "内存使用率。" },
      { name: "disk_percent", type: "number", description: "磁盘使用率。" },
      { name: "running_tasks", type: "number", description: "当前运行任务数。" },
      { name: "labels[]", type: "string[]", description: "节点标签。" },
      { name: "interfaces[]", type: "string[]", description: "节点上报的网卡信息。" },
      { name: "ffmpeg_protocols[]", type: "string[]", description: "FFmpeg 支持的协议。" },
      { name: "ffmpeg_encoders[]", type: "string[]", description: "FFmpeg 支持的编码器。" },
    ],
    enums: {
      healthy: [{ value: "true", label: "健康" }, { value: "false", label: "异常" }],
      network_mode: Object.entries(LABELS.networkMode).map(([value, label]) => ({ value, label })),
    },
  },
  "GET /api/v1/nodes/{id}/heartbeats": {
    requestSample: {
      headers: {
        Authorization: EXAMPLE_AUTHORIZATION,
      },
      pathParams: {
        id: EXAMPLE_NODE_ID,
      },
      query: {
        limit: 24,
      },
    },
    responseFields: [
      { name: "received_at", type: "string", description: "控制面接收到心跳的时间。" },
      { name: "node_time", type: "string", description: "节点本地上报时间。" },
      { name: "cpu_percent", type: "number", description: "CPU 使用率。" },
      { name: "mem_percent", type: "number", description: "内存使用率。" },
      { name: "disk_percent", type: "number", description: "磁盘使用率。" },
      { name: "running_tasks", type: "number", description: "运行任务数。" },
      { name: "slot_usage", type: "number", description: "槽位使用率，0-1。" },
      { name: "zlm_alive", type: "boolean", description: "ZLM 是否存活。" },
      { name: "ffmpeg_alive", type: "boolean", description: "FFmpeg 是否存活。" },
    ],
  },
  };
}

const SYSTEM_CAPABILITIES = [
  {
    title: "输入协议",
    summary: "支持多种网络源、文件和组播输入。",
    items: ["RTSP", "RTMP", "HLS", "HTTP-FLV", "HTTP-TS", "文件", "UDP MPEGTS 组播", "RTP 组播", "国标 RTP"],
  },
  {
    title: "输出与分发",
    summary: "支持内部流、组播输出和多协议播放暴露。",
    items: ["内部流发布", "文件输出", "UDP MPEGTS 组播", "RTP 组播", "RTSP 播放", "RTMP 播放", "HTTP-TS", "HTTP-FMP4", "HLS"],
  },
  {
    title: "录像能力",
    summary: "支持媒体录像索引与多格式落盘。",
    items: ["MP4 录像", "HLS 切片", "MP4 + HLS 同时输出", "录像路径检索", "任务回溯", "文件路径复制"],
  },
  {
    title: "调度与恢复",
    summary: "支持任务调度、节点筛选和失败恢复。",
    items: ["立即启动", "手动启动", "指定时间", "Cron 计划", "失败恢复", "节点标签调度", "GPU 需求", "优先级调度"],
  },
];

const STATUS_THEME = {
  RUNNING: "status-running",
  STARTING: "status-starting",
  DISPATCHING: "status-dispatching",
  RECOVERING: "status-recovering",
  STOPPING: "status-stopping",
  FAILED: "status-failed",
  LOST: "status-lost",
  CREATED: "status-created",
  VALIDATING: "status-validating",
  QUEUED: "status-queued",
  SUCCEEDED: "status-succeeded status-outline",
  CANCELED: "status-canceled status-outline",
};

const LABELS = {
  taskType: Object.fromEntries(TASK_TYPES.map((item) => [item.value, item.label])),
  inputKind: {
    rtsp: "RTSP",
    rtmp: "RTMP",
    hls: "HLS",
    http_mp4: "HTTP-MP4",
    http_flv: "HTTP-FLV",
    http_ts: "HTTP-TS",
    file: "文件",
    udp_mpegts_multicast: "UDP MPEGTS 组播",
    rtp_multicast: "RTP 组播",
    gb_rtp: "国标 RTP",
  },
  publishKind: {
    file: "文件输出",
    zlm_ingest: "内部流发布",
    udp_mpegts_multicast: "UDP MPEGTS 组播",
    rtp_multicast: "RTP 组播",
  },
  startMode: {
    immediate: "立即启动",
    manual: "手动启动",
    cron: "定时计划",
    at: "指定时间",
  },
  recordFormat: {
    mp4: "MP4",
    hls: "HLS",
    both: "MP4 + HLS",
  },
  recoveryPolicy: {
    never: "不恢复",
    on_failure: "失败时恢复",
    always: "始终恢复",
  },
  profile: {
    realtime_compat: "实时兼容",
    archive_quality: "归档优先",
    multicast_ts: "组播传输流",
    rtmp_hevc_ext: "RTMP HEVC 扩展",
  },
  status: {
    CREATED: "已创建",
    VALIDATING: "校验中",
    QUEUED: "排队中",
    DISPATCHING: "派发中",
    STARTING: "启动中",
    RUNNING: "运行中",
    STOPPING: "停止中",
    RECOVERING: "恢复中",
    SUCCEEDED: "已成功",
    FAILED: "已失败",
    CANCELED: "已取消",
    LOST: "已丢失",
  },
  route: {
    login: "登录",
    overview: "系统总览",
    "api-docs": "外部 API 文档",
    tasks: "任务中心",
    "task-detail": "任务详情",
    streams: "流中心",
    multicast: "组播中心",
    records: "录像中心",
    "transcode-artifacts": "转码产物",
    security: "安全设置",
    nodes: "节点中心",
    debug: "调试台",
  },
  apiRole: {
    admin: "管理员",
  },
  networkMode: {
    host: "主机网络",
    bridge: "桥接网络",
  },
  eventSource: {
    core: "控制面",
    agent: "工作节点",
    ffmpeg: "FFmpeg",
    zlm_api: "ZLM API",
    zlm_hook: "ZLM Hook",
    scheduler: "调度器",
    user: "用户操作",
  },
  eventLevel: {
    debug: "调试",
    info: "信息",
    warn: "警告",
    error: "错误",
  },
  recordSource: {
    hook: "Hook 回调",
  },
  bool: {
    true: "是",
    false: "否",
  },
};

const API_DOC_DETAILS = buildApiDocDetails();

const state = {
  token: "",
  refreshToken: window.localStorage.getItem(REFRESH_TOKEN_STORAGE_KEY) || "",
  session: null,
  sessionError: null,
  sessionLoading: true,
  route: parseRoute(window.location.pathname, window.location.search),
  routeData: null,
  pageError: null,
  loading: false,
  toasts: [],
  cache: {
    taskDetails: new Map(),
    templates: null,
    templateDetails: new Map(),
    nodes: null,
    nodeInsights: new Map(),
  },
  ui: {
    themePreference: readThemePreference(),
    authModalOpen: false,
    apiDocModalKey: "",
    createOpen: false,
    openNodeId: "",
    createStep: 1,
    createDraft: createDefaultDraft(),
    createPreview: null,
    createError: null,
    authDraftToken: "",
    securityAllowlistText: "",
    securityAllowlistDirty: false,
    scrollPositions: new Map(),
    debug: {
      nodeId: "",
      mediaResult: null,
      sessionsResult: null,
      playersResult: null,
      statisticResult: null,
      threadsLoadResult: null,
      workThreadsLoadResult: null,
      snapResult: null,
      hooksResult: null,
      lastError: null,
    },
  },
};
let autoRefreshTimer = null;
let authRefreshPromise = null;

const appRoot = document.getElementById("app");
const shell = {
  ready: false,
  sidebar: null,
  topbar: null,
  pageBody: null,
  drawer: null,
  authModal: null,
  apiDocModal: null,
  toasts: null,
};

boot().catch((error) => {
  console.error(error);
  appRoot.innerHTML = renderFatal(error);
});

async function boot() {
  applyTheme(state.ui.themePreference);
  watchSystemTheme();
  window.addEventListener("popstate", async () => {
    state.route = parseRoute(window.location.pathname, window.location.search);
    await refreshRoute({ preserveScroll: false, restoreStoredScroll: true });
  });
  document.addEventListener("click", handleClick);
  document.addEventListener("submit", handleSubmit);
  document.addEventListener("change", handleChange);
  document.addEventListener("input", handleInput);
  startAutoRefresh();
  renderApp();
  await refreshSession(true);
  await refreshRoute({ preserveScroll: false, restoreStoredScroll: true });
}

function ensureShell() {
  if (shell.ready) {
    return;
  }
  appRoot.className = "app-shell";
  appRoot.innerHTML = `
    <aside id="sidebar-slot"></aside>
    <main class="main-panel">
      <header id="topbar-slot"></header>
      <section id="page-body-slot" class="page-body"></section>
    </main>
    <div id="drawer-slot"></div>
    <div id="auth-modal-slot"></div>
    <div id="api-doc-modal-slot"></div>
    <div id="toast-slot"></div>
  `;
  shell.sidebar = document.getElementById("sidebar-slot");
  shell.topbar = document.getElementById("topbar-slot");
  shell.pageBody = document.getElementById("page-body-slot");
  shell.drawer = document.getElementById("drawer-slot");
  shell.authModal = document.getElementById("auth-modal-slot");
  shell.apiDocModal = document.getElementById("api-doc-modal-slot");
  shell.toasts = document.getElementById("toast-slot");
  shell.ready = true;
}

function renderApp(options = {}) {
  const settings = {
    chrome: true,
    page: true,
    overlays: true,
    toasts: true,
    ...options,
  };
  ensureShell();
  const standalone = shouldUseStandaloneAuthShell();
  appRoot.className = standalone ? "app-shell auth-shell" : "app-shell";
  if (standalone) {
    shell.sidebar.innerHTML = "";
    shell.topbar.innerHTML = "";
  } else if (settings.chrome) {
    shell.sidebar.innerHTML = renderSidebar();
    shell.topbar.innerHTML = renderTopbar();
  }
  if (settings.page) {
    shell.pageBody.innerHTML = standalone ? renderStandalonePage() : renderPageBody();
  }
  if (settings.overlays) {
    shell.drawer.innerHTML = renderCreateDrawer();
    shell.authModal.innerHTML = renderAuthModal();
    shell.apiDocModal.innerHTML = renderApiDocModal();
  }
  if (settings.toasts) {
    shell.toasts.innerHTML = renderToasts();
  }
}

function startAutoRefresh() {
  if (autoRefreshTimer) {
    window.clearInterval(autoRefreshTimer);
  }
  autoRefreshTimer = window.setInterval(async () => {
    if (shouldPauseAutoRefresh()) {
      return;
    }
    try {
      await refreshSession(true);
      await refreshRoute({ preserveScroll: true });
    } catch (error) {
      console.error(error);
    }
  }, AUTO_REFRESH_MS);
}

function shouldPauseAutoRefresh() {
  if (document.hidden || state.loading || state.ui.authModalOpen || state.ui.createOpen || state.ui.apiDocModalKey) {
    return true;
  }
  const selection = window.getSelection();
  if (selection && !selection.isCollapsed && selection.toString().trim()) {
    return true;
  }
  const activeElement = document.activeElement;
  if (!activeElement || activeElement === document.body) {
    return false;
  }
  if (
    activeElement instanceof HTMLInputElement ||
    activeElement instanceof HTMLTextAreaElement ||
    activeElement instanceof HTMLSelectElement ||
    activeElement.isContentEditable
  ) {
    return true;
  }
  return Boolean(activeElement.closest("form"));
}

async function refreshSession(silent) {
  state.sessionLoading = true;
  try {
    if (!state.token && state.refreshToken) {
      await refreshAccessToken(silent);
    }

    let sessionError = null;
    try {
      state.session = await apiRequest("/api/v1/me");
      state.sessionError = null;
      return;
    } catch (error) {
      sessionError = error;
    }

    if (
      isAuthError(sessionError) &&
      state.refreshToken &&
      await refreshAccessToken(silent)
    ) {
      try {
        state.session = await apiRequest("/api/v1/me");
        state.sessionError = null;
        return;
      } catch (error) {
        sessionError = error;
      }
    }

    state.session = null;
    state.sessionError = sessionError;
    if (!silent && !isAuthError(sessionError)) {
      toast(errorMessage(sessionError), "error");
    }
  } finally {
    state.sessionLoading = false;
    syncRouteWithSessionState();
  }
}

async function refreshAccessToken(silent) {
  if (!state.refreshToken) {
    return false;
  }
  if (!authRefreshPromise) {
    authRefreshPromise = (async () => {
      try {
        const tokens = await apiRequest("/api/v1/auth/refresh", {
          method: "POST",
          skipAuth: true,
          body: { refresh_token: state.refreshToken },
        });
        applyAuthTokens(tokens);
        return true;
      } catch (error) {
        clearAuthTokens({ clearRefresh: true });
        if (!silent && !isAuthError(error)) {
          toast(errorMessage(error), "error");
        }
        return false;
      } finally {
        authRefreshPromise = null;
      }
    })();
  }
  return await authRefreshPromise;
}

function applyAuthTokens(tokens) {
  if (!tokens || typeof tokens !== "object") {
    return;
  }
  if (typeof tokens.access_token === "string") {
    state.token = tokens.access_token;
  }
  if (typeof tokens.refresh_token === "string") {
    state.refreshToken = tokens.refresh_token;
    window.localStorage.setItem(REFRESH_TOKEN_STORAGE_KEY, state.refreshToken);
  }
}

function clearAuthTokens(options = {}) {
  const { clearRefresh = false } = options;
  state.token = "";
  if (clearRefresh) {
    state.refreshToken = "";
    window.localStorage.removeItem(REFRESH_TOKEN_STORAGE_KEY);
  }
}

function routeHref(route = state.route) {
  const query = route.searchParams.toString();
  return `${route.path}${query ? `?${query}` : ""}`;
}

function sanitizeReturnTo(value) {
  if (!value || typeof value !== "string") {
    return "/overview";
  }
  try {
    const url = new URL(value, window.location.origin);
    if (url.origin !== window.location.origin) {
      return "/overview";
    }
    const candidate = `${url.pathname}${url.search}`;
    if (!candidate.startsWith("/") || candidate.startsWith("//") || candidate === "/login") {
      return "/overview";
    }
    return candidate;
  } catch (_) {
    if (!value.startsWith("/") || value.startsWith("//") || value === "/login") {
      return "/overview";
    }
    return value;
  }
}

function buildLoginHref(nextHref) {
  const params = new URLSearchParams();
  const sanitizedNext = sanitizeReturnTo(nextHref);
  if (sanitizedNext && sanitizedNext !== "/overview") {
    params.set("next", sanitizedNext);
  }
  const query = params.toString();
  return `/login${query ? `?${query}` : ""}`;
}

function replaceRoute(href) {
  if (`${window.location.pathname}${window.location.search}` !== href) {
    window.history.replaceState({}, "", href);
  }
  state.route = parseRoute(window.location.pathname, window.location.search);
}

function syncRouteWithSessionState() {
  if (state.session) {
    if (state.route.name === "login") {
      replaceRoute(sanitizeReturnTo(state.route.searchParams.get("next")));
      return true;
    }
    return false;
  }
  if (state.sessionLoading) {
    return false;
  }
  if (state.sessionError && isAuthError(state.sessionError) && state.route.name !== "login") {
    replaceRoute(buildLoginHref(routeHref()));
    return true;
  }
  return false;
}

function shouldUseStandaloneAuthShell() {
  return state.route.name === "login" || state.sessionLoading || (!state.session && isAuthError(state.sessionError));
}

async function refreshRoute(options = {}) {
  const { preserveScroll = true, restoreStoredScroll = false } = options;
  syncRouteWithSessionState();
  const routeKey = currentRouteKey();
  if (preserveScroll) {
    rememberRouteScroll(routeKey);
  }
  state.loading = true;
  state.routeData = null;
  state.pageError = null;
  renderApp();
  try {
    state.routeData = await loadRouteData(state.route);
  } catch (error) {
    state.routeData = null;
    state.pageError = error;
  }
  state.loading = false;
  renderApp();
  if (preserveScroll || restoreStoredScroll) {
    restoreRouteScroll(routeKey);
  }
}

function currentRouteKey() {
  return routeHref();
}

function rememberRouteScroll(routeKey) {
  state.ui.scrollPositions.set(routeKey, {
    x: window.scrollX,
    y: window.scrollY,
  });
}

function restoreRouteScroll(routeKey) {
  const position = state.ui.scrollPositions.get(routeKey);
  if (!position) {
    return;
  }
  window.requestAnimationFrame(() => {
    window.scrollTo(position.x, position.y);
  });
}

function renderSidebar() {
  const visibleItems = NAV_ITEMS.filter((item) => canAccess(item.permission));
  const manualTokenOnly = !state.session || state.session.auth_mode !== "local_password";
  return `
    <aside class="sidebar">
      <div class="brand">
        <div class="brand-mark">控制台</div>
        <div>
          <h1>StreamServer</h1>
          <p>统一承载任务编排、媒体流转发、录像索引、节点观测和第三方业务接口。</p>
        </div>
      </div>
      <section class="session-card">
        <strong>${escapeHtml(state.session?.subject || "未认证会话")}</strong>
        <span class="muted">${escapeHtml(sessionSubtitle())}</span>
        <div class="toolbar-actions">
          ${state.session ? renderRolePill(state.session.role) : ""}
          ${state.session ? `<button class="ghost-button" data-action="logout">退出</button>` : `<a class="ghost-button" href="/login" data-link>登录</a>`}
          ${manualTokenOnly ? `<button class="ghost-button" data-action="open-auth-modal">令牌</button>` : ""}
        </div>
      </section>
      <nav class="sidebar-nav">
        ${visibleItems
          .map(
            (item, index) => `
              <a class="nav-item ${state.route.path.startsWith(item.path) ? "active" : ""}" href="${item.path}" data-link>
                <span>
                  <strong>${escapeHtml(item.label)}</strong>
                  <small>${escapeHtml(item.note)}</small>
                </span>
                <span class="nav-badge">${index + 1}</span>
              </a>
            `,
          )
          .join("")}
      </nav>
    </aside>
  `;
}

function renderTopbar() {
  const title = currentRouteTitle();
  const subtitle = state.pageError
    ? "页面加载失败"
    : state.loading && !state.routeData
      ? "正在读取控制面数据"
      : currentRouteSubtitle();
  return `
    <header class="topbar">
      <div>
        <h2>${escapeHtml(title)}</h2>
        <p>${escapeHtml(subtitle)}</p>
      </div>
      <div class="topbar-actions">
        <div class="theme-switch" aria-label="主题切换">
          ${THEME_OPTIONS.map(
            (option) => `
              <button
                class="theme-option ${state.ui.themePreference === option ? "active" : ""}"
                data-action="set-theme"
                data-theme-value="${option}"
                type="button"
              >
                ${escapeHtml(themeLabel(option))}
              </button>
            `,
          ).join("")}
        </div>
        <span class="tag">${escapeHtml(state.session?.environment || "未知环境")}</span>
        <button class="ghost-button" data-action="refresh-page">刷新</button>
        ${canAccess("task_write") ? `<button class="button" data-action="open-create-drawer">新建任务</button>` : ""}
      </div>
    </header>
  `;
}

function renderPageBody() {
  if (state.loading && !state.routeData) {
    return renderLoadingPanel();
  }
  if (state.pageError) {
    if (shouldRenderAuthRequired(state.pageError)) {
      return renderAuthRequired();
    }
    return renderErrorPanel("页面加载失败", errorMessage(state.pageError));
  }
  if (!state.routeData) {
    return renderEmptyState("暂无内容", "当前页面还没有可展示的数据。");
  }
  return renderRouteBody(state.route, state.routeData);
}

function renderStandalonePage() {
  if (state.sessionLoading && !state.session) {
    return renderStandaloneState(
      "正在建立会话",
      "控制台正在校验现有令牌并尝试恢复登录状态，请稍候。",
      "正在同步",
    );
  }
  if (state.session) {
    return renderStandaloneState(
      "正在进入控制台",
      `已恢复账号 ${state.session.subject} 的会话，正在跳转到控制台。`,
      "会话恢复",
    );
  }
  return renderLoginPage();
}

async function loadRouteData(route) {
  if (route.name === "login") {
    return {};
  }
  if (state.sessionError && isAuthError(state.sessionError)) {
    return { authRequired: true };
  }
  switch (route.name) {
    case "overview":
      return await loadOverviewData();
    case "api-docs":
      return await loadApiDocsData();
    case "tasks":
      return await loadTasksData(route);
    case "task-detail":
      return await loadTaskDetailData(route);
    case "streams":
      return await loadStreamsData(route);
    case "multicast":
      return await loadMulticastData(route);
    case "records":
      return await loadRecordsData(route);
    case "transcode-artifacts":
      return await loadTranscodeArtifactsData(route);
    case "security":
      return await loadSecurityData();
    case "nodes":
      return await loadNodesData(route);
    case "debug":
      return await loadDebugData(route);
    default:
      return await loadTasksData(route);
  }
}

function renderRouteBody(route, data) {
  if (data.authRequired) {
    return renderAuthRequired();
  }
  switch (route.name) {
    case "login":
      return renderLoginPage();
    case "overview":
      return renderOverviewPage(data);
    case "api-docs":
      return renderApiDocsPage(data);
    case "tasks":
      return renderTasksPage(data);
    case "task-detail":
      return renderTaskDetailPage(route, data);
    case "streams":
      return renderStreamsPage(data);
    case "multicast":
      return renderMulticastPage(data);
    case "records":
      return renderRecordsPage(data);
    case "transcode-artifacts":
      return renderTranscodeArtifactsPage(data);
    case "security":
      return renderSecurityPage(data);
    case "nodes":
      return renderNodesPage(data);
    case "debug":
      return renderDebugPage(data);
    default:
      return renderTasksPage(data);
  }
}

async function loadTasksData(route) {
  const params = route.searchParams;
  const query = new URLSearchParams();
  copyIfPresent(params, query, ["status", "type", "assigned_node_id", "keyword", "created_from", "created_to", "page", "page_size", "sort_by", "sort_order"]);
  if (!query.get("page_size")) {
    query.set("page_size", DEFAULT_PAGE_SIZE);
  }
  const [tasksPage, nodes, templates] = await Promise.all([
    apiRequest(`/api/v1/tasks?${query.toString()}`),
    canAccess("node_read") ? fetchNodesCached(false) : Promise.resolve([]),
    canAccess("template_read") ? fetchTemplatesCached(false) : Promise.resolve([]),
  ]);
  return { tasksPage, nodes, templates };
}

async function loadOverviewData() {
  const [nodes, streams, recentTasksPage, runningTasksPage, failedTasksPage, queuedTasksPage, recordsPage] = await Promise.all([
    canAccess("node_read") ? fetchNodesCached(true) : Promise.resolve([]),
    canAccess("task_read") ? apiRequest("/api/v1/streams") : Promise.resolve([]),
    canAccess("task_read")
      ? apiRequest(`/api/v1/tasks?page_size=8&sort_by=updated_at&sort_order=desc`)
      : Promise.resolve({ items: [], total: 0, page: 1, page_size: 8 }),
    canAccess("task_read")
      ? apiRequest("/api/v1/tasks?status=RUNNING&page_size=1")
      : Promise.resolve({ items: [], total: 0, page: 1, page_size: 1 }),
    canAccess("task_read")
      ? apiRequest("/api/v1/tasks?status=FAILED&page_size=1")
      : Promise.resolve({ items: [], total: 0, page: 1, page_size: 1 }),
    canAccess("task_read")
      ? apiRequest("/api/v1/tasks?status=QUEUED&page_size=1")
      : Promise.resolve({ items: [], total: 0, page: 1, page_size: 1 }),
    canAccess("record_read")
      ? apiRequest("/api/v1/records?page_size=1")
      : Promise.resolve({ items: [], total: 0, page: 1, page_size: 1 }),
  ]);

  const nodeList = Array.isArray(nodes) ? nodes : [];
  const streamList = Array.isArray(streams) ? streams : [];
  const recentTasks = recentTasksPage?.items || [];
  const healthyNodes = nodeList.filter((node) => node.healthy);
  const unhealthyNodes = nodeList.filter((node) => !node.healthy);
  const activeIssues = [
    ...recentTasks
      .filter((task) => ["FAILED", "LOST"].includes(task.status))
      .map((task) => ({
        title: task.name,
        description: `${taskStatusLabel(task.status)} · ${taskTypeLabel(task.type)}`,
        time: task.updated_at || task.created_at,
      })),
    ...unhealthyNodes.map((node) => ({
      title: node.node_name,
      description: `节点异常 · CPU ${formatPercent(node.cpu_percent)} · 内存 ${formatPercent(node.mem_percent)}`,
      time: node.last_seen_at,
    })),
  ]
    .sort((left, right) => new Date(right.time || 0).getTime() - new Date(left.time || 0).getTime())
    .slice(0, 6);

  const statusSummary = {
    running: runningTasksPage?.total || 0,
    failed: failedTasksPage?.total || 0,
    queued: queuedTasksPage?.total || 0,
    records: recordsPage?.total || 0,
  };

  let systemState = "稳定运行";
  let systemNote = "控制面、节点与流状态看起来正常，可以继续观测实时业务任务。";
  if (!nodeList.length && !recentTasks.length) {
    systemState = "等待接入";
    systemNote = "当前没有可展示的节点或任务，适合先完成工作节点接入。";
  } else if (unhealthyNodes.length || statusSummary.failed > 0) {
    systemState = "需要关注";
    systemNote = "存在离线节点或失败任务，建议优先查看节点详情与最近异常。";
  } else if (statusSummary.queued > 0) {
    systemState = "有任务排队";
    systemNote = "系统整体可用，但仍有任务等待调度或启动。";
  }

  return {
    nodes: nodeList,
    healthyNodes,
    unhealthyNodes,
    streams: streamList,
    recentTasks,
    activeIssues,
    statusSummary,
    systemState,
    systemNote,
  };
}

async function loadApiDocsData() {
  return {
    docs: EXTERNAL_API_DOCS,
    authEnabled: Boolean(state.session?.auth_enabled ?? !state.sessionError),
  };
}

async function loadTaskDetailData(route) {
  const taskId = route.params.id;
  const params = route.searchParams;
  const tab = params.get("tab") || "overview";
  const detail = await fetchTaskDetail(taskId, true);
  const [recordsPage, streams] = await Promise.all([
    canAccess("record_read")
      ? apiRequest(`/api/v1/records?task_id=${encodeURIComponent(taskId)}&page_size=5`)
      : Promise.resolve({ items: [], page: 1, page_size: 5, total: 0 }),
    canAccess("task_read")
      ? apiRequest(`/api/v1/streams?task_id=${encodeURIComponent(taskId)}`)
      : Promise.resolve([]),
  ]);

  const eventParams = new URLSearchParams();
  copyIfPresent(params, eventParams, ["attempt_no", "source", "event_type", "page", "page_size"]);
  if (!eventParams.get("page_size")) {
    eventParams.set("page_size", "20");
  }

  const logParams = new URLSearchParams();
  copyIfPresent(params, logParams, ["log_attempt_no", "log_stream", "log_cursor", "log_limit"]);
  if (!logParams.get("limit") && params.get("log_limit")) {
    logParams.set("limit", params.get("log_limit"));
  }
  if (!logParams.get("limit")) {
    logParams.set("limit", "200");
  }
  if (params.get("log_stream")) {
    logParams.set("stream", params.get("log_stream"));
  }
  if (params.get("log_attempt_no")) {
    logParams.set("attempt_no", params.get("log_attempt_no"));
  }
  if (params.get("log_cursor")) {
    logParams.set("cursor", params.get("log_cursor"));
  }

  const [eventsPage, logs] = await Promise.all([
    apiRequest(`/api/v1/tasks/${taskId}/events?${eventParams.toString()}`),
    apiRequest(`/api/v1/tasks/${taskId}/logs?${logParams.toString()}`),
  ]);

  return { detail, recordsPage, streams, eventsPage, logs, activeTab: tab };
}

async function loadStreamsData(route) {
  const params = route.searchParams;
  const query = new URLSearchParams();
  copyIfPresent(params, query, ["schema", "app", "stream", "node_id", "has_viewer", "task_id"]);
  const [streams, nodes] = await Promise.all([
    apiRequest(`/api/v1/streams?${query.toString()}`),
    canAccess("node_read") ? fetchNodesCached(false) : Promise.resolve([]),
  ]);

  const taskDetails = new Map();
  await Promise.all(
    [...new Set(streams.map((stream) => stream.task_id))]
      .slice(0, 30)
      .map(async (taskId) => {
        taskDetails.set(taskId, await fetchTaskDetail(taskId, false));
      }),
  );

  return { streams, nodes, taskDetails };
}

async function loadMulticastData(route) {
  const params = route.searchParams;
  const query = new URLSearchParams();
  query.set("type", "multicast_bridge");
  query.set("page_size", params.get("page_size") || "100");
  if (params.get("status")) {
    query.set("status", params.get("status"));
  }
  const [tasksPage, nodes, streams] = await Promise.all([
    apiRequest(`/api/v1/tasks?${query.toString()}`),
    canAccess("node_read") ? fetchNodesCached(false) : Promise.resolve([]),
    canAccess("task_read") ? apiRequest("/api/v1/streams") : Promise.resolve([]),
  ]);
  const taskDetails = new Map();
  await Promise.all(
    tasksPage.items.map(async (task) => {
      taskDetails.set(task.id, await fetchTaskDetail(task.id, false));
    }),
  );
  const streamsByTask = new Map();
  (streams || []).forEach((stream) => {
    if (!streamsByTask.has(stream.task_id)) {
      streamsByTask.set(stream.task_id, []);
    }
    streamsByTask.get(stream.task_id).push(stream);
  });
  return { tasksPage, nodes, taskDetails, streamsByTask };
}

async function loadRecordsData(route) {
  const params = route.searchParams;
  const query = new URLSearchParams();
  copyIfPresent(params, query, ["task_id", "stream", "date_from", "date_to", "page", "page_size"]);
  if (!query.get("page_size")) {
    query.set("page_size", "20");
  }
  const recordsPage = await apiRequest(`/api/v1/records?${query.toString()}`);
  return { recordsPage };
}

async function loadTranscodeArtifactsData(route) {
  const params = route.searchParams;
  const query = new URLSearchParams();
  copyIfPresent(params, query, ["task_id", "date_from", "date_to", "page", "page_size"]);
  if (!query.get("page_size")) {
    query.set("page_size", "20");
  }
  const artifactsPage = await apiRequest(`/api/v1/transcode-artifacts?${query.toString()}`);
  return { artifactsPage };
}

async function loadSecurityData() {
  const allowlist = await apiRequest("/api/v1/security/machine-allowlist");
  return { allowlist };
}

async function loadNodesData() {
  const nodes = await fetchNodesCached(true);
  if (state.ui.openNodeId) {
    state.cache.nodeInsights.set(state.ui.openNodeId, await loadNodeInsight(state.ui.openNodeId));
  }
  return { nodes };
}

async function loadDebugData() {
  const nodes = await fetchNodesCached(false);
  if (!state.ui.debug.nodeId && nodes.length > 0) {
    state.ui.debug.nodeId = nodes[0].id;
  }
  return { nodes };
}

function renderOverviewPage(data) {
  return `
    <section class="hero-panel overview-hero">
      <div class="section-header">
        <div>
          <div class="brand-mark">总览</div>
          <h3>系统总览</h3>
          <p>集中查看系统能力、整体健康度、节点负载、在线流和最近的异常动态。</p>
        </div>
        <div class="section-actions">
          <span class="pill ${data.systemState === "稳定运行" ? "status-running" : data.systemState === "等待接入" ? "status-created" : "status-stopping"}">${escapeHtml(data.systemState)}</span>
        </div>
      </div>
      <div class="overview-grid">
        ${metricCard("整体状态", data.systemState)}
        ${metricCard("在线节点", `${data.healthyNodes.length} / ${data.nodes.length || 0}`)}
        ${metricCard("运行中任务", String(data.statusSummary.running))}
        ${metricCard("在线流", String(data.streams.length))}
        ${metricCard("录像记录", String(data.statusSummary.records))}
        ${metricCard("待处理问题", String(data.activeIssues.length))}
      </div>
      <p class="hero-note">${escapeHtml(data.systemNote)}</p>
    </section>
    <section class="panel">
      <div class="panel-header">
        <div>
          <h3>系统能力</h3>
          <p>从输入、输出、录像和调度角度概括当前系统可落地的媒体能力。</p>
        </div>
      </div>
      <div class="feature-grid">
        ${OVERVIEW_FEATURES.map(
          (feature) => `
            <article class="feature-card">
              <h4>${escapeHtml(feature.title)}</h4>
              <p>${escapeHtml(feature.description)}</p>
            </article>
          `,
        ).join("")}
      </div>
    </section>
    <section class="panel">
      <div class="panel-header">
        <div>
          <h3>能力矩阵</h3>
          <p>面向部署与联调的技术能力摘要，帮助快速判断当前系统能接什么、出什么、怎么录和怎么调度。</p>
        </div>
      </div>
      <div class="feature-grid">
        ${SYSTEM_CAPABILITIES.map(
          (capability) => `
            <article class="feature-card">
              <h4>${escapeHtml(capability.title)}</h4>
              <p>${escapeHtml(capability.summary)}</p>
              <div class="inline-list" style="margin-top: 12px;">
                ${capability.items.map((item) => `<span class="tag">${escapeHtml(item)}</span>`).join("")}
              </div>
            </article>
          `,
        ).join("")}
      </div>
    </section>
    <section class="split-grid">
      <div class="panel">
        <div class="panel-header">
          <div>
            <h3>节点状态与负载</h3>
            <p>展示每个节点当前健康度、CPU、内存、磁盘和任务数。</p>
          </div>
        </div>
        <div class="node-summary-list">
          ${
            data.nodes.length
              ? data.nodes
                  .map(
                    (node) => `
                      <article class="node-summary-card">
                        <div class="toolbar-actions">
                          <strong>${escapeHtml(node.node_name)}</strong>
                          <span class="pill ${node.healthy ? "status-running" : "status-failed"}">${node.healthy ? "健康" : "异常"}</span>
                        </div>
                        <div class="subtle">${escapeHtml(node.hostname || "未知主机")} · ${escapeHtml(networkModeLabel(node.network_mode))}</div>
                        <div class="inline-list">
                          <span class="tag">CPU ${formatPercent(node.cpu_percent)}</span>
                          <span class="tag">内存 ${formatPercent(node.mem_percent)}</span>
                          <span class="tag">磁盘 ${formatPercent(node.disk_percent)}</span>
                          <span class="tag">任务 ${escapeHtml(String(node.running_tasks ?? 0))}</span>
                        </div>
                        <div class="subtle">最近心跳：${escapeHtml(formatTime(node.last_seen_at))}</div>
                      </article>
                    `,
                  )
                  .join("")
              : renderInlineEmpty("当前没有节点数据。")
          }
        </div>
      </div>
      <div class="panel">
        <div class="panel-header">
          <div>
            <h3>最近异常与动态</h3>
            <p>优先聚合失败任务、离线节点和最近更新的业务任务。</p>
          </div>
        </div>
        <div class="event-list">
          ${
            data.activeIssues.length
              ? data.activeIssues
                  .map(
                    (issue) => `
                      <article class="event-item">
                        <strong>${escapeHtml(issue.title)}</strong>
                        <div class="subtle">${escapeHtml(issue.description)}</div>
                        <div class="subtle">${escapeHtml(formatTime(issue.time))}</div>
                      </article>
                    `,
                  )
                  .join("")
              : renderInlineEmpty("最近没有需要重点关注的异常。")
          }
        </div>
      </div>
    </section>
    <section class="panel">
      <div class="panel-header">
        <div>
          <h3>最近任务动态</h3>
          <p>按更新时间倒序展示最近 8 条任务，方便快速回到详情和排障。</p>
        </div>
      </div>
      <div class="table-wrap">
        <table>
          <thead>
            <tr>
              <th>任务</th>
              <th>类型</th>
              <th>状态</th>
              <th>优先级</th>
              <th>节点</th>
              <th>更新时间</th>
            </tr>
          </thead>
          <tbody>
            ${
              data.recentTasks.length
                ? data.recentTasks
                    .map(
                      (task) => `
                        <tr>
                          <td>
                            <a href="/tasks/${task.id}" data-link class="mono">${shortId(task.id)}</a>
                            <div><strong>${escapeHtml(task.name)}</strong></div>
                          </td>
                          <td>${escapeHtml(taskTypeLabel(task.type))}</td>
                          <td>${statusPill(task.status)}</td>
                          <td>${escapeHtml(String(task.priority ?? "—"))}</td>
                          <td>${escapeHtml(task.assigned_node_id || "未分配")}</td>
                          <td>${escapeHtml(formatTime(task.updated_at || task.created_at))}</td>
                        </tr>
                      `,
                    )
                    .join("")
                : `<tr><td colspan="6">${renderInlineEmpty("最近没有任务动态。")}</td></tr>`
            }
          </tbody>
        </table>
      </div>
    </section>
  `;
}

function renderApiDocsPage(data) {
  const docsByCategory = groupBy(data.docs, (item) => item.category);
  return `
    <section class="hero-panel">
      <div class="section-header">
        <div>
          <div class="brand-mark">外部接口</div>
          <h3>外部 API 文档</h3>
          <p>只展示对第三方业务系统开放的北向接口，不包含控制台内部和调试接口。</p>
        </div>
      </div>
      <div class="overview-grid">
        ${metricCard("接口分组", String(Object.keys(docsByCategory).length))}
        ${metricCard("公开接口数", String(data.docs.length))}
        ${metricCard("鉴权方式", "Bearer Token")}
        ${metricCard("接口范围", "任务、流、录像、节点")}
      </div>
      <div class="subtle">
        ${
          data.authEnabled
            ? '当前环境启用了鉴权，请在请求头中携带 <code>Authorization: Bearer &lt;token&gt;</code>。'
            : '当前环境未启用鉴权，可直接调用接口。'
        }
        文档中的示例路径均以 <code>media-core</code> 提供的 HTTP 地址为准。
      </div>
    </section>
    ${Object.entries(docsByCategory)
      .map(
        ([category, items]) => `
          <section class="panel">
            <div class="panel-header">
              <div>
                <h3>${escapeHtml(category)}</h3>
                <p>适合第三方业务系统直接调用的接口清单与示例。</p>
              </div>
            </div>
            <div class="api-doc-grid">
              ${items
                .map(
                  (item) => `
                    <article class="api-doc-card">
                      <div class="toolbar-actions">
                        <span class="pill status-outline">${escapeHtml(item.method)}</span>
                      </div>
                      <h4>${escapeHtml(item.title)}</h4>
                      <p>${escapeHtml(item.summary)}</p>
                      <div class="subtle">${escapeHtml(item.description)}</div>
                      <div class="api-doc-path-row">
                        <code class="api-path selectable">${escapeHtml(item.path)}</code>
                        <button class="ghost-button" data-action="copy" data-value="${escapeAttr(item.path)}">复制路径</button>
                      </div>
                      <div class="toolbar-actions" style="margin-top: 16px;">
                        <button class="button" data-action="open-api-doc" data-api-doc-key="${escapeAttr(apiDocKey(item))}">查看完整参数与返回值</button>
                      </div>
                    </article>
                  `,
                )
                .join("")}
            </div>
          </section>
        `,
      )
      .join("")}
  `;
}

function renderApiDocModal() {
  const doc = getSelectedApiDoc();
  const open = Boolean(doc);
  if (!doc) {
    return `
      <div class="modal-backdrop ${open ? "open" : ""}"></div>
      <section class="modal ${open ? "open" : ""}"></section>
    `;
  }
  const details = API_DOC_DETAILS[apiDocKey(doc)] || {};
  const requestSample = details.requestSample || {};
  return `
    <div class="modal-backdrop open" data-action="close-api-doc"></div>
    <section class="modal api-doc-modal open">
      <div class="section-header">
        <div>
          <div class="brand-mark">接口详情</div>
          <h3>${escapeHtml(doc.title)}</h3>
          <p>${escapeHtml(doc.summary)}</p>
        </div>
        <div class="section-actions">
          <span class="pill status-outline">${escapeHtml(doc.method)}</span>
          <button class="ghost-button" data-action="close-api-doc">关闭</button>
        </div>
      </div>
      <div class="api-doc-modal-grid">
        <div class="doc-stack">
          <div class="doc-block">
            <strong>接口路径</strong>
            <div class="api-doc-path-row">
              <div class="toolbar-actions">
                <span class="pill status-outline">${escapeHtml(doc.method)}</span>
                <code class="api-path selectable">${escapeHtml(doc.path)}</code>
              </div>
              <button class="ghost-button" data-action="copy" data-value="${escapeAttr(doc.path)}">复制路径</button>
            </div>
            <div class="subtle">${escapeHtml(doc.description)}</div>
            ${requestSample.note ? `<div class="subtle">${escapeHtml(requestSample.note)}</div>` : ""}
          </div>
          <div class="doc-block">
            <strong>完整请求示例</strong>
            <div class="subtle doc-example-note">左侧示例会把请求头、路径参数、查询参数和请求体完整展开，方便第三方业务系统直接对照实现。</div>
            ${renderApiExample(buildFullRequestExample(doc, requestSample))}
          </div>
        </div>
        <div class="doc-stack">
          <div class="doc-block">
            <strong>请求参数</strong>
            ${renderFieldDocs(doc.params, "当前接口没有额外的路径、查询或请求头参数。", {
              includeLocation: true,
              exampleResolver: (field) => resolveRequestParamExample(field, requestSample),
            })}
          </div>
          <div class="doc-block">
            <strong>请求体字段</strong>
            ${renderFieldDocs(details.requestFields, "当前接口没有请求体，或请求体仅包含少量覆盖字段。", {
              exampleResolver: (field) => lookupDocValue(requestSample.body, field.name),
            })}
          </div>
          <div class="doc-block">
            <strong>返回值字段</strong>
            ${renderFieldDocs(details.responseFields, "当前接口返回值结构较简单，请参考底部响应示例。", {
              includeRequired: false,
              exampleResolver: (field) => lookupDocValue(doc.responseExample, field.name),
            })}
          </div>
          <div class="doc-block">
            <strong>枚举解释</strong>
            ${renderEnumDocs(details.enums)}
          </div>
        </div>
      </div>
      <div class="doc-block">
        <strong>响应示例</strong>
        ${renderApiExample(doc.responseExample)}
      </div>
    </section>
  `;
}

function renderTasksPage(data) {
  const params = state.route.searchParams;
  const nodeOptions = data.nodes || [];
  const templateLookup = new Map((data.templates || []).map((template) => [template.id, template.name]));
  return `
    <section class="hero-panel">
      <div class="section-header">
        <div>
          <div class="brand-mark">任务</div>
          <h3>任务中心</h3>
          <p>筛选、排序、启动、停止、重试、克隆，并从同一控制台进入详情和调试。</p>
        </div>
        <div class="section-actions">
          ${canAccess("task_write") ? `<button class="button" data-action="open-create-drawer">新建任务</button>` : ""}
        </div>
      </div>
      <form id="tasks-filter-form" class="filters">
        ${renderSelectField("状态", "status", ["", "CREATED", "VALIDATING", "QUEUED", "DISPATCHING", "STARTING", "RUNNING", "STOPPING", "RECOVERING", "SUCCEEDED", "FAILED", "CANCELED", "LOST"], params.get("status") || "", (value) => value ? taskStatusLabel(value) : "全部状态")}
        ${renderSelectField("类型", "type", ["", ...TASK_TYPES.map((item) => item.value)], params.get("type") || "", (value) => value ? taskTypeLabel(value) : "全部类型")}
        ${renderSelectField("节点", "assigned_node_id", ["", ...nodeOptions.map((node) => node.id)], params.get("assigned_node_id") || "", (value) => value === "" ? "全部节点" : nodeLabel(nodeOptions.find((node) => node.id === value)))}
        ${renderTextField("关键字", "keyword", params.get("keyword") || "", "任务名 / 任务 ID")}
        ${renderDateTimeField("创建开始", "created_from", params.get("created_from") || "")}
        ${renderDateTimeField("创建结束", "created_to", params.get("created_to") || "")}
        ${renderSelectField("排序字段", "sort_by", ["", "created_at", "updated_at", "priority", "status"], params.get("sort_by") || "", (value) => sortFieldLabel(value))}
        ${renderSelectField("排序方向", "sort_order", ["", "asc", "desc"], params.get("sort_order") || "", (value) => sortOrderLabel(value))}
        <div class="toolbar-actions">
          <button class="button" type="submit">应用筛选</button>
          <button class="ghost-button" type="button" data-action="reset-task-filters">重置</button>
        </div>
      </form>
    </section>
    <section class="table-panel">
      <div class="table-toolbar">
        <div>
          <h3>任务列表</h3>
          <p>共 ${data.tasksPage.total} 条，当前第 ${data.tasksPage.page} 页。</p>
        </div>
        <div class="toolbar-actions">
          ${renderPager("tasks", data.tasksPage)}
        </div>
      </div>
      <div class="table-wrap">
        <table>
          <thead>
            <tr>
              <th>任务 ID</th>
              <th>名称</th>
              <th>类型</th>
              <th>状态</th>
              <th>优先级</th>
              <th>节点</th>
              <th>模板</th>
              <th>创建人</th>
              <th>创建时间</th>
              <th>更新时间</th>
              <th>操作</th>
            </tr>
          </thead>
          <tbody>
            ${
              data.tasksPage.items.length
                ? data.tasksPage.items
                    .map((task) => {
                      const node = nodeOptions.find((item) => item.id === task.assigned_node_id);
                      return `
                        <tr>
                          <td><a href="/tasks/${task.id}" data-link class="mono">${shortId(task.id)}</a></td>
                          <td>
                            <strong>${escapeHtml(task.name)}</strong>
                            <div class="subtle">第 ${task.current_attempt_no || 0} 次尝试</div>
                          </td>
                          <td><span class="tag">${escapeHtml(taskTypeLabel(task.type))}</span></td>
                          <td>${statusPill(task.status)}</td>
                          <td>${escapeHtml(String(task.priority))}</td>
                          <td>${escapeHtml(nodeLabel(node))}</td>
                          <td>${escapeHtml(templateLookup.get(task.template_id) || task.template_id || "—")}</td>
                          <td>${escapeHtml(task.created_by || "—")}</td>
                          <td>${escapeHtml(formatTime(task.created_at))}</td>
                          <td>${escapeHtml(formatTime(task.updated_at))}</td>
                          <td>${renderTaskActions(task)}</td>
                        </tr>
                      `;
                    })
                    .join("")
                : `<tr><td colspan="11">${renderInlineEmpty("没有命中条件的任务。")}</td></tr>`
            }
          </tbody>
        </table>
      </div>
    </section>
  `;
}

function renderTaskDetailPage(route, data) {
  const detail = data.detail;
  const task = detail.task;
  const params = state.route.searchParams;
  const activeTab = data.activeTab;
  const lastIssue = deriveLastIssue(detail.recent_events);
  const diffPaths = computeDiffPaths(detail.requested_spec, detail.resolved_spec || {});
  return `
    <section class="hero-panel">
      <div class="section-header">
        <div>
          <div class="brand-mark">TASK DETAIL</div>
          <h3>${escapeHtml(task.name)}</h3>
          <p>${escapeHtml(task.id)} · ${escapeHtml(taskTypeLabel(task.type))} · 当前第 ${escapeHtml(String(task.current_attempt_no || 0))} 次尝试</p>
        </div>
        <div class="section-actions">
          ${statusPill(task.status)}
          ${renderTaskActions(task, true)}
        </div>
      </div>
      <div class="overview-grid">
        ${metricCard("当前状态", statusPill(task.status), true)}
        ${metricCard("执行节点", task.assigned_node_id || "未分配")}
        ${metricCard("最近错误", lastIssue || "—")}
        ${metricCard("录像摘要", `${data.recordsPage.total} 条文件记录`)}
        ${metricCard("流绑定摘要", `${data.streams.length} 条流绑定`)}
        ${metricCard("规格差异", `${diffPaths.length} 个差异路径`)}
      </div>
    </section>
    <section class="panel">
      <div class="panel-header">
        <div>
          <h3>详情页签</h3>
          <p>概览、事件、日志、requested_spec、resolved_spec。</p>
        </div>
      <div class="tabs">
          ${renderTaskDetailTab(route.params.id, activeTab, "overview", "概览")}
          ${renderTaskDetailTab(route.params.id, activeTab, "events", "事件")}
          ${renderTaskDetailTab(route.params.id, activeTab, "logs", "日志")}
          ${renderTaskDetailTab(route.params.id, activeTab, "requested", "请求规格")}
          ${renderTaskDetailTab(route.params.id, activeTab, "resolved", "解析规格")}
        </div>
      </div>
      ${
        activeTab === "overview"
          ? renderTaskOverview(detail, data.recordsPage, data.streams, diffPaths)
          : activeTab === "events"
            ? renderTaskEventsTab(route.params.id, data.eventsPage)
            : activeTab === "logs"
              ? renderTaskLogsTab(route.params.id, data.logs, params)
              : activeTab === "requested"
                ? `<pre class="json-block">${escapeHtml(JSON.stringify(detail.requested_spec, null, 2))}</pre>`
                : `<pre class="json-block">${escapeHtml(JSON.stringify(detail.resolved_spec || {}, null, 2))}</pre>`
      }
    </section>
  `;
}

function renderStreamsPage(data) {
  const params = state.route.searchParams;
  const nodeMap = new Map((data.nodes || []).map((node) => [node.id, node]));
  return `
    <section class="hero-panel">
      <div class="section-header">
        <div>
          <div class="brand-mark">流</div>
          <h3>流中心</h3>
          <p>在线流、播放地址、关联任务、观众状态，以及管理员关流操作。</p>
        </div>
      </div>
      <form id="streams-filter-form" class="filters">
        ${renderTextField("协议", "schema", params.get("schema") || "", "rtsp / rtmp / http")}
        ${renderTextField("应用名", "app", params.get("app") || "", "live")}
        ${renderTextField("流名", "stream", params.get("stream") || "", "camera01")}
        ${renderTextField("任务 ID", "task_id", params.get("task_id") || "", "可选")}
        ${renderSelectField("节点", "node_id", ["", ...(data.nodes || []).map((node) => node.id)], params.get("node_id") || "", (value) => value === "" ? "全部节点" : nodeLabel(nodeMap.get(value)))}
        ${renderSelectField("有观众", "has_viewer", ["", "true", "false"], params.get("has_viewer") || "", (value) => value === "" ? "全部" : boolLabel(value))}
        <div class="toolbar-actions">
          <button class="button" type="submit">筛选</button>
          <button class="ghost-button" type="button" data-action="reset-stream-filters">重置</button>
        </div>
      </form>
    </section>
    <section class="table-panel">
      <div class="table-toolbar">
        <div>
          <h3>在线流</h3>
          <p>共 ${data.streams.length} 条。</p>
          <p class="subtle">播放地址表示同一条内部流当前可暴露的协议集合，不代表任务并行输出了多个独立目标。</p>
        </div>
      </div>
      <div class="table-wrap">
        <table>
          <thead>
            <tr>
              <th>协议</th>
              <th>Vhost / 应用 / 流</th>
              <th>任务</th>
              <th>节点</th>
              <th>观众数</th>
              <th>录制状态</th>
              <th>播放地址</th>
              <th>操作</th>
            </tr>
          </thead>
          <tbody>
            ${
              data.streams.length
                ? data.streams
                    .map((stream) => {
                      const task = data.taskDetails.get(stream.task_id);
                      const node = nodeMap.get(stream.node_id);
                      return `
                        <tr>
                          <td><span class="tag">${escapeHtml(schemaLabel(stream.schema))}</span></td>
                          <td>
                            <strong>${escapeHtml(stream.vhost)}</strong>
                            <div class="mono">${escapeHtml(`${stream.app}/${stream.stream}`)}</div>
                          </td>
                          <td><a href="/tasks/${stream.task_id}" data-link class="mono">${shortId(stream.task_id)}</a></td>
                          <td>${escapeHtml(nodeLabel(node))}</td>
                          <td>${escapeHtml(viewerCountLabel(stream.viewer_count, stream.has_viewer))}</td>
                          <td>${escapeHtml(renderRecordingLabel(task))}</td>
                          <td>${renderPlayUrls(stream.play_urls || [])}</td>
                          <td>
                            <div class="toolbar-actions">
                              <a class="ghost-button" href="/tasks/${stream.task_id}" data-link>任务</a>
                              ${canAccess("debug_read") && stream.node_id ? `<button class="danger-button" data-action="close-stream" data-node-id="${stream.node_id}" data-schema="${escapeAttr(stream.schema)}" data-vhost="${escapeAttr(stream.vhost)}" data-app="${escapeAttr(stream.app)}" data-stream="${escapeAttr(stream.stream)}">关流</button>` : ""}
                            </div>
                          </td>
                        </tr>
                      `;
                    })
                    .join("")
                : `<tr><td colspan="8">${renderInlineEmpty("当前没有在线流。")}</td></tr>`
            }
          </tbody>
        </table>
      </div>
    </section>
  `;
}

function renderMulticastPage(data) {
  const nodeMap = new Map((data.nodes || []).map((node) => [node.id, node]));
  return `
    <section class="hero-panel">
      <div class="section-header">
        <div>
          <div class="brand-mark">组播</div>
          <h3>组播中心</h3>
          <p>集中查看组播任务、网卡、TTL、上下游，以及最近错误。</p>
        </div>
      </div>
    </section>
    <section class="table-panel">
      <div class="table-toolbar">
        <div>
          <h3>组播任务</h3>
          <p>共 ${data.tasksPage.total} 条 multicast_bridge 任务。</p>
        </div>
      </div>
      <div class="table-wrap">
        <table>
          <thead>
            <tr>
              <th>任务</th>
              <th>模式</th>
              <th>组播地址</th>
              <th>端口</th>
              <th>绑定地址</th>
              <th>TTL</th>
              <th>节点</th>
              <th>状态</th>
              <th>额外信息</th>
            </tr>
          </thead>
          <tbody>
            ${
              data.tasksPage.items.length
                ? data.tasksPage.items
                    .map((task) => {
                      const detail = data.taskDetails.get(task.id);
                      const spec = detail?.resolved_spec || {};
                      const row = multicastRowModel(
                        task,
                        spec,
                        detail,
                        nodeMap.get(task.assigned_node_id),
                        data.streamsByTask.get(task.id) || [],
                      );
                      return `
                        <tr>
                          <td><a href="/tasks/${task.id}" data-link class="mono">${shortId(task.id)}</a></td>
                          <td>${escapeHtml(row.mode)}</td>
                          <td>${escapeHtml(row.group)}</td>
                          <td>${escapeHtml(row.port)}</td>
                          <td>${escapeHtml(row.interfaceIp)}</td>
                          <td>${escapeHtml(row.ttl)}</td>
                          <td>${escapeHtml(row.node)}</td>
                          <td>${statusPill(task.status)}</td>
                          <td>
                            <div class="subtle">最近码率：${escapeHtml(row.bitrate)}</div>
                            <div class="subtle">最近错误：${escapeHtml(row.lastError)}</div>
                            <div class="subtle">上下游：${escapeHtml(row.binding)}</div>
                          </td>
                        </tr>
                      `;
                    })
                    .join("")
                : `<tr><td colspan="9">${renderInlineEmpty("当前没有 multicast_bridge 任务。")}</td></tr>`
            }
          </tbody>
        </table>
      </div>
    </section>
  `;
}

function renderRecordsPage(data) {
  const params = state.route.searchParams;
  return `
    <section class="hero-panel">
      <div class="section-header">
        <div>
          <div class="brand-mark">录像</div>
          <h3>录像中心</h3>
          <p>按照日期、任务和流名检索录像，并直接复制路径、复制 HTTP 地址或在新标签页打开。</p>
        </div>
      </div>
      <form id="records-filter-form" class="filters">
        ${renderTextField("任务 ID", "task_id", params.get("task_id") || "", "uuid")}
        ${renderTextField("流名", "stream", params.get("stream") || "", "camera01")}
        ${renderDateTimeField("开始时间", "date_from", params.get("date_from") || "")}
        ${renderDateTimeField("结束时间", "date_to", params.get("date_to") || "")}
        <div class="toolbar-actions">
          <button class="button" type="submit">筛选</button>
          <button class="ghost-button" type="button" data-action="reset-record-filters">重置</button>
        </div>
      </form>
    </section>
    <section class="table-panel">
      <div class="table-toolbar">
        <div>
          <h3>录像文件</h3>
          <p>共 ${data.recordsPage.total} 条，当前第 ${data.recordsPage.page} 页。</p>
        </div>
        <div class="toolbar-actions">
          ${renderPager("records", data.recordsPage)}
        </div>
      </div>
      <div class="table-wrap">
        <table>
          <thead>
            <tr>
              <th>录像 ID</th>
              <th>任务</th>
              <th>流</th>
              <th>文件路径</th>
              <th>HTTP 地址</th>
              <th>Size</th>
              <th>时长</th>
              <th>开始时间</th>
              <th>来源</th>
              <th>操作</th>
            </tr>
          </thead>
          <tbody>
            ${
              data.recordsPage.items.length
                ? data.recordsPage.items
                    .map(
                      (record) => `
                        <tr>
                          <td class="mono">${shortId(record.id)}</td>
                          <td><a href="/tasks/${record.task_id}" data-link class="mono">${shortId(record.task_id)}</a></td>
                          <td>${escapeHtml([record.vhost, record.app, record.stream].filter(Boolean).join("/") || "—")}</td>
                          <td><code class="selectable">${escapeHtml(record.file_path)}</code></td>
                          <td>${
                            record.http_url
                              ? `<code class="selectable">${escapeHtml(record.http_url)}</code>`
                              : "—"
                          }</td>
                          <td>${escapeHtml(formatBytes(record.file_size))}</td>
                          <td>${escapeHtml(record.time_len ? `${record.time_len}s` : "—")}</td>
                          <td>${escapeHtml(formatTime(record.start_time || record.created_at))}</td>
                          <td>${escapeHtml(recordSourceLabel(record.source))}</td>
                          <td>
                            <div class="toolbar-actions">
                              <button class="ghost-button" data-action="copy" data-value="${escapeAttr(record.file_path)}">复制路径</button>
                              ${
                                record.http_url
                                  ? `
                                    <button class="ghost-button" data-action="copy" data-value="${escapeAttr(record.http_url)}">复制 HTTP 地址</button>
                                    <a class="ghost-button" href="${escapeAttr(record.http_url)}" target="_blank" rel="noreferrer">打开</a>
                                  `
                                  : ""
                              }
                              <a class="ghost-button" href="/tasks/${record.task_id}" data-link>任务</a>
                            </div>
                          </td>
                        </tr>
                      `,
                    )
                    .join("")
                : `<tr><td colspan="10">${renderInlineEmpty("当前没有录像文件。")}</td></tr>`
            }
          </tbody>
        </table>
      </div>
    </section>
  `;
}

function renderTranscodeArtifactsPage(data) {
  const params = state.route.searchParams;
  return `
    <section class="hero-panel">
      <div class="section-header">
        <div>
          <div class="brand-mark">转码</div>
          <h3>转码产物</h3>
          <p>查询 file_transcode 生成的离线文件，并直接访问工作节点上的 HTTP 文件地址。</p>
        </div>
      </div>
      <form id="transcode-artifacts-filter-form" class="filters">
        ${renderTextField("任务 ID", "task_id", params.get("task_id") || "", "uuid")}
        ${renderDateTimeField("开始时间", "date_from", params.get("date_from") || "")}
        ${renderDateTimeField("结束时间", "date_to", params.get("date_to") || "")}
        <div class="toolbar-actions">
          <button class="button" type="submit">筛选</button>
          <button class="ghost-button" type="button" data-action="reset-transcode-artifact-filters">重置</button>
        </div>
      </form>
    </section>
    <section class="table-panel">
      <div class="table-toolbar">
        <div>
          <h3>离线转码产物</h3>
          <p>共 ${data.artifactsPage.total} 条，当前第 ${data.artifactsPage.page} 页。</p>
        </div>
        <div class="toolbar-actions">
          ${renderPager("transcode-artifacts", data.artifactsPage)}
        </div>
      </div>
      <div class="table-wrap">
        <table>
          <thead>
            <tr>
              <th>产物 ID</th>
              <th>任务</th>
              <th>节点</th>
              <th>文件名</th>
              <th>文件路径</th>
              <th>HTTP 地址</th>
              <th>Size</th>
              <th>创建时间</th>
              <th>操作</th>
            </tr>
          </thead>
          <tbody>
            ${
              data.artifactsPage.items.length
                ? data.artifactsPage.items
                    .map(
                      (artifact) => `
                        <tr>
                          <td class="mono">${shortId(artifact.id)}</td>
                          <td><a href="/tasks/${artifact.task_id}" data-link class="mono">${shortId(artifact.task_id)}</a></td>
                          <td class="mono">${shortId(artifact.node_id)}</td>
                          <td>${escapeHtml(artifact.file_name)}</td>
                          <td><code class="selectable">${escapeHtml(artifact.file_path)}</code></td>
                          <td><code class="selectable">${escapeHtml(artifact.http_url)}</code></td>
                          <td>${escapeHtml(formatBytes(artifact.file_size))}</td>
                          <td>${escapeHtml(formatTime(artifact.created_at))}</td>
                          <td>
                            <div class="toolbar-actions">
                              <button class="ghost-button" data-action="copy" data-value="${escapeAttr(artifact.file_path)}">复制路径</button>
                              <button class="ghost-button" data-action="copy" data-value="${escapeAttr(artifact.http_url)}">复制 HTTP 地址</button>
                              <a class="ghost-button" href="${escapeAttr(artifact.http_url)}" target="_blank" rel="noreferrer">打开</a>
                              <a class="ghost-button" href="/tasks/${artifact.task_id}" data-link>任务</a>
                            </div>
                          </td>
                        </tr>
                      `,
                    )
                    .join("")
                : `<tr><td colspan="9">${renderInlineEmpty("当前没有转码产物。")}</td></tr>`
            }
          </tbody>
        </table>
      </div>
    </section>
  `;
}

function renderNodesPage(data) {
  return `
    <section class="hero-panel">
      <div class="section-header">
        <div>
          <div class="brand-mark">节点</div>
          <h3>节点中心</h3>
          <p>查看节点健康、能力矩阵、实时负载和 ZLM 概览。</p>
        </div>
      </div>
      <div class="metric-grid">
        ${data.nodes.map((node) => renderNodeMetric(node)).join("") || renderInlineEmpty("暂无节点。")}
      </div>
    </section>
    <section class="panel">
      <div class="panel-header">
        <div>
          <h3>节点明细</h3>
          <p>展开单个节点可查看能力矩阵、当前任务和 ZLM 概览。</p>
        </div>
      </div>
      <div class="node-detail-grid">
        ${
          data.nodes.length
            ? data.nodes
                .map((node) => {
                  const insight = state.cache.nodeInsights.get(node.id);
                  const open = state.ui.openNodeId === node.id;
                  return `
                    <article class="node-detail-card">
                      <div class="section-header">
                        <div>
                          <h3>${escapeHtml(node.node_name)}</h3>
                          <p>${escapeHtml(node.hostname)} · ${escapeHtml(networkModeLabel(node.network_mode))} · ${escapeHtml(node.id)}</p>
                        </div>
                        <div class="section-actions">
                          ${node.healthy ? `<span class="pill status-running">健康</span>` : `<span class="pill status-failed">异常</span>`}
                          <button class="ghost-button" data-action="toggle-node-detail" data-node-id="${node.id}">${open ? "收起" : "展开"}</button>
                          <a class="ghost-button" href="/tasks?assigned_node_id=${node.id}" data-link>任务</a>
                        </div>
                      </div>
                      ${
                        open
                          ? renderExpandedNodeInsight(node, insight)
                          : `<div class="subtle">上次心跳：${escapeHtml(formatTime(node.last_seen_at))} · CPU ${formatPercent(node.cpu_percent)} · 内存 ${formatPercent(node.mem_percent)} · 运行任务 ${escapeHtml(String(node.running_tasks ?? 0))}</div>`
                      }
                    </article>
                  `;
                })
                .join("")
            : renderInlineEmpty("暂无节点明细。")
        }
      </div>
    </section>
  `;
}

function renderDebugPage(data) {
  const selectedNode = data.nodes.find((node) => node.id === state.ui.debug.nodeId);
  return `
    <section class="hero-panel">
      <div class="section-header">
        <div>
          <div class="brand-mark">调试</div>
          <h3>调试台</h3>
          <p>管理员专用，封装 ZLM 媒体列表、会话、玩家、关流、抓图和 Hook 排障入口。</p>
        </div>
      </div>
      <div class="form-grid">
        ${renderSelectField("节点", "debug-node-id", ["", ...data.nodes.map((node) => node.id)], state.ui.debug.nodeId || "", (value) => value === "" ? "请选择节点" : nodeLabel(data.nodes.find((node) => node.id === value)), true)}
      </div>
      ${
        selectedNode
          ? `<p class="subtle">当前节点：${escapeHtml(selectedNode.node_name)} · ${escapeHtml(selectedNode.zlm_version || "未知 ZLM 版本")}</p>`
          : `<p class="subtle">先选择一个节点，再执行调试查询。</p>`
      }
    </section>
    <section class="debug-grid">
      <div class="panel">
        <div class="panel-header">
          <div>
            <h3>ZLM 统计</h3>
            <p>对象统计、前台线程负载和后台线程负载。</p>
          </div>
        </div>
        <div class="toolbar-actions">
          <button class="button" data-action="debug-load-statistic">加载统计</button>
        </div>
        <div class="split-grid">
          <div>
            <h4>getStatistic</h4>
            ${renderDebugResult(state.ui.debug.statisticResult)}
          </div>
          <div>
            <h4>线程负载明细</h4>
            ${renderThreadLoadPanel(state.ui.debug.threadsLoadResult, state.ui.debug.workThreadsLoadResult)}
          </div>
        </div>
      </div>
      <div class="panel">
        <div class="panel-header">
          <div>
            <h3>媒体列表</h3>
            <p>按 schema / vhost / app / stream 查询。</p>
          </div>
        </div>
        <form id="debug-media-form" class="form-grid">
          ${renderTextField("协议", "schema", "", "rtsp / rtmp")}
          ${renderTextField("Vhost", "vhost", "", "__defaultVhost__")}
          ${renderTextField("应用名", "app", "", "live")}
          ${renderTextField("流名", "stream", "", "camera01")}
          <div class="toolbar-actions">
            <button class="button" type="submit">查询媒体</button>
          </div>
        </form>
        ${renderDebugResult(state.ui.debug.mediaResult)}
      </div>
      <div class="panel">
        <div class="panel-header">
          <div>
            <h3>会话与播放器</h3>
            <p>读取 ZLM 的全部会话和播放器列表。</p>
          </div>
        </div>
        <div class="toolbar-actions">
          <button class="button" data-action="debug-load-sessions">查询会话</button>
          <button class="soft-button" data-action="debug-load-players">查询播放器</button>
        </div>
        <div class="split-grid">
          <div>
            <h4>会话</h4>
            ${renderDebugResult(state.ui.debug.sessionsResult)}
          </div>
          <div>
            <h4>播放器</h4>
            ${renderDebugResult(state.ui.debug.playersResult)}
          </div>
        </div>
      </div>
      <div class="panel">
        <div class="panel-header">
          <div>
            <h3>执行动作</h3>
            <p>单个踢会话、批量踢会话、主动关流和截图。</p>
          </div>
        </div>
        <form id="debug-kick-form" class="form-grid">
          ${renderTextField("会话 ID", "session_id", "", "必填")}
          <div class="toolbar-actions">
            <button class="danger-button" type="submit">踢会话</button>
          </div>
        </form>
        <form id="debug-kick-batch-form" class="form-grid">
          ${renderTextField("本地端口", "local_port", "", "例如 554")}
          ${renderTextField("对端 IP", "peer_ip", "", "例如 10.0.0.8")}
          <div class="toolbar-actions">
            <button class="danger-button" type="submit">批量踢会话</button>
          </div>
        </form>
        <form id="debug-close-form" class="form-grid">
          ${renderTextField("协议", "schema", "", "rtsp / rtmp / http")}
          ${renderTextField("Vhost", "vhost", "", "__defaultVhost__")}
          ${renderTextField("应用名", "app", "", "live")}
          ${renderTextField("流名", "stream", "", "camera01")}
          <div class="checkbox-field">
            <input id="debug-force-close" type="checkbox" name="force" checked />
            <label for="debug-force-close">强制关闭</label>
          </div>
          <div class="toolbar-actions">
            <button class="danger-button" type="submit">关闭流</button>
          </div>
        </form>
        <form id="debug-snap-form" class="form-grid">
          ${renderTextField("截图地址", "url", "", "rtsp://127.0.0.1/live/camera01")}
          ${renderTextField("超时（秒）", "timeout_sec", "10", "10", "number")}
          ${renderTextField("保留（秒）", "expire_sec", "30", "30", "number")}
          <div class="toolbar-actions">
            <button class="button" type="submit">抓图</button>
          </div>
        </form>
        ${
          state.ui.debug.snapResult?.data_url
            ? `
              <div class="panel" style="margin-top: 16px;">
                <div class="panel-header">
                  <div>
                    <h3>截图结果</h3>
                    <p>${escapeHtml(state.ui.debug.snapResult.content_type || "image/jpeg")}</p>
                  </div>
                  <div class="section-actions">
                    <button class="ghost-button" data-action="copy" data-value="${escapeAttr(state.ui.debug.snapResult.data_url)}">复制 Data URL</button>
                  </div>
                </div>
                <img class="snap-preview" src="${escapeAttr(state.ui.debug.snapResult.data_url)}" alt="ZLM 抓图预览" />
              </div>
            `
            : ""
        }
      </div>
      <div class="panel">
        <div class="panel-header">
          <div>
            <h3>Hook 时间线</h3>
            <p>查看该节点最近收到的 Hook 事件和去重处理状态。</p>
          </div>
        </div>
        <div class="toolbar-actions">
          <button class="button" data-action="debug-load-hooks">加载 Hook 时间线</button>
        </div>
        ${renderHookTimeline(state.ui.debug.hooksResult)}
      </div>
    </section>
  `;
}

function renderTaskOverview(detail, recordsPage, streams, diffPaths) {
  const callback = detail.callback_delivery;
  const callbackTime = callback ? formatTime(callback.delivered_at || callback.updated_at) : "—";
  return `
    <div class="overview-grid">
      <div class="metric">
        <label>当前尝试</label>
        <strong>${escapeHtml(detail.current_attempt ? `${detail.current_attempt.attempt_no}` : "0")}</strong>
        <span class="subtle">${escapeHtml(taskStatusLabel(detail.current_attempt?.status || ""))}</span>
      </div>
      <div class="metric">
        <label>执行节点</label>
        <strong>${escapeHtml(detail.current_attempt?.node_id || detail.task.assigned_node_id || "未分配")}</strong>
        <span class="subtle">${escapeHtml(detail.current_attempt?.worker_kind || taskTypeLabel(detail.task.type))}</span>
      </div>
      <div class="metric">
        <label>录像摘要</label>
        <strong>${escapeHtml(String(recordsPage.total))}</strong>
        <span class="subtle">最近 5 条已加载</span>
      </div>
      <div class="metric">
        <label>流绑定</label>
        <strong>${escapeHtml(String(streams.length))}</strong>
        <span class="subtle">${escapeHtml(streams.map((item) => `${item.app}/${item.stream}`).join(", ") || "暂无")}</span>
      </div>
      <div class="metric">
        <label>最近回调</label>
        <strong>${escapeHtml(callback ? callback.status : "未配置")}</strong>
        <span class="subtle">${escapeHtml(callbackTime)}</span>
      </div>
    </div>
    <div class="split-grid">
      <div class="panel">
        <div class="panel-header">
          <div>
            <h3>最近事件</h3>
            <p>从任务详情自带的 recent_events 展示。</p>
          </div>
        </div>
        <div class="event-list">
          ${
            detail.recent_events.length
              ? detail.recent_events
                  .map(
                    (event) => `
                      <article class="event-item">
                        <div class="toolbar-actions">
                          <span class="tag">${escapeHtml(eventSourceLabel(event.source))}</span>
                          <span class="tag">${escapeHtml(event.event_type)}</span>
                          <span class="subtle">${escapeHtml(formatTime(event.created_at))}</span>
                        </div>
                        <div class="subtle">${escapeHtml(eventLevelLabel(event.event_level))}</div>
                        <pre class="json-block">${escapeHtml(JSON.stringify(event.payload, null, 2))}</pre>
                      </article>
                    `,
                  )
                  .join("")
              : renderInlineEmpty("暂无事件。")
          }
        </div>
      </div>
      <div class="panel">
        <div class="panel-header">
          <div>
            <h3>回调状态</h3>
            <p>展示最近一次任务回调的事件类型、状态和错误摘要。</p>
          </div>
        </div>
        ${
          callback
            ? `
              <div class="detail-grid">
                <div class="detail-item">
                  <label>回调地址</label>
                  <div class="mono">${escapeHtml(callback.callback_url)}</div>
                </div>
                <div class="detail-item">
                  <label>事件类型</label>
                  <div>${escapeHtml(callback.event_type || "task.completed")}</div>
                </div>
                <div class="detail-item">
                  <label>最近状态</label>
                  <div>${escapeHtml(callback.status)}</div>
                </div>
                <div class="detail-item">
                  <label>触发原因</label>
                  <div>${escapeHtml(callback.reason || "terminal_state")}</div>
                </div>
                <div class="detail-item">
                  <label>最近时间</label>
                  <div>${escapeHtml(callbackTime)}</div>
                </div>
                <div class="detail-item">
                  <label>尝试次数</label>
                  <div>${escapeHtml(String(callback.delivery_attempts || 0))}</div>
                </div>
                <div class="detail-item">
                  <label>HTTP 状态</label>
                  <div>${escapeHtml(callback.last_http_status ? String(callback.last_http_status) : "—")}</div>
                </div>
                <div class="detail-item full-width">
                  <label>最近错误</label>
                  <div>${escapeHtml(callback.last_error || "—")}</div>
                </div>
              </div>
            `
            : renderInlineEmpty("当前任务未配置回调。")
        }
      </div>
      <div class="panel">
        <div class="panel-header">
          <div>
            <h3>规格差异</h3>
            <p>请求规格与解析规格之间的差异路径。</p>
          </div>
        </div>
        <div class="diff-list">
          ${
            diffPaths.length
              ? diffPaths.map((path) => `<div class="diff-item mono">${escapeHtml(path)}</div>`).join("")
              : renderInlineEmpty("请求规格与解析规格当前没有差异路径。")
          }
        </div>
      </div>
    </div>
  `;
}

function renderTaskEventsTab(taskId, eventsPage) {
  const params = state.route.searchParams;
  return `
    <form id="task-events-filter-form" class="filters">
      ${renderTextField("尝试号", "attempt_no", params.get("attempt_no") || "", "留空表示全部")}
      ${renderSelectField("来源", "source", ["", "core", "agent", "ffmpeg", "zlm_api", "zlm_hook", "scheduler", "user"], params.get("source") || "", (value) => value ? eventSourceLabel(value) : "全部来源")}
      ${renderTextField("事件类型", "event_type", params.get("event_type") || "", "task_started")}
      <div class="toolbar-actions">
        <button class="button" type="submit">筛选事件</button>
      </div>
    </form>
    <div class="event-list">
      ${
        eventsPage.items.length
          ? eventsPage.items
              .map(
                (event) => `
                  <article class="event-item">
                    <div class="toolbar-actions">
                      <span class="tag">${escapeHtml(eventSourceLabel(event.source))}</span>
                      <span class="tag">${escapeHtml(event.event_type)}</span>
                      <span class="subtle">${escapeHtml(formatTime(event.created_at))}</span>
                    </div>
                    <div class="subtle">尝试 ${escapeHtml(String(event.attempt_no || 0))} · ${escapeHtml(eventLevelLabel(event.event_level))}</div>
                    <pre class="json-block">${escapeHtml(JSON.stringify(event.payload, null, 2))}</pre>
                  </article>
                `,
              )
              .join("")
          : renderInlineEmpty("当前筛选没有事件。")
      }
    </div>
    <div class="pager">${renderPager("task-events", eventsPage, taskId)}</div>
  `;
}

function renderTaskLogsTab(taskId, logs, params) {
  return `
    <form id="task-logs-filter-form" class="filters">
      ${renderTextField("尝试号", "log_attempt_no", params.get("log_attempt_no") || "", "默认当前尝试")}
      ${renderSelectField("日志流", "log_stream", ["merged", "stdout", "stderr"], params.get("log_stream") || "merged", (value) => logStreamLabel(value))}
      ${renderTextField("条数上限", "log_limit", params.get("log_limit") || "200", "1 - 500")}
      <div class="toolbar-actions">
        <button class="button" type="submit">读取日志</button>
      </div>
    </form>
    <pre class="log-block">${escapeHtml(
      logs.lines.length
        ? logs.lines.map((line) => `${formatTime(line.ts)} [${logStreamLabel(line.stream)}] ${line.line}`).join("\n")
        : "当前尝试没有日志。",
    )}</pre>
    ${
      logs.next_cursor
        ? `<div class="pager"><button class="ghost-button" data-action="load-more-logs" data-task-id="${taskId}" data-cursor="${logs.next_cursor}">加载更早日志</button></div>`
        : ""
    }
  `;
}

function renderCreateDrawer() {
  const open = state.ui.createOpen;
  const templates = state.cache.templates || [];
  const draft = state.ui.createDraft;
  return `
    <div class="drawer-backdrop ${open ? "open" : ""}" data-action="close-create-drawer"></div>
    <aside class="drawer ${open ? "open" : ""}">
      <div class="section-header">
        <div>
          <div class="brand-mark">创建任务</div>
          <h3>任务创建向导</h3>
          <p>共 7 步：任务类型、模板、输入源、处理与发布、恢复与调度、规格预览、提交创建。</p>
        </div>
        <div class="section-actions">
          <button class="ghost-button" data-action="close-create-drawer">关闭</button>
        </div>
      </div>
      <div class="wizard-steps">
        ${["类型", "模板", "输入源", "处理发布", "恢复调度", "规格预览", "提交"]
          .map(
            (label, index) => `
              <div class="wizard-step ${state.ui.createStep === index + 1 ? "active" : ""}">
                <strong>0${index + 1}</strong>
                <span>${escapeHtml(label)}</span>
              </div>
            `,
          )
          .join("")}
      </div>
      <div class="panel">
        ${renderCreateStep(state.ui.createStep, draft, templates)}
      </div>
      ${
        state.ui.createError
          ? renderErrorPanel("创建向导错误", errorMessage(state.ui.createError))
          : ""
      }
      <div class="modal-actions">
        <button class="ghost-button" data-action="create-prev-step" ${state.ui.createStep === 1 ? "disabled" : ""}>上一步</button>
        ${
          state.ui.createStep < 7
            ? `<button class="button" data-action="create-next-step">${state.ui.createStep === 6 ? "进入提交" : "下一步"}</button>`
            : `<button class="button" data-action="create-submit">提交创建</button>`
        }
      </div>
    </aside>
  `;
}

function renderCreateStep(step, draft, templates) {
  switch (step) {
    case 1:
      return `
        <div class="create-grid">
          ${renderSelectModelField("任务类型", "task_type", TASK_TYPES.map((item) => item.value), draft.task_type, (value) => taskTypeLabel(value))}
          ${renderTextModelField("任务名称", "name", draft.name, "relay-camera-01")}
          ${renderSelectModelField("任务预设", "profile", PROFILE_OPTIONS, draft.profile, (value) => profileLabel(value, "不使用预设"))}
          ${renderTextModelField("创建人", "common.created_by", draft.common.created_by, "console-user")}
          ${renderTextModelField("优先级", "priority", draft.priority, "0 - 100", "number")}
        </div>
      `;
    case 2: {
      const templateOptions = ["", ...templates.filter((item) => item.type === draft.task_type).map((item) => item.name)];
      return `
        <div class="create-grid">
          ${renderSelectModelField("模板", "template", templateOptions, draft.template || "", (value) => value || "不使用模板")}
          ${renderTextareaModelField("标签", "common.labels_text", draft.common.labels_text, "逗号分隔") }
          ${renderTextModelField("回调地址", "common.callback_url", draft.common.callback_url, "可选")}
        </div>
        <div class="subtle">模板切换会立刻把默认值回填到向导字段，最终提交前仍会通过服务端 <code>resolved_spec</code> 预览再次校验。</div>
      `;
    }
    case 3:
      return renderCreateInputStep(draft);
    case 4:
      return renderCreateProcessStep(draft);
    case 5:
      return renderCreatePolicyStep(draft);
    case 6:
      return `
        <div class="section-header">
          <div>
            <h3>解析规格预览</h3>
            <p>提交前由服务端计算最终规格，方便确认模板和默认值的实际落点。</p>
          </div>
          <div class="section-actions">
            <button class="button" data-action="create-preview">生成预览</button>
          </div>
        </div>
        <div class="field-block">
          <label>高级 JSON 覆盖</label>
          <textarea data-model="advanced_json" placeholder='{"publish":{"enable_hls":true}}'>${escapeHtml(draft.advanced_json)}</textarea>
        </div>
        <pre class="json-block">${escapeHtml(JSON.stringify(state.ui.createPreview?.resolved_spec || {}, null, 2) || "{}")}</pre>
      `;
    case 7:
      return `
        <div class="overview-grid">
          ${metricCard("任务类型", taskTypeLabel(draft.task_type))}
          ${metricCard("任务名称", draft.name || "未填写")}
          ${metricCard("启动模式", startModeLabel(draft.schedule.start_mode || "immediate"))}
          ${metricCard("模板", draft.template || "无")}
        </div>
        <pre class="json-block">${escapeHtml(JSON.stringify(state.ui.createPreview?.resolved_spec || buildDraftPayload(draft), null, 2))}</pre>
        <div class="subtle">提交将调用 <code>POST /api/v1/tasks</code>。如果服务端返回验证错误，会保留当前向导状态。</div>
      `;
    default:
      return "";
  }
}

function renderCreateInputStep(draft) {
  const taskType = draft.task_type;
  const fixedInputKind =
    taskType === "file_transcode"
      ? "file"
      : taskType === "rtp_receive"
        ? "gb_rtp"
        : "";
  const selectableInputKinds =
    taskType === "live_relay"
      ? ["rtsp", "rtmp", "hls", "http_flv", "http_ts"]
      : taskType === "file_to_live"
        ? ["file", "http_mp4", "hls", "http_ts"]
      : taskType === "multicast_bridge"
        ? ["rtsp", "rtmp", "hls", "http_flv", "http_ts", "file", "udp_mpegts_multicast", "rtp_multicast"]
        : INPUT_KINDS;
  const inputKind = fixedInputKind || draft.input.kind || "";
  const showUrl = ["rtsp", "rtmp", "hls", "http_mp4", "http_flv", "http_ts", "file"].includes(inputKind);
  const showMulticastInput = ["udp_mpegts_multicast", "rtp_multicast"].includes(inputKind);
  const showRtpInput = taskType === "rtp_receive";
  return `
    <div class="create-grid">
      ${
        fixedInputKind
          ? renderStaticModelField("输入类型", inputKindLabel(fixedInputKind))
          : renderSelectModelField("输入类型", "input.kind", selectableInputKinds, draft.input.kind || "", (value) => inputKindLabel(value))
      }
      ${showUrl ? renderTextModelField("输入 URL", "input.url", draft.input.url, inputKind === "file" ? "/data/media/input.mp4" : inputKind === "http_mp4" ? "http://vod.example.com/archive.mp4" : "rtsp://camera/live") : ""}
      ${showMulticastInput ? renderTextModelField("组播地址", "input.group", draft.input.group, "239.0.0.1") : ""}
      ${(showMulticastInput || showRtpInput) ? renderTextModelField("端口", "input.port", draft.input.port, showRtpInput ? "30000" : "5004", "number") : ""}
      ${showMulticastInput ? renderTextModelField("绑定网卡名", "input.interface_name", draft.input.interface_name, "留空则使用节点默认组播网卡") : ""}
      ${showMulticastInput ? renderTextModelField("绑定本地地址", "input.interface_ip", draft.input.interface_ip, "可选，本地监听地址") : ""}
      ${showMulticastInput ? renderTextModelField("TTL", "input.ttl", draft.input.ttl, "1", "number") : ""}
      ${taskType !== "rtp_receive" ? renderTextModelField("探测超时（毫秒）", "input.probe_timeout_ms", draft.input.probe_timeout_ms, "7000", "number") : ""}
      ${showRtpInput ? renderTextModelField("TCP 模式", "input.tcp_mode", draft.input.tcp_mode, "0 / 1 / 2", "number") : ""}
      ${showRtpInput ? renderTextModelField("SSRC", "input.ssrc", draft.input.ssrc, "可选", "number") : ""}
      ${(showMulticastInput || showRtpInput) ? renderCheckboxModelField("端口重用", "input.reuse", draft.input.reuse) : ""}
    </div>
  `;
}

function renderCreateProcessStep(draft) {
  const taskType = draft.task_type;
  const publishKind =
    taskType === "file_transcode"
      ? "file"
      : taskType === "file_to_live"
        ? "zlm_ingest"
        : taskType === "rtp_receive" || taskType === "live_relay"
          ? ""
          : draft.publish.kind || "";
  const showProcess = ["file_transcode", "file_to_live", "multicast_bridge"].includes(taskType);
  const showPublishKindSelect = taskType === "multicast_bridge";
  const showPublishUrl = taskType === "file_transcode" || taskType === "file_to_live" || ["file", "zlm_ingest"].includes(publishKind);
  const showPublishNetwork = ["udp_mpegts_multicast", "rtp_multicast"].includes(publishKind);
  const showProtocolFlags = taskType === "live_relay" || taskType === "file_to_live" || taskType === "rtp_receive" || publishKind === "zlm_ingest";
  const publishTargetLabel = publishKind === "file" ? "输出文件路径" : publishKind === "zlm_ingest" ? "推流目标 URL" : "发布 URL";
  const publishTargetPlaceholder = publishKind === "file" ? "/data/media/output.mp4" : "rtmp://zlm/live/stream";
  const publishFormatPlaceholder =
    showPublishNetwork
      ? publishKind === "rtp_multicast"
        ? "rtp_mpegts"
        : "mpegts"
      : publishKind === "file"
        ? "可留空，按文件名推断"
        : publishKind === "zlm_ingest"
          ? "可留空，按 URL 协议推断"
          : "mpegts";
  const directOutputHint = showPublishUrl
    ? publishKind === "file"
      ? `<div class="subtle">当前任务会直接写文件；这里填最终输出文件路径。<code>输出封装格式</code>通常可以留空，系统会按文件名自动推断。</div>`
      : `<div class="subtle">当前任务会把结果直接推到目标地址；这里填实际推流 URL。<code>输出封装格式</code>通常可以留空，系统会按 URL 协议自动推断。</div>`
    : showPublishNetwork
      ? `<div class="subtle">当前任务会直接发组播；填写目标组播地址和端口即可，<code>输出封装格式</code>通常保持默认。</div>`
      : `<div class="subtle">当前任务默认只维护内部流，不额外指定一个直接输出目标。</div>`;
  return `
    <div class="create-grid">
      ${showProcess ? renderTextModelField("处理模式", "process.mode", draft.process.mode, "copy_or_transcode") : ""}
      ${showProcess ? renderTextModelField("目标码率", "process.bitrate", draft.process.bitrate, "2000", "number") : ""}
      ${showProcess ? renderTextModelField("帧率", "process.fps", draft.process.fps, "25", "number") : ""}
      ${showProcess ? renderTextModelField("GOP", "process.gop", draft.process.gop, "50", "number") : ""}
      ${showPublishKindSelect ? renderSelectModelField("直接输出目标", "publish.kind", ["", ...PUBLISH_KINDS], draft.publish.kind || "", (value) => value ? publishKindLabel(value) : "仅内部流（不额外输出）") : renderStaticModelField("直接输出目标", publishKindLabel(publishKind, "仅内部流"))}
      ${showPublishUrl ? renderTextModelField(publishTargetLabel, "publish.url", draft.publish.url, publishTargetPlaceholder) : ""}
      ${showPublishNetwork ? renderTextModelField("发布组播地址", "publish.group", draft.publish.group, "239.1.1.10") : ""}
      ${showPublishNetwork ? renderTextModelField("发布端口", "publish.port", draft.publish.port, "1234", "number") : ""}
      ${showPublishNetwork ? renderTextModelField("发布网卡名", "publish.interface_name", draft.publish.interface_name, "留空则使用节点默认组播网卡") : ""}
      ${showPublishNetwork ? renderTextModelField("发布本地地址", "publish.interface_ip", draft.publish.interface_ip, "可选，本地发送地址") : ""}
      ${showPublishNetwork ? renderTextModelField("发布 TTL", "publish.ttl", draft.publish.ttl, "1", "number") : ""}
      ${showPublishUrl || showPublishNetwork ? renderTextModelField("输出封装格式（可选）", "publish.format", draft.publish.format, publishFormatPlaceholder) : ""}
      ${showProtocolFlags ? renderCheckboxModelField("enable_rtsp", "publish.enable_rtsp", draft.publish.enable_rtsp) : ""}
      ${showProtocolFlags ? renderCheckboxModelField("enable_rtmp", "publish.enable_rtmp", draft.publish.enable_rtmp) : ""}
      ${showProtocolFlags ? renderCheckboxModelField("enable_http_ts", "publish.enable_http_ts", draft.publish.enable_http_ts) : ""}
      ${showProtocolFlags ? renderCheckboxModelField("enable_http_fmp4", "publish.enable_http_fmp4", draft.publish.enable_http_fmp4) : ""}
      ${showProtocolFlags ? renderCheckboxModelField("enable_hls", "publish.enable_hls", draft.publish.enable_hls) : ""}
      ${showProtocolFlags ? renderCheckboxModelField("无人观看自动停止", "publish.stop_on_no_reader", draft.publish.stop_on_no_reader) : ""}
    </div>
    ${directOutputHint}
    ${
      showProtocolFlags
        ? `<div class="subtle">这些 <code>publish.enable_*</code> 开关只控制内部流额外暴露哪些播放协议，不会新增一个直接输出目标。例：<code>input.kind=http_ts</code> 是 HTTP-TS 输入源，<code>publish.enable_http_ts=true</code> 是内部流暴露 HTTP-TS 播放地址。</div>`
        : ""
    }
  `;
}

function renderCreatePolicyStep(draft) {
  const showRecordFields = Boolean(draft.record.enabled);
  const startMode = draft.schedule.start_mode || "immediate";
  return `
    <div class="create-grid">
      ${renderCheckboxModelField("启用录制", "record.enabled", draft.record.enabled)}
      ${showRecordFields ? renderSelectModelField("录制格式", "record.format", ["", ...RECORD_FORMATS], draft.record.format || "", (value) => value ? recordFormatLabel(value) : "默认") : ""}
      ${showRecordFields ? renderTextModelField("录制总时长（秒）", "record.duration_sec", draft.record.duration_sec, "300", "number") : ""}
      ${showRecordFields ? renderTextModelField("录制切片秒数", "record.segment_sec", draft.record.segment_sec, "60", "number") : ""}
      ${showRecordFields ? renderTextModelField("录制路径", "record.save_path", draft.record.save_path, "/data/zlm/record") : ""}
      ${showRecordFields ? renderCheckboxModelField("按播放器口径记账", "record.as_player", draft.record.as_player) : ""}
      ${renderSelectModelField("恢复策略", "recovery.policy", ["", ...RECOVERY_POLICIES], draft.recovery.policy || "", (value) => value ? recoveryPolicyLabel(value) : "默认")}
      ${renderTextModelField("恢复模式", "recovery.resume_mode", draft.recovery.resume_mode, "auto")}
      ${renderTextModelField("最大连续失败", "recovery.max_consecutive_failures", draft.recovery.max_consecutive_failures, "3", "number")}
      ${renderSelectModelField("启动模式", "schedule.start_mode", START_MODES, draft.schedule.start_mode, (value) => startModeLabel(value))}
      ${startMode === "at" ? renderTextModelField("指定启动时间", "schedule.start_at", draft.schedule.start_at, "2026-03-30T12:00:00Z") : ""}
      ${startMode === "cron" ? renderTextModelField("Cron 表达式", "schedule.cron", draft.schedule.cron, "0 */5 * * * *") : ""}
      ${renderTextareaModelField("必需标签", "resource.required_labels_text", draft.resource.required_labels_text, "逗号分隔")}
      ${renderTextareaModelField("优选标签", "resource.preferred_labels_text", draft.resource.preferred_labels_text, "逗号分隔")}
    </div>
  `;
}

function renderAuthModal() {
  const open = state.ui.authModalOpen;
  return `
    <div class="modal-backdrop ${open ? "open" : ""}" data-action="close-auth-modal"></div>
    <section class="modal ${open ? "open" : ""}">
      <div class="section-header">
        <div>
          <div class="brand-mark">认证</div>
          <h3>访问令牌</h3>
          <p>兼容 <code>external_jwt</code> 模式。这里输入的 Bearer Token 只保存在当前页面内存，不会写入本地存储。</p>
        </div>
        <div class="section-actions">
          <button class="ghost-button" data-action="close-auth-modal">关闭</button>
        </div>
      </div>
      <div class="field-block">
        <label for="auth-token-input">访问令牌（Bearer）</label>
        <textarea id="auth-token-input" data-action="auth-token-input" placeholder="eyJhbGciOi..." rows="8">${escapeHtml(state.ui.authDraftToken || state.token)}</textarea>
      </div>
      <div class="modal-actions">
        <button class="ghost-button" data-action="clear-auth-token">清空</button>
        <button class="button" data-action="save-auth-token">保存并刷新</button>
      </div>
    </section>
  `;
}

function renderToasts() {
  return `
    <div class="toast-stack">
      ${state.toasts
        .map(
          (item) => `
            <article class="toast ${item.kind}">
              <strong>${escapeHtml(item.title)}</strong>
              <div class="subtle">${escapeHtml(item.message)}</div>
            </article>
          `,
        )
        .join("")}
    </div>
  `;
}

function getSelectedApiDoc() {
  if (!state.ui.apiDocModalKey) {
    return null;
  }
  return EXTERNAL_API_DOCS.find((item) => apiDocKey(item) === state.ui.apiDocModalKey) || null;
}

async function handleClick(event) {
  const link = event.target.closest("[data-link]");
  if (link) {
    if (link.getAttribute("aria-disabled") === "true") {
      event.preventDefault();
    }
    return;
  }

  const actionTarget = event.target.closest("[data-action]");
  if (!actionTarget) {
    return;
  }
  const action = actionTarget.dataset.action;
  try {
    switch (action) {
      case "refresh-page":
        await refreshSession(true);
        await refreshRoute();
        break;
      case "set-theme":
        state.ui.themePreference = actionTarget.dataset.themeValue || "system";
        window.localStorage.setItem(THEME_STORAGE_KEY, state.ui.themePreference);
        applyTheme(state.ui.themePreference);
        renderApp({ chrome: true, page: false, overlays: false, toasts: false });
        break;
      case "open-api-doc":
        state.ui.apiDocModalKey = actionTarget.dataset.apiDocKey || "";
        renderApp({ chrome: false, page: false, overlays: true, toasts: false });
        break;
      case "close-api-doc":
        state.ui.apiDocModalKey = "";
        renderApp({ chrome: false, page: false, overlays: true, toasts: false });
        break;
      case "open-auth-modal":
        state.ui.authDraftToken = state.token;
        state.ui.authModalOpen = true;
        renderApp({ chrome: false, page: false, overlays: true, toasts: false });
        break;
      case "close-auth-modal":
        state.ui.authModalOpen = false;
        renderApp({ chrome: false, page: false, overlays: true, toasts: false });
        break;
      case "save-auth-token":
        state.token = (state.ui.authDraftToken || "").trim();
        state.ui.authModalOpen = false;
        await refreshSession(false);
        await refreshRoute();
        break;
      case "clear-auth-token":
        state.ui.authDraftToken = "";
        clearAuthTokens();
        await refreshSession(true);
        renderApp({ chrome: true, page: false, overlays: true, toasts: false });
        break;
      case "logout":
        if (state.refreshToken) {
          try {
            await apiRequest("/api/v1/auth/logout", {
              method: "POST",
              skipAuth: true,
              body: { refresh_token: state.refreshToken },
            });
          } catch (error) {
            console.error(error);
          }
        }
        clearAuthTokens({ clearRefresh: true });
        state.session = null;
        state.sessionError = null;
        await navigate("/login");
        break;
      case "open-create-drawer":
        if (!canAccess("task_write")) {
          return;
        }
        await ensureTemplatesLoaded();
        state.ui.createOpen = true;
        state.ui.createError = null;
        renderApp({ chrome: false, page: false, overlays: true, toasts: false });
        break;
      case "close-create-drawer":
        state.ui.createOpen = false;
        renderApp({ chrome: false, page: false, overlays: true, toasts: false });
        break;
      case "create-prev-step":
        state.ui.createStep = Math.max(1, state.ui.createStep - 1);
        renderApp({ chrome: false, page: false, overlays: true, toasts: false });
        break;
      case "create-next-step":
        if (state.ui.createStep === 6 && !state.ui.createPreview) {
          await requestTaskPreview();
        }
        if (state.ui.createStep < 7) {
          state.ui.createStep += 1;
        }
        renderApp({ chrome: false, page: false, overlays: true, toasts: false });
        break;
      case "create-preview":
        await requestTaskPreview();
        renderApp({ chrome: false, page: false, overlays: true, toasts: false });
        break;
      case "create-submit":
        await submitTaskCreate();
        break;
      case "reset-task-filters":
        await navigate("/tasks");
        break;
      case "reset-stream-filters":
        await navigate("/streams");
        break;
      case "reset-record-filters":
        await navigate("/records");
        break;
      case "reset-transcode-artifact-filters":
        await navigate("/transcode-artifacts");
        break;
      case "load-more-logs":
        await updateTaskDetailQuery({
          tab: "logs",
          log_cursor: actionTarget.dataset.cursor,
        });
        break;
      case "task-start":
        await performTaskAction(actionTarget.dataset.taskId, "start");
        break;
      case "task-stop":
        await performTaskAction(actionTarget.dataset.taskId, "stop");
        break;
      case "task-cancel":
        await performTaskAction(actionTarget.dataset.taskId, "cancel");
        break;
      case "task-retry":
        await performTaskAction(actionTarget.dataset.taskId, "retry");
        break;
      case "task-clone":
        await cloneTask(actionTarget.dataset.taskId);
        break;
      case "copy":
        await copyText(actionTarget.dataset.value || "");
        break;
      case "close-stream":
        await closeStream(actionTarget.dataset);
        break;
      case "toggle-node-detail":
        await toggleNodeInsight(actionTarget.dataset.nodeId);
        break;
      case "debug-load-sessions":
        await loadDebugSessions();
        break;
      case "debug-load-players":
        await loadDebugPlayers();
        break;
      case "debug-load-statistic":
        await loadDebugStatistics();
        break;
      case "debug-load-hooks":
        await loadDebugHooks();
        break;
      default:
        break;
    }
  } catch (error) {
    console.error(error);
    toast(errorMessage(error), "error");
  }
}

async function handleSubmit(event) {
  const form = event.target;
  if (!(form instanceof HTMLFormElement)) {
    return;
  }
  event.preventDefault();
  try {
    switch (form.id) {
      case "login-form":
        await submitLogin(new FormData(form));
        break;
      case "tasks-filter-form":
        await navigate(`/tasks?${buildQueryString(new FormData(form))}`);
        break;
      case "streams-filter-form":
        await navigate(`/streams?${buildQueryString(new FormData(form))}`);
        break;
      case "records-filter-form":
        await navigate(`/records?${buildQueryString(new FormData(form))}`);
        break;
      case "transcode-artifacts-filter-form":
        await navigate(`/transcode-artifacts?${buildQueryString(new FormData(form))}`);
        break;
      case "task-events-filter-form":
        await updateTaskDetailQuery({
          tab: "events",
          page: "1",
          attempt_no: formValue(form, "attempt_no"),
          source: formValue(form, "source"),
          event_type: formValue(form, "event_type"),
        });
        break;
      case "task-logs-filter-form":
        await updateTaskDetailQuery({
          tab: "logs",
          log_attempt_no: formValue(form, "log_attempt_no"),
          log_stream: formValue(form, "log_stream"),
          log_limit: formValue(form, "log_limit"),
          log_cursor: "",
        });
        break;
      case "debug-media-form":
        await loadDebugMedia(new FormData(form));
        break;
      case "debug-kick-form":
        await kickDebugSession(new FormData(form));
        break;
      case "debug-kick-batch-form":
        await submitDebugKickBatch(new FormData(form));
        break;
      case "debug-close-form":
        await submitDebugClose(new FormData(form));
        break;
      case "debug-snap-form":
        await submitDebugSnap(new FormData(form));
        break;
      case "change-password-form":
        await submitPasswordChange(new FormData(form));
        break;
      case "machine-allowlist-form":
        await submitMachineAllowlist(new FormData(form));
        break;
      default:
        break;
    }
  } catch (error) {
    console.error(error);
    toast(errorMessage(error), "error");
  }
}

async function handleChange(event) {
  const target = event.target;
  if (!(target instanceof HTMLElement)) {
    return;
  }
  try {
    if (target.dataset.model) {
      updateCreateDraftFromElement(target);
      if (shouldRerenderCreateModalOnModelChange(target)) {
        renderApp({ chrome: false, page: false, overlays: true, toasts: false });
      }
      if (target.dataset.model === "template" && target instanceof HTMLSelectElement) {
        await applySelectedTemplate(target.value);
      }
    }
    if (target.id === "debug-node-id" && target instanceof HTMLSelectElement) {
      state.ui.debug.nodeId = target.value;
      state.ui.debug.mediaResult = null;
      state.ui.debug.sessionsResult = null;
      state.ui.debug.playersResult = null;
      state.ui.debug.statisticResult = null;
      state.ui.debug.threadsLoadResult = null;
      state.ui.debug.workThreadsLoadResult = null;
      state.ui.debug.snapResult = null;
      state.ui.debug.hooksResult = null;
      renderApp({ chrome: false, page: true, overlays: false, toasts: false });
    }
  } catch (error) {
    console.error(error);
    toast(errorMessage(error), "error");
  }
}

function handleInput(event) {
  const target = event.target;
  if (!(target instanceof HTMLElement)) {
    return;
  }
  if (target.dataset.model) {
    updateCreateDraftFromElement(target);
  }
  if (target.dataset.action === "auth-token-input" && target instanceof HTMLTextAreaElement) {
    state.ui.authDraftToken = target.value;
  }
  if (target.dataset.action === "machine-allowlist-input" && target instanceof HTMLTextAreaElement) {
    state.ui.securityAllowlistText = target.value;
    state.ui.securityAllowlistDirty = true;
  }
}

function updateCreateDraftFromElement(target) {
  const path = target.dataset.model;
  if (!path) {
    return;
  }
  const value =
    target instanceof HTMLInputElement && target.type === "checkbox"
      ? target.checked
      : target.value;
  setPath(state.ui.createDraft, path, value);
  if (path === "task_type") {
    normalizeDraftForTaskType(state.ui.createDraft, value);
    state.ui.createDraft.template = "";
  }
  state.ui.createPreview = null;
  state.ui.createError = null;
}

function shouldRerenderCreateModalOnModelChange(target) {
  if (!state.ui.createOpen || !target.dataset.model) {
    return false;
  }
  if (target instanceof HTMLSelectElement) {
    return true;
  }
  return target instanceof HTMLInputElement && target.type === "checkbox";
}

async function navigate(href) {
  window.history.pushState({}, "", href);
  state.route = parseRoute(window.location.pathname, window.location.search);
  await refreshRoute();
}

function parseRoute(pathname, search) {
  const cleanPath = pathname || "/overview";
  const searchParams = new URLSearchParams(search || "");
  const taskMatch = cleanPath.match(/^\/tasks\/([^/]+)$/);
  if (taskMatch) {
    return { name: "task-detail", path: cleanPath, searchParams, params: { id: taskMatch[1] } };
  }
  if (cleanPath === "/login") return { name: "login", path: cleanPath, searchParams, params: {} };
  if (cleanPath === "/overview") return { name: "overview", path: cleanPath, searchParams, params: {} };
  if (cleanPath === "/api-docs") return { name: "api-docs", path: cleanPath, searchParams, params: {} };
  if (cleanPath === "/streams") return { name: "streams", path: cleanPath, searchParams, params: {} };
  if (cleanPath === "/multicast") return { name: "multicast", path: cleanPath, searchParams, params: {} };
  if (cleanPath === "/records") return { name: "records", path: cleanPath, searchParams, params: {} };
  if (cleanPath === "/transcode-artifacts") return { name: "transcode-artifacts", path: cleanPath, searchParams, params: {} };
  if (cleanPath === "/security") return { name: "security", path: cleanPath, searchParams, params: {} };
  if (cleanPath === "/nodes") return { name: "nodes", path: cleanPath, searchParams, params: {} };
  if (cleanPath.startsWith("/debug")) return { name: "debug", path: cleanPath, searchParams, params: {} };
  if (cleanPath.startsWith("/tasks")) return { name: "tasks", path: "/tasks", searchParams, params: {} };
  return { name: "overview", path: "/overview", searchParams, params: {} };
}

function currentRouteTitle() {
  return LABELS.route[state.route.name] || "系统总览";
}

function currentRouteSubtitle() {
  switch (state.route.name) {
    case "login":
      return "本地账号登录与会话恢复入口。";
    case "overview":
      return "系统介绍、整体状态、节点健康、在线流与最近任务动态。";
    case "api-docs":
      return "仅保留第三方业务系统需要调用的北向接口，并附示例。";
    case "task-detail":
      return "聚焦单个任务的运行状态、事件、日志和规格差异。";
    case "streams":
      return "查看在线流、播放地址、观众数和流关闭操作。";
    case "multicast":
      return "聚合组播任务、上下游绑定、网卡和最近异常。";
    case "records":
      return "按任务、流和时间检索录像文件，并直接打开或复制 HTTP 地址。";
    case "transcode-artifacts":
      return "查询 file_transcode 生成的离线产物，并直接打开工作节点上的 HTTP 文件地址。";
    case "security":
      return "修改当前密码，并维护允许直连业务 API 的机器 IP 白名单。";
    case "nodes":
      return "查看节点健康、能力矩阵、当前任务和 ZLM 概览。";
    case "debug":
      return "提供管理员调试入口，覆盖会话、媒体、玩家、关流和抓图。";
    case "tasks":
      return "任务筛选、创建、派发、回溯和批量运维入口。";
    default:
      return "任务筛选、创建、派发、回溯和批量运维入口。";
  }
}

function canAccess(permission) {
  if (!permission) {
    return true;
  }
  if (!state.session) {
    return !state.sessionError;
  }
  return state.session.permissions.includes(permission);
}

function sessionSubtitle() {
  if (!state.session) {
    return state.sessionError ? errorMessage(state.sessionError) : "正在建立会话";
  }
  return `${apiRoleLabel(state.session.role)} · ${state.session.auth_mode || "disabled"}`;
}

async function apiRequest(path, options = {}) {
  const headers = new Headers(options.headers || {});
  if (!options.skipAuth && state.token) {
    headers.set("Authorization", `Bearer ${state.token}`);
  }
  let body = options.body;
  if (body && typeof body === "object" && !(body instanceof FormData) && !(body instanceof Blob)) {
    headers.set("Content-Type", "application/json");
    body = JSON.stringify(body);
  }

  const response = await fetch(path, {
    method: options.method || "GET",
    headers,
    body,
  });
  const contentType = response.headers.get("content-type") || "";
  const payload =
    response.status === 204
      ? null
      : contentType.includes("application/json")
        ? await response.json()
        : await response.text();
  if (!response.ok) {
    const error = new Error(
      payload?.message || `HTTP ${response.status}`,
    );
    error.status = response.status;
    error.payload = payload;
    throw error;
  }
  return payload;
}

async function fetchTaskDetail(taskId, force) {
  if (!force && state.cache.taskDetails.has(taskId)) {
    return state.cache.taskDetails.get(taskId);
  }
  const detail = await apiRequest(`/api/v1/tasks/${taskId}`);
  state.cache.taskDetails.set(taskId, detail);
  return detail;
}

async function fetchNodesCached(force) {
  if (!force && state.cache.nodes) {
    return state.cache.nodes;
  }
  const nodes = await apiRequest("/api/v1/nodes");
  state.cache.nodes = nodes;
  return nodes;
}

async function fetchTemplatesCached(force) {
  if (!force && state.cache.templates) {
    return state.cache.templates;
  }
  const templates = await apiRequest("/api/v1/templates");
  state.cache.templates = templates;
  return templates;
}

async function fetchTemplateDetail(templateId, force) {
  if (!force && state.cache.templateDetails.has(templateId)) {
    return state.cache.templateDetails.get(templateId);
  }
  const detail = await apiRequest(`/api/v1/templates/${templateId}`);
  state.cache.templateDetails.set(templateId, detail);
  return detail;
}

async function ensureTemplatesLoaded() {
  if (!canAccess("template_read")) {
    state.cache.templates = [];
    return [];
  }
  return await fetchTemplatesCached(true);
}

async function applySelectedTemplate(templateName) {
  if (!templateName) {
    return;
  }
  const summary = (state.cache.templates || []).find((item) => item.name === templateName);
  if (!summary) {
    return;
  }
  const detail = await fetchTemplateDetail(summary.id, false);
  applyTaskSpecDefaultsToDraft(state.ui.createDraft, detail.default_spec || {});
  if (detail.profile) {
    state.ui.createDraft.profile = detail.profile;
  }
  state.ui.createPreview = null;
  state.ui.createError = null;
  renderApp({ chrome: false, page: false, overlays: true, toasts: false });
}

async function performTaskAction(taskId, action) {
  if (!taskId) {
    return;
  }
  const confirmed = window.confirm(`确认执行“${taskActionLabel(action)}”吗？`);
  if (!confirmed) {
    return;
  }
  await apiRequest(`/api/v1/tasks/${taskId}/${action}`, {
    method: "POST",
  });
  toast(`任务 ${shortId(taskId)} 已执行${taskActionLabel(action)}`, "success");
  state.cache.taskDetails.delete(taskId);
  await refreshRoute();
}

async function cloneTask(taskId) {
  const name = window.prompt("请输入克隆后的任务名称", "task-copy");
  if (!name) {
    return;
  }
  const cloned = await apiRequest(`/api/v1/tasks/${taskId}/clone`, {
    method: "POST",
    body: { name },
  });
  toast(`已克隆任务 ${shortId(cloned.id)}`, "success");
  await navigate(`/tasks/${cloned.id}`);
}

async function submitLogin(formData) {
  const username = String(formData.get("username") || "").trim();
  const password = String(formData.get("password") || "");
  const destination = sanitizeReturnTo(state.route.searchParams.get("next"));
  const tokens = await apiRequest("/api/v1/auth/login", {
    method: "POST",
    skipAuth: true,
    body: { username, password },
  });
  applyAuthTokens(tokens);
  await refreshSession(false);
  toast(`已登录 ${tokens.subject || username}`, "success");
  await navigate(destination);
}

async function submitPasswordChange(formData) {
  const currentPassword = String(formData.get("current_password") || "");
  const newPassword = String(formData.get("new_password") || "");
  await apiRequest("/api/v1/auth/change-password", {
    method: "POST",
    body: {
      current_password: currentPassword,
      new_password: newPassword,
    },
  });
  clearAuthTokens({ clearRefresh: true });
  state.session = null;
  state.sessionError = null;
  toast("密码已更新，请重新登录", "success");
  await navigate("/login");
}

async function submitMachineAllowlist(formData) {
  const raw = String(formData.get("entries_text") || "").trim();
  const parsed = parseMachineAllowlistText(raw);
  const response = await apiRequest("/api/v1/security/machine-allowlist", {
    method: "PUT",
    body: { entries: parsed },
  });
  state.ui.securityAllowlistDirty = false;
  state.ui.securityAllowlistText = formatMachineAllowlistEntries(response.entries || []);
  toast("机器 API 白名单已更新", "success");
  await refreshRoute({ preserveScroll: true });
}

async function closeStream(data) {
  const confirmed = window.confirm(`确认关闭流 ${data.app}/${data.stream} 吗？`);
  if (!confirmed) {
    return;
  }
  await apiRequest("/api/v1/debug/zlm/close-stream", {
    method: "POST",
    body: {
      node_id: data.nodeId,
      schema: data.schema,
      vhost: data.vhost,
      app: data.app,
      stream: data.stream,
      force: true,
    },
  });
  toast(`已请求关闭 ${data.app}/${data.stream}`, "success");
  await refreshRoute();
}

async function toggleNodeInsight(nodeId) {
  if (state.ui.openNodeId === nodeId) {
    state.ui.openNodeId = "";
    renderApp({ chrome: false, page: true, overlays: false, toasts: false });
    return;
  }
  state.ui.openNodeId = nodeId;
  renderApp({ chrome: false, page: true, overlays: false, toasts: false });
  const insight = await loadNodeInsight(nodeId);
  state.cache.nodeInsights.set(nodeId, insight);
  renderApp({ chrome: false, page: true, overlays: false, toasts: false });
}

async function loadNodeInsight(nodeId) {
  const [tasksPage, heartbeats, media, sessions, players, statistic, threadsLoad, workThreadsLoad] = await Promise.all([
    apiRequest(`/api/v1/tasks?assigned_node_id=${encodeURIComponent(nodeId)}&page_size=6&sort_by=updated_at&sort_order=desc`),
    apiRequest(`/api/v1/nodes/${encodeURIComponent(nodeId)}/heartbeats?limit=12`).catch(() => []),
    canAccess("debug_read") ? apiRequest(`/api/v1/debug/zlm/media?node_id=${encodeURIComponent(nodeId)}`).catch(() => null) : Promise.resolve(null),
    canAccess("debug_read") ? apiRequest(`/api/v1/debug/zlm/sessions?node_id=${encodeURIComponent(nodeId)}`).catch(() => null) : Promise.resolve(null),
    canAccess("debug_read") ? apiRequest(`/api/v1/debug/zlm/players?node_id=${encodeURIComponent(nodeId)}`).catch(() => null) : Promise.resolve(null),
    canAccess("debug_read") ? apiRequest(`/api/v1/debug/zlm/statistic?node_id=${encodeURIComponent(nodeId)}`).catch(() => null) : Promise.resolve(null),
    canAccess("debug_read") ? apiRequest(`/api/v1/debug/zlm/threads-load?node_id=${encodeURIComponent(nodeId)}`).catch(() => null) : Promise.resolve(null),
    canAccess("debug_read") ? apiRequest(`/api/v1/debug/zlm/work-threads-load?node_id=${encodeURIComponent(nodeId)}`).catch(() => null) : Promise.resolve(null),
  ]);
  return { tasksPage, heartbeats, media, sessions, players, statistic, threadsLoad, workThreadsLoad };
}

async function loadDebugMedia(formData) {
  ensureDebugNode();
  const query = new URLSearchParams({ node_id: state.ui.debug.nodeId });
  ["schema", "vhost", "app", "stream"].forEach((key) => {
    const value = (formData.get(key) || "").toString().trim();
    if (value) {
      query.set(key, value);
    }
  });
  state.ui.debug.mediaResult = await apiRequest(`/api/v1/debug/zlm/media?${query.toString()}`);
  state.ui.debug.lastError = null;
  renderApp({ chrome: false, page: true, overlays: false, toasts: false });
}

async function loadDebugSessions() {
  ensureDebugNode();
  state.ui.debug.sessionsResult = await apiRequest(`/api/v1/debug/zlm/sessions?node_id=${encodeURIComponent(state.ui.debug.nodeId)}`);
  state.ui.debug.lastError = null;
  renderApp({ chrome: false, page: true, overlays: false, toasts: false });
}

async function loadDebugPlayers() {
  ensureDebugNode();
  state.ui.debug.playersResult = await apiRequest(`/api/v1/debug/zlm/players?node_id=${encodeURIComponent(state.ui.debug.nodeId)}`);
  state.ui.debug.lastError = null;
  renderApp({ chrome: false, page: true, overlays: false, toasts: false });
}

async function loadDebugStatistics() {
  ensureDebugNode();
  const [statistic, threadsLoad, workThreadsLoad] = await Promise.all([
    apiRequest(`/api/v1/debug/zlm/statistic?node_id=${encodeURIComponent(state.ui.debug.nodeId)}`),
    apiRequest(`/api/v1/debug/zlm/threads-load?node_id=${encodeURIComponent(state.ui.debug.nodeId)}`),
    apiRequest(`/api/v1/debug/zlm/work-threads-load?node_id=${encodeURIComponent(state.ui.debug.nodeId)}`),
  ]);
  state.ui.debug.statisticResult = statistic;
  state.ui.debug.threadsLoadResult = threadsLoad;
  state.ui.debug.workThreadsLoadResult = workThreadsLoad;
  state.ui.debug.lastError = null;
  renderApp({ chrome: false, page: true, overlays: false, toasts: false });
}

async function loadDebugHooks() {
  ensureDebugNode();
  state.ui.debug.hooksResult = await apiRequest(`/api/v1/debug/hooks?node_id=${encodeURIComponent(state.ui.debug.nodeId)}&limit=40`);
  state.ui.debug.lastError = null;
  renderApp({ chrome: false, page: true, overlays: false, toasts: false });
}

async function kickDebugSession(formData) {
  ensureDebugNode();
  const sessionId = (formData.get("session_id") || "").toString().trim();
  if (!sessionId) {
    toast("会话 ID 不能为空", "error");
    return;
  }
  await apiRequest("/api/v1/debug/zlm/kick-session", {
    method: "POST",
    body: {
      node_id: state.ui.debug.nodeId,
      session_id: sessionId,
    },
  });
  toast(`已请求踢出会话 ${sessionId}`, "success");
}

async function submitDebugKickBatch(formData) {
  ensureDebugNode();
  await apiRequest("/api/v1/debug/zlm/kick-sessions", {
    method: "POST",
    body: {
      node_id: state.ui.debug.nodeId,
      local_port: toNullableNumber(formData.get("local_port")),
      peer_ip: (formData.get("peer_ip") || "").toString().trim() || null,
    },
  });
  toast("已发送批量踢会话请求", "success");
}

async function submitDebugClose(formData) {
  ensureDebugNode();
  await apiRequest("/api/v1/debug/zlm/close-stream", {
    method: "POST",
    body: {
      node_id: state.ui.debug.nodeId,
      schema: (formData.get("schema") || "").toString().trim(),
      vhost: (formData.get("vhost") || "").toString().trim(),
      app: (formData.get("app") || "").toString().trim(),
      stream: (formData.get("stream") || "").toString().trim(),
      force: Boolean(formData.get("force")),
    },
  });
  toast("已发送关流请求", "success");
}

async function submitDebugSnap(formData) {
  ensureDebugNode();
  const query = new URLSearchParams({
    node_id: state.ui.debug.nodeId,
    url: (formData.get("url") || "").toString().trim(),
    timeout_sec: String(toNullableNumber(formData.get("timeout_sec")) || 10),
    expire_sec: String(toNullableNumber(formData.get("expire_sec")) || 30),
  });
  state.ui.debug.snapResult = await apiRequest(`/api/v1/debug/zlm/snap?${query.toString()}`);
  state.ui.debug.lastError = null;
  renderApp({ chrome: false, page: true, overlays: false, toasts: false });
}

function ensureDebugNode() {
  if (!state.ui.debug.nodeId) {
    throw new Error("请先选择调试节点");
  }
}

async function requestTaskPreview() {
  try {
    const payload = buildDraftPayload(state.ui.createDraft);
    state.ui.createPreview = await apiRequest("/api/v1/tasks/preview", {
      method: "POST",
      body: payload,
    });
    state.ui.createError = null;
    toast("规格预览已更新", "success");
  } catch (error) {
    state.ui.createError = error;
    toast(errorMessage(error), "error");
  }
}

async function submitTaskCreate() {
  try {
    const payload = buildDraftPayload(state.ui.createDraft);
    const task = await apiRequest("/api/v1/tasks", {
      method: "POST",
      headers: {
        "Idempotency-Key": window.crypto?.randomUUID?.() || `console-${Date.now()}`,
      },
      body: payload,
    });
    toast(`任务 ${task.name} 已创建`, "success");
    state.ui.createOpen = false;
    state.ui.createStep = 1;
    state.ui.createDraft = createDefaultDraft();
    state.ui.createPreview = null;
    await navigate(`/tasks/${task.id}`);
  } catch (error) {
    state.ui.createError = error;
    toast(errorMessage(error), "error");
    renderApp({ chrome: false, page: false, overlays: true, toasts: true });
  }
}

function buildDraftPayload(draft) {
  const payload = {
    type: draft.task_type,
    name: draft.name.trim(),
    priority: toNumberOrDefault(draft.priority, 50),
    common: {},
    input: {},
    process: {},
    publish: {},
    record: {},
    recovery: {},
    schedule: {},
    resource: {},
  };

  setIfPresent(payload, "template", draft.template);
  setIfPresent(payload, "profile", draft.profile);

  setIfPresent(payload.common, "created_by", draft.common.created_by);
  setIfPresent(payload.common, "callback_url", draft.common.callback_url);
  setIfList(payload.common, "labels", draft.common.labels_text);

  setIfPresent(payload.input, "kind", draft.input.kind);
  setIfPresent(payload.input, "url", draft.input.url);
  setIfPresent(payload.input, "group", draft.input.group);
  setIfNumber(payload.input, "port", draft.input.port);
  setIfPresent(payload.input, "interface_name", draft.input.interface_name);
  setIfPresent(payload.input, "interface_ip", draft.input.interface_ip);
  setIfNumber(payload.input, "ttl", draft.input.ttl);
  setIfBoolean(payload.input, "reuse", draft.input.reuse);
  setIfNumber(payload.input, "probe_timeout_ms", draft.input.probe_timeout_ms);
  setIfNumber(payload.input, "tcp_mode", draft.input.tcp_mode);
  setIfNumber(payload.input, "ssrc", draft.input.ssrc);

  setIfPresent(payload.process, "mode", draft.process.mode);
  setIfNumber(payload.process, "bitrate", draft.process.bitrate);
  setIfNumber(payload.process, "fps", draft.process.fps);
  setIfNumber(payload.process, "gop", draft.process.gop);

  setIfPresent(payload.publish, "kind", draft.publish.kind);
  setIfPresent(payload.publish, "url", draft.publish.url);
  setIfPresent(payload.publish, "group", draft.publish.group);
  setIfNumber(payload.publish, "port", draft.publish.port);
  setIfPresent(payload.publish, "interface_name", draft.publish.interface_name);
  setIfPresent(payload.publish, "interface_ip", draft.publish.interface_ip);
  setIfNumber(payload.publish, "ttl", draft.publish.ttl);
  setIfPresent(payload.publish, "format", draft.publish.format);
  setIfBoolean(payload.publish, "enable_rtsp", draft.publish.enable_rtsp);
  setIfBoolean(payload.publish, "enable_rtmp", draft.publish.enable_rtmp);
  setIfBoolean(payload.publish, "enable_http_ts", draft.publish.enable_http_ts);
  setIfBoolean(payload.publish, "enable_http_fmp4", draft.publish.enable_http_fmp4);
  setIfBoolean(payload.publish, "enable_hls", draft.publish.enable_hls);
  setIfBoolean(payload.publish, "stop_on_no_reader", draft.publish.stop_on_no_reader);

  setIfBoolean(payload.record, "enabled", draft.record.enabled);
  setIfPresent(payload.record, "format", draft.record.format);
  setIfNumber(payload.record, "duration_sec", draft.record.duration_sec);
  setIfNumber(payload.record, "segment_sec", draft.record.segment_sec);
  setIfPresent(payload.record, "save_path", draft.record.save_path);
  setIfBoolean(payload.record, "as_player", draft.record.as_player);

  setIfPresent(payload.recovery, "policy", draft.recovery.policy);
  setIfPresent(payload.recovery, "resume_mode", draft.recovery.resume_mode);
  setIfNumber(payload.recovery, "max_consecutive_failures", draft.recovery.max_consecutive_failures);

  setIfPresent(payload.schedule, "start_mode", draft.schedule.start_mode);
  setIfPresent(payload.schedule, "start_at", draft.schedule.start_at);
  setIfPresent(payload.schedule, "cron", draft.schedule.cron);

  setIfList(payload.resource, "required_labels", draft.resource.required_labels_text);
  setIfList(payload.resource, "preferred_labels", draft.resource.preferred_labels_text);

  pruneEmptyObjects(payload);

  const advanced = parseAdvancedJson(draft.advanced_json);
  return deepMerge(payload, advanced);
}

function createDefaultDraft() {
  const draft = {
    task_type: "live_relay",
    template: "",
    profile: "",
    name: "",
    priority: "50",
    advanced_json: "{}",
    common: {
      created_by: "console",
      callback_url: "",
      labels_text: "",
    },
    input: {
      kind: "rtsp",
      url: "",
      group: "",
      port: "",
      interface_name: "",
      interface_ip: "",
      ttl: "",
      reuse: false,
      probe_timeout_ms: "",
      tcp_mode: "",
      ssrc: "",
    },
    process: {
      mode: "",
      bitrate: "",
      fps: "",
      gop: "",
    },
    publish: {
      kind: "",
      url: "",
      group: "",
      port: "",
      interface_name: "",
      interface_ip: "",
      ttl: "",
      format: "",
      enable_rtsp: true,
      enable_rtmp: true,
      enable_http_ts: true,
      enable_http_fmp4: true,
      enable_hls: false,
      stop_on_no_reader: false,
    },
    record: {
      enabled: false,
      format: "",
      duration_sec: "",
      segment_sec: "",
      save_path: "",
      as_player: false,
    },
    recovery: {
      policy: "",
      resume_mode: "",
      max_consecutive_failures: "",
    },
    schedule: {
      start_mode: "immediate",
      start_at: "",
      cron: "",
    },
    resource: {
      required_labels_text: "",
      preferred_labels_text: "",
    },
  };
  normalizeDraftForTaskType(draft, draft.task_type);
  return draft;
}

function normalizeDraftForTaskType(draft, taskType) {
  draft.task_type = taskType;
  switch (taskType) {
    case "file_transcode":
      draft.input.kind = "file";
      draft.publish.kind = "file";
      break;
    case "file_to_live":
      draft.input.kind = draft.input.kind || "file";
      draft.publish.kind = "zlm_ingest";
      break;
    case "multicast_bridge":
      draft.input.kind = "udp_mpegts_multicast";
      draft.publish.kind = "zlm_ingest";
      break;
    case "rtp_receive":
      draft.input.kind = "gb_rtp";
      draft.publish.kind = "";
      break;
    default:
      draft.input.kind = draft.input.kind || "rtsp";
      draft.publish.kind = draft.publish.kind || "";
      break;
  }
}

function listGpuDevices(node) {
  return Array.isArray(node?.gpu_devices) ? node.gpu_devices : [];
}

function listGpuRuntime(nodeOrHeartbeat) {
  return Array.isArray(nodeOrHeartbeat?.gpu_runtime) ? nodeOrHeartbeat.gpu_runtime : [];
}

function hasGpuTelemetry(node) {
  return listGpuDevices(node).length > 0 || listGpuRuntime(node).length > 0 || (Array.isArray(node?.gpu) && node.gpu.length > 0);
}

function mergeGpuTelemetry(devices, runtime) {
  const merged = new Map();
  devices.forEach((device) => {
    const index = Number(device?.index ?? merged.size);
    merged.set(index, { device, runtime: null });
  });
  runtime.forEach((sample) => {
    const index = Number(sample?.index ?? merged.size);
    const current = merged.get(index) || { device: null, runtime: null };
    current.runtime = sample;
    merged.set(index, current);
  });
  return Array.from(merged.entries())
    .sort((left, right) => left[0] - right[0])
    .map(([, entry]) => entry);
}

function formatGpuMemoryUsage(runtime) {
  if (!runtime) {
    return "—";
  }
  return `${formatBytes((runtime.memory_used_mb || 0) * 1024 * 1024)} / ${formatBytes((runtime.memory_total_mb || 0) * 1024 * 1024)}`;
}

function formatGpuDeviceTitle(device, runtime, fallbackIndex) {
  const index = Number.isFinite(Number(device?.index)) ? Number(device.index) : Number(runtime?.index);
  const suffix = Number.isFinite(index) ? ` #${index}` : ` #${fallbackIndex}`;
  return `${device?.name || "GPU"}${suffix}`;
}

function renderGpuRuntimePanel(devices, runtime) {
  const rows = mergeGpuTelemetry(devices, runtime);
  if (!rows.length) {
    return renderInlineEmpty("该节点没有 GPU 遥测。");
  }
  return `
    <div class="event-list">
      ${rows
        .map(
          (entry, index) => `
            <article class="event-item">
              <div class="toolbar-actions">
                <strong>${escapeHtml(formatGpuDeviceTitle(entry.device, entry.runtime, index))}</strong>
                ${entry.device?.uuid ? `<span class="subtle mono">${escapeHtml(shortId(entry.device.uuid))}</span>` : ""}
              </div>
              <div class="inline-list">
                ${entry.device?.memory_total_mb ? `<span class="tag">显存 ${formatBytes(entry.device.memory_total_mb * 1024 * 1024)}</span>` : ""}
                ${entry.runtime ? `<span class="tag">GPU ${formatPercent(entry.runtime.gpu_util_percent)}</span>` : ""}
                ${entry.runtime ? `<span class="tag">ENC ${formatPercent(entry.runtime.encoder_util_percent)}</span>` : ""}
                ${entry.runtime ? `<span class="tag">DEC ${formatPercent(entry.runtime.decoder_util_percent)}</span>` : ""}
                ${entry.runtime ? `<span class="tag">显存占用 ${escapeHtml(formatGpuMemoryUsage(entry.runtime))}</span>` : ""}
              </div>
            </article>
          `,
        )
        .join("")}
    </div>
  `;
}

function renderGpuHeartbeatTags(runtime) {
  const rows = mergeGpuTelemetry([], runtime);
  if (!rows.length) {
    return "";
  }
  return rows
    .map(
      (entry, index) => `
        <span class="tag">${escapeHtml(formatGpuDeviceTitle(entry.device, entry.runtime, index))} ${formatPercent(entry.runtime?.gpu_util_percent)}</span>
        <span class="tag">ENC ${formatPercent(entry.runtime?.encoder_util_percent)}</span>
        <span class="tag">DEC ${formatPercent(entry.runtime?.decoder_util_percent)}</span>
      `,
    )
    .join("");
}

function renderNodeMetric(node) {
  const gpuDeviceCount = Math.max(listGpuDevices(node).length, Array.isArray(node?.gpu) ? node.gpu.length : 0);
  return `
    <div class="metric-panel">
      <label>${escapeHtml(node.node_name)}</label>
      <strong>${node.healthy ? "在线" : "离线"}</strong>
      <div class="subtle">${escapeHtml(node.hostname)} · ${escapeHtml(networkModeLabel(node.network_mode))}</div>
      <div class="inline-list" style="margin-top: 12px;">
        ${node.zlm_version ? `<span class="tag">${escapeHtml(node.zlm_version)}</span>` : ""}
        <span class="tag">CPU ${formatPercent(node.cpu_percent)}</span>
        <span class="tag">内存 ${formatPercent(node.mem_percent)}</span>
        <span class="tag">任务 ${escapeHtml(String(node.running_tasks ?? 0))}</span>
        ${gpuDeviceCount ? `<span class="tag">GPU ${escapeHtml(String(gpuDeviceCount))} 卡</span>` : ""}
      </div>
    </div>
  `;
}

function renderExpandedNodeInsight(node, insight) {
  const gpuDevices = listGpuDevices(node);
  const gpuRuntime = listGpuRuntime(node);
  return `
    <div class="overview-grid">
      ${metricCard("CPU", formatPercent(node.cpu_percent))}
      ${metricCard("内存", formatPercent(node.mem_percent))}
      ${metricCard("磁盘", formatPercent(node.disk_percent))}
      ${metricCard("ZLM", node.zlm_alive === false ? "异常" : "正常")}
      ${metricCard("FFmpeg", node.ffmpeg_alive === false ? "异常" : "正常")}
      ${metricCard("GPU", hasGpuTelemetry(node) ? `${Math.max(gpuDevices.length, Array.isArray(node.gpu) ? node.gpu.length : 0)} 卡` : "无")}
      ${metricCard("最近心跳", formatTime(node.last_seen_at))}
    </div>
    <div class="split-grid" style="margin-top: 16px;">
      <div class="panel">
        <div class="panel-header">
          <div>
            <h3>能力矩阵</h3>
            <p>${escapeHtml(node.labels.join(", ") || "无标签")}</p>
          </div>
        </div>
        <div class="inline-list">
          ${node.ffmpeg_protocols.slice(0, 8).map((item) => `<span class="tag">${escapeHtml(item)}</span>`).join("")}
        </div>
        <div class="subtle" style="margin-top: 12px;">编码器：${escapeHtml(node.ffmpeg_encoders.slice(0, 6).join(", ") || "—")}</div>
        <div class="subtle">网卡：${escapeHtml(node.interfaces.join(", ") || "—")}</div>
      </div>
      ${
        hasGpuTelemetry(node)
          ? `
            <div class="panel">
              <div class="panel-header">
                <div>
                  <h3>GPU 概览</h3>
                  <p>显卡型号、显存和当前 GPU/编码/解码占用。</p>
                </div>
              </div>
              ${renderGpuRuntimePanel(gpuDevices, gpuRuntime)}
            </div>
          `
          : ""
      }
      <div class="panel">
        <div class="panel-header">
          <div>
            <h3>ZLM 概览</h3>
            <p>媒体对象、会话、播放器数量，以及线程负载和对象统计。</p>
          </div>
        </div>
        <div class="overview-grid">
          ${metricCard("媒体对象", safeCollectionSize(insight?.media?.data))}
          ${metricCard("会话数", safeCollectionSize(insight?.sessions?.data))}
          ${metricCard("播放器", safeCollectionSize(insight?.players?.data))}
          ${metricCard("前台线程均值", formatThreadLoadAverage(insight?.threadsLoad))}
          ${metricCard("工作线程均值", formatThreadLoadAverage(insight?.workThreadsLoad))}
          ${metricCard("对象数", formatStatisticObjectCount(insight?.statistic))}
        </div>
      </div>
    </div>
    <div class="split-grid" style="margin-top: 16px;">
      <div class="panel">
        <div class="panel-header">
          <div>
            <h3>最近心跳</h3>
            <p>最近 12 次心跳采样的负载快照。</p>
          </div>
        </div>
        ${renderHeartbeatTimeline(insight?.heartbeats)}
      </div>
      <div class="panel">
        <div class="panel-header">
          <div>
            <h3>线程与对象统计</h3>
            <p>汇总前台线程、工作线程与对象统计结果。</p>
          </div>
        </div>
        ${renderThreadLoadPanel(insight?.threadsLoad, insight?.workThreadsLoad)}
        <pre class="json-block">${escapeHtml(JSON.stringify(insight?.statistic || {}, null, 2))}</pre>
      </div>
    </div>
    <div class="panel" style="margin-top: 16px;">
      <div class="panel-header">
        <div>
          <h3>当前任务</h3>
          <p>按节点过滤的最近任务。</p>
        </div>
      </div>
      <div class="event-list">
        ${
          insight?.tasksPage?.items?.length
            ? insight.tasksPage.items
                .map(
                  (task) => `
                    <article class="event-item">
                      <div class="toolbar-actions">
                        <a href="/tasks/${task.id}" data-link class="mono">${shortId(task.id)}</a>
                        ${statusPill(task.status)}
                      </div>
                      <div><strong>${escapeHtml(task.name)}</strong></div>
                      <div class="subtle">${escapeHtml(taskTypeLabel(task.type))} · 优先级 ${escapeHtml(String(task.priority))}</div>
                    </article>
                  `,
                )
                .join("")
            : renderInlineEmpty("当前没有关联任务。")
        }
      </div>
    </div>
  `;
}

function renderTaskActions(task, compact) {
  if (!canAccess("task_write")) {
    return `<a class="ghost-button" href="/tasks/${task.id}" data-link>查看</a>`;
  }
  const actions = [];
  if (["CREATED", "FAILED", "CANCELED", "VALIDATING", "QUEUED"].includes(task.status)) {
    actions.push(`<button class="ghost-button" data-action="task-start" data-task-id="${task.id}">启动</button>`);
  }
  if (["DISPATCHING", "STARTING", "RUNNING", "RECOVERING"].includes(task.status)) {
    actions.push(`<button class="soft-button" data-action="task-stop" data-task-id="${task.id}">停止</button>`);
    actions.push(`<button class="danger-button" data-action="task-cancel" data-task-id="${task.id}">取消</button>`);
  }
  if (["FAILED", "LOST"].includes(task.status)) {
    actions.push(`<button class="ghost-button" data-action="task-retry" data-task-id="${task.id}">重试</button>`);
  }
  actions.push(`<button class="ghost-button" data-action="task-clone" data-task-id="${task.id}">克隆</button>`);
  if (!compact) {
    actions.push(`<a class="ghost-button" href="/tasks/${task.id}" data-link>详情</a>`);
  }
  return `<div class="toolbar-actions">${actions.join("")}</div>`;
}

function renderTaskDetailTab(taskId, activeTab, tab, label) {
  const query = new URLSearchParams(state.route.searchParams.toString());
  query.set("tab", tab);
  return `<a href="/tasks/${taskId}?${query.toString()}" data-link class="tab ${activeTab === tab ? "active" : ""}">${escapeHtml(label)}</a>`;
}

function renderRolePill(role) {
  return `<span class="role-pill">${escapeHtml(apiRoleLabel(role))}</span>`;
}

function renderPlayUrls(stream, node, task) {
  const urls = Array.isArray(stream) ? stream : [];
  return urls.length
    ? `
      <div class="play-url-list">
        ${urls
          .map(
            (url) => `
              <div class="play-url">
                <code class="selectable">${escapeHtml(url)}</code>
                <button class="ghost-button" data-action="copy" data-value="${escapeAttr(url)}">复制</button>
              </div>
            `,
          )
          .join("")}
      </div>
    `
    : "—";
}

function renderRecordingLabel(task) {
  const enabled = Boolean(task?.resolved_spec?.record?.enabled);
  const format = task?.resolved_spec?.record?.format;
  return enabled ? `已启用${format ? `（${recordFormatLabel(format)}）` : ""}` : "未启用";
}

function renderDebugResult(value) {
  if (!value) {
    return renderInlineEmpty("还没有查询结果。");
  }
  return `<pre class="json-block">${escapeHtml(JSON.stringify(value, null, 2))}</pre>`;
}

function renderThreadLoadPanel(threadsLoad, workThreadsLoad) {
  if (!threadsLoad && !workThreadsLoad) {
    return renderInlineEmpty("还没有线程负载结果。");
  }
  return `
    <div class="overview-grid">
      ${metricCard("前台线程均值", formatThreadLoadAverage(threadsLoad))}
      ${metricCard("前台线程峰值", formatThreadLoadMax(threadsLoad))}
      ${metricCard("工作线程均值", formatThreadLoadAverage(workThreadsLoad))}
      ${metricCard("工作线程峰值", formatThreadLoadMax(workThreadsLoad))}
    </div>
    <pre class="json-block">${escapeHtml(JSON.stringify({
      threads: threadsLoad?.data || threadsLoad || [],
      work_threads: workThreadsLoad?.data || workThreadsLoad || [],
    }, null, 2))}</pre>
  `;
}

function renderHeartbeatTimeline(heartbeats) {
  if (!Array.isArray(heartbeats) || !heartbeats.length) {
    return renderInlineEmpty("当前没有心跳历史。");
  }
  return `
    <div class="event-list">
      ${heartbeats
        .map(
          (item) => `
            <article class="event-item">
              <div class="toolbar-actions">
                <span class="subtle">${escapeHtml(formatTime(item.received_at || item.node_time))}</span>
                <span class="tag">${item.zlm_alive === false ? "ZLM 异常" : "ZLM 正常"}</span>
                <span class="tag">${item.ffmpeg_alive === false ? "FFmpeg 异常" : "FFmpeg 正常"}</span>
              </div>
              <div class="inline-list">
                <span class="tag">CPU ${formatPercent(item.cpu_percent)}</span>
                <span class="tag">内存 ${formatPercent(item.mem_percent)}</span>
                <span class="tag">磁盘 ${formatPercent(item.disk_percent)}</span>
                <span class="tag">任务 ${escapeHtml(String(item.running_tasks ?? 0))}</span>
                <span class="tag">槽位 ${formatPercent((item.slot_usage ?? 0) * 100)}</span>
                ${renderGpuHeartbeatTags(item.gpu_runtime)}
              </div>
            </article>
          `,
        )
        .join("")}
    </div>
  `;
}

function renderHookTimeline(items) {
  if (!Array.isArray(items) || !items.length) {
    return renderInlineEmpty("当前没有 Hook 事件。");
  }
  return `
    <div class="event-list">
      ${items
        .map(
          (item) => `
            <article class="event-item">
              <div class="toolbar-actions">
                <span class="tag">${escapeHtml(item.hook_name)}</span>
                <span class="subtle">${escapeHtml(formatTime(item.received_at))}</span>
              </div>
              <div class="subtle">${escapeHtml(item.processed_at ? `已处理：${formatTime(item.processed_at)}` : "待处理")}</div>
              <pre class="json-block">${escapeHtml(JSON.stringify(item.payload, null, 2))}</pre>
            </article>
          `,
        )
        .join("")}
    </div>
  `;
}

function renderPager(kind, page, taskId) {
  const totalPages = Math.max(1, Math.ceil(page.total / page.page_size));
  const prevPage = Math.max(1, page.page - 1);
  const nextPage = Math.min(totalPages, page.page + 1);
  const prevDisabled = page.page <= 1;
  const nextDisabled = page.page >= totalPages;
  const prevHref = pageHref(kind, prevPage, taskId);
  const nextHref = pageHref(kind, nextPage, taskId);
  return `
    <span class="subtle">第 ${page.page} / ${totalPages} 页</span>
    <a class="ghost-button ${prevDisabled ? "disabled" : ""}" href="${prevHref}" data-link ${prevDisabled ? "aria-disabled=true" : ""}>上一页</a>
    <a class="ghost-button ${nextDisabled ? "disabled" : ""}" href="${nextHref}" data-link ${nextDisabled ? "aria-disabled=true" : ""}>下一页</a>
  `;
}

function pageHref(kind, pageNumber, taskId) {
  const query = new URLSearchParams(state.route.searchParams.toString());
  switch (kind) {
    case "records":
      query.set("page", String(pageNumber));
      return `/records?${query.toString()}`;
    case "transcode-artifacts":
      query.set("page", String(pageNumber));
      return `/transcode-artifacts?${query.toString()}`;
    case "task-events":
      query.set("tab", "events");
      query.set("page", String(pageNumber));
      return `/tasks/${taskId}?${query.toString()}`;
    default:
      query.set("page", String(pageNumber));
      return `/tasks?${query.toString()}`;
  }
}

function renderTextField(label, name, value, placeholder, type = "text") {
  return `
    <label class="field">
      <span>${escapeHtml(label)}</span>
      <input type="${type}" name="${escapeAttr(name)}" value="${escapeAttr(value)}" placeholder="${escapeAttr(placeholder)}" />
    </label>
  `;
}

function renderDateTimeField(label, name, value) {
  return renderTextField(label, name, value, "2026-03-29T00:00:00Z");
}

function renderSelectField(label, name, values, selected, labelForValue = (value) => value || "全部", idMode = false) {
  const id = idMode ? name : "";
  return `
    <label class="field">
      <span>${escapeHtml(label)}</span>
      <select ${id ? `id="${escapeAttr(id)}"` : ""} name="${escapeAttr(name)}">
        ${values
          .map((value) => `<option value="${escapeAttr(value)}" ${value === selected ? "selected" : ""}>${escapeHtml(labelForValue(value))}</option>`)
          .join("")}
      </select>
    </label>
  `;
}

function renderTextModelField(label, path, value, placeholder, type = "text") {
  return `
    <label class="field">
      <span>${escapeHtml(label)}</span>
      <input type="${type}" data-model="${escapeAttr(path)}" value="${escapeAttr(value || "")}" placeholder="${escapeAttr(placeholder)}" />
    </label>
  `;
}

function renderTextareaModelField(label, path, value, placeholder) {
  return `
    <label class="field-block">
      <span>${escapeHtml(label)}</span>
      <textarea data-model="${escapeAttr(path)}" placeholder="${escapeAttr(placeholder)}">${escapeHtml(value || "")}</textarea>
    </label>
  `;
}

function renderSelectModelField(label, path, values, selected, labelForValue = (value) => value || "未设置") {
  return `
    <label class="field">
      <span>${escapeHtml(label)}</span>
      <select data-model="${escapeAttr(path)}">
        ${values
          .map((value) => `<option value="${escapeAttr(value)}" ${value === selected ? "selected" : ""}>${escapeHtml(labelForValue(value))}</option>`)
          .join("")}
      </select>
    </label>
  `;
}

function renderCheckboxModelField(label, path, checked) {
  return `
    <label class="checkbox-field">
      <input type="checkbox" data-model="${escapeAttr(path)}" ${checked ? "checked" : ""} />
      <span>${escapeHtml(label)}</span>
    </label>
  `;
}

function renderStaticModelField(label, value) {
  return `
    <div class="metric">
      <label>${escapeHtml(label)}</label>
      <strong>${escapeHtml(String(value || "—"))}</strong>
    </div>
  `;
}

function metricCard(label, value, rawValue = false) {
  return `
    <div class="metric">
      <label>${escapeHtml(label)}</label>
      <strong>${rawValue ? value : escapeHtml(String(value))}</strong>
    </div>
  `;
}

function statusPill(status) {
  return `<span class="status-pill ${STATUS_THEME[status] || "status-created"}">${escapeHtml(taskStatusLabel(status))}</span>`;
}

function renderLoadingPanel() {
  return `
    <section class="empty-state">
      <h3>正在加载</h3>
      <p>控制面正在同步任务、节点、流与调试数据。</p>
    </section>
  `;
}

function renderErrorPanel(title, message) {
  return `
    <section class="auth-panel">
      <h3>${escapeHtml(title)}</h3>
      <p>${escapeHtml(message)}</p>
      <div class="actions">
        <button class="ghost-button" data-action="refresh-page">重试</button>
      </div>
    </section>
  `;
}

function renderAuthRequired() {
  return `
    <section class="auth-panel">
      <h3>需要认证</h3>
      <p>${escapeHtml(errorMessage(state.sessionError) || "当前环境启用了鉴权，请先登录或提供 Bearer Token。")}</p>
      <div class="actions">
        <a class="button" href="/login" data-link>前往登录</a>
        <button class="button" data-action="open-auth-modal">输入令牌</button>
      </div>
    </section>
  `;
}

function renderStandaloneState(title, message, mark = "ACCESS") {
  return `
    <section class="auth-stage">
      <article class="auth-stage-card">
        <div class="boot-mark">${escapeHtml(mark)}</div>
        <h1>${escapeHtml(title)}</h1>
        <p>${escapeHtml(message)}</p>
      </article>
    </section>
  `;
}

function ensureSecurityAllowlistDraft(data) {
  if (state.ui.securityAllowlistDirty) {
    return state.ui.securityAllowlistText;
  }
  const entries = Array.isArray(data?.allowlist?.entries) ? data.allowlist.entries : [];
  const text = formatMachineAllowlistEntries(entries);
  state.ui.securityAllowlistText = text;
  return text;
}

function formatMachineAllowlistEntries(entries) {
  if (!Array.isArray(entries) || !entries.length) {
    return "";
  }
  return entries
    .map((entry) => {
      const cidr = String(entry?.cidr || "").trim();
      const description = String(entry?.description || "").trim();
      return description ? `${cidr} # ${description}` : cidr;
    })
    .join("\n");
}

function parseMachineAllowlistText(raw) {
  const lines = String(raw || "").split(/\r?\n/);
  const entries = [];
  lines.forEach((line, index) => {
    const trimmed = line.trim();
    if (!trimmed || trimmed.startsWith("#")) {
      return;
    }
    const hashIndex = trimmed.indexOf("#");
    const body = hashIndex >= 0 ? trimmed.slice(0, hashIndex).trim() : trimmed;
    const comment = hashIndex >= 0 ? trimmed.slice(hashIndex + 1).trim() : "";
    if (!body) {
      return;
    }
    const [cidrToken, ...descriptionParts] = body.split(/\s+/);
    const cidr = (cidrToken || "").trim();
    const description = [descriptionParts.join(" ").trim(), comment].filter(Boolean).join(" ").trim();
    if (!cidr) {
      throw new Error(`第 ${index + 1} 行缺少 CIDR 或 IP`);
    }
    entries.push({ cidr, description });
  });
  return entries;
}

function renderLoginPage() {
  const nextHref = sanitizeReturnTo(state.route.searchParams.get("next"));
  const returnHint = nextHref !== "/overview"
    ? `<p class="subtle">登录成功后将返回 ${escapeHtml(nextHref)}</p>`
    : `<p class="subtle">登录成功后进入控制台总览页。</p>`;
  const authHint = state.sessionError && isAuthError(state.sessionError)
    ? `<div class="auth-alert danger-text">${escapeHtml(errorMessage(state.sessionError) || "当前会话无效，请重新登录。")}</div>`
    : "";
  return `
    <section class="auth-stage">
      <article class="auth-stage-card login-stage">
        <div class="login-stage-grid">
          <section class="login-stage-copy">
            <div class="boot-mark">ACCESS</div>
            <h1>进入 StreamServer</h1>
            <p>未建立管理员会话前，控制台页面不会开放。请先完成登录，再进入任务、流、录像、节点和调试页面。</p>
            ${returnHint}
            <div class="login-stage-notes">
              <div class="note-card">
                <strong>本地账号</strong>
                <span>适用于 <code>local_password</code>，登录成功后会自动保存 refresh token 并恢复会话。</span>
              </div>
              <div class="note-card">
                <strong>Bearer Token</strong>
                <span>兼容 <code>external_jwt</code>，可直接在当前页输入临时令牌，不写入本地存储。</span>
              </div>
            </div>
          </section>
          <section class="auth-panel login-stage-form">
            <h3>管理员登录</h3>
            <p>输入用户名和密码建立控制台会话。</p>
            ${authHint}
            <form id="login-form" class="stack-form auth-form-grid">
              <label class="field">
                <span>用户名</span>
                <input name="username" autocomplete="username" placeholder="admin" required />
              </label>
              <label class="field">
                <span>密码</span>
                <input name="password" type="password" autocomplete="current-password" required />
              </label>
              <div class="actions">
                <button class="button" type="submit">登录</button>
                <button class="ghost-button" type="button" data-action="open-auth-modal">使用 Bearer Token</button>
              </div>
            </form>
          </section>
        </div>
      </article>
    </section>
  `;
}

function renderSecurityPage(data) {
  const allowlistText = ensureSecurityAllowlistDraft(data);
  return `
    <section class="stack-section">
      <div class="overview-grid">
        ${metricCard("当前账号", state.session?.subject || "—")}
        ${metricCard("角色", apiRoleLabel(state.session?.role || ""))}
        ${metricCard("鉴权模式", state.session?.auth_mode || "disabled")}
        ${metricCard("强制改密", state.session?.must_change_password ? "是" : "否")}
      </div>
      <div class="panel">
        <div class="section-header">
          <div>
            <h3>修改当前密码</h3>
            <p>提交后会吊销当前账号的 refresh token，会话需要重新登录恢复。</p>
          </div>
        </div>
        <form id="change-password-form" class="stack-form auth-form-grid">
          <label class="field">
            <span>当前密码</span>
            <input name="current_password" type="password" autocomplete="current-password" required />
          </label>
          <label class="field">
            <span>新密码</span>
            <input name="new_password" type="password" autocomplete="new-password" minlength="8" required />
          </label>
          <div class="actions">
            <button class="button" type="submit">更新密码</button>
          </div>
        </form>
      </div>
      <div class="panel">
        <div class="section-header">
          <div>
            <h3>机器 API 白名单</h3>
            <p>每行一条，格式为 <code>IP/CIDR # 说明</code>。说明可选，空行和以 <code>#</code> 开头的行会被忽略。</p>
          </div>
        </div>
        <form id="machine-allowlist-form" class="stack-form">
          <label class="field-block">
            <span>白名单条目</span>
            <textarea
              name="entries_text"
              rows="14"
              data-action="machine-allowlist-input"
              placeholder="192.168.1.10/32 # ingest-gateway&#10;10.0.0.0/24 # office-network"
            >${escapeHtml(allowlistText)}</textarea>
          </label>
          <p class="subtle">示例：<code>192.168.6.20/32 # 采集机</code>，或仅填写 <code>10.0.0.0/24</code>。</p>
          <div class="actions">
            <button class="button" type="submit">保存白名单</button>
          </div>
        </form>
      </div>
    </section>
  `;
}

function renderEmptyState(title, message) {
  return `
    <section class="empty-state">
      <h3>${escapeHtml(title)}</h3>
      <p>${escapeHtml(message)}</p>
    </section>
  `;
}

function renderInlineEmpty(message) {
  return `<div class="subtle">${escapeHtml(message)}</div>`;
}

function renderFatal(error) {
  return `
    <div class="boot-shell">
      <div class="boot-panel">
        <div class="boot-mark">FATAL</div>
        <h1>前端初始化失败</h1>
        <p>${escapeHtml(error?.message || String(error))}</p>
      </div>
    </div>
  `;
}

function nodeLabel(node) {
  if (!node) {
    return "—";
  }
  return `${node.node_name}`;
}

function viewerCountLabel(viewerCount, hasViewer) {
  if (viewerCount !== null && viewerCount !== undefined) return String(viewerCount);
  if (hasViewer === true) return "至少 1";
  if (hasViewer === false) return "0";
  return "—";
}

function multicastRowModel(task, spec, detail, node, streams) {
  const input = spec.input || {};
  const publish = spec.publish || {};
  const usePublish = publish.group || publish.port || publish.interface_ip;
  const progress = deriveLatestProgress(detail?.recent_events || []);
  const streamRuntime = Array.isArray(streams) ? streams.find((item) => item.bitrate_kbps || item.viewer_count !== undefined) : null;
  return {
    mode: `${inputKindLabel(input.kind, "未知输入")} -> ${publish.kind ? publishKindLabel(publish.kind) : "内部流"}`,
    group: usePublish ? publish.group || "—" : input.group || "—",
    port: String(usePublish ? publish.port || "—" : input.port || "—"),
    interfaceIp: usePublish
      ? publish.interface_name || publish.interface_ip || "—"
      : input.interface_name || input.interface_ip || "—",
    ttl: String(usePublish ? publish.ttl || "—" : input.ttl || "—"),
    node: nodeLabel(node),
    bitrate: formatBitrateKbps(streamRuntime?.bitrate_kbps ?? progress?.bitrate_kbps),
    lastError: deriveLastIssue(detail?.recent_events || []) || "—",
    binding: streamRuntime?.play_urls?.[0] || publish.url || `${publish.group || "—"}:${publish.port || "—"}`,
  };
}

function deriveLastIssue(events) {
  const list = Array.isArray(events) ? events : [];
  const critical = list.find((event) => ["error", "warn"].includes(String(event.event_level).toLowerCase()));
  if (!critical) {
    return "";
  }
  return (
    critical.payload?.failure_reason ||
    critical.payload?.message ||
    critical.event_type ||
    "最近存在异常事件"
  );
}

function computeDiffPaths(left, right, prefix = "") {
  const paths = [];
  const leftObj = left && typeof left === "object" ? left : {};
  const rightObj = right && typeof right === "object" ? right : {};
  const keys = new Set([...Object.keys(leftObj), ...Object.keys(rightObj)]);
  for (const key of keys) {
    const nextPrefix = prefix ? `${prefix}.${key}` : key;
    const leftValue = leftObj[key];
    const rightValue = rightObj[key];
    if (isPlainObject(leftValue) && isPlainObject(rightValue)) {
      paths.push(...computeDiffPaths(leftValue, rightValue, nextPrefix));
      continue;
    }
    if (JSON.stringify(leftValue) !== JSON.stringify(rightValue)) {
      paths.push(nextPrefix);
    }
  }
  return paths;
}

function parseAdvancedJson(text) {
  const raw = String(text || "").trim();
  if (!raw || raw === "{}") {
    return {};
  }
  try {
    const parsed = JSON.parse(raw);
    return isPlainObject(parsed) ? parsed : {};
  } catch (_error) {
    return {};
  }
}

function setIfPresent(target, key, value) {
  const trimmed = String(value ?? "").trim();
  if (trimmed) {
    target[key] = trimmed;
  }
}

function setIfNumber(target, key, value) {
  const parsed = toOptionalNumber(value);
  if (parsed !== undefined) {
    target[key] = parsed;
  }
}

function setIfBoolean(target, key, value) {
  if (typeof value === "boolean") {
    target[key] = value;
  }
}

function setIfList(target, key, text) {
  const items = String(text || "")
    .split(",")
    .map((value) => value.trim())
    .filter(Boolean);
  if (items.length) {
    target[key] = items;
  }
}

function pruneEmptyObjects(target) {
  Object.keys(target).forEach((key) => {
    const value = target[key];
    if (Array.isArray(value)) {
      return;
    }
    if (isPlainObject(value)) {
      pruneEmptyObjects(value);
      if (Object.keys(value).length === 0) {
        delete target[key];
      }
    }
  });
}

function deepMerge(base, overlay) {
  const output = structuredClone(base);
  mergeInto(output, overlay);
  return output;
}

function mergeInto(target, overlay) {
  if (!isPlainObject(overlay)) {
    return;
  }
  for (const [key, value] of Object.entries(overlay)) {
    if (isPlainObject(value) && isPlainObject(target[key])) {
      mergeInto(target[key], value);
    } else {
      target[key] = value;
    }
  }
}

function isPlainObject(value) {
  return value && typeof value === "object" && !Array.isArray(value);
}

function copyIfPresent(from, to, keys) {
  keys.forEach((key) => {
    const value = from.get(key);
    if (value) {
      to.set(key, value);
    }
  });
}

function buildQueryString(formData) {
  const query = new URLSearchParams();
  for (const [key, value] of formData.entries()) {
    const stringValue = String(value).trim();
    if (stringValue) {
      query.set(key, stringValue);
    }
  }
  return query.toString();
}

async function updateTaskDetailQuery(updates) {
  const query = new URLSearchParams(state.route.searchParams.toString());
  Object.entries(updates).forEach(([key, value]) => {
    if (value === undefined || value === null || value === "") {
      query.delete(key);
    } else {
      query.set(key, value);
    }
  });
  await navigate(`/tasks/${state.route.params.id}?${query.toString()}`);
}

function formValue(form, name) {
  return (new FormData(form).get(name) || "").toString().trim();
}

function shortId(value) {
  return String(value || "").slice(0, 8);
}

function formatTime(value) {
  if (!value) {
    return "—";
  }
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) {
    return String(value);
  }
  return date.toLocaleString("zh-CN", {
    hour12: false,
  });
}

function formatBytes(bytes) {
  if (bytes === null || bytes === undefined) {
    return "—";
  }
  const numeric = Number(bytes);
  if (!Number.isFinite(numeric)) {
    return String(bytes);
  }
  const units = ["B", "KB", "MB", "GB", "TB"];
  let value = numeric;
  let index = 0;
  while (value >= 1024 && index < units.length - 1) {
    value /= 1024;
    index += 1;
  }
  return `${value.toFixed(value >= 10 || index === 0 ? 0 : 1)} ${units[index]}`;
}

function formatPercent(value) {
  if (value === null || value === undefined) {
    return "—";
  }
  const numeric = Number(value);
  return Number.isFinite(numeric) ? `${numeric.toFixed(1)}%` : "—";
}

function formatBitrateKbps(value) {
  if (value === null || value === undefined) {
    return "未上报";
  }
  const numeric = Number(value);
  return Number.isFinite(numeric) ? `${numeric.toFixed(numeric >= 100 ? 0 : 1)} kbps` : "未上报";
}

function formatThreadLoadAverage(payload) {
  const items = Array.isArray(payload?.data) ? payload.data : Array.isArray(payload) ? payload : [];
  if (!items.length) {
    return "—";
  }
  const total = items.reduce((sum, item) => sum + (Number(item.load) || 0), 0);
  return `${(total / items.length).toFixed(1)}%`;
}

function formatThreadLoadMax(payload) {
  const items = Array.isArray(payload?.data) ? payload.data : Array.isArray(payload) ? payload : [];
  if (!items.length) {
    return "—";
  }
  const max = items.reduce((value, item) => Math.max(value, Number(item.load) || 0), 0);
  return `${max.toFixed(1)}%`;
}

function formatStatisticObjectCount(payload) {
  const stats = payload?.data && typeof payload.data === "object" ? payload.data : payload;
  if (!stats || typeof stats !== "object") {
    return "—";
  }
  const total = Object.values(stats).reduce((sum, value) => sum + (Number(value) || 0), 0);
  return String(total);
}

function safeCollectionSize(value) {
  return Array.isArray(value) ? String(value.length) : "—";
}

function toOptionalNumber(value) {
  const raw = String(value ?? "").trim();
  if (!raw) {
    return undefined;
  }
  const numeric = Number(raw);
  return Number.isFinite(numeric) ? numeric : undefined;
}

function toNullableNumber(value) {
  const numeric = toOptionalNumber(value);
  return numeric === undefined ? null : numeric;
}

function toNumberOrDefault(value, fallback) {
  const numeric = toOptionalNumber(value);
  return numeric === undefined ? fallback : numeric;
}

function errorMessage(error) {
  if (!error) {
    return "未知错误";
  }
  return error.payload?.message || error.message || String(error);
}

function isAuthError(error) {
  return Number(error?.status) === 403;
}

function shouldRenderAuthRequired(error) {
  return isAuthError(error) && !state.session;
}

function setPath(target, path, value) {
  const parts = path.split(".");
  let current = target;
  for (let index = 0; index < parts.length - 1; index += 1) {
    const part = parts[index];
    if (!isPlainObject(current[part])) {
      current[part] = {};
    }
    current = current[part];
  }
  current[parts.at(-1)] = value;
}

function deriveLatestProgress(events) {
  const list = Array.isArray(events) ? events : [];
  const progressEvent = list.find((event) => event.event_type === "task_progress");
  return progressEvent?.payload || null;
}

function applyTaskSpecDefaultsToDraft(draft, spec) {
  if (!isPlainObject(spec)) {
    return;
  }
  if (spec.type) {
    normalizeDraftForTaskType(draft, String(spec.type));
  }
  if (spec.name !== undefined) draft.name = String(spec.name || "");
  if (spec.profile !== undefined) draft.profile = String(spec.profile || "");
  if (spec.priority !== undefined) draft.priority = String(spec.priority ?? "");
  applyDraftSectionDefaults(draft.common, spec.common, {
    created_by: "string",
    callback_url: "string",
    labels: "list:labels_text",
  });
  applyDraftSectionDefaults(draft.input, spec.input, {
    kind: "string",
    url: "string",
    group: "string",
    port: "number",
    interface_name: "string",
    interface_ip: "string",
    ttl: "number",
    reuse: "boolean",
    probe_timeout_ms: "number",
    tcp_mode: "number",
    ssrc: "number",
  });
  applyDraftSectionDefaults(draft.process, spec.process, {
    mode: "string",
    bitrate: "number",
    fps: "number",
    gop: "number",
  });
  applyDraftSectionDefaults(draft.publish, spec.publish, {
    kind: "string",
    url: "string",
    group: "string",
    port: "number",
    interface_name: "string",
    interface_ip: "string",
    ttl: "number",
    format: "string",
    enable_rtsp: "boolean",
    enable_rtmp: "boolean",
    enable_http_ts: "boolean",
    enable_http_fmp4: "boolean",
    enable_hls: "boolean",
    stop_on_no_reader: "boolean",
  });
  applyDraftSectionDefaults(draft.record, spec.record, {
    enabled: "boolean",
    format: "string",
    duration_sec: "number",
    segment_sec: "number",
    save_path: "string",
    as_player: "boolean",
  });
  applyDraftSectionDefaults(draft.recovery, spec.recovery, {
    policy: "string",
    resume_mode: "string",
    max_consecutive_failures: "number",
  });
  applyDraftSectionDefaults(draft.schedule, spec.schedule, {
    start_mode: "string",
    start_at: "string",
    cron: "string",
  });
  applyDraftSectionDefaults(draft.resource, spec.resource, {
    required_labels: "list:required_labels_text",
    preferred_labels: "list:preferred_labels_text",
  });
}

function applyDraftSectionDefaults(target, source, mapping) {
  if (!isPlainObject(source)) {
    return;
  }
  Object.entries(mapping).forEach(([key, kind]) => {
    if (!(key in source)) {
      return;
    }
    const value = source[key];
    if (kind === "string") {
      target[key] = value === null || value === undefined ? "" : String(value);
      return;
    }
    if (kind === "number") {
      target[key] = value === null || value === undefined ? "" : String(value);
      return;
    }
    if (kind === "boolean") {
      target[key] = Boolean(value);
      return;
    }
    if (kind.startsWith("list:")) {
      const field = kind.split(":")[1];
      target[field] = Array.isArray(value) ? value.join(", ") : "";
    }
  });
}

function readThemePreference() {
  const value = window.localStorage.getItem(THEME_STORAGE_KEY) || "system";
  return THEME_OPTIONS.includes(value) ? value : "system";
}

function resolveTheme(preference) {
  if (preference === "light" || preference === "dark") {
    return preference;
  }
  return window.matchMedia && window.matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light";
}

function applyTheme(preference) {
  const theme = resolveTheme(preference);
  document.documentElement.dataset.theme = theme;
  document.documentElement.style.colorScheme = theme;
}

function watchSystemTheme() {
  if (!window.matchMedia) {
    return;
  }
  const mediaQuery = window.matchMedia("(prefers-color-scheme: dark)");
  const update = () => {
    if (state.ui.themePreference === "system") {
      applyTheme("system");
      if (shell.ready) {
        renderApp({ chrome: true, page: false, overlays: false, toasts: false });
      }
    }
  };
  if (typeof mediaQuery.addEventListener === "function") {
    mediaQuery.addEventListener("change", update);
  } else if (typeof mediaQuery.addListener === "function") {
    mediaQuery.addListener(update);
  }
}

function themeLabel(value) {
  return { system: "跟随系统", light: "浅色", dark: "深色" }[value] || value;
}

function labelFor(group, value, fallback = "—") {
  if (value === null || value === undefined || value === "") {
    return fallback;
  }
  return LABELS[group]?.[value] || String(value);
}

function taskTypeLabel(value, fallback = "未设置") {
  return labelFor("taskType", value, fallback);
}

function inputKindLabel(value, fallback = "未设置") {
  return labelFor("inputKind", value, fallback);
}

function publishKindLabel(value, fallback = "未设置") {
  return labelFor("publishKind", value, fallback);
}

function startModeLabel(value, fallback = "未设置") {
  return labelFor("startMode", value, fallback);
}

function recordFormatLabel(value, fallback = "未设置") {
  return labelFor("recordFormat", value, fallback);
}

function recoveryPolicyLabel(value, fallback = "未设置") {
  return labelFor("recoveryPolicy", value, fallback);
}

function profileLabel(value, fallback = "未设置") {
  return labelFor("profile", value, fallback);
}

function taskStatusLabel(value, fallback = "未设置") {
  return labelFor("status", value, fallback);
}

function apiRoleLabel(value, fallback = "未知角色") {
  return labelFor("apiRole", value, fallback);
}

function networkModeLabel(value, fallback = "未知网络") {
  return labelFor("networkMode", value, fallback);
}

function eventSourceLabel(value, fallback = "未知来源") {
  return labelFor("eventSource", value, fallback);
}

function eventLevelLabel(value, fallback = "未标记") {
  return labelFor("eventLevel", value, fallback);
}

function recordSourceLabel(value, fallback = "未知来源") {
  return labelFor("recordSource", value, fallback);
}

function schemaLabel(value, fallback = "未知协议") {
  return inputKindLabel(value, fallback);
}

function boolLabel(value) {
  if (value === true || value === "true") return "是";
  if (value === false || value === "false") return "否";
  return "全部";
}

function sortFieldLabel(value) {
  return {
    "": "默认排序",
    created_at: "创建时间",
    updated_at: "更新时间",
    priority: "优先级",
    status: "状态",
  }[value ?? ""] || value;
}

function sortOrderLabel(value) {
  return { "": "默认方向", asc: "升序", desc: "降序" }[value ?? ""] || value;
}

function logStreamLabel(value) {
  return { merged: "合并", stdout: "标准输出", stderr: "标准错误" }[value] || value || "合并";
}

function taskActionLabel(value) {
  return { start: "启动", stop: "停止", cancel: "取消", retry: "重试", clone: "克隆" }[value] || value;
}

function groupBy(items, pick) {
  return (items || []).reduce((accumulator, item) => {
    const key = pick(item);
    if (!accumulator[key]) {
      accumulator[key] = [];
    }
    accumulator[key].push(item);
    return accumulator;
  }, {});
}

function renderApiExample(example) {
  if (typeof example === "string") {
    return `<pre class="json-block selectable">${escapeHtml(example)}</pre>`;
  }
  return `<pre class="json-block selectable">${escapeHtml(JSON.stringify(example, null, 2))}</pre>`;
}

function renderFieldDocs(fields, emptyMessage, options = {}) {
  if (!Array.isArray(fields) || !fields.length) {
    return `<div class="subtle">${escapeHtml(emptyMessage)}</div>`;
  }
  const includeLocation = Boolean(options.includeLocation);
  const includeType = options.includeType !== false;
  const includeRequired = options.includeRequired !== false;
  const includeExample = options.includeExample !== false;
  const resolveExample = typeof options.exampleResolver === "function" ? options.exampleResolver : (field) => field.example;
  return `
    <div class="table-wrap">
      <table class="doc-table">
        <thead>
          <tr>
            <th>字段</th>
            ${includeLocation ? "<th>位置</th>" : ""}
            ${includeType ? "<th>类型</th>" : ""}
            ${includeRequired ? "<th>必填</th>" : ""}
            <th>说明 / 用途</th>
            ${includeExample ? "<th>示例</th>" : ""}
          </tr>
        </thead>
        <tbody>
          ${fields
            .map(
              (field) => `
                <tr>
                  <td><code class="selectable">${escapeHtml(field.name)}</code></td>
                  ${includeLocation ? `<td>${escapeHtml(field.location || "—")}</td>` : ""}
                  ${includeType ? `<td>${escapeHtml(field.type || "—")}</td>` : ""}
                  ${includeRequired ? `<td>${escapeHtml(requiredLabel(field.required))}</td>` : ""}
                  <td>${escapeHtml(field.description || "—")}</td>
                  ${includeExample ? `<td><code class="selectable">${escapeHtml(formatDocExample(resolveExample(field)))}</code></td>` : ""}
                </tr>
              `,
            )
            .join("")}
        </tbody>
      </table>
    </div>
  `;
}

function renderEnumDocs(groups) {
  if (!groups || !Object.keys(groups).length) {
    return `<div class="subtle">当前接口没有额外枚举说明。</div>`;
  }
  return Object.entries(groups)
    .map(
      ([groupName, items]) => `
        <div class="doc-block">
          <strong>${escapeHtml(groupName)}</strong>
          <div class="table-wrap">
            <table class="doc-table">
              <thead>
                <tr>
                  <th>枚举值</th>
                  <th>中文说明</th>
                  <th>补充说明</th>
                </tr>
              </thead>
              <tbody>
                ${(items || [])
                  .map(
                    (item) => `
                      <tr>
                        <td><code class="selectable">${escapeHtml(item.value)}</code></td>
                        <td>${escapeHtml(item.label || "—")}</td>
                        <td>${escapeHtml(item.description || "—")}</td>
                      </tr>
                    `,
                  )
                  .join("")}
              </tbody>
            </table>
          </div>
        </div>
      `,
    )
    .join("");
}

function requiredLabel(value) {
  if (value === undefined) return "—";
  if (value === true) return "是";
  if (value === false) return "否";
  return String(value);
}

function formatDocExample(value) {
  if (value === undefined || value === null || value === "") {
    return "—";
  }
  if (typeof value === "object") {
    return JSON.stringify(value);
  }
  return String(value);
}

function lookupDocValue(source, path) {
  if (source === undefined || source === null || !path) {
    return undefined;
  }
  const parts = String(path)
    .replace(/\[\]/g, ".0")
    .split(".")
    .filter(Boolean);
  let current = source;
  for (const part of parts) {
    if (current === undefined || current === null) {
      return undefined;
    }
    if (Array.isArray(current)) {
      if (/^\d+$/.test(part)) {
        current = current[Number(part)];
      } else {
        current = current[0];
        if (current === undefined || current === null) {
          return undefined;
        }
        current = current[part];
      }
      continue;
    }
    current = current[part];
  }
  return current;
}

function resolveApiPath(path, pathParams = {}) {
  return path.replace(/\{([^}]+)\}/g, (_match, key) => {
    const value = pathParams[key];
    return value === undefined || value === null || value === "" ? `{${key}}` : String(value);
  });
}

function buildApiQueryString(query = {}) {
  const params = new URLSearchParams();
  Object.entries(query || {}).forEach(([key, value]) => {
    if (value === undefined || value === null || value === "") {
      return;
    }
    if (Array.isArray(value)) {
      value.forEach((item) => params.append(key, String(item)));
      return;
    }
    params.append(key, String(value));
  });
  return params.toString();
}

function buildFullRequestExample(doc, requestSample = {}) {
  const path = resolveApiPath(doc.path, requestSample.pathParams);
  const queryString = buildApiQueryString(requestSample.query);
  const request = {
    method: doc.method,
    url: `${API_EXAMPLE_BASE_URL}${path}${queryString ? `?${queryString}` : ""}`,
  };
  if (requestSample.headers && Object.keys(requestSample.headers).length) {
    request.headers = requestSample.headers;
  }
  if (requestSample.pathParams && Object.keys(requestSample.pathParams).length) {
    request.path_params = requestSample.pathParams;
  }
  if (requestSample.query && Object.keys(requestSample.query).length) {
    request.query = requestSample.query;
  }
  if (requestSample.body !== undefined) {
    request.body = requestSample.body;
  }
  return request;
}

function resolveRequestParamExample(field, requestSample = {}) {
  if (field.example !== undefined) {
    return field.example;
  }
  if (field.location === "Header") {
    return requestSample.headers?.[field.name];
  }
  if (field.location === "Path") {
    return requestSample.pathParams?.[field.name];
  }
  if (field.location === "Query") {
    return requestSample.query?.[field.name];
  }
  return undefined;
}

function apiDocKey(doc) {
  return `${doc.method} ${doc.path}`;
}

async function copyText(value) {
  try {
    if (navigator.clipboard?.writeText) {
      await navigator.clipboard.writeText(value);
    } else {
      copyTextFallback(value);
    }
    toast("已复制到剪贴板", "success");
  } catch (_error) {
    try {
      copyTextFallback(value);
      toast("已复制到剪贴板", "success");
    } catch (_fallbackError) {
      toast("复制失败，请手动选中文本复制", "error");
    }
  }
}

function copyTextFallback(value) {
  const textarea = document.createElement("textarea");
  textarea.value = value;
  textarea.setAttribute("readonly", "readonly");
  textarea.style.position = "fixed";
  textarea.style.top = "-9999px";
  textarea.style.left = "-9999px";
  document.body.append(textarea);
  textarea.select();
  textarea.setSelectionRange(0, textarea.value.length);
  const copied = document.execCommand("copy");
  document.body.removeChild(textarea);
  if (!copied) {
    throw new Error("copy failed");
  }
}

function toast(message, kind = "success") {
  const title = kind === "error" ? "操作失败" : "操作成功";
  const item = {
    id: `${Date.now()}-${Math.random()}`,
    title,
    message,
    kind,
  };
  state.toasts = [...state.toasts, item].slice(-4);
  renderApp({ chrome: false, page: false, overlays: false, toasts: true });
  window.setTimeout(() => {
    state.toasts = state.toasts.filter((toastItem) => toastItem.id !== item.id);
    renderApp({ chrome: false, page: false, overlays: false, toasts: true });
  }, 2600);
}

function escapeHtml(value) {
  return String(value ?? "")
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;")
    .replaceAll("'", "&#39;");
}

function escapeAttr(value) {
  return escapeHtml(value).replaceAll("\n", " ");
}
