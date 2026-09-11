import { describe, expect, it } from "vitest";
import { fleetViewOf, groupFleet, nestChildren, TASK_STATUS, type TaskCard } from "./grouping";

const card = (state: number, taskId: string): TaskCard => ({
  taskId,
  sessionId: "s",
  title: `task ${taskId}`,
  state,
  createdAt: "2026-09-04T00:00:00.000Z",
  generation: "1",
});

describe("fleet view grouping (MOD-UX-001)", () => {
  it("maps every canonical state to its first-class view", () => {
    expect(fleetViewOf(TASK_STATUS.NEEDS_ATTENTION)).toBe("attention");
    expect(fleetViewOf(TASK_STATUS.READY_FOR_REVIEW)).toBe("review");
    expect(fleetViewOf(TASK_STATUS.STARTED)).toBe("running");
    expect(fleetViewOf(TASK_STATUS.QUEUED)).toBe("running");
    expect(fleetViewOf(TASK_STATUS.WAITING)).toBe("waiting");
    expect(fleetViewOf(TASK_STATUS.COMPLETED)).toBe("completed");
    expect(fleetViewOf(TASK_STATUS.FAILED)).toBe("failed");
    expect(fleetViewOf(TASK_STATUS.CANCELLED)).toBe("archived");
    expect(fleetViewOf(TASK_STATUS.CREATED)).toBe("running");
  });

  it("groups a fleet snapshot into ordered views", () => {
    const grouped = groupFleet([
      card(TASK_STATUS.COMPLETED, "t1"),
      card(TASK_STATUS.STARTED, "t2"),
      card(TASK_STATUS.READY_FOR_REVIEW, "t3"),
      card(TASK_STATUS.FAILED, "t4"),
    ]);
    expect(grouped.completed.map((t) => t.taskId)).toEqual(["t1"]);
    expect(grouped.running.map((t) => t.taskId)).toEqual(["t2"]);
    expect(grouped.review.map((t) => t.taskId)).toEqual(["t3"]);
    expect(grouped.failed.map((t) => t.taskId)).toEqual(["t4"]);
  });
});

describe("nestChildren (Phase 7 item 1)", () => {
  it("nests children under their parent and keeps orphans top-level", () => {
    const parent = { taskId: "p", sessionId: "s", title: "parent", state: 2, createdAt: "", generation: "1" };
    const childA = { taskId: "a", sessionId: "s", title: "child a", state: 2, createdAt: "", generation: "1", parentTaskId: "p" };
    const childB = { taskId: "b", sessionId: "s", title: "child b", state: 3, createdAt: "", generation: "2", parentTaskId: "p" };
    const orphan = { taskId: "o", sessionId: "s", title: "lost child", state: 2, createdAt: "", generation: "1", parentTaskId: "missing" };

    const roots = nestChildren([parent, childA, childB, orphan]);
    expect(roots.map((r) => r.taskId).sort()).toEqual(["o", "p"]);
    const p = roots.find((r) => r.taskId === "p")!;
    expect(p.children.map((c) => c.taskId).sort()).toEqual(["a", "b"]);
  });
});
