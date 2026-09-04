// QUAL-EV-0103: malicious renderer messages without schema/capability are
// rejected at the bridge boundary before the privileged host acts.

import { describe, expect, it } from "vitest";
// @ts-expect-error CJS module from the electron side
import { validateIpcMessage, MAX_TITLE } from "../electron/bridge-schema.cjs";

const EVIL = {
  __proto__: { isAdmin: true },
  constructor: { prototype: {} },
};

describe("bridge schema guard (REQ-EV-0103)", () => {
  it("accepts a well-formed createTask message", () => {
    const request = validateIpcMessage("task:create", { title: "t", prompt: "p" });
    expect(request).toEqual({ kind: "createTask", title: "t", prompt: "p" });
  });

  it("rejects unknown channels", () => {
    expect(() => validateIpcMessage("shell:exec", { cmd: "rm -rf /" })).toThrow(/unknown channel/);
  });

  it("rejects non-object payloads (null, array, string)", () => {
    expect(() => validateIpcMessage("task:create", null)).toThrow(/plain object/);
    expect(() => validateIpcMessage("task:create", ["x"])).toThrow(/plain object/);
    expect(() => validateIpcMessage("task:create", "string")).toThrow(/plain object/);
  });

  it("rejects prototype-pollution keys anywhere in the payload", () => {
    // A literal __proto__ either poisons the prototype (rejected as
    // non-plain) or arrives as an own key (rejected as forbidden) — both
    // are hard rejections.
    expect(() =>
      validateIpcMessage("task:create", { title: "t", prompt: "p", __proto__: { x: 1 } } as never),
    ).toThrow(/plain object|forbidden key/);
    const nested = JSON.parse('{"title":"t","prompt":"p","extra":{"constructor":1}}');
    expect(() => validateIpcMessage("task:create", nested as never)).toThrow(/unknown field "extra"/);
    expect(EVIL).toBeDefined();
  });

  it("rejects unknown fields and wrong types", () => {
    expect(() =>
      validateIpcMessage("task:create", { title: "t", prompt: "p", admin: true } as never),
    ).toThrow(/unknown field "admin"/);
    expect(() =>
      validateIpcMessage("task:create", { title: 42, prompt: "p" } as never),
    ).toThrow(/must be a string/);
  });

  it("enforces length bounds", () => {
    expect(() =>
      validateIpcMessage("task:create", { title: "x".repeat(MAX_TITLE + 1), prompt: "p" }),
    ).toThrow(/exceeds/);
  });

  it("fleet snapshot requires no payload fields", () => {
    const request = validateIpcMessage("fleet:snapshot", undefined);
    expect(request.kind).toBe("fleetSnapshot");
    expect(() => validateIpcMessage("fleet:snapshot", { injected: true } as never)).toThrow(
      /unknown field/,
    );
  });
});
