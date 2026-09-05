/**
 * Task-workspace view model tests: run timeline ordering, step failure
 * surfacing, diff summary math and the conversation projection.
 */
import { describe, expect, it } from "vitest";
import {
  conversationFromEvents,
  diffSummary,
  runTimeline,
} from "./view";
import type { RunDetailView } from "@modbit/surface-protocol";

const detail: RunDetailView = {
  taskId: "t1",
  runState: "running",
  failureCode: "",
  turns: [
    {
      turnId: "turn-aaaa",
      state: "completed",
      steps: [
        { stepId: "s1", turnId: "turn-aaaa", stepType: "model_invoke", state: "completed", failureCode: "" },
        { stepId: "s2", turnId: "turn-aaaa", stepType: "tool_call", state: "failed", failureCode: "tool_refused_or_failed" },
      ],
    },
    {
      turnId: "turn-bbbb",
      state: "streaming",
      steps: [
        { stepId: "s3", turnId: "turn-bbbb", stepType: "model_invoke", state: "prepared", failureCode: "" },
      ],
    },
  ],
};

describe("runTimeline", () => {
  it("interleaves turns and their steps in order", () => {
    const entries = runTimeline(detail);
    expect(entries.map((e) => e.id)).toEqual([
      "turn-aaaa",
      "s1",
      "s2",
      "turn-bbbb",
      "s3",
    ]);
  });

  it("surfaces step failure codes as details", () => {
    const failed = runTimeline(detail).find((e) => e.id === "s2");
    expect(failed?.state).toBe("failed");
    expect(failed?.detail).toBe("tool_refused_or_failed");
  });

  it("empty for absent detail", () => {
    expect(runTimeline(null)).toEqual([]);
    expect(runTimeline(undefined)).toEqual([]);
  });
});

describe("diffSummary", () => {
  it("sorts files and totals churn", () => {
    const summary = diffSummary({
      taskId: "t1",
      branch: "modbit/abc",
      baseRevision: "rev1",
      files: [
        { path: "z.rs", additions: "1", deletions: "0" },
        { path: "a.rs", additions: "2", deletions: "5" },
      ],
    });
    expect(summary.files.map((f) => f.path)).toEqual(["a.rs", "z.rs"]);
    expect(summary.additions).toBe(3);
    expect(summary.deletions).toBe(5);
    expect(summary.branch).toBe("modbit/abc");
  });
});

describe("conversationFromEvents", () => {
  it("projects durable task events into a conversation", () => {
    const items = conversationFromEvents([
      { eventId: "e1", eventType: "task_created", payload: JSON.stringify({ prompt: "fix the bug" }) },
      { eventId: "e2", eventType: "task_input_queued", payload: JSON.stringify({ text: "also tests" }) },
      { eventId: "e3", eventType: "task_steered", payload: JSON.stringify({ steerNote: "focus" }) },
      { eventId: "e4", eventType: "task_ready_for_review", payload: "{}" },
    ]);
    expect(items.map((i) => i.kind)).toEqual(["user", "user", "user", "system"]);
    expect(items[0]?.text).toBe("fix the bug");
  });

  it("surfaces tool/test steps and failures from the run plane", () => {
    const items = conversationFromEvents([
      {
        eventId: "s1",
        eventType: "run_step_prepared",
        payload: JSON.stringify({ stepType: "tool_call", ordinal: 2 }),
      },
      {
        eventId: "s2",
        eventType: "run_step_failed",
        payload: JSON.stringify({ failureCode: "tool_refused_or_failed" }),
      },
    ]);
    expect(items[0]?.text).toBe("step: tool_call");
    expect(items[1]?.kind).toBe("failure");
  });
});
