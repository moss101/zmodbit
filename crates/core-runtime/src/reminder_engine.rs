//! Reminder/decision engine (M6, REQ-EV-0275): the host derives
//! actionable reminders from canonical unresolved state — unresolved
//! approvals, unanswered questions, blocked plan nodes, approaching
//! deadlines. NO second scheduler: reminders are pure projections of the
//! canonical state and are created/cleared solely from it.

use serde::{Deserialize, Serialize};

/// One derived reminder.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Reminder {
    pub subject: String,
    pub kind: &'static str, // approval | question | blocker | deadline
    pub action: String,
}

/// Canonical unresolved state snapshot.
#[derive(Clone, Debug, Default)]
pub struct UnresolvedState {
    pub pending_approvals: Vec<String>,
    pub unanswered_questions: Vec<String>,
    pub blocked_nodes: Vec<String>,
    /// (subject, deadline_ms).
    pub deadlines: Vec<(String, i64)>,
    pub now_ms: i64,
}

/// Derives reminders purely from the canonical unresolved state.
/// Deterministic: same state → same reminders (created/cleared solely by
/// state changes).
pub fn derive_reminders(state: &UnresolvedState) -> Vec<Reminder> {
    let mut reminders = Vec::new();
    for approval in &state.pending_approvals {
        reminders.push(Reminder {
            subject: approval.clone(),
            kind: "approval",
            action: format!("approve or deny {approval}"),
        });
    }
    for question in &state.unanswered_questions {
        reminders.push(Reminder {
            subject: question.clone(),
            kind: "question",
            action: format!("answer {question}"),
        });
    }
    for node in &state.blocked_nodes {
        reminders.push(Reminder {
            subject: node.clone(),
            kind: "blocker",
            action: format!("resolve the blocker on {node}"),
        });
    }
    for (subject, deadline) in &state.deadlines {
        if *deadline - state.now_ms < 3_600_000 {
            reminders.push(Reminder {
                subject: subject.clone(),
                kind: "deadline",
                action: format!("{subject} deadline within the hour"),
            });
        }
    }
    reminders
}

#[cfg(test)]
mod tests {
    use super::*;

    /// QUAL-EV-0275: attention items are created/cleared solely from
    /// canonical unresolved state.
    #[test]
    fn reminders_derive_and_clear_from_canonical_state() {
        let with_items = UnresolvedState {
            pending_approvals: vec!["apr-1".into()],
            unanswered_questions: vec!["q-2".into()],
            blocked_nodes: vec!["plan-node-3".into()],
            deadlines: vec![("release".into(), 1_000_000 + 60_000)],
            now_ms: 1_000_000,
        };
        let reminders = derive_reminders(&with_items);
        assert_eq!(reminders.len(), 4);

        // Resolve everything: reminders clear with no scheduler action.
        let resolved = UnresolvedState {
            now_ms: 1_000_000,
            ..Default::default()
        };
        assert!(derive_reminders(&resolved).is_empty());
        // Determinism: identical state → identical reminders.
        assert_eq!(derive_reminders(&with_items), reminders);
    }
}
