// Trusted Code Surface (M2.9, docs/20 § Trusted Code Surface): the renderer
// requests IMMUTABLE file payloads bound to workspace + file revisions. It
// never owns unsaved buffers; staleness is detected by revision comparison.

import { describe, expect, it } from "vitest";

/// Mirrors the pb::CodeViewModel the surface returns.
export interface CodeViewModel {
  workspaceRevision: string;
  fileRevision: string;
  path: string;
  contentSha256: string;
  contentText: string;
}

/// Stale-reference handling (docs/20 § Stale reference handling): a cached
/// view bound to revisions older than the workspace's current ones is stale,
/// and old line numbers are not current truth.
export function isStale(
  view: Pick<CodeViewModel, "workspaceRevision" | "fileRevision">,
  currentWorkspaceRevision: string,
  currentFileRevision: string,
): boolean {
  return (
    BigInt(view.workspaceRevision) < BigInt(currentWorkspaceRevision) ||
    BigInt(view.fileRevision) < BigInt(currentFileRevision)
  );
}

describe("trusted code surface revision binding", () => {
  const view = {
    workspaceRevision: "7",
    fileRevision: "2",
    path: "src/main.rs",
    contentSha256: "abc",
    contentText: "fn main() {}",
  };

  it("detects a stale view when the workspace advanced", () => {
    expect(isStale(view, "8", "2")).toBe(true);
  });

  it("detects a stale view when the file advanced", () => {
    expect(isStale(view, "7", "3")).toBe(true);
  });

  it("a view bound to current revisions is fresh", () => {
    expect(isStale(view, "7", "2")).toBe(false);
  });

  it("never reports fresher-than-current", () => {
    // A view can never be ahead of the workspace (revisions only grow).
    expect(isStale({ ...view, workspaceRevision: "99" }, "7", "2")).toBe(false);
  });
});
