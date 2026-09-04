import { describe, expect, it } from "vitest";
import { toInspectorRows, type InspectorEvent } from "./inspector";

const event = (generation: number, eventType: string, payload: unknown): InspectorEvent => ({
  eventId: `e${generation}`,
  aggregateId: "task",
  generation: String(generation),
  eventType,
  payload,
});

describe("context inspector rows (IMP-EV-0035/0175)", () => {
  it("summarizes a durable stream with sequences in order", () => {
    const rows = toInspectorRows([
      event(1, "task_created", { event: "task_created", title: "write tests", prompt: "p" }),
      event(2, "task_started", {}),
      event(3, "task_input_queued", { event: "task_input_queued", input_id: "i", mode: "steer", text: "focus on edge cases" }),
      event(4, "task_completed", { event: "task_completed", summary: "all green" }),
    ]);
    expect(rows.map((r) => r.sequence)).toEqual([1, 2, 3, 4]);
    expect(rows[0]!.summary).toContain("write tests");
    expect(rows[2]!.summary).toContain("focus on edge cases");
    expect(rows[3]!.summary).toContain("all green");
  });

  it("renders only committed facts: an empty stream shows nothing", () => {
    expect(toInspectorRows([])).toEqual([]);
  });
});
