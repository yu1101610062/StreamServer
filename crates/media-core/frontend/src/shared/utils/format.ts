import type { TaskEventSummary } from "@/shared/api/types";

export function shortId(value?: string | null) {
  return String(value ?? "").slice(0, 8);
}

export function formatTime(value?: string | null) {
  if (!value) {
    return "—";
  }
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) {
    return value;
  }
  return date.toLocaleString("zh-CN", { hour12: false });
}

export function formatBytes(bytes?: number | null) {
  if (bytes === undefined || bytes === null) {
    return "—";
  }
  const units = ["B", "KB", "MB", "GB", "TB"];
  let value = bytes;
  let unitIndex = 0;
  while (value >= 1024 && unitIndex < units.length - 1) {
    value /= 1024;
    unitIndex += 1;
  }
  return `${value.toFixed(value >= 10 || unitIndex === 0 ? 0 : 1)} ${units[unitIndex]}`;
}

export function formatPercent(value?: number | null) {
  if (value === undefined || value === null) {
    return "—";
  }
  return `${value.toFixed(1)}%`;
}

export function formatBitrateKbps(value?: number | null) {
  if (value === undefined || value === null) {
    return "未上报";
  }
  return `${value.toFixed(value >= 100 ? 0 : 1)} kbps`;
}

export function errorMessage(error: unknown) {
  if (!error) {
    return "未知错误";
  }
  if (typeof error === "string") {
    return error;
  }
  if (error instanceof Error) {
    const payloadMessage = (error as Error & { payload?: { message?: string } }).payload?.message;
    return payloadMessage || error.message;
  }
  return String(error);
}

export function taskValidationMessage(error: unknown) {
  if (error instanceof Error) {
    const payload = (error as Error & { payload?: Record<string, unknown> }).payload;
    const details = isPlainObject(payload?.details) ? payload.details : null;
    const issues = Array.isArray(details?.issues) ? details.issues : [];
    const firstIssue = issues.find((issue) => isPlainObject(issue)) as
      | { field?: unknown; message?: unknown }
      | undefined;
    const field = typeof firstIssue?.field === "string" ? firstIssue.field.trim() : "";
    const message = typeof firstIssue?.message === "string" ? firstIssue.message.trim() : "";
    if (message) {
      return field ? `${field}: ${message}` : message;
    }
  }
  return errorMessage(error);
}

export function isPlainObject(value: unknown): value is Record<string, unknown> {
  return Boolean(value) && typeof value === "object" && !Array.isArray(value);
}

export function formatJson(value: unknown) {
  return JSON.stringify(value ?? {}, null, 2);
}

export function deriveLatestProgress(events: TaskEventSummary[]) {
  return events.find((event) => event.event_type === "task_progress")?.payload ?? null;
}

export function deriveLastIssue(events: TaskEventSummary[]) {
  const issue = events.find((event) => ["error", "warn"].includes(event.event_level.toLowerCase()));
  if (!issue) {
    return "";
  }
  const payload = issue.payload as Record<string, unknown>;
  return (
    String(payload.failure_reason ?? "") ||
    String(payload.message ?? "") ||
    issue.event_type ||
    "最近存在异常事件"
  );
}
