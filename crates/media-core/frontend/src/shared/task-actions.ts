import type { TaskSummary } from "@/shared/api/types";

export const TASK_OPERATION_CONFIGS = [
  {
    key: "start",
    label: "启动",
    supportedStatuses: ["CREATED", "VALIDATING", "FAILED", "CANCELED"],
    danger: false,
  },
  {
    key: "stop",
    label: "停止",
    supportedStatuses: ["DISPATCHING", "STARTING", "RUNNING", "RECOVERING"],
    danger: false,
  },
  {
    key: "cancel",
    label: "取消",
    supportedStatuses: [
      "CREATED",
      "VALIDATING",
      "QUEUED",
      "DISPATCHING",
      "STARTING",
      "RUNNING",
      "RECOVERING",
    ],
    danger: false,
  },
  {
    key: "retry",
    label: "重试",
    supportedStatuses: ["FAILED", "LOST"],
    danger: false,
  },
  {
    key: "delete",
    label: "删除",
    supportedStatuses: ["CREATED", "VALIDATING", "QUEUED", "SUCCEEDED", "FAILED", "CANCELED", "LOST"],
    danger: true,
  },
] as const;

export type TaskOperation = (typeof TASK_OPERATION_CONFIGS)[number]["key"];

const CLONEABLE_STATUSES = ["SUCCEEDED", "FAILED", "CANCELED", "LOST"];

export function taskOperationConfig(action: TaskOperation) {
  return TASK_OPERATION_CONFIGS.find((config) => config.key === action)!;
}

export function canRunTaskOperation(task: TaskSummary, action: TaskOperation) {
  return (taskOperationConfig(action).supportedStatuses as readonly string[]).includes(task.status);
}

export function tasksSupportingOperation(tasks: TaskSummary[], action: TaskOperation) {
  return tasks.filter((task) => canRunTaskOperation(task, action));
}

export function availableTaskOperations(tasks: TaskSummary[]) {
  return TASK_OPERATION_CONFIGS.filter((config) => tasks.some((task) => canRunTaskOperation(task, config.key)));
}

export function canCloneTask(task: TaskSummary) {
  return CLONEABLE_STATUSES.includes(task.status);
}

export function rowActions(task: TaskSummary) {
  return {
    canStart: canRunTaskOperation(task, "start"),
    canStop: canRunTaskOperation(task, "stop"),
    canCancel: canRunTaskOperation(task, "cancel"),
    canRetry: canRunTaskOperation(task, "retry"),
    canClone: canCloneTask(task),
    canDelete: canRunTaskOperation(task, "delete"),
  };
}
