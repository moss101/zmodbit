//! Mutating commands (docs/30 § Commands). Every command carries a stable
//! `command_id`; mutating commands are idempotent by `command_id` — a retried
//! command returns the original outcome instead of appending duplicates.

use serde::{Deserialize, Serialize};

use crate::events::{Actor, WaitingReason};
use crate::ids::{SessionId, TaskId};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Command {
    pub command_id: String,
    pub actor: Actor,
    pub payload: CommandPayload,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum CommandPayload {
    CreateSession {
        display_name: String,
    },
    CreateTask {
        session_id: SessionId,
        title: String,
        prompt: String,
    },
    QueueTask {
        task_id: TaskId,
    },
    StartTask {
        task_id: TaskId,
    },
    TaskWaiting {
        task_id: TaskId,
        reason: WaitingReason,
    },
    TaskReadyForReview {
        task_id: TaskId,
    },
    CompleteTask {
        task_id: TaskId,
        summary: String,
    },
    FailTask {
        task_id: TaskId,
        failure_code: String,
        message: String,
    },
    CancelTask {
        task_id: TaskId,
        reason: String,
    },
    SteerTask {
        task_id: TaskId,
        steer_note: String,
    },
    /// Fork from a source session at a sequence point, carrying the selected
    /// decisions/evidence capsule (never pending approvals — REQ-EV-0122).
    ForkSession {
        source_session: SessionId,
        at_sequence: u64,
        carried_decisions: Vec<String>,
        carried_evidence_refs: Vec<String>,
    },
    /// Rewind a session to `to_sequence`. `expected_last_hash` is the
    /// integrity hash the caller believes the stream currently ends with —
    /// an optimistic concurrency check (REQ-EV-0123).
    RewindSession {
        session_id: SessionId,
        to_sequence: u64,
        expected_last_hash: String,
    },
}

impl CommandPayload {
    /// The aggregate type this command targets; `None` for creation commands
    /// that mint a new aggregate id.
    /// The aggregate this command targets, when it targets an existing one.
    pub fn target_aggregate(&self) -> Option<String> {
        match self {
            CommandPayload::QueueTask { task_id }
            | CommandPayload::StartTask { task_id }
            | CommandPayload::TaskWaiting { task_id, .. }
            | CommandPayload::TaskReadyForReview { task_id }
            | CommandPayload::CompleteTask { task_id, .. }
            | CommandPayload::FailTask { task_id, .. }
            | CommandPayload::CancelTask { task_id, .. }
            | CommandPayload::SteerTask { task_id, .. } => Some(task_id.to_string()),
            CommandPayload::ForkSession { source_session, .. } => Some(source_session.to_string()),
            CommandPayload::RewindSession { session_id, .. } => Some(session_id.to_string()),
            CommandPayload::CreateSession { .. } | CommandPayload::CreateTask { .. } => None,
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            CommandPayload::ForkSession { .. } => "fork_session",
            CommandPayload::RewindSession { .. } => "rewind_session",
            CommandPayload::CreateSession { .. } => "create_session",
            CommandPayload::CreateTask { .. } => "create_task",
            CommandPayload::QueueTask { .. } => "queue_task",
            CommandPayload::StartTask { .. } => "start_task",
            CommandPayload::TaskWaiting { .. } => "task_waiting",
            CommandPayload::TaskReadyForReview { .. } => "task_ready_for_review",
            CommandPayload::CompleteTask { .. } => "complete_task",
            CommandPayload::FailTask { .. } => "fail_task",
            CommandPayload::CancelTask { .. } => "cancel_task",
            CommandPayload::SteerTask { .. } => "steer_task",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_commands_target_their_aggregate() {
        let task_id = TaskId::generate();
        let cmd = CommandPayload::CancelTask {
            task_id,
            reason: "x".into(),
        };
        assert_eq!(cmd.target_aggregate(), Some(task_id.to_string()));
        assert_eq!(cmd.kind(), "cancel_task");
        assert_eq!(
            CommandPayload::CreateSession {
                display_name: "s".into()
            }
            .target_aggregate(),
            None
        );
    }
}
