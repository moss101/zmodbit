import { useCallback, useEffect, useState } from "react";
import { groupFleet, FLEET_VIEW_ORDER, FLEET_VIEW_LABELS, TASK_STATUS, type TaskCard } from "./fleet/grouping";
import { superviseFleet } from "./fleet/supervision";
import { statusSummary } from "./status-center/status";
import { TaskWorkspace } from "./task-workspace/TaskWorkspace";

// docs/32: the renderer never fabricates completion — every card renders
// from Core data (projections derived from committed events only).
export default function App() {
  const [tasks, setTasks] = useState<TaskCard[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [title, setTitle] = useState("");
  const [prompt, setPrompt] = useState("");
  const [submitting, setSubmitting] = useState(false);
  const [selectedTask, setSelectedTask] = useState<string | null>(null);

  const refresh = useCallback(async () => {
    try {
      const snapshot = await window.modbit.fleetSnapshot();
      if (snapshot.ok) {
        setTasks(snapshot.fleet.tasks);
        setError(null);
      } else {
        setError(snapshot.error ?? "unknown core error");
      }
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  }, []);

  // Event-driven updates (docs/30 § SubscribeEvents): one initial snapshot,
  // then the forwarded Core event stream drives refreshes — the 1.5s poll
  // is gone. Any task event implies fleet state may have changed.
  useEffect(() => {
    void refresh();
    return window.modbit.onCoreEvent(() => void refresh());
  }, [refresh]);

  const submit = useCallback(async () => {
    if (!title.trim() || submitting) return;
    setSubmitting(true);
    try {
      const response = await window.modbit.createTask(title.trim(), prompt.trim());
      if (!response.ok) {
        setError(response.error ?? "task creation failed");
      } else {
        setTitle("");
        setPrompt("");
        await refresh();
      }
    } finally {
      setSubmitting(false);
    }
  }, [title, prompt, submitting, refresh]);

  const grouped = groupFleet(tasks);
  const summary = statusSummary(tasks);
  const supervised = superviseFleet(tasks);
  const workspaceTask = tasks.find((t) => t.taskId === selectedTask) ?? null;

  return (
    <main>
      <h1>Modbit Fleet</h1>
      <section aria-label="Status center">
        <h2>Status center</h2>
        <p>
          {summary.totalTasks} tasks · {summary.attention} need attention ·{" "}
          {summary.bySession.length} session(s)
        </p>
      </section>
      <section aria-label="Needs attention supervision">
        <h2>Needs attention — single next action</h2>
        {supervised.length === 0 ? (
          <p className="empty">nothing needs attention</p>
        ) : (
          supervised.map(({ task, nextAction }) => (
            <article key={task.taskId}>
              <strong>{task.title}</strong> — {nextAction}
            </article>
          ))
        )}
      </section>
      {error ? <p role="alert">Core error: {error}</p> : null}
      <section aria-label="New task">
        <input
          placeholder="Task title"
          value={title}
          onChange={(e) => setTitle(e.target.value)}
        />
        <textarea
          placeholder="What should the agent do?"
          value={prompt}
          onChange={(e) => setPrompt(e.target.value)}
        />
        <button type="button" disabled={submitting || !title.trim()} onClick={() => void submit()}>
          {submitting ? "Creating…" : "New task"}
        </button>
      </section>
      {FLEET_VIEW_ORDER.map((view) => (
        <section key={view} aria-label={FLEET_VIEW_LABELS[view]}>
          <h2>
            {FLEET_VIEW_LABELS[view]} <span className="count">{grouped[view].length}</span>
          </h2>
          {grouped[view].length === 0 ? (
            <p className="empty">none</p>
          ) : (
            grouped[view].map((t) => (
              <article
                key={t.taskId}
                className={selectedTask === t.taskId ? "task-card selected" : "task-card"}
                tabIndex={0}
                role="button"
                onClick={() => setSelectedTask(t.taskId)}
                onKeyDown={(e) => {
                  if (e.key === "Enter") setSelectedTask(t.taskId);
                }}
              >
                <strong>{t.title}</strong>
                <span> {t.taskId}</span>
              </article>
            ))
          )}
        </section>
      ))}
      {workspaceTask ? (
        <TaskWorkspace task={workspaceTask} onClose={() => setSelectedTask(null)} />
      ) : null}
    </main>
  );
}
