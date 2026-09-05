/**
 * SSE parser + daemon-address extraction tests (pure functions from
 * electron/event-stream.cjs; the subscription loop is exercised live in the
 * daemon e2e).
 */
import { describe, expect, it } from "vitest";
// eslint-disable-next-line @typescript-eslint/no-require-imports
const { createSseParser, parseDaemonAddr } = require("../electron/event-stream.cjs");

describe("createSseParser", () => {
  it("parses complete frames and skips comments", () => {
    const seen: unknown[] = [];
    const parser = createSseParser((e: unknown) => seen.push(e));
    parser.feed('data: {"sequence":1}\n\n: keepalive\n');
    expect(seen).toEqual([{ sequence: 1 }]);
  });

  it("reassembles frames split across chunks", () => {
    const seen: unknown[] = [];
    const parser = createSseParser((e: unknown) => seen.push(e));
    parser.feed('data: {"event_id":"a"');
    parser.feed(',"event_type":"task_started"}\n');
    expect(seen).toEqual([{ event_id: "a", event_type: "task_started" }]);
  });

  it("ignores malformed json frames", () => {
    const seen: unknown[] = [];
    const parser = createSseParser((e: unknown) => seen.push(e));
    parser.feed("data: not-json\n");
    expect(seen).toEqual([]);
  });
});

describe("parseDaemonAddr", () => {
  it("extracts the bound daemon address", () => {
    expect(parseDaemonAddr("modbit-core: http daemon on 127.0.0.1:54321")).toBe(
      "127.0.0.1:54321",
    );
  });

  it("returns null for unrelated lines", () => {
    expect(parseDaemonAddr("modbit-core: serving on /tmp/x.sock")).toBeNull();
    expect(parseDaemonAddr("")).toBeNull();
  });
});
