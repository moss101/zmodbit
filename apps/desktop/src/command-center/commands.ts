// Task-centric command center logic (M1, IMP-EV-0152) and attention
// supervision (IMP-EV-0037, docs/32: each attention card shows the SINGLE
// next action, never raw agent logs). Pure functions — fully unit-tested.

import { TASK_STATUS, type TaskCard } from "../fleet/grouping";

export type TaskCommand = "queue" | "start" | "ready-for-review" | "complete" | "cancel";

/// Which commands the host offers for a task in this state (docs/13 Task
/// state machine). The renderer can only issue these through Core.
export function availableCommands(state: number): TaskCommand[] {
  switch (state) {
    case TASK_STATUS.CREATED:
      return ["queue", "cancel"];
    case TASK_STATUS.QUEUED:
      return ["start", "cancel"];
    case TASK_STATUS.STARTED:
      return ["ready-for-review", "cancel"];
    case TASK_STATUS.WAITING:
      return ["start", "cancel"];
    case TASK_STATUS.READY_FOR_REVIEW:
      return ["complete", "cancel"];
    case TASK_STATUS.COMPLETED:
      return [];
    case TASK_STATUS.FAILED:
      return ["cancel"];
    case TASK_STATUS.CANCELLED:
      return [];
    default:
      return [];
  }
}

/// The single next action for an attention-state task (docs/32: one action,
/// not raw logs). Waiting reasons map to concrete user actions; the actual
/// reason arrives with the event stream in later milestones.
export function attentionNextAction(state: number): string | null {
  if (state === TASK_STATUS.NEEDS_ATTENTION) {
    return "review the task and choose the next step";
  }
  if (state === TASK_STATUS.WAITING) {
    return "respond to the pending input or approve the effect";
  }
  return null;
}

export function attentionTasks(tasks: TaskCard[]): TaskCard[] {
  return tasks.filter((t) => attentionNextAction(t.state) !== null);
}

/// Task-centric command center: everything the operator can do to one task
/// right now (commands + metadata), derived from durable state only.
export interface CommandCenterEntry {
  task: TaskCard;
  commands: TaskCommand[];
  attention: string | null;
}

export function commandCenterEntry(task: TaskCard): CommandCenterEntry {
  return {
    task,
    commands: availableCommands(task.state),
    attention: attentionNextAction(task.state),
  };
}
