import {
  apiRequest,
  type RequestOptions,
} from "@/shared/api/client";
import type {
  AuthTokensResponse,
  CurrentSession,
  DebugSnapResponse,
  FileArtifactSummary,
  HookEventSummary,
  MachineAllowlistEntry,
  NodeHeartbeatSummary,
  NodeSummary,
  PageResult,
  RecordFileSummary,
  RecordingControlRequest,
  RecordingControlResponse,
  StreamSummary,
  TaskDetail,
  TaskEventSummary,
  TaskLogResponse,
  TaskPreview,
  TaskSummary,
  UnknownJson,
} from "@/shared/api/types";

export function toQuery(params: Record<string, unknown>) {
  const query = new URLSearchParams();
  Object.entries(params).forEach(([key, value]) => {
    if (value === undefined || value === null || value === "") {
      return;
    }
    query.set(key, String(value));
  });
  const stringified = query.toString();
  return stringified ? `?${stringified}` : "";
}

function randomHex(bytes: number) {
  const cryptoApi = globalThis.crypto;
  if (cryptoApi?.getRandomValues) {
    const buffer = new Uint8Array(bytes);
    cryptoApi.getRandomValues(buffer);
    return Array.from(buffer, (value) => value.toString(16).padStart(2, "0")).join("");
  }

  return Array.from({ length: bytes * 2 }, () => Math.floor(Math.random() * 16).toString(16)).join("");
}

function createIdempotencyKey() {
  const cryptoApi = globalThis.crypto;
  if (typeof cryptoApi?.randomUUID === "function") {
    return cryptoApi.randomUUID();
  }
  return `${Date.now().toString(36)}-${randomHex(12)}`;
}

export const authApi = {
  currentSession: () => apiRequest<CurrentSession>("/api/v1/me"),
  login: (payload: { username: string; password: string }) =>
    apiRequest<AuthTokensResponse>("/api/v1/auth/login", {
      method: "POST",
      skipAuth: true,
      body: payload,
    }),
  refresh: (refreshToken: string) =>
    apiRequest<AuthTokensResponse>("/api/v1/auth/refresh", {
      method: "POST",
      skipAuth: true,
      body: { refresh_token: refreshToken },
    }),
  logout: (refreshToken: string, options: RequestOptions = {}) =>
    apiRequest<null>("/api/v1/auth/logout", {
      method: "POST",
      body: { refresh_token: refreshToken },
      ...options,
    }),
  changePassword: (payload: { current_password: string; new_password: string }) =>
    apiRequest<null>("/api/v1/auth/change-password", {
      method: "POST",
      body: payload,
    }),
};

export const taskApi = {
  preview: (payload: UnknownJson) =>
    apiRequest<TaskPreview>("/api/v1/tasks/preview", {
      method: "POST",
      body: payload,
    }),
  create: (payload: UnknownJson) =>
    apiRequest<TaskSummary>("/api/v1/tasks", {
      method: "POST",
      headers: { "Idempotency-Key": createIdempotencyKey() },
      body: payload,
    }),
  list: (params: Record<string, unknown>) =>
    apiRequest<PageResult<TaskSummary>>(`/api/v1/tasks${toQuery(params)}`),
  detail: (taskId: string) => apiRequest<TaskDetail>(`/api/v1/tasks/${taskId}`),
  events: (taskId: string, params: Record<string, unknown>) =>
    apiRequest<PageResult<TaskEventSummary>>(`/api/v1/tasks/${taskId}/events${toQuery(params)}`),
  logs: (taskId: string, params: Record<string, unknown>) =>
    apiRequest<TaskLogResponse>(`/api/v1/tasks/${taskId}/logs${toQuery(params)}`),
  resolvedSpec: (taskId: string) =>
    apiRequest<UnknownJson>(`/api/v1/tasks/${taskId}/resolved-spec`),
  start: (taskId: string) => apiRequest<TaskSummary>(`/api/v1/tasks/${taskId}/start`, { method: "POST" }),
  stop: (taskId: string) => apiRequest<TaskSummary>(`/api/v1/tasks/${taskId}/stop`, { method: "POST" }),
  cancel: (taskId: string) => apiRequest<TaskSummary>(`/api/v1/tasks/${taskId}/cancel`, { method: "POST" }),
  delete: (taskId: string) => apiRequest<TaskSummary>(`/api/v1/tasks/${taskId}`, { method: "DELETE" }),
  retry: (taskId: string) => apiRequest<TaskSummary>(`/api/v1/tasks/${taskId}/retry`, { method: "POST" }),
  startRecording: (taskId: string, payload: RecordingControlRequest) =>
    apiRequest<RecordingControlResponse>(`/api/v1/tasks/${taskId}/recording/start`, {
      method: "POST",
      body: payload,
    }),
  stopRecording: (taskId: string, reason = "user_requested") =>
    apiRequest<RecordingControlResponse>(`/api/v1/tasks/${taskId}/recording/stop`, {
      method: "POST",
      body: { reason },
    }),
  clone: (taskId: string, payload: UnknownJson) =>
    apiRequest<TaskSummary>(`/api/v1/tasks/${taskId}/clone`, {
      method: "POST",
      body: payload,
    }),
};

