//! Turn aggregate and state machine (docs/13 § Turn):
//! `Prepared → Streaming → Executing → Verifying → Completed | Interrupted |
//! Failed`. A tool failure can move `Executing → Streaming/Executing` for
//! repair without failing the turn.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnState {
    Prepared,
    Streaming,
    Executing,
    Verifying,
    Completed,
    Interrupted,
    Failed,
}

/// Legal turn transitions including the repair edge `Executing → Streaming`.
pub fn can_transition(from: TurnState, to: TurnState) -> bool {
    use TurnState::*;
    matches!(
        (from, to),
        (Prepared, Streaming)
            | (Streaming, Executing)
            | (Executing, Streaming) // repair loop
            | (Executing, Executing) // re-entry after tool result
            | (Executing, Verifying)
            | (Verifying, Completed)
            | (Verifying, Executing) // verification failure re-executes
            | (_, Interrupted)
            | (_, Failed)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repair_loop_is_legal_but_completion_requires_verification() {
        assert!(can_transition(TurnState::Executing, TurnState::Streaming));
        assert!(can_transition(TurnState::Verifying, TurnState::Executing));
        assert!(!can_transition(TurnState::Prepared, TurnState::Completed));
        assert!(!can_transition(TurnState::Streaming, TurnState::Completed));
    }

    #[test]
    fn interrupted_and_failed_are_reachable_from_any_live_state() {
        for from in [
            TurnState::Streaming,
            TurnState::Executing,
            TurnState::Verifying,
        ] {
            assert!(can_transition(from, TurnState::Interrupted));
            assert!(can_transition(from, TurnState::Failed));
        }
    }
}
