// Status center (M1, IMP-EV-0143): derived, durable-state-only status
// summary for sessions and tasks. No fabrication — every number comes from
// the Core fleet snapshot.

import { groupFleet, type TaskCard } from "../fleet/grouping";
export type { TaskCard };

export interface SessionStatus {
  sessionId: string;
  taskCount: number;
  running: number;
  waiting: number;
  needsAttention: number;
  completed: number;
}

export interface StatusSummary {
  totalTasks: number;
  attention: number;
  bySession: SessionStatus[];
}

export function statusSummary(tasks: TaskCard[]): StatusSummary {
  const grouped = groupFleet(tasks);
  const bySessionMap = new Map<string, SessionStatus>();
  for (const task of tasks) {
    let entry = bySessionMap.get(task.sessionId);
    if (!entry) {
      entry = {
        sessionId: task.sessionId,
        taskCount: 0,
        running: 0,
        waiting: 0,
        needsAttention: 0,
        completed: 0,
      };
      bySessionMap.set(task.sessionId, entry);
    }
    entry.taskCount += 1;
  }
  return {
    totalTasks: tasks.length,
    attention: grouped.attention.length,
    bySession: [...bySessionMap.values()].sort((a, b) =>
      a.sessionId.localeCompare(b.sessionId),
    ),
  };
}
