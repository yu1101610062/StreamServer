import { describe, expect, it } from "vitest";

import {
  availableTaskOperations,
  rowActions,
  tasksSupportingOperation,
} from "@/shared/task-actions";
import type { TaskSummary } from "@/shared/api/types";

function task(id: string, status: string): TaskSummary {
  return {
    id,
    name: id,
    type: "stream_ingest",
    status,
    priority: 0,
    created_by: "test",
    current_attempt_no: 0,
    created_at: "2026-01-01T00:00:00Z",
    updated_at: "2026-01-01T00:00:00Z",
  };
}

describe("task-actions", () => {
  it("returns the union of operations available on selected tasks", () => {
    const actions = availableTaskOperations([
      task("created", "CREATED"),
      task("running", "RUNNING"),
      task("failed", "FAILED"),
    ]).map((action) => action.key);

    expect(actions).toEqual(["start", "stop", "cancel", "retry", "delete"]);
  });

  it("filters a batch operation down to tasks that support it", () => {
    const selectedTasks = [
      task("created", "CREATED"),
      task("running", "RUNNING"),
      task("failed", "FAILED"),
    ];

    expect(tasksSupportingOperation(selectedTasks, "retry").map((item) => item.id)).toEqual(["failed"]);
    expect(tasksSupportingOperation(selectedTasks, "stop").map((item) => item.id)).toEqual(["running"]);
  });

  it("keeps row actions aligned with batch operation rules", () => {
    expect(rowActions(task("failed", "FAILED"))).toMatchObject({
      canStart: true,
      canStop: false,
      canCancel: false,
      canRetry: true,
      canClone: true,
      canDelete: true,
    });
  });
});
