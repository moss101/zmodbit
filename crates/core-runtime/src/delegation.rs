//! Default background delegation policy (M1, REQ-EV-0180; docs/32 § task
//! composer delegation). Never a blind default: the decision is derived from
//! the dependency graph — a child that blocks its parent runs FOREGROUND; a
//! child the parent can progress without runs BACKGROUND. The decision and
//! its reason are returned for audit.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Delegation {
    Foreground,
    Background,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DependencySignal {
    /// The parent task's next step consumes this child's output.
    pub output_blocks_parent: bool,
    /// The parent can keep making progress while the child runs.
    pub parent_progresses_independently: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct DelegationDecision {
    pub delegation: Delegation,
    pub reason: Reason,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    /// Child output is needed before the parent can continue.
    OutputBlocksParent,
    /// Parent progresses independently; running the child in the background
    /// cannot stall the parent.
    ParentProgressesIndependently,
}

/// Dependency-sensitive delegation decision (QUAL-EV-0180: blocking child
/// stays foreground, separable child goes background).
pub fn decide(signal: DependencySignal) -> DelegationDecision {
    if signal.output_blocks_parent || !signal.parent_progresses_independently {
        DelegationDecision {
            delegation: Delegation::Foreground,
            reason: Reason::OutputBlocksParent,
        }
    } else {
        DelegationDecision {
            delegation: Delegation::Background,
            reason: Reason::ParentProgressesIndependently,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// QUAL-EV-0180: blocking child stays foreground.
    #[test]
    fn blocking_child_stays_foreground() {
        let decision = decide(DependencySignal {
            output_blocks_parent: true,
            parent_progresses_independently: true,
        });
        assert_eq!(decision.delegation, Delegation::Foreground);
        assert_eq!(decision.reason, Reason::OutputBlocksParent);
    }

    /// QUAL-EV-0180: separable child goes background.
    #[test]
    fn separable_child_goes_background() {
        let decision = decide(DependencySignal {
            output_blocks_parent: false,
            parent_progresses_independently: true,
        });
        assert_eq!(decision.delegation, Delegation::Background);
        assert_eq!(decision.reason, Reason::ParentProgressesIndependently);
    }

    /// A child whose parent cannot progress independently is never
    /// backgrounded — the "never blind default" rule.
    #[test]
    fn non_progressing_parent_blocks_backgrounding() {
        let decision = decide(DependencySignal {
            output_blocks_parent: false,
            parent_progresses_independently: false,
        });
        assert_eq!(decision.delegation, Delegation::Foreground);
        assert_eq!(decision.reason, Reason::OutputBlocksParent);
    }
}
