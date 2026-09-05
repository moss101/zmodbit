import type { CSSProperties } from "react";
import { statusColor } from "@modbit/design-tokens";

/** Human label for a wire state string. */
export function statusLabel(state: string): string {
  return state
    .replace(/[()]/g, "")
    .replace(/_/g, " ")
    .trim();
}

/** Colored state pill: task status, run state or step state. */
export function StatusPill({ state, title }: { state: string; title?: string }) {
  const style: CSSProperties = {
    color: statusColor(state),
    borderColor: statusColor(state),
  };
  return (
    <span className="modbit-status-pill" style={style} title={title ?? state}>
      {statusLabel(state)}
    </span>
  );
}
