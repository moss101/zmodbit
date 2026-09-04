//! Typed input queue (M1, docs/30 § Commands; MOD-INPUT-001; REQ-EV-0191/
//! 0261/0262). All user input is a durable, ordered event; the dispatch mode
//! determines how Core applies it relative to the running turn:
//!
//! - `Steer` — interrupt-and-replace: cancels pending input and redirects the
//!   running task now (emits TaskSteered);
//! - `Collect` — coalesce after the current turn;
//! - `FollowUp` — ordered separate turn, FIFO;
//! - `SideQuestion` — non-disruptive: answered against a bounded recent
//!   context snapshot and NEVER mutates the main task state.

use serde::{Deserialize, Serialize};

use crate::task::TaskState;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputMode {
    Steer,
    Collect,
    FollowUp,
    SideQuestion,
}

/// The dispatch policy implied by each input mode (REQ-EV-0191).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SteeringPolicy {
    InterruptAndReplace,
    CoalesceAfterCurrent,
    OrderedSeparateTurns,
    NonDisruptiveSnapshot,
}

impl InputMode {
    pub fn policy(&self) -> SteeringPolicy {
        match self {
            InputMode::Steer => SteeringPolicy::InterruptAndReplace,
            InputMode::Collect => SteeringPolicy::CoalesceAfterCurrent,
            InputMode::FollowUp => SteeringPolicy::OrderedSeparateTurns,
            InputMode::SideQuestion => SteeringPolicy::NonDisruptiveSnapshot,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            InputMode::Steer => "steer",
            InputMode::Collect => "collect",
            InputMode::FollowUp => "follow_up",
            InputMode::SideQuestion => "side_question",
        }
    }
}

/// What applying a queued input does to the main task state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputEffect {
    /// Interrupt-and-replace: emits TaskSteered, task keeps running.
    RedirectsTask,
    /// Durable queued input: task state is unchanged (consumed later).
    QueuedAfterCurrent,
    /// Side question: not a task input at all; never touches the task.
    SessionLevelOnly,
}

/// Validates applying an input of `mode` to a task in `state`.
/// Returns the effect the core must implement. Rejects impossible
/// combinations instead of silently coalescing (REQ-EV-0191).
pub fn input_effect(state: TaskState, mode: InputMode) -> Result<InputEffect, String> {
    use TaskState::*;
    match mode {
        InputMode::Steer => match state {
            Running | Waiting(_) | Queued => Ok(InputEffect::RedirectsTask),
            other => Err(format!(
                "steer requires an active task, task is {other:?} (interrupt-and-replace)"
            )),
        },
        InputMode::Collect => match state {
            Running | Waiting(_) => Ok(InputEffect::QueuedAfterCurrent),
            other => Err(format!(
                "collect requires a running/waiting task, task is {other:?} (coalesce-after-current)"
            )),
        },
        InputMode::FollowUp => match state {
            Queued | Running | Waiting(_) | ReadyForReview => Ok(InputEffect::QueuedAfterCurrent),
            other => Err(format!(
                "follow-up requires an open task, task is {other:?} (ordered separate turns)"
            )),
        },
        InputMode::SideQuestion => match state {
            Created | Completed | Cancelled => Err(format!(
                "side question requires a live task, task is {state:?}"
            )),
            _ => Ok(InputEffect::SessionLevelOnly),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::WaitingReason;

    #[test]
    fn policies_map_from_modes() {
        assert_eq!(
            InputMode::Steer.policy(),
            SteeringPolicy::InterruptAndReplace
        );
        assert_eq!(
            InputMode::Collect.policy(),
            SteeringPolicy::CoalesceAfterCurrent
        );
        assert_eq!(
            InputMode::FollowUp.policy(),
            SteeringPolicy::OrderedSeparateTurns
        );
        assert_eq!(
            InputMode::SideQuestion.policy(),
            SteeringPolicy::NonDisruptiveSnapshot
        );
    }

    #[test]
    fn steer_is_rejected_on_terminal_tasks() {
        assert!(input_effect(TaskState::Completed, InputMode::Steer).is_err());
        assert!(input_effect(TaskState::Failed, InputMode::Steer).is_err());
        assert!(input_effect(TaskState::Cancelled, InputMode::Steer).is_err());
        assert_eq!(
            input_effect(TaskState::Running, InputMode::Steer).unwrap(),
            InputEffect::RedirectsTask
        );
    }

    #[test]
    fn collect_followup_and_side_question_have_distinct_reach() {
        let running = TaskState::Running;
        assert_eq!(
            input_effect(running, InputMode::Collect).unwrap(),
            InputEffect::QueuedAfterCurrent
        );
        assert_eq!(
            input_effect(running, InputMode::FollowUp).unwrap(),
            InputEffect::QueuedAfterCurrent
        );
        assert_eq!(
            input_effect(running, InputMode::SideQuestion).unwrap(),
            InputEffect::SessionLevelOnly
        );
        // Waiting on approval: collect still valid, side question session-level.
        let waiting = TaskState::Waiting(WaitingReason::Approval);
        assert_eq!(
            input_effect(waiting, InputMode::Collect).unwrap(),
            InputEffect::QueuedAfterCurrent
        );
        assert_eq!(
            input_effect(waiting, InputMode::SideQuestion).unwrap(),
            InputEffect::SessionLevelOnly
        );
    }
}
