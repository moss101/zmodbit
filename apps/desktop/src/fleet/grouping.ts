// Fleet view grouping (MOD-UX-001): Needs Attention / Ready for Review /
// Running / Waiting / Completed / Failed are first-class views. State values
// are the canonical TaskStatus enum from modbit.protocol.v1 (proto/domain.proto).

export const TASK_STATUS = {
  UNSPECIFIED: 0,
  QUEUED: 1,
  STARTED: 2,
  WAITING: 3,
  NEEDS_ATTENTION: 4,
  READY_FOR_REVIEW: 5,
  COMPLETED: 6,
  FAILED: 7,
  CANCELLED: 8,
  CREATED: 9,
} as const;

export interface TaskCard {
  taskId: string;
  sessionId: string;
  title: string;
  state: number;
  createdAt: string;
  generation: string;
}

export type FleetViewName =
  | "attention"
  | "review"
  | "running"
  | "waiting"
  | "completed"
  | "failed"
  | "archived";

export const FLEET_VIEW_ORDER: FleetViewName[] = [
  "attention",
  "review",
  "running",
  "waiting",
  "completed",
  "failed",
  "archived",
];

export const FLEET_VIEW_LABELS: Record<FleetViewName, string> = {
  attention: "Needs Attention",
  review: "Ready for Review",
  running: "Running",
  waiting: "Waiting",
  completed: "Completed",
  failed: "Failed",
  archived: "Cancelled",
};

export function fleetViewOf(state: number): FleetViewName {
  switch (state) {
    case TASK_STATUS.NEEDS_ATTENTION:
      return "attention";
    case TASK_STATUS.READY_FOR_REVIEW:
      return "review";
    case TASK_STATUS.QUEUED:
    case TASK_STATUS.STARTED:
    case TASK_STATUS.CREATED:
      return "running";
    case TASK_STATUS.WAITING:
      return "waiting";
    case TASK_STATUS.COMPLETED:
      return "completed";
    case TASK_STATUS.FAILED:
      return "failed";
    case TASK_STATUS.CANCELLED:
      return "archived";
    default:
      return "archived";
  }
}

export function groupFleet(tasks: TaskCard[]): Record<FleetViewName, TaskCard[]> {
  const grouped: Record<FleetViewName, TaskCard[]> = {
    attention: [],
    review: [],
    running: [],
    waiting: [],
    completed: [],
    failed: [],
    archived: [],
  };
  for (const task of tasks) {
    grouped[fleetViewOf(task.state)].push(task);
  }
  return grouped;
}
