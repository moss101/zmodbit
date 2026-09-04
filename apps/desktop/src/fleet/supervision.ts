// Attention/fleet supervision (M1, IMP-EV-0037): durable-state-only
// supervision view. Pure functions over the fleet snapshot.
import { TASK_STATUS, type TaskCard } from "./grouping";

export interface SupervisionCard {
  task: TaskCard;
  nextAction: string;
}

export function superviseFleet(tasks: TaskCard[]): SupervisionCard[] {
  return tasks
    .filter((t) => t.state === TASK_STATUS.NEEDS_ATTENTION || t.state === TASK_STATUS.WAITING)
    .map((t) => ({
      task: t,
      nextAction:
        t.state === TASK_STATUS.NEEDS_ATTENTION
          ? "review and choose the next step"
          : "respond to the pending input or approve",
    }));
}

/// Tasks the operator can act on right now (running or ready for review).
export function actionableTasks(tasks: TaskCard[]): TaskCard[] {
  return tasks.filter(
    (t) => t.state === TASK_STATUS.STARTED || t.state === TASK_STATUS.READY_FOR_REVIEW,
  );
}
