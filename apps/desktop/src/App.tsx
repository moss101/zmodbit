import { useCallback, useEffect, useState } from "react";
import type { RecentRepoView } from "@modbit/surface-protocol";
import { groupFleet, FLEET_VIEW_ORDER, FLEET_VIEW_LABELS, TASK_STATUS, type TaskCard } from "./fleet/grouping";
import { superviseFleet } from "./fleet/supervision";
import { statusSummary } from "./status-center/status";
import { TaskWorkspace } from "./task-workspace/TaskWorkspace";

// docs/32: the renderer never fabricates completion — every card renders
// from Core data (projections derived from committed events only).
// docs/32 § task composer + Phase 4.1: the composer carries the
// repository picker (recent repos, register-by-path or clone-by-URL)
// and the per-task base branch. Repo data comes from Core (projections
// over committed registrations) — the renderer never invents it.
export default function App() {
  const [tasks, setTasks] = useState<TaskCard[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [title, setTitle] = useState("");
  const [prompt, setPrompt] = useState("");
  const [submitting, setSubmitting] = useState(false);
  const [selectedTask, setSelectedTask] = useState<string | null>(null);
  const [repos, setRepos] = useState<RecentRepoView[]>([]);
  const [selectedRepo, setSelectedRepo] = useState("");
  const [baseBranch, setBaseBranch] = useState("");
  const [repoPath, setRepoPath] = useState("");
  const [cloneUrl, setCloneUrl] = useState("");

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

  const refreshRepos = useCallback(async () => {
    try {
      const listed = await window.modbit.listRecentRepos();
      if (listed.ok && listed.recentRepos) setRepos(listed.recentRepos.repos);
    } catch {
      // Repo listing is advisory; the composer stays usable without it.
    }
  }, []);

  // Event-driven updates (docs/30 § SubscribeEvents): one initial snapshot,
  // then the forwarded Core event stream drives refreshes — the 1.5s poll
  // is gone. Any task event implies fleet state may have changed.
  useEffect(() => {
    void refresh();
    void refreshRepos();
    return window.modbit.onCoreEvent(() => void refresh());
  }, [refresh, refreshRepos]);

  const submit = useCallback(async () => {
    if (!title.trim() || submitting) return;
    setSubmitting(true);
    try {
      const response = await window.modbit.createTask(
        title.trim(),
        prompt.trim(),
        selectedRepo,
        baseBranch.trim(),
      );
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
  }, [title, prompt, selectedRepo, baseBranch, submitting, refresh]);

  const registerRepo = useCallback(async () => {
    if (!repoPath.trim() && !cloneUrl.trim()) return;
    setSubmitting(true);
    try {
      const response = await window.modbit.registerRepo(repoPath.trim(), cloneUrl.trim());
      if (!response.ok) {
        setError(response.error ?? "repo registration failed");
      } else {
        setRepoPath("");
        setCloneUrl("");
        await refreshRepos();
        if (response.repo) setSelectedRepo(response.repo.repoId);
      }
    } finally {
      setSubmitting(false);
    }
  }, [repoPath, cloneUrl, refreshRepos]);

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
      <section aria-label="Repository">
        <h2>Repository</h2>
        <select
          aria-label="Recent repositories"
          value={selectedRepo}
          onChange={(e) => setSelectedRepo(e.target.value)}
        >
          <option value="">Default repository (MODBIT_REPO_ROOT)</option>
          {repos.map((r) => (
            <option key={r.repoId} value={r.repoId}>
              {r.path || r.cloneUrl} ({r.defaultBranch || "?"})
            </option>
          ))}
        </select>
        <input
          placeholder="Register a local repo: /path/to/repo"
          value={repoPath}
          onChange={(e) => setRepoPath(e.target.value)}
        />
        <input
          placeholder="…or clone by URL: https://host/org/repo.git"
          value={cloneUrl}
          onChange={(e) => setCloneUrl(e.target.value)}
        />
        <button
          type="button"
          disabled={submitting || (!repoPath.trim() && !cloneUrl.trim())}
          onClick={() => void registerRepo()}
        >
          Register repository
        </button>
      </section>
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
        <input
          placeholder="Base branch (optional; default = repo default)"
          value={baseBranch}
          onChange={(e) => setBaseBranch(e.target.value)}
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
