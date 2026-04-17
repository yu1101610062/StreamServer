export interface CurrentSession {
  auth_enabled: boolean;
  auth_mode: string;
  subject: string;
  role: string;
  must_change_password: boolean;
  permissions: string[];
  environment: string;
}

export interface AuthTokensResponse {
  access_token: string;
  refresh_token?: string;
  subject?: string;
}

export interface PageResult<T> {
  items: T[];
  page: number;
  page_size: number;
  total: number;
}

export interface TaskSummary {
  id: string;
  name: string;
  type: string;
  status: string;
  priority: number;
  created_by: string;
  assigned_node_id?: string | null;
  current_attempt_no: number;
  created_at: string;
  updated_at: string;
  started_at?: string | null;
  finished_at?: string | null;
  transcode_mode?: "none" | "adaptive" | "forced" | null;
}

export interface AttemptSummary {
  id: string;
  attempt_no: number;
  worker_kind: string;
  status: string;
  node_id?: string | null;
  pid?: number | null;
  exit_code?: number | null;
  failure_code?: string | null;
  failure_reason?: string | null;
  started_at?: string | null;
  ended_at?: string | null;
}

export interface TaskEventSummary {
  id: string;
  attempt_no?: number | null;
  source: string;
  event_type: string;
  event_level: string;
  payload: Record<string, unknown>;
  created_at: string;
}

export interface CallbackDeliverySummary {
  callback_url: string;
  event_type: string;
  reason: string;
  status: string;
  delivery_attempts: number;
  last_http_status?: number | null;
  last_error?: string | null;
  delivered_at?: string | null;
  updated_at: string;
}

export interface TaskDetail {
  task: TaskSummary;
  requested_spec: Record<string, unknown>;
  resolved_spec?: Record<string, unknown> | null;
  current_attempt?: AttemptSummary | null;
  recent_events: TaskEventSummary[];
  callback_delivery?: CallbackDeliverySummary | null;
  records: RecordFileSummary[];
  file_artifacts: FileArtifactSummary[];
}

export interface TaskPreview {
  requested_spec: Record<string, unknown>;
  resolved_spec: Record<string, unknown>;
}

export interface TaskLogLine {
  ts: string;
  stream: string;
  line: string;
}

export interface TaskLogResponse {
  attempt_no: number;
  next_cursor?: string | null;
  lines: TaskLogLine[];
}

export interface StreamSummary {
  id: string;
  task_id: string;
  attempt_id: string;
  attempt_no: number;
  task_name: string;
  node_id?: string | null;
  schema: string;
  vhost: string;
  app: string;
  stream: string;
  zlm_proxy_key?: string | null;
  zlm_pusher_key?: string | null;
  rtp_stream_id?: string | null;
  started_at?: string | null;
  updated_at: string;
  has_viewer?: boolean | null;
  viewer_count?: number | null;
  bitrate_kbps?: number | null;
  play_urls: string[];
}

export interface RecordFileSummary {
  id: string;
  task_id: string;
  task_name: string;
  attempt_id?: string | null;
  vhost?: string | null;
  app?: string | null;
  stream?: string | null;
  file_path: string;
  http_url?: string | null;
  file_size: number;
  time_len?: number | null;
  start_time?: string | null;
  source: string;
  created_at: string;
}

export interface FileArtifactSummary {
  id: string;
  artifact_kind: "transcode_output" | "bridge_output" | "stream_ingest_record";
  task_id: string;
  task_name: string;
  attempt_id?: string | null;
  node_id: string;
  file_name: string;
  file_path: string;
  http_url: string;
  file_size: number;
  created_at: string;
}

export interface GpuRuntimeStats {
  name?: string;
  utilization_gpu?: number;
  utilization_memory?: number;
  memory_total_mb?: number;
  memory_used_mb?: number;
}

export interface NodeSummary {
  id: string;
  node_name: string;
  hostname: string;
  labels: string[];
  zlm_api_base: string;
  agent_stream_addr: string;
  network_mode: string;
  interfaces: string[];
  healthy: boolean;
  last_seen_at?: string | null;
  created_at: string;
  updated_at: string;
  ffmpeg_protocols: string[];
  ffmpeg_formats: string[];
  ffmpeg_encoders: string[];
  ffmpeg_decoders: string[];
  zlm_api_list: string[];
  zlm_version?: string | null;
  gpu: string[];
  gpu_devices: Record<string, unknown>[];
  capability_captured_at?: string | null;
  slot_usage?: number | null;
  running_tasks?: number | null;
  connected?: boolean | null;
  cpu_percent?: number | null;
  mem_percent?: number | null;
  disk_percent?: number | null;
  zlm_alive?: boolean | null;
  ffmpeg_alive?: boolean | null;
}

export interface NodeHeartbeatSummary {
  node_id: string;
  cpu_percent: number;
  mem_percent: number;
  disk_percent: number;
  running_tasks: number;
  slot_usage: number;
  zlm_alive: boolean;
  ffmpeg_alive: boolean;
  gpu_runtime: GpuRuntimeStats[];
  node_time: string;
  received_at: string;
}

export interface MachineAllowlistEntry {
  id: string;
  cidr: string;
  description?: string | null;
  created_at: string;
  updated_at: string;
}

export interface HookEventSummary {
  id: string;
  server_id: string;
  hook_name: string;
  dedup_key: string;
  payload: Record<string, unknown>;
  created_at: string;
}

export interface DebugSnapResponse {
  data_url: string;
}

export interface ApiErrorPayload {
  message?: string;
  [key: string]: unknown;
}

export class ApiError extends Error {
  status: number;
  payload?: ApiErrorPayload;

  constructor(message: string, status: number, payload?: ApiErrorPayload) {
    super(message);
    this.name = "ApiError";
    this.status = status;
    this.payload = payload;
  }
}

export type UnknownJson = Record<string, unknown>;
