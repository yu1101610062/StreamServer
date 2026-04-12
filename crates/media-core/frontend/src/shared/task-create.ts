import {
  INPUT_KINDS,
  PROCESS_MODES,
  PUBLISH_FORMATS,
  PUBLISH_KINDS,
  RECORD_FORMATS,
  RECOVERY_POLICIES,
  SOURCE_MODES,
  START_MODES,
  TASK_TYPES,
  inputKindLabel,
  processModeLabel,
  publishKindLabel,
  sourceModeLabel,
  taskTypeLabel,
} from "@/shared/labels";
import { isPlainObject } from "@/shared/utils/format";

export interface TaskCreateDraft {
  task_type: string;
  name: string;
  priority: string;
  common: {
    created_by: string;
    callback_url: string;
    labels_text: string;
  };
  input: {
    kind: string;
    source_mode: string;
    url: string;
    group: string;
    port: string;
    interface_name: string;
    interface_ip: string;
    ttl: string;
    reuse: boolean;
    probe_timeout_ms: string;
    tcp_mode: string;
    ssrc: string;
  };
  process: {
    mode: string;
    bitrate: string;
    fps: string;
    gop: string;
  };
  stream: {
    app: string;
    name: string;
    vhost: string;
  };
  expose: {
    enable_rtsp: boolean;
    enable_rtmp: boolean;
    enable_http_ts: boolean;
    enable_http_fmp4: boolean;
    enable_hls: boolean;
    stop_on_no_reader: boolean;
  };
  publish: {
    kind: string;
    url: string;
    group: string;
    port: string;
    interface_name: string;
    interface_ip: string;
    ttl: string;
    format: string;
  };
  record: {
    enabled: boolean;
    format: string;
    duration_sec: string;
    segment_sec: string;
    save_path: string;
    as_player: boolean;
  };
  recovery: {
    policy: string;
    resume_mode: string;
    max_consecutive_failures: string;
  };
  schedule: {
    start_mode: string;
    start_at: string;
    cron: string;
  };
  resource: {
    required_labels_text: string;
    preferred_labels_text: string;
  };
  advanced_json: string;
}

export const guidedScenarios = [
  {
    id: "live-ingest",
    title: "接入实时流并对外播放",
    description: "适合 RTSP/RTMP/HLS 等实时源，接入后在平台内部形成统一流。",
    apply: (draft: TaskCreateDraft) => {
      draft.task_type = "stream_ingest";
      draft.input.kind = "rtsp";
      draft.input.source_mode = "live";
      draft.process.mode = "copy_or_transcode";
      draft.record.enabled = false;
    },
  },
  {
    id: "ingest-record",
    title: "接入并录制",
    description: "保留在线访问能力，同时将内部流录制到文件，适合值守回看场景。",
    apply: (draft: TaskCreateDraft) => {
      draft.task_type = "stream_ingest";
      draft.input.kind = "rtsp";
      draft.input.source_mode = "live";
      draft.record.enabled = true;
      draft.record.format = "mp4";
      draft.record.duration_sec = "300";
    },
  },
  {
    id: "bridge-out",
    title: "桥接到文件、组播或外部推流",
    description: "把源直接导出到文件、组播目标或外部 RTMP / RTMPS 平台，不保留为平台内部流。",
    apply: (draft: TaskCreateDraft) => {
      draft.task_type = "stream_bridge";
      draft.input.kind = "rtsp";
      draft.input.source_mode = "live";
      draft.publish.kind = "file";
    },
  },
  {
    id: "file-transcode",
    title: "离线转码导出",
    description: "将文件或点播源转为目标文件，用于归档和文件产物输出。",
    apply: (draft: TaskCreateDraft) => {
      draft.task_type = "file_transcode";
      draft.input.kind = "file";
      draft.input.source_mode = "vod";
      draft.publish.kind = "file";
    },
  },
];

export function defaultSourceModeForInputKind(kind: string) {
  if (kind === "file" || kind === "http_mp4") return "vod";
  if (
    ["rtsp", "rtmp", "http_flv", "udp_mpegts_multicast", "rtp_multicast", "gb_rtp"].includes(kind)
  ) {
    return "live";
  }
  return "";
}

export function inputKindSupportsExplicitSourceMode(kind: string) {
  return kind === "hls" || kind === "http_ts";
}

