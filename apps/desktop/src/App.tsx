import { useCallback, useEffect, useState } from "react";
import type { RecentRepoView } from "@modbit/surface-protocol";
import { groupFleet, nestChildren, FLEET_VIEW_ORDER, FLEET_VIEW_LABELS, TASK_STATUS, type FleetCard, type TaskCard } from "./fleet/grouping";
import { superviseFleet } from "./fleet/supervision";
import { statusSummary } from "./status-center/status";
import { TaskWorkspace } from "./task-workspace/TaskWorkspace";
import { SettingsScreen } from "./settings/SettingsScreen";

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
  // Phase 5 residual: pending protected-effect approval cards.
  const [pendingApprovals, setPendingApprovals] = useState<
    { approvalId: string; taskId: string; tool: string; scope: string }[]
  >([]);
  // Phase 7 item 2: "run N variants" — 1 means a single normal task.
  const [variantCount, setVariantCount] = useState(1);
  const [submitting, setSubmitting] = useState(false);
  const [selectedTask, setSelectedTask] = useState<string | null>(null);
  const [screen, setScreen] = useState<"fleet" | "settings">("fleet");
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
      // Approval cards ride the same refresh (docs/13: approvals are
      // Needs-Attention work, never silent).
      const approvals = await window.modbit.listPendingApprovals();
      if (approvals.ok && approvals.pendingApprovals) {
        setPendingApprovals(approvals.pendingApprovals.approvals);
      } else {
        setPendingApprovals([]);
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

  const decideApproval = useCallback(
    async (approvalId: string, decision: "approve" | "deny") => {
      try {
        const response =
          decision === "approve"
            ? await window.modbit.approveEffect(approvalId)
            : await window.modbit.denyEffect(approvalId, "");
        if (!response.ok) setError(response.error ?? "decision failed");
      } catch (e) {
        setError(e instanceof Error ? e.message : String(e));
      } finally {
        await refresh();
      }
    },
    [refresh],
  );

  const submit = useCallback(async () => {
    if (!title.trim() || submitting) return;
    setSubmitting(true);
    try {
      const objective = prompt.trim();
      // Phase 7 item 2: N > 1 runs the objective as parallel variants —
      // one umbrella task with N admitted children, each in its own
      // worktree; compare and merge the winner.
      const response =
        variantCount > 1
          ? await window.modbit.runVariants(
              objective || title.trim(),
              variantCount,
              selectedRepo,
              baseBranch.trim(),
            )
          : await window.modbit.createTask(
              title.trim(),
              objective,
              selectedRepo,
              baseBranch.trim(),
            );
      if (!response.ok) {
        setError(response.error ?? "task creation failed");
      } else {
        setTitle("");
        setPrompt("");
        setVariantCount(1);
        await refresh();
      }
    } finally {
      setSubmitting(false);
    }
  }, [title, prompt, variantCount, selectedRepo, baseBranch, submitting, refresh]);

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

  // Phase 7 item 1: children render nested under their parent's card.
  const grouped = groupFleet(tasks);
  const nested: Record<string, FleetCard[]> = {};
  for (const [view, cards] of Object.entries(grouped)) {
    nested[view] = nestChildren(cards);
  }
  const renderCard = (t: FleetCard, depth = 0) => (
    <article
      key={t.taskId}
      className={selectedTask === t.taskId ? "task-card selected" : "task-card"}
      tabIndex={0}
      role="button"
      style={depth > 0 ? { marginLeft: depth * 16 } : undefined}
      onClick={() => setSelectedTask(t.taskId)}
      onKeyDown={(e) => {
        if (e.key === "Enter") setSelectedTask(t.taskId);
      }}
    >
      <strong>{t.title}</strong>
      <span> {t.taskId}</span>
      {t.children.length > 0 ? (
        <span className="count" aria-label="child agents">
          {" "}
          {t.children.length} agent{t.children.length === 1 ? "" : "s"}
        </span>
      ) : null}
    </article>
  );
  const summary = statusSummary(tasks);
  const supervised = superviseFleet(tasks);
  const workspaceTask = tasks.find((t) => t.taskId === selectedTask) ?? null;

  if (screen === "settings") {
    return <SettingsScreen onBack={() => setScreen("fleet")} />;
  }

  return (
    <main>
      <h1>Modbit Fleet</h1>
      <button type="button" onClick={() => setScreen("settings")}>
        Settings
      </button>
      <section aria-label="Status center">
        <h2>Status center</h2>
        <p>
          {summary.totalTasks} tasks · {summary.attention} need attention ·{" "}
          {summary.bySession.length} session(s)
        </p>
      </section>
      <section aria-label="Needs attention supervision">
        <h2>Needs attention — single next action</h2>
        {pendingApprovals.length > 0 ? (
          pendingApprovals.map((a) => (
            <article key={a.approvalId} className="approval-card">
              <strong>Protected effect pending</strong> — {a.tool} on {a.scope}{" "}
              <span>(task {a.taskId})</span>
              <button type="button" onClick={() => void decideApproval(a.approvalId, "approve")}>
                Approve
              </button>
              <button type="button" onClick={() => void decideApproval(a.approvalId, "deny")}>
                Deny
              </button>
            </article>
          ))
        ) : supervised.length === 0 ? (
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
        <label>
          Variants{" "}
          <select
            aria-label="Variant count"
            value={variantCount}
            onChange={(e) => setVariantCount(Number(e.target.value))}
          >
            {[1, 2, 3, 4].map((n) => (
              <option key={n} value={n}>
                {n === 1 ? "1" : `run ${n} variants`}
              </option>
            ))}
          </select>
        </label>
        <button type="button" disabled={submitting || !title.trim()} onClick={() => void submit()}>
          {submitting ? "Creating…" : variantCount > 1 ? `New task ×${variantCount}` : "New task"}
        </button>
      </section>
      {FLEET_VIEW_ORDER.map((view) => {
        const cards = nested[view] ?? [];
        return (
        <section key={view} aria-label={FLEET_VIEW_LABELS[view]}>
          <h2>
            {FLEET_VIEW_LABELS[view]} <span className="count">{cards.length}</span>
          </h2>
          {cards.length === 0 ? (
            <p className="empty">none</p>
          ) : (
            cards.map((t) => (
              <div key={t.taskId}>
                {renderCard(t)}
                {t.children.map((c) => renderCard(c as FleetCard, 1))}
              </div>
            ))
          )}
        </section>
        );
      })}
      {workspaceTask ? (
        <TaskWorkspace task={workspaceTask} onClose={() => setSelectedTask(null)} />
      ) : null}
    </main>
  );
}
