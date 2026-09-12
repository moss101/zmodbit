// Preload bridge contract (docs/32): the renderer sees ONLY these typed
// SurfaceProtocol functions — never node APIs, never Core internals.
import type {
  TaskView,
  RunDetailView,
  DiffView,
  RecentRepoView,
  SettingsView,
} from "@modbit/surface-protocol";

/** Core event forwarded from the main-process SSE subscription. */
interface CoreEventForward {
  eventId: string;
  aggregateId: string;
  eventType: string;
  sequence: number;
}

declare global {
  interface Window {
    modbit: {
      fleetSnapshot(): Promise<{
        ok: boolean;
        error?: string;
        fleet: { tasks: TaskView[]; defaultSessionId: string };
      }>;
      createTask(
        title: string,
        prompt: string,
        repoId?: string,
        baseBranch?: string,
      ): Promise<{ ok: boolean; error?: string; task?: TaskView }>;
      runVariants(
        objective: string,
        count: number,
        repoId?: string,
        baseBranch?: string,
      ): Promise<{ ok: boolean; error?: string; task?: TaskView }>;
      approveEffect(
        approvalId: string,
        resolvedBy?: string,
      ): Promise<{ ok: boolean; error?: string }>;
      denyEffect(
        approvalId: string,
        reason: string,
        resolvedBy?: string,
      ): Promise<{ ok: boolean; error?: string }>;
      getBrowserView(taskId: string): Promise<{
        ok: boolean;
        error?: string;
        browserView?: {
          taskId: string;
          url: string;
          title: string;
          pngBase64: string;
          lease: string;
        };
      }>;
      setBrowserLease(
        taskId: string,
        owner: "agent" | "user",
      ): Promise<{
        ok: boolean;
        error?: string;
        browserView?: {
          taskId: string;
          url: string;
          title: string;
          pngBase64: string;
          lease: string;
        };
      }>;
      listPendingApprovals(): Promise<{
        ok: boolean;
        error?: string;
        pendingApprovals?: {
          approvals: {
            approvalId: string;
            taskId: string;
            tool: string;
            scope: string;
            createdAt: string;
          }[];
        };
      }>;
      listRecentRepos(): Promise<{
        ok: boolean;
        error?: string;
        recentRepos?: { repos: RecentRepoView[] };
      }>;
      registerRepo(
        path: string,
        cloneUrl: string,
      ): Promise<{ ok: boolean; error?: string; repo?: RecentRepoView }>;
      getSettings(): Promise<{
        ok: boolean;
        error?: string;
        settings?: SettingsView;
      }>;
      updateSettings(patch: {
        provider?: string;
        model?: string;
        baseUrl?: string;
        maxTurns?: number;
        executionMode?: string;
        apiKey?: string;
      }): Promise<{ ok: boolean; error?: string; settings?: SettingsView }>;
      createSession(displayName: string): Promise<{ ok: boolean; sessionId?: string }>;
      taskEvents(taskId: string): Promise<{
        ok: boolean;
        error?: string;
        taskEvents?: {
          taskId: string;
          events: { eventId: string; aggregateId: string; eventType: string; payload: unknown }[];
        };
      }>;
      codeView(path: string): Promise<{ ok: boolean; error?: string }>;
      runDetail(taskId: string): Promise<{
        ok: boolean;
        error?: string;
        runDetail?: RunDetailView;
      }>;
      diff(taskId: string): Promise<{ ok: boolean; error?: string; diff?: DiffView }>;
      steerTask(taskId: string, note: string): Promise<{ ok: boolean; error?: string }>;
      pauseTask(taskId: string): Promise<{ ok: boolean; error?: string }>;
      stopTask(taskId: string, reason: string): Promise<{ ok: boolean; error?: string }>;
      /** Subscribe to forwarded Core events; returns an unsubscribe. */
      onCoreEvent(listener: (event: CoreEventForward) => void): () => void;
      onTaskEvent(listener: (event: CoreEventForward) => void): () => void;
    };
  }
}

export {};
