// Preload bridge contract (docs/32): the renderer sees ONLY these typed
// SurfaceProtocol functions — never node APIs, never Core internals.
import type { TaskView } from "@modbit/surface-protocol";

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
      ): Promise<{ ok: boolean; error?: string; task?: TaskView }>;
      createSession(displayName: string): Promise<{ ok: boolean; sessionId?: string }>;
    };
  }
}

export {};
