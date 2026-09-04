import { describe, expect, it } from "vitest";
import { statusSummary } from "./status";
import type { TaskCard } from "../fleet/grouping";
import { TASK_STATUS } from "../fleet/grouping";

const card = (sessionId: string, state: number): TaskCard => ({
  taskId: `t-${Math.random().toString(36).slice(2)}`,
  sessionId,
  title: "task",
  state,
  createdAt: "2026-09-04T00:00:00.000Z",
  generation: "1",
});

describe("status center (IMP-EV-0143)", () => {
  it("summarizes counts per session from durable state only", () => {
    const s1 = "session-1";
    const s2 = "session-2";
    const summary = statusSummary([
      card(s1, TASK_STATUS.QUEUED),
      card(s1, TASK_STATUS.STARTED),
      card(s2, TASK_STATUS.COMPLETED),
    ]);
    expect(summary.totalTasks).toBe(3);
    expect(summary.bySession).toHaveLength(2);
    const session1 = summary.bySession.find((s) => s.sessionId === s1)!;
    expect(session1.taskCount).toBe(2);
  });

  it("reports attention count across sessions", () => {
    const s1 = "session-1";
    const summary = statusSummary([
      card(s1, TASK_STATUS.NEEDS_ATTENTION),
      card(s1, TASK_STATUS.COMPLETED),
    ]);
    expect(summary.attention).toBe(1);
  });
});
