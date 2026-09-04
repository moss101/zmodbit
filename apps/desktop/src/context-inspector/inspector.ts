// Context Inspector view model (M1, IMP-EV-0035/0175): renders the task's
// durable event stream — committed facts only, never fabricated.
export interface InspectorEvent {
  eventId: string;
  aggregateId: string;
  generation: string;
  eventType: string;
  payload: unknown;
}

export interface InspectorRow {
  sequence: number;
  eventType: string;
  summary: string;
}

export function toInspectorRows(events: InspectorEvent[]): InspectorRow[] {
  return events.map((e) => ({
    sequence: Number(e.generation),
    eventType: e.eventType,
    summary: summarizePayload(e.eventType, e.payload),
  }));
}

function summarizePayload(eventType: string, payload: unknown): string {
  if (payload === null || typeof payload !== "object") return eventType;
  const p = payload as Record<string, unknown>;
  const title = typeof p.title === "string" ? p.title : undefined;
  const summary = typeof p.summary === "string" ? p.summary : undefined;
  if (eventType === "task_created" && title) return `created: ${title}`;
  if (eventType === "task_completed" && summary) return `completed: ${summary}`;
  if (eventType === "task_input_queued") {
    const text = typeof p.text === "string" ? p.text : "";
    const mode = typeof p.mode === "string" ? p.mode : "input";
    return `input (${mode}): ${text}`;
  }
  if (eventType === "goal_set") {
    const objective = typeof p.objective === "string" ? p.objective : "";
    return `goal set: ${objective}`;
  }
  return title ? `${eventType}: ${title}` : eventType;
}