export const streamApi = {
  list: (params: Record<string, unknown>) =>
    apiRequest<StreamSummary[]>(`/api/v1/streams${toQuery(params)}`),
  close: (payload: UnknownJson) =>
    apiRequest<null>("/api/v1/debug/zlm/close-stream", {
      method: "POST",
      body: payload,
    }),
};

export const recordApi = {
  list: (params: Record<string, unknown>) =>
    apiRequest<PageResult<RecordFileSummary>>(`/api/v1/records${toQuery(params)}`),
};

export const artifactApi = {
  list: (params: Record<string, unknown>) =>
    apiRequest<PageResult<FileArtifactSummary>>(`/api/v1/file-artifacts${toQuery(params)}`),
};

export const nodeApi = {
  list: () => apiRequest<NodeSummary[]>("/api/v1/nodes"),
  heartbeats: (nodeId: string, limit = 24) =>
    apiRequest<NodeHeartbeatSummary[]>(`/api/v1/nodes/${nodeId}/heartbeats${toQuery({ limit })}`),
};

export const securityApi = {
  listMachineAllowlist: () =>
    apiRequest<{ entries: MachineAllowlistEntry[] }>("/api/v1/security/machine-allowlist"),
  updateMachineAllowlist: (entries: Array<{ cidr: string; description?: string | null }>) =>
    apiRequest<{ entries: MachineAllowlistEntry[] }>("/api/v1/security/machine-allowlist", {
      method: "PUT",
      body: { entries },
    }),
};

export const debugApi = {
  media: (params: Record<string, unknown>) =>
    apiRequest<UnknownJson>(`/api/v1/debug/zlm/media${toQuery(params)}`),
  sessions: (params: Record<string, unknown>) =>
    apiRequest<UnknownJson>(`/api/v1/debug/zlm/sessions${toQuery(params)}`),
  players: (params: Record<string, unknown>) =>
    apiRequest<UnknownJson>(`/api/v1/debug/zlm/players${toQuery(params)}`),
  statistic: (params: Record<string, unknown>) =>
    apiRequest<UnknownJson>(`/api/v1/debug/zlm/statistic${toQuery(params)}`),
  threadsLoad: (params: Record<string, unknown>) =>
    apiRequest<UnknownJson>(`/api/v1/debug/zlm/threads-load${toQuery(params)}`),
  workThreadsLoad: (params: Record<string, unknown>) =>
    apiRequest<UnknownJson>(`/api/v1/debug/zlm/work-threads-load${toQuery(params)}`),
  hooks: (params: Record<string, unknown>) =>
    apiRequest<HookEventSummary[]>(`/api/v1/debug/hooks${toQuery(params)}`),
  kickSession: (payload: UnknownJson) =>
    apiRequest<null>("/api/v1/debug/zlm/kick-session", {
      method: "POST",
      body: payload,
    }),
  kickSessions: (payload: UnknownJson) =>
    apiRequest<null>("/api/v1/debug/zlm/kick-sessions", {
      method: "POST",
      body: payload,
    }),
  closeStream: (payload: UnknownJson) =>
    apiRequest<null>("/api/v1/debug/zlm/close-stream", {
      method: "POST",
      body: payload,
    }),
  snap: (params: Record<string, unknown>) =>
    apiRequest<DebugSnapResponse>(`/api/v1/debug/zlm/snap${toQuery(params)}`),
};
