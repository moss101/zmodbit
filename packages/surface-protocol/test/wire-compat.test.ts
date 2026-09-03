// Rust↔TS wire compatibility (M0.3 acceptance: round-trip compatibility tests).
//
// The golden fixtures under proto/fixtures/ were produced by the Rust encoder
// (crates/protocol/tests/wire_compat.rs). These tests prove the TypeScript
// bindings decode those exact bytes, and re-encode them byte-identically —
// which, combined with the Rust-side round trip, proves both directions:
// Rust→TS (decode fixture) and TS→Rust (byte-equal re-encode).

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";

import { CommandEnvelope } from "../src/generated/modbit/protocol/v1/commands";
import { TaskEvent } from "../src/generated/modbit/protocol/v1/events";
import { TaskStatus } from "../src/generated/modbit/protocol/v1/domain";

const fixture = (name: string): Uint8Array =>
  new Uint8Array(
    readFileSync(new URL(`../../../proto/fixtures/${name}`, import.meta.url)),
  );

describe("TaskEvent wire compatibility (Rust-produced fixture)", () => {
  const bytes = fixture("task_event_v1.bin");

  it("decodes the Rust-encoded fixture with expected field values", () => {
    const evt = TaskEvent.decode(bytes);
    expect(evt.taskId).toBe("0198c7a2-7b10-7cc2-9d4e-6a1f2b3c4d5e");
    expect(evt.generation).toBe("7");
    expect(evt.taskCreated?.sessionId).toBe("0198c7a2-7b10-7cc2-9d4e-000000000001");
    expect(evt.taskCreated?.title).toBe("Implement event store projections");
    expect(evt.taskCreated?.prompt).toBe(
      "Create the durable task aggregate with idempotent commands.",
    );
    expect(evt.taskCreated?.initialStatus).toBe(TaskStatus.TASK_STATUS_QUEUED);
  });

  it("re-encodes byte-identically (TS→Rust direction)", () => {
    const decoded = TaskEvent.decode(bytes);
    expect(TaskEvent.encode(decoded).finish()).toEqual(bytes);
  });

  it("matches a TS-constructed canonical message byte-for-byte", () => {
    const built: TaskEvent = {
      taskId: "0198c7a2-7b10-7cc2-9d4e-6a1f2b3c4d5e",
      generation: "7",
      taskCreated: {
        sessionId: "0198c7a2-7b10-7cc2-9d4e-000000000001",
        title: "Implement event store projections",
        prompt: "Create the durable task aggregate with idempotent commands.",
        initialStatus: TaskStatus.TASK_STATUS_QUEUED,
      },
    };
    expect(TaskEvent.encode(built).finish()).toEqual(bytes);
  });
});

describe("CommandEnvelope wire compatibility (Rust-produced fixture)", () => {
  const bytes = fixture("command_envelope_v1.bin");

  it("decodes the Rust-encoded fixture with expected field values", () => {
    const cmd = CommandEnvelope.decode(bytes);
    expect(cmd.commandId).toBe("0198c7a2-7b10-7cc2-9d4e-ffffffffffff");
    expect(cmd.tenantId).toBe("tenant-alpha");
    expect(cmd.userId).toBe("user-mohsin");
    expect(cmd.sessionId).toBe("0198c7a2-7b10-7cc2-9d4e-000000000001");
    expect(cmd.aggregateId).toBe("0198c7a2-7b10-7cc2-9d4e-6a1f2b3c4d5e");
    expect(cmd.expectedGeneration).toBe("7");
    expect(cmd.commandType).toBe("CreateTask");
    expect(cmd.schemaVersion?.major).toBe(1);
    expect(cmd.schemaVersion?.minor).toBe(0);
    expect(cmd.issuedAt?.getTime()).toBe(1_785_000_000_123);
  });

  it("re-encodes byte-identically (TS→Rust direction)", () => {
    const decoded = CommandEnvelope.decode(bytes);
    expect(CommandEnvelope.encode(decoded).finish()).toEqual(bytes);
  });

  it("matches a TS-constructed canonical message byte-for-byte", () => {
    const built: CommandEnvelope = {
      commandId: "0198c7a2-7b10-7cc2-9d4e-ffffffffffff",
      tenantId: "tenant-alpha",
      userId: "user-mohsin",
      sessionId: "0198c7a2-7b10-7cc2-9d4e-000000000001",
      aggregateId: "0198c7a2-7b10-7cc2-9d4e-6a1f2b3c4d5e",
      expectedGeneration: "7",
      commandType: "CreateTask",
      schemaVersion: { major: 1, minor: 0 },
      payload: new Uint8Array(0),
      issuedAt: new Date(1_785_000_000_123),
    };
    expect(CommandEnvelope.encode(built).finish()).toEqual(bytes);
  });
});
