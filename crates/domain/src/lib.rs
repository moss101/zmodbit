//! modbit-domain — IDs, domain objects, state transitions.
//!
//! Implements the canonical domain model of docs/13: opaque identity types,
//! the Session/Task/Run/Turn/RunStep aggregates, their locked state machines,
//! and the event envelope with integrity hashing. Pure domain logic: no I/O,
//! no storage — persistence lives in `modbit-event-store`.
//!
//! Canonical owner subsystem: domain-events (docs/81). Layout: docs/12.

pub mod commands;
pub mod events;
pub mod ids;
pub mod run;
pub mod run_step;
pub mod session;
pub mod task;
pub mod turn;

pub use commands::{Command, CommandPayload};
pub use events::{
    Actor, ActorType, AggregateType, DomainEvent, EventEnvelope, StepType, WaitingReason,
};
pub use ids::{RunId, RunStepId, SessionId, TaskId, TenantId, TurnId, UserId};
pub use run::RunState;
pub use run_step::{RunStep, RunStepState};
pub use session::SessionState;
pub use task::{TaskState, TransitionError};
pub use turn::TurnState;

/// Envelope schema version for the M1 slice (docs/30 § Version compatibility).
pub const SCHEMA_VERSION: (u32, u32) = (1, 0);
