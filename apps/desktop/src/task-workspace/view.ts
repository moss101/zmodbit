/**
 * Task-workspace view model (docs/32 § task workspace): pure functions
 * turning surface payloads into renderable structures. The renderer never
 * fabricates state — everything derives from committed run-plane facts.
 */
import type { RunDetailView, TurnView, RunStepView, DiffView } from "@modbit/surface-protocol";
import type { TimelineEntry } from "@modbit/ui";

/** Timeline entries: one per turn (label) and one per step (indented detail). */
export function runTimeline(detail: RunDetailView | null | undefined): TimelineEntry[] {
  if (!detail) return [];
  const entries: TimelineEntry[] = [];
  for (const turn of detail.turns ?? []) {
    entries.push({
      id: turn.turnId,
      label: `Turn ${turn.turnId.slice(0, 8)}`,
      state: turn.state || undefined,
    });
    for (const step of turn.steps ?? []) {
      entries.push(stepEntry(step));
    }
  }
  return entries;
}

/** One step row: type label + failure detail when failed. */
export function stepEntry(step: RunStepView): TimelineEntry {
  return {
    id: step.stepId,
    label: step.stepType,
    state: step.state || undefined,
    detail: step.state === "failed" ? step.failureCode || "failed" : undefined,
  };
}

/** Ordered turns by their appearance in the detail (server order). */
export function orderedTurns(detail: RunDetailView | null | undefined): TurnView[] {
  return detail?.turns ?? [];
}

/** Diff rows sorted by path; total churn for the header. */
export function diffSummary(diff: DiffView | null | undefined): {
  files: { path: string; additions: number; deletions: number }[];
  additions: number;
  deletions: number;
  branch: string;
  baseRevision: string;
} {
  const files = [...(diff?.files ?? [])]
    .map((f) => ({ path: f.path, additions: Number(f.additions), deletions: Number(f.deletions) }))
    .sort((a, b) => a.path.localeCompare(b.path));
  const additions = files.reduce((n, f) => n + f.additions, 0);
  const deletions = files.reduce((n, f) => n + f.deletions, 0);
  return {
    files,
    additions,
    deletions,
    branch: diff?.branch ?? "",
    baseRevision: diff?.baseRevision ?? "",
  };
}

/** Conversation feed: durable task events rendered as messages. */
export interface ConversationItem {
  id: string;
  kind: "user" | "system" | "failure";
  text: string;
}

export function conversationFromEvents(
  events: { eventId: string; eventType: string; payload: string }[],
): ConversationItem[] {
  const items: ConversationItem[] = [];
  for (const e of events) {
    if (e.eventType === "task_created") {
      items.push({ id: e.eventId, kind: "user", text: promptOf(e.payload) });
    } else if (e.eventType === "task_input_queued") {
      items.push({ id: e.eventId, kind: "user", text: textOf(e.payload) });
    } else if (e.eventType === "task_steered") {
      items.push({ id: e.eventId, kind: "user", text: `steer: ${steerNoteOf(e.payload)}` });
    } else if (e.eventType === "task_failed") {
      items.push({ id: e.eventId, kind: "failure", text: messageOf(e.payload) });
    } else if (e.eventType === "task_waiting") {
      items.push({ id: e.eventId, kind: "system", text: "waiting for user input" });
    } else if (e.eventType === "task_ready_for_review") {
      items.push({ id: e.eventId, kind: "system", text: "ready for review" });
    } else if (e.eventType === "run_step_prepared") {
      const step = stepTypeOf(e.payload);
      // Tool/test steps carry the work the model did (incl. test.run).
      if (step === "tool_call" || step === "verification") {
        items.push({ id: e.eventId, kind: "system", text: `step: ${step}` });
      }
    } else if (e.eventType === "run_step_failed") {
      items.push({
        id: e.eventId,
        kind: "failure",
        text: `step failed: ${messageOf(e.payload)}`,
      });
    } else if (e.eventType === "run_failed") {
      items.push({ id: e.eventId, kind: "failure", text: `run failed: ${messageOf(e.payload)}` });
    } else if (e.eventType === "run_completed") {
      items.push({ id: e.eventId, kind: "system", text: "run completed" });
    }
  }
  return items;
}

function parsePayload(raw: string): Record<string, unknown> {
  try {
    return JSON.parse(raw) as Record<string, unknown>;
  } catch {
    return {};
  }
}

function promptOf(raw: string): string {
  const p = parsePayload(raw);
  return typeof p.prompt === "string" ? p.prompt : "";
}

function textOf(raw: string): string {
  const p = parsePayload(raw);
  return typeof p.text === "string" ? p.text : "";
}

function steerNoteOf(raw: string): string {
  const p = parsePayload(raw);
  return typeof p.steerNote === "string" ? p.steerNote : textOf(raw);
}

function stepTypeOf(raw: string): string {
  const p = parsePayload(raw);
  return typeof p.stepType === "string" ? p.stepType : "";
}

function messageOf(raw: string): string {
  const p = parsePayload(raw);
  return typeof p.message === "string" ? p.message : "task failed";
}
