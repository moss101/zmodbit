//! Task aggregate and state machine (docs/13 § Task).
//!
//! ```text
//! Created → Queued → Running ↔ Waiting
//!                     ├→ ReadyForReview → Completed
//!                     ├→ Failed
//!                     └→ Cancelled
//! ```

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::events::{DomainEvent, WaitingReason};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Created,
    Queued,
    Running,
    Waiting(WaitingReason),
    ReadyForReview,
    Completed,
    Failed,
    Cancelled,
}

impl fmt::Display for TaskState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TaskState::Waiting(reason) => write!(f, "waiting({reason:?})"),
            other => write!(f, "{other:?}"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransitionError {
    pub from: TaskState,
    pub event: &'static str,
}

impl fmt::Display for TransitionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "illegal task transition: {} cannot accept {}",
            self.from, self.event
        )
    }
}

impl std::error::Error for TransitionError {}

/// Applies a domain event to the task state, enforcing the locked state
/// machine. Only valid transitions return a new state.
pub fn apply_task_event(
    state: TaskState,
    event: &DomainEvent,
) -> Result<TaskState, TransitionError> {
    let reject = |event: &'static str| TransitionError { from: state, event };
    match event {
        DomainEvent::TaskQueued => match state {
            TaskState::Created => Ok(TaskState::Queued),
            _ => Err(reject("task_queued")),
        },
        DomainEvent::TaskStarted => match state {
            TaskState::Queued | TaskState::Waiting(_) => Ok(TaskState::Running),
            _ => Err(reject("task_started")),
        },
        DomainEvent::TaskWaiting { .. } => match state {
            TaskState::Running => Ok(TaskState::Waiting(resume_reason_of(event))),
            _ => Err(reject("task_waiting")),
        },
        DomainEvent::TaskReadyForReview => match state {
            TaskState::Running | TaskState::Waiting(_) => Ok(TaskState::ReadyForReview),
            _ => Err(reject("task_ready_for_review")),
        },
        DomainEvent::TaskCompleted { .. } => match state {
            TaskState::ReadyForReview => Ok(TaskState::Completed),
            _ => Err(reject("task_completed")),
        },
        DomainEvent::TaskFailed { .. } => match state {
            TaskState::Running | TaskState::Waiting(_) | TaskState::ReadyForReview => {
                Ok(TaskState::Failed)
            }
            _ => Err(reject("task_failed")),
        },
        DomainEvent::TaskCancelled { .. } => match state {
            TaskState::Created | TaskState::Queued | TaskState::Running | TaskState::Waiting(_) => {
                Ok(TaskState::Cancelled)
            }
            _ => Err(reject("task_cancelled")),
        },
        // Steer changes intent, not lifecycle state (docs/30 `TaskSteered`).
        DomainEvent::TaskSteered { .. } => match state {
            TaskState::Running | TaskState::Waiting(_) => Ok(state),
            _ => Err(reject("task_steered")),
        },
        // Creation events do not transition an existing aggregate.
        DomainEvent::TaskCreated { .. } => match state {
            TaskState::Created => Ok(TaskState::Created),
            _ => Err(reject("task_created")),
        },
        _ => Err(reject("non-task event")),
    }
}

fn resume_reason_of(event: &DomainEvent) -> WaitingReason {
    match event {
        DomainEvent::TaskWaiting { reason } => *reason,
        _ => WaitingReason::External,
    }
}

/// Folds a task event stream into the current state. `None` before creation.
pub fn fold_task(events: &[DomainEvent]) -> Option<Result<TaskState, TransitionError>> {
    let mut state = None;
    for event in events {
        state = Some(match state {
            None => match event {
                DomainEvent::TaskCreated { .. } => Ok(TaskState::Created),
                _ => Err(TransitionError {
                    from: TaskState::Created,
                    event: "non-creation event first",
                }),
            },
            Some(Ok(s)) => apply_task_event(s, event),
            Some(Err(e)) => Err(e),
        });
    }
    state
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(f: impl FnOnce() -> DomainEvent) -> DomainEvent {
        f()
    }

    #[test]
    fn happy_path_created_to_completed() {
        let events = vec![
            ev(|| DomainEvent::TaskCreated {
                session_id: crate::ids::SessionId::generate(),
                title: "t".into(),
                prompt: "p".into(),
            }),
            DomainEvent::TaskQueued,
            DomainEvent::TaskStarted,
            DomainEvent::TaskWaiting {
                reason: WaitingReason::Approval,
            },
            DomainEvent::TaskStarted,
            DomainEvent::TaskReadyForReview,
            DomainEvent::TaskCompleted {
                summary: "done".into(),
            },
        ];
        assert_eq!(fold_task(&events), Some(Ok(TaskState::Completed)));
    }

    #[test]
    fn tool_failure_repair_is_not_task_failure() {
        let events = vec![
            ev(|| DomainEvent::TaskCreated {
                session_id: crate::ids::SessionId::generate(),
                title: "t".into(),
                prompt: "p".into(),
            }),
            DomainEvent::TaskQueued,
            DomainEvent::TaskStarted,
            DomainEvent::TaskWaiting {
                reason: WaitingReason::Provider,
            },
            DomainEvent::TaskStarted,
        ];
        assert_eq!(fold_task(&events), Some(Ok(TaskState::Running)));
    }

    #[test]
    fn illegal_transitions_are_rejected() {
        let created = vec![ev(|| DomainEvent::TaskCreated {
            session_id: crate::ids::SessionId::generate(),
            title: "t".into(),
            prompt: "p".into(),
        })];
        // Created → Completed directly is illegal (must pass ReadyForReview).
        let mut events = created.clone();
        events.push(DomainEvent::TaskCompleted {
            summary: "skip".into(),
        });
        assert!(fold_task(&events).unwrap().is_err());

        // Completed is terminal.
        let mut done = vec![
            ev(|| DomainEvent::TaskCreated {
                session_id: crate::ids::SessionId::generate(),
                title: "t".into(),
                prompt: "p".into(),
            }),
            DomainEvent::TaskQueued,
            DomainEvent::TaskStarted,
            DomainEvent::TaskReadyForReview,
            DomainEvent::TaskCompleted {
                summary: "done".into(),
            },
        ];
        done.push(DomainEvent::TaskCancelled {
            reason: "late".into(),
        });
        assert!(fold_task(&done).unwrap().is_err());
    }

    #[test]
    fn steer_keeps_running_state() {
        let events = vec![
            ev(|| DomainEvent::TaskCreated {
                session_id: crate::ids::SessionId::generate(),
                title: "t".into(),
                prompt: "p".into(),
            }),
            DomainEvent::TaskQueued,
            DomainEvent::TaskStarted,
            DomainEvent::TaskSteered {
                steer_note: "focus on tests".into(),
            },
        ];
        assert_eq!(fold_task(&events), Some(Ok(TaskState::Running)));
    }
}
