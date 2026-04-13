export interface OptionItem {
  value: string;
  label: string;
  note?: string;
}

export const NAV_ITEMS: Array<{ path: string; label: string; note: string; permission?: string | null }> = [
  { path: "/overview", label: "系统总览", note: "系统介绍、运行概况、节点容量" },
  { path: "/api-docs", label: "外部 API 文档", note: "第三方业务系统对接说明与示例" },
  { path: "/tasks", label: "任务中心", note: "创建、筛选、派发、重试", permission: "task_read" },
  { path: "/streams", label: "流中心", note: "在线流、播放地址、关闭流", permission: "task_read" },
  { path: "/multicast", label: "组播中心", note: "组播任务、网卡、TTL、上下游", permission: "task_read" },
  { path: "/records", label: "录像中心", note: "录像索引、日期检索、路径复制", permission: "record_read" },
  { path: "/file-artifacts", label: "文件产物", note: "桥接输出、转码输出与快录文件的 HTTP 地址、文件路径", permission: "record_read" },
  { path: "/security", label: "安全设置", note: "修改密码、维护机器 API 白名单", permission: "security_write" },
  { path: "/debug", label: "调试台", note: "ZLM 原始调试、会话、踢人、关流", permission: "debug_read" },
];

export const TASK_TYPES: OptionItem[] = [
  { value: "stream_ingest", label: "流接入", note: "接入源为平台内部流，可选暴露播放协议与录制" },
  { value: "stream_bridge", label: "流桥接", note: "把源桥接到文件、组播或外部 RTMP/RTMPS 等显式输出目标" },
  { value: "file_transcode", label: "文件转码", note: "离线转码并生成目标文件" },
];

export const INPUT_KINDS: OptionItem[] = [
  { value: "rtsp", label: "RTSP" },
  { value: "rtmp", label: "RTMP" },
  { value: "hls", label: "HLS" },
  { value: "http_mp4", label: "HTTP MP4" },
  { value: "http_flv", label: "HTTP-FLV" },
  { value: "http_ts", label: "HTTP-TS" },
  { value: "file", label: "文件" },
  { value: "udp_mpegts_multicast", label: "UDP MPEGTS 组播" },
  { value: "rtp_multicast", label: "RTP 组播" },
  { value: "gb_rtp", label: "GB RTP" },
];

export const SOURCE_MODES: OptionItem[] = [
  { value: "live", label: "实时源" },
  { value: "vod", label: "离线源" },
];

export const PUBLISH_KINDS: OptionItem[] = [
  { value: "file", label: "文件输出" },
  { value: "udp_mpegts_multicast", label: "UDP MPEGTS 组播" },
  { value: "rtp_multicast", label: "RTP 组播" },
  { value: "rtmp_push", label: "RTMP / RTMPS 推流" },
];

export const PUBLISH_FORMATS: OptionItem[] = [
  { value: "", label: "系统默认", note: "文件输出默认 MP4；组播输出按目标自动选择合适封装格式" },
  { value: "mp4", label: "MP4" },
  { value: "flv", label: "FLV" },
  { value: "mpegts", label: "MPEGTS" },
  { value: "rtp_mpegts", label: "RTP MPEGTS" },
  { value: "matroska", label: "Matroska / MKV" },
  { value: "mov", label: "MOV" },
  { value: "webm", label: "WebM" },
];

export const START_MODES: OptionItem[] = [
  { value: "immediate", label: "立即启动" },
  { value: "manual", label: "手动启动" },
  { value: "cron", label: "定时计划" },
  { value: "at", label: "指定时间" },
];

export const RECORD_FORMATS: OptionItem[] = [
  { value: "mp4", label: "MP4" },
  { value: "hls", label: "HLS" },
  { value: "both", label: "MP4 + HLS" },
];

export const RECOVERY_POLICIES: OptionItem[] = [
  { value: "never", label: "不恢复" },
  { value: "auto", label: "恢复" },
];

export const PROCESS_MODES: OptionItem[] = [
  { value: "passthrough", label: "直通" },
  { value: "copy_or_transcode", label: "拷贝优先，必要时转码" },
  { value: "force_transcode", label: "强制转码" },
];

const labelGroups = {
  taskType: Object.fromEntries(TASK_TYPES.map((item) => [item.value, item.label])),
  inputKind: Object.fromEntries(INPUT_KINDS.map((item) => [item.value, item.label])),
  publishKind: Object.fromEntries(PUBLISH_KINDS.map((item) => [item.value, item.label])),
  publishFormat: Object.fromEntries(PUBLISH_FORMATS.map((item) => [item.value, item.label])),
  sourceMode: Object.fromEntries(SOURCE_MODES.map((item) => [item.value, item.label])),
  processMode: Object.fromEntries(PROCESS_MODES.map((item) => [item.value, item.label])),
  startMode: Object.fromEntries(START_MODES.map((item) => [item.value, item.label])),
  recordFormat: Object.fromEntries(RECORD_FORMATS.map((item) => [item.value, item.label])),
  recoveryPolicy: Object.fromEntries(
    [...RECOVERY_POLICIES, { value: "on_failure", label: "恢复" }, { value: "always", label: "恢复" }].map((item) => [item.value, item.label]),
  ),
} as const;

export function labelFor(group: keyof typeof labelGroups, value?: string | null, fallback = "—") {
  if (!value) {
    return fallback;
  }
  return labelGroups[group][value as keyof (typeof labelGroups)[typeof group]] ?? value;
}

export const taskTypeLabel = (value?: string | null, fallback = "未设置") =>
  labelFor("taskType", value, fallback);
export const inputKindLabel = (value?: string | null, fallback = "未设置") =>
  labelFor("inputKind", value, fallback);
export const publishKindLabel = (value?: string | null, fallback = "未设置") =>
  labelFor("publishKind", value, fallback);
export const publishFormatLabel = (value?: string | null, fallback = "自动推断") =>
  labelFor("publishFormat", value ?? "", fallback);
export const sourceModeLabel = (value?: string | null, fallback = "未设置") =>
  labelFor("sourceMode", value, fallback);
export const processModeLabel = (value?: string | null, fallback = "未设置") =>
  labelFor("processMode", value, fallback);
export const startModeLabel = (value?: string | null, fallback = "未设置") =>
  labelFor("startMode", value, fallback);
export const recordFormatLabel = (value?: string | null, fallback = "未设置") =>
  labelFor("recordFormat", value, fallback);
export const recoveryPolicyLabel = (value?: string | null, fallback = "未设置") =>
  labelFor("recoveryPolicy", value, fallback);
