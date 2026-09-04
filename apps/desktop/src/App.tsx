import { useCallback, useEffect, useState } from "react";
import { groupFleet, FLEET_VIEW_ORDER, FLEET_VIEW_LABELS, type TaskCard } from "./fleet/grouping";

// docs/32: the renderer never fabricates completion — every card renders
// from Core data (projections derived from committed events only).
export default function App() {
  const [tasks, setTasks] = useState<TaskCard[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [title, setTitle] = useState("");
  const [prompt, setPrompt] = useState("");
  const [submitting, setSubmitting] = useState(false);

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

  useEffect(() => {
    void refresh();
    const timer = setInterval(() => void refresh(), 1500);
    return () => clearInterval(timer);
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

  return (
    <main>
      <h1>Modbit Fleet</h1>
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
              <article key={t.taskId}>
                <strong>{t.title}</strong>
                <span> {t.taskId}</span>
              </article>
            ))
          )}
        </section>
      ))}
    </main>
  );
}
