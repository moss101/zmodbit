/**
 * Bridge schema guard for the new task-workspace channels (REQ-EV-0103:
 * every renderer message is validated before the privileged host acts).
 */
import { describe, expect, it } from "vitest";
// eslint-disable-next-line @typescript-eslint/no-require-imports
const { validateIpcMessage, Rejected } = require("../electron/bridge-schema.cjs");

describe("bridge schema: task workspace channels", () => {
  it("accepts valid steer/pause/stop/runDetail/diff messages", () => {
    expect(validateIpcMessage("task:steer", { taskId: "t1", note: "focus" })).toEqual({
      kind: "steer",
      taskId: "t1",
      note: "focus",
    });
    expect(validateIpcMessage("task:pause", { taskId: "t1" }).kind).toBe("pause");
    expect(validateIpcMessage("task:stop", { taskId: "t1", reason: "" }).kind).toBe("stop");
    expect(validateIpcMessage("task:runDetail", { taskId: "t1" }).kind).toBe("runDetail");
    expect(validateIpcMessage("task:diff", { taskId: "t1" }).kind).toBe("diff");
  });

  it("rejects unknown fields and missing task ids", () => {
    expect(() => validateIpcMessage("task:steer", { taskId: "t1", note: "n", extra: 1 })).toThrow(
      Rejected,
    );
    expect(() => validateIpcMessage("task:pause", {})).toThrow(Rejected);
    expect(() => validateIpcMessage("task:diff", { taskId: "" })).toThrow(Rejected);
  });

  it("rejects prototype pollution", () => {
    expect(() =>
      validateIpcMessage("task:stop", JSON.parse('{"taskId":"t","__proto__":{"x":1}}')),
    ).toThrow(Rejected);
  });
});