export function createDefaultDraft(): TaskCreateDraft {
  const draft: TaskCreateDraft = {
    task_type: "stream_ingest",
    name: "",
    priority: "50",
    common: {
      created_by: "",
      callback_url: "",
      labels_text: "",
    },
    input: {
      kind: "rtsp",
      source_mode: "live",
      url: "",
      group: "",
      port: "",
      interface_name: "",
      interface_ip: "",
      ttl: "",
      reuse: false,
      probe_timeout_ms: "7000",
      tcp_mode: "",
      ssrc: "",
    },
    process: {
      mode: "copy_or_transcode",
      bitrate: "",
      fps: "",
      gop: "",
    },
    stream: {
      app: "live",
      name: "",
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
    publish: {
      kind: "",
      url: "",
      group: "",
      port: "",
      interface_name: "",
      interface_ip: "",
      ttl: "",
      format: "",
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
      policy: "auto",
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
    advanced_json: "{}",
  };
  normalizeDraftForTaskType(draft, draft.task_type);
  return draft;
}

export function normalizeDraftForTaskType(draft: TaskCreateDraft, taskType: string) {
  draft.task_type = taskType;
  if (taskType === "file_transcode") {
    if (!["file", "http_mp4", "hls", "http_ts"].includes(draft.input.kind)) {
      draft.input.kind = "file";
    }
    draft.publish.kind = "file";
    draft.input.source_mode = draft.input.source_mode || defaultSourceModeForInputKind(draft.input.kind) || "vod";
    draft.record.enabled = false;
  } else if (taskType === "stream_bridge") {
    if (draft.input.kind === "gb_rtp" || !draft.input.kind) {
      draft.input.kind = "rtsp";
    }
    draft.publish.kind = draft.publish.kind || "file";
    draft.input.source_mode = draft.input.source_mode || defaultSourceModeForInputKind(draft.input.kind) || "live";
    draft.record.enabled = false;
  } else {
    draft.publish.kind = "";
    draft.input.source_mode = draft.input.source_mode || defaultSourceModeForInputKind(draft.input.kind) || "live";
  }
}

function toOptionalNumber(value: string) {
  const numeric = Number(String(value ?? "").trim());
  return Number.isFinite(numeric) ? numeric : undefined;
}

function parseAdvancedJson(raw: string) {
  const trimmed = raw.trim();
  if (!trimmed || trimmed === "{}") {
    return {};
  }
  try {
    const parsed = JSON.parse(trimmed) as Record<string, unknown>;
    return isPlainObject(parsed) ? parsed : {};
  } catch {
    return {};
  }
}

function setIfPresent(target: Record<string, unknown>, key: string, value: string) {
  const trimmed = value.trim();
  if (trimmed) {
    target[key] = trimmed;
  }
}

function setIfNumber(target: Record<string, unknown>, key: string, value: string) {
  const parsed = toOptionalNumber(value);
  if (parsed !== undefined) {
    target[key] = parsed;
  }
}

function setIfBoolean(target: Record<string, unknown>, key: string, value: boolean) {
  if (typeof value === "boolean") {
    target[key] = value;
  }
}

function setIfList(target: Record<string, unknown>, key: string, raw: string) {
  const list = raw
    .split(",")
    .map((item) => item.trim())
    .filter(Boolean);
  if (list.length) {
    target[key] = list;
  }
}

function pruneEmptyObjects(target: Record<string, unknown>) {
  Object.entries(target).forEach(([key, value]) => {
    if (isPlainObject(value)) {
      pruneEmptyObjects(value);
      if (Object.keys(value).length === 0) {
        delete target[key];
      }
    }
  });
}

function mergeInto(target: Record<string, unknown>, overlay: Record<string, unknown>) {
  Object.entries(overlay).forEach(([key, value]) => {
    if (isPlainObject(value) && isPlainObject(target[key])) {
      mergeInto(target[key] as Record<string, unknown>, value);
    } else {
      target[key] = value;
    }
  });
}

export function buildDraftPayload(draft: TaskCreateDraft) {
  const payload: Record<string, unknown> = {
    type: draft.task_type,
    name: draft.name.trim(),
    priority: toOptionalNumber(draft.priority) ?? 50,
    common: {},
    input: {},
    process: {},
    stream: {},
    expose: {},
    publish: {},
    record: {},
    recovery: {},
    schedule: {},
    resource: {},
  };

  setIfPresent(payload.common as Record<string, unknown>, "created_by", draft.common.created_by);
  setIfPresent(payload.common as Record<string, unknown>, "callback_url", draft.common.callback_url);
  setIfList(payload.common as Record<string, unknown>, "labels", draft.common.labels_text);

  setIfPresent(payload.input as Record<string, unknown>, "kind", draft.input.kind);
  setIfPresent(payload.input as Record<string, unknown>, "source_mode", draft.input.source_mode);
  setIfPresent(payload.input as Record<string, unknown>, "url", draft.input.url);
  setIfPresent(payload.input as Record<string, unknown>, "group", draft.input.group);
  setIfNumber(payload.input as Record<string, unknown>, "port", draft.input.port);
  setIfPresent(payload.input as Record<string, unknown>, "interface_name", draft.input.interface_name);
  setIfPresent(payload.input as Record<string, unknown>, "interface_ip", draft.input.interface_ip);
  setIfNumber(payload.input as Record<string, unknown>, "ttl", draft.input.ttl);
  setIfBoolean(payload.input as Record<string, unknown>, "reuse", draft.input.reuse);
  setIfNumber(payload.input as Record<string, unknown>, "probe_timeout_ms", draft.input.probe_timeout_ms);
  setIfNumber(payload.input as Record<string, unknown>, "tcp_mode", draft.input.tcp_mode);
  setIfNumber(payload.input as Record<string, unknown>, "ssrc", draft.input.ssrc);

  setIfPresent(payload.process as Record<string, unknown>, "mode", draft.process.mode);
  setIfNumber(payload.process as Record<string, unknown>, "bitrate", draft.process.bitrate);
  setIfNumber(payload.process as Record<string, unknown>, "fps", draft.process.fps);
  setIfNumber(payload.process as Record<string, unknown>, "gop", draft.process.gop);

  setIfPresent(payload.stream as Record<string, unknown>, "app", draft.stream.app);
  setIfPresent(payload.stream as Record<string, unknown>, "name", draft.stream.name);
  setIfPresent(payload.stream as Record<string, unknown>, "vhost", draft.stream.vhost);

  [
    "enable_rtsp",
    "enable_rtmp",
    "enable_http_ts",
    "enable_http_fmp4",
    "enable_hls",
    "stop_on_no_reader",
  ].forEach((key) => {
    setIfBoolean(payload.expose as Record<string, unknown>, key, draft.expose[key as keyof typeof draft.expose] as boolean);
  });

  setIfPresent(payload.publish as Record<string, unknown>, "kind", draft.publish.kind);
  if (draft.publish.kind === "rtmp_push") {
    setIfPresent(payload.publish as Record<string, unknown>, "url", draft.publish.url);
  }
  setIfPresent(payload.publish as Record<string, unknown>, "group", draft.publish.group);
  setIfNumber(payload.publish as Record<string, unknown>, "port", draft.publish.port);
  setIfPresent(payload.publish as Record<string, unknown>, "interface_name", draft.publish.interface_name);
  setIfPresent(payload.publish as Record<string, unknown>, "interface_ip", draft.publish.interface_ip);
  setIfNumber(payload.publish as Record<string, unknown>, "ttl", draft.publish.ttl);
  setIfPresent(payload.publish as Record<string, unknown>, "format", draft.publish.format);

  setIfBoolean(payload.record as Record<string, unknown>, "enabled", draft.record.enabled);
  setIfPresent(payload.record as Record<string, unknown>, "format", draft.record.format);
  setIfNumber(payload.record as Record<string, unknown>, "duration_sec", draft.record.duration_sec);
  setIfNumber(payload.record as Record<string, unknown>, "segment_sec", draft.record.segment_sec);
  setIfPresent(payload.record as Record<string, unknown>, "save_path", draft.record.save_path);
  setIfBoolean(payload.record as Record<string, unknown>, "as_player", draft.record.as_player);

  setIfPresent(payload.recovery as Record<string, unknown>, "policy", draft.recovery.policy);
  setIfPresent(payload.recovery as Record<string, unknown>, "resume_mode", draft.recovery.resume_mode);
  setIfNumber(payload.recovery as Record<string, unknown>, "max_consecutive_failures", draft.recovery.max_consecutive_failures);

  setIfPresent(payload.schedule as Record<string, unknown>, "start_mode", draft.schedule.start_mode);
  setIfPresent(payload.schedule as Record<string, unknown>, "start_at", draft.schedule.start_at);
  setIfPresent(payload.schedule as Record<string, unknown>, "cron", draft.schedule.cron);

  setIfList(payload.resource as Record<string, unknown>, "required_labels", draft.resource.required_labels_text);
  setIfList(payload.resource as Record<string, unknown>, "preferred_labels", draft.resource.preferred_labels_text);

  pruneEmptyObjects(payload);
  mergeInto(payload, parseAdvancedJson(draft.advanced_json));
  return payload;
}

export function humanSummary(draft: TaskCreateDraft) {
  const parts = [
    `目标是${taskTypeLabel(draft.task_type)}`,
    draft.input.kind ? `输入源为${inputKindLabel(draft.input.kind)}` : "",
    draft.input.source_mode ? `按${sourceModeLabel(draft.input.source_mode)}处理` : "",
    draft.process.mode ? `处理策略为${processModeLabel(draft.process.mode)}` : "",
    draft.task_type === "stream_bridge" && draft.publish.kind
      ? `直接输出到${publishKindLabel(draft.publish.kind)}`
      : "",
    draft.publish.kind === "file" ? "文件路径由平台自动生成" : "",
    draft.publish.kind === "rtmp_push" && draft.publish.url.trim()
      ? `推送到 ${draft.publish.url.trim()}`
      : "",
    draft.record.enabled ? `并开启录制` : "",
  ].filter(Boolean);
  return `${parts.join("，")}。`;
}

export const optionSets = {
  taskTypes: TASK_TYPES,
  inputKinds: INPUT_KINDS,
  sourceModes: SOURCE_MODES,
  publishKinds: PUBLISH_KINDS,
  publishFormats: PUBLISH_FORMATS,
  processModes: PROCESS_MODES,
  recordFormats: RECORD_FORMATS,
  recoveryPolicies: RECOVERY_POLICIES,
  startModes: START_MODES,
};
