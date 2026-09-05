/**
 * Token tests: the exported set must stay complete (every status key maps,
 * the CSS var sheet covers the core ramp) and `statusColor` must resolve
 * the wire forms the surface actually emits.
 */
import { describe, expect, it } from "vitest";
import {
  TOKEN_STYLE,
  colorStatus,
  statusColor,
  tokenVars,
  space,
  font,
} from "./index";

describe("design tokens", () => {
  it("covers the status vocabulary used by the surface", () => {
    for (const key of [
      "created",
      "queued",
      "running",
      "readyForReview",
      "completed",
      "failed",
      "cancelled",
      "waiting",
    ] as const) {
      expect(colorStatus[key]).toMatch(/^#[0-9a-f]{6}$/i);
    }
  });

  it("emits a css var sheet from the token set", () => {
    expect(TOKEN_STYLE).toContain(":root {");
    expect(TOKEN_STYLE).toContain(tokenVars["--modbit-accent"]);
    expect(Object.keys(tokenVars).length).toBeGreaterThan(15);
  });

  it("resolves wire state strings to status colors", () => {
    expect(statusColor("ready_for_review")).toBe(colorStatus.readyForReview);
    expect(statusColor("running")).toBe(colorStatus.running);
    expect(statusColor("exited(0)")).toBe(colorStatus.completed);
    expect(statusColor("tool_refused_or_failed")).toBe(colorStatus.failed);
    expect(statusColor("waiting_for_approval")).toBe(colorStatus.waiting);
    expect(statusColor("mystery-state")).toBe(colorStatus.created);
  });

  it("keeps a 4px spacing base", () => {
    expect(space.xs).toBe("4px");
    expect(font.familyMono).toContain("monospace");
  });
});
