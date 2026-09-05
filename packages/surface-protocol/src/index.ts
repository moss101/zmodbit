/**
 * TypeScript protocol/API types shared with the Modbit Core SurfaceProtocol.
 * Generated from the canonical protobuf schemas (proto/, docs/30) via
 * `pnpm --filter @modbit/surface-protocol generate`; CI asserts the committed
 * bindings match the schema source (docs/70).
 */
export type { SchemaVersion } from "./generated/modbit/protocol/v1/common";
export { TaskStatus } from "./generated/modbit/protocol/v1/domain";
export type {
  RunDetailView,
  TurnView,
  RunStepView,
  DiffView,
  DiffFileView,
  SteerTaskCommand,
  PauseTaskCommand,
  StopTaskCommand,
} from "./generated/modbit/protocol/v1/surface";
export type {
  CommandEnvelope,
  CreateSessionCommand,
  CreateTaskCommand,
  CancelTaskCommand,
} from "./generated/modbit/protocol/v1/commands";
export type {
  EventEnvelope,
  TaskEvent,
  TaskCreated,
  TaskStarted,
  TaskCompleted,
  TaskFailed,
} from "./generated/modbit/protocol/v1/events";
