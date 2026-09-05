// Task workspace screen (docs/32 § task workspace): conversation + steering,
// run timeline, revision-bound diff and test output — every value derived
// from committed Core facts, never fabricated by the renderer.
import { useCallback, useEffect, useState } from "react";
import {
  ActionButton,
  DiffFileRow,
  Panel,
  StatusPill,
  SteerComposer,
  Timeline,
} from "@modbit/ui";
import {
  conversationFromEvents,
  diffSummary,
  runTimeline,
} from "./view";
import type { TaskCard } from "../fleet/grouping";
import { TASK_STATUS } from "../fleet/grouping";

/** Reverse enum: numeric TaskStatus → wire label for the pill. */
const STATE_LABEL: Record<number, string> = Object.fromEntries(
  Object.entries(TASK_STATUS).map(([name, code]) => [code, name.toLowerCase()]),
);

/** Surface payloads shaped by the preload bridge (camelCase). */
interface RunDetail {
  taskId: string;
  runState: string;
  failureCode: string;
  turns: { turnId: string; state: string; steps: { stepId: string; stepType: string; state: string; failureCode: string }[] }[];
}
interface Diff {
  taskId: string;
  branch: string;
  baseRevision: string;
  files: { path: string; additions: string; deletions: string }[];
}
interface TaskEventEnvelope {
  eventId: string;
  eventType: string;
  payload: unknown;
}

export function TaskWorkspace({
  task,
  onClose,
}: {
  task: TaskCard;
  onClose: () => void;
}) {
  const [detail, setDetail] = useState<RunDetail | null>(null);
  const [diff, setDiff] = useState<Diff | null>(null);
  const [events, setEvents] = useState<TaskEventEnvelope[]>([]);
  const [busy, setBusy] = useState(false);

  const reload = useCallback(async () => {
    setBusy(true);
    try {
      const [d, g, e] = await Promise.all([
        window.modbit.runDetail(task.taskId),
        window.modbit.diff(task.taskId),
        window.modbit.taskEvents(task.taskId),
      ]);
      if (d.ok && d.runDetail) setDetail(d.runDetail);
      if (g.ok && g.diff) setDiff(g.diff);
      if (e.ok && e.taskEvents) setEvents(e.taskEvents.events ?? []);
    } finally {
      setBusy(false);
    }
  }, [task.taskId]);

  useEffect(() => {
    void reload();
  }, [reload]);

  // Refresh when this task's events arrive over SSE (offset-correct).
  useEffect(() => {
    return window.modbit.onTaskEvent((evt) => {
      if (evt.aggregateId === task.taskId) void reload();
    });
  }, [reload, task.taskId]);

  const steer = async (note: string) => {
    await window.modbit.steerTask(task.taskId, note);
    void reload();
  };
  const pause = async () => {
    await window.modbit.pauseTask(task.taskId);
    void reload();
  };
  const stop = async () => {
    await window.modbit.stopTask(task.taskId, "");
    void reload();
  };

  const summary = diffSummary(diff as never);
  const conversation = conversationFromEvents(
    events.map((e) => ({
      eventId: e.eventId,
      eventType: e.eventType,
      payload:
        typeof e.payload === "string" ? e.payload : JSON.stringify(e.payload ?? {}),
    })),
  );

  return (
    <section className="task-workspace" aria-label={`Task workspace ${task.title}`}>
      <header>
        <h2>{task.title}</h2>
        <StatusPill state={STATE_LABEL[task.state] ?? ""} />
        <div className="task-workspace-actions">
          <ActionButton kind="secondary" onClick={() => void reload()} disabled={busy}>
            Refresh
          </ActionButton>
          <ActionButton kind="secondary" onClick={() => void pause()}>
            Pause
          </ActionButton>
          <ActionButton kind="danger" onClick={() => void stop()}>
            Stop
          </ActionButton>
          <ActionButton kind="secondary" onClick={onClose}>
            Close
          </ActionButton>
        </div>
      </header>
      <div className="task-workspace-grid">
        <Panel title="Conversation">
          <ul className="conversation">
            {conversation.map((item) => (
              <li key={item.id} data-kind={item.kind}>
                {item.text}
              </li>
            ))}
          </ul>
          <SteerComposer onSteer={(note) => void steer(note)} />
        </Panel>
        <Panel
          title="Run timeline"
          actions={<StatusPill state={detail?.runState ?? ""} />}
        >
          <Timeline
            entries={runTimeline(
              detail
                ? ({
                    turns: detail.turns.map((t) => ({
                      turnId: t.turnId,
                      state: t.state,
                      steps: t.steps.map((s) => ({
                        stepId: s.stepId,
                        turnId: t.turnId,
                        stepType: s.stepType,
                        state: s.state,
                        failureCode: s.failureCode,
                      })),
                    })),
                  } as never)
                : null,
            )}
          />
        </Panel>
        <Panel
          title={`Diff vs ${summary.baseRevision.slice(0, 8) || "base"}`}
          actions={
            <span className="diff-summary">
              {summary.branch} · +{summary.additions} −{summary.deletions}
            </span>
          }
        >
          {summary.files.length === 0 ? (
            <p className="modbit-empty">no changes yet</p>
          ) : (
            <ul className="diff-files">
              {summary.files.map((f) => (
                <DiffFileRow key={f.path} path={f.path} additions={f.additions} deletions={f.deletions} />
              ))}
            </ul>
          )}
        </Panel>
      </div>
    </section>
  );
}
