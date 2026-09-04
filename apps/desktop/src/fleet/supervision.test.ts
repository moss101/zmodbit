import { describe, expect, it } from "vitest";
import { superviseFleet, actionableTasks } from "./supervision";
import { TASK_STATUS, type TaskCard } from "./grouping";

const card = (state: number, taskId: string): TaskCard => ({
  taskId,
  sessionId: "s",
  title: `t ${taskId}`,
  state,
  createdAt: "",
  generation: "1",
});

describe("fleet supervision (IMP-EV-0037)", () => {
  it("surfaces attention and waiting cards with a single next action", () => {
    const cards = superviseFleet([
      card(TASK_STATUS.NEEDS_ATTENTION, "a"),
      card(TASK_STATUS.WAITING, "b"),
      card(TASK_STATUS.STARTED, "c"),
    ]);
    expect(cards).toHaveLength(2);
    expect(cards[0]?.nextAction).toContain("review");
    expect(cards[1]?.nextAction).toContain("respond");
  });

  it("identifies actionable running/review tasks", () => {
    const actionable = actionableTasks([
      card(TASK_STATUS.QUEUED, "q"),
      card(TASK_STATUS.STARTED, "r"),
      card(TASK_STATUS.READY_FOR_REVIEW, "v"),
    ]);
    expect(actionable.map((t) => t.taskId)).toEqual(["r", "v"]);
  });
});
