//! Canonical event envelope and domain events (docs/13 § Canonical event
//! envelope). Every state transition is an immutable event; ordering is per
//! aggregate `sequence`, never global.

use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::ids::{RunId, RunStepId, SessionId, TaskId, TurnId};

/// Which aggregate kind an event belongs to (docs/31 `events.aggregate_type`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AggregateType {
    Session,
    Task,
    Run,
    Turn,
    RunStep,
}

impl AggregateType {
    pub fn as_str(&self) -> &'static str {
        match self {
            AggregateType::Session => "session",
            AggregateType::Task => "task",
            AggregateType::Run => "run",
            AggregateType::Turn => "turn",
            AggregateType::RunStep => "run_step",
        }
    }
}

impl fmt::Display for AggregateType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Who caused the event (docs/31 `events.actor_type`/`actor_id`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Actor {
    pub actor_type: ActorType,
    pub actor_id: String,
}

impl ActorType {
    pub fn as_str(&self) -> &'static str {
        match self {
            ActorType::System => "system",
            ActorType::User => "user",
            ActorType::Agent => "agent",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "system" => Some(ActorType::System),
            "user" => Some(ActorType::User),
            "agent" => Some(ActorType::Agent),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActorType {
    System,
    User,
    Agent,
}

/// Typed domain event payloads. Event type strings stored in the envelope use
/// the snake_case variant name (`task_created`, ...), matching docs/30 event
/// naming (`TaskCreated`, ...).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum DomainEvent {
    SessionCreated {
        display_name: String,
    },
    TaskCreated {
        session_id: SessionId,
        title: String,
        prompt: String,
    },
    TaskQueued,
    TaskStarted,
    TaskWaiting {
        reason: WaitingReason,
    },
    TaskReadyForReview,
    TaskCompleted {
        summary: String,
    },
    TaskFailed {
        failure_code: String,
        message: String,
    },
    TaskCancelled {
        reason: String,
    },
    TaskSteered {
        steer_note: String,
    },
    RunStarted {
        task_id: TaskId,
        attempt: u32,
    },
    RunCompleted,
    RunFailed {
        failure_code: String,
    },
    TurnPrepared {
        run_id: RunId,
        ordinal: u32,
    },
    TurnCompleted,
    TurnFailed {
        failure_code: String,
    },
    RunStepPrepared {
        turn_id: TurnId,
        step_type: StepType,
        ordinal: u32,
    },
    RunStepCompleted,
    RunStepFailed {
        failure_code: String,
    },
    /// Context compaction applied to the model-visible conversation
    /// (docs/19 § compaction): the canonical event history is untouched —
    /// only the projection the model sees changed. Emitted by the
    /// one-agent loop when the conversation exceeds the input-token
    /// budget (Future-tasks Phase 2 item 2).
    CompactionApplied {
        turn_id: TurnId,
        epoch_id: String,
        /// Conversation messages replaced or truncated.
        affected_messages: u32,
        /// Estimated input tokens removed from the projection.
        reclaimed_tokens: u64,
        /// sha256 digest of the CompactionManifest (modbit-compaction).
        manifest_digest: String,
    },
    /// Conversation checkpoint at a turn boundary (docs/19 § Checkpoint
    /// epochs; Future-tasks Phase 2 item 5): recovery data for resuming a
    /// run after a Core kill — the model-visible conversation serialized
    /// as JSON. Bounded by the emitter; the canonical history is the
    /// event log itself.
    ConversationCheckpointed {
        turn_ordinal: u32,
        /// JSON array of the gateway ChatMessage projection.
        conversation_json: String,
    },
    /// Session fork (REQ-EV-0122): a new branch carries the selected
    /// decisions/evidence capsule and NEVER pending approvals.
    SessionForked {
        source_session: SessionId,
        at_sequence: u64,
        carried_decisions: Vec<String>,
        carried_evidence_refs: Vec<String>,
    },
    /// Session rewind (REQ-EV-0123): events after `to_sequence` are
    /// superseded (append-only tombstone; the store never truncates).
    SessionRewound {
        to_sequence: u64,
        reverted_event_count: u64,
        previous_last_hash: String,
    },
    /// Host-owned goal configuration (REQ-EV-0119): the model can never
    /// self-certify completion while a goal is set.
    GoalSet {
        objective: String,
        acceptance_criteria: Vec<String>,
    },
    /// Durable queued input (REQ-EV-0191/0262): every user input is an event,
    /// ordered per task aggregate across reconnects.
    TaskInputQueued {
        input_id: String,
        mode: crate::input_queue::InputMode,
        text: String,
    },
    /// Non-disruptive side question (REQ-EV-0261): session-level event; the
    /// question is answered against a bounded recent context snapshot and
    /// never mutates main task state.
    SideQuestionAsked {
        question_id: String,
        question: String,
        context_event_count: u64,
    },
}

/// Why a running task is waiting (docs/13 Task state machine `Waiting` kinds).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitingReason {
    UserInput,
    Approval,
    Capacity,
    External,
    Provider,
}

/// Typed atomic runtime step kinds (docs/13 § RunStep).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepType {
    ContextCompile,
    ModelInvoke,
    ToolCall,
    ProcedureRun,
    ApprovalWait,
    Verification,
    Checkpoint,
    Handoff,
    UserQuestion,
}

impl StepType {
    pub fn as_str(&self) -> &'static str {
        match self {
            StepType::ContextCompile => "context_compile",
            StepType::ModelInvoke => "model_invoke",
            StepType::ToolCall => "tool_call",
            StepType::ProcedureRun => "procedure_run",
            StepType::ApprovalWait => "approval_wait",
            StepType::Verification => "verification",
            StepType::Checkpoint => "checkpoint",
            StepType::Handoff => "handoff",
            StepType::UserQuestion => "user_question",
        }
    }
}

/// Envelope-scoped event with ids, sequence and integrity hash (docs/13).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EventEnvelope {
    pub event_id: String,
    pub session_id: SessionId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<RunId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<TurnId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub step_id: Option<RunStepId>,
    pub aggregate_type: AggregateType,
    pub aggregate_id: String,
    pub sequence: u64,
    pub event_type: String,
    pub schema_version: (u32, u32),
    pub occurred_at: String,
    pub actor: Actor,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub causation_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    pub payload: DomainEvent,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload_object_hash: Option<String>,
    /// SHA-256 over the canonical serialization of every other field.
    pub integrity_hash: String,
}

impl EventEnvelope {
    /// Event type string derived from the payload variant.
    pub fn event_type_of(payload: &DomainEvent) -> &'static str {
        match payload {
            DomainEvent::SessionCreated { .. } => "session_created",
            DomainEvent::TaskCreated { .. } => "task_created",
            DomainEvent::TaskQueued => "task_queued",
            DomainEvent::TaskStarted => "task_started",
            DomainEvent::TaskWaiting { .. } => "task_waiting",
            DomainEvent::TaskReadyForReview => "task_ready_for_review",
            DomainEvent::TaskCompleted { .. } => "task_completed",
            DomainEvent::TaskFailed { .. } => "task_failed",
            DomainEvent::TaskCancelled { .. } => "task_cancelled",
            DomainEvent::TaskSteered { .. } => "task_steered",
            DomainEvent::RunStarted { .. } => "run_started",
            DomainEvent::RunCompleted => "run_completed",
            DomainEvent::RunFailed { .. } => "run_failed",
            DomainEvent::TurnPrepared { .. } => "turn_prepared",
            DomainEvent::TurnCompleted => "turn_completed",
            DomainEvent::TurnFailed { .. } => "turn_failed",
            DomainEvent::RunStepPrepared { .. } => "run_step_prepared",
            DomainEvent::RunStepCompleted => "run_step_completed",
            DomainEvent::RunStepFailed { .. } => "run_step_failed",
            DomainEvent::CompactionApplied { .. } => "compaction_applied",
            DomainEvent::ConversationCheckpointed { .. } => "conversation_checkpointed",
            DomainEvent::SessionForked { .. } => "session_forked",
            DomainEvent::SessionRewound { .. } => "session_rewound",
            DomainEvent::GoalSet { .. } => "goal_set",
            DomainEvent::TaskInputQueued { .. } => "task_input_queued",
            DomainEvent::SideQuestionAsked { .. } => "side_question_asked",
        }
    }

    /// Canonical serialization used for the integrity hash. Field order is
    /// fixed by the struct definition, so the same code always agrees with
    /// itself; the store recomputes this on append and on verification.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut copy = self.clone();
        copy.integrity_hash = String::new();
        serde_json::to_vec(&copy).expect("envelope serialization cannot fail")
    }

    pub fn compute_integrity_hash(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.canonical_bytes());
        format!("{:x}", hasher.finalize())
    }

    /// Stamps the integrity hash; call after filling all other fields.
    pub fn seal(&mut self) {
        self.integrity_hash = self.compute_integrity_hash();
    }

    /// Recomputes and compares the integrity hash (tamper detection).
    pub fn verify_integrity(&self) -> bool {
        self.integrity_hash == self.compute_integrity_hash()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> EventEnvelope {
        let session = SessionId::generate();
        let task = TaskId::generate();
        let mut e = EventEnvelope {
            event_id: uuid::Uuid::now_v7().to_string(),
            session_id: session,
            task_id: Some(task),
            run_id: None,
            turn_id: None,
            step_id: None,
            aggregate_type: AggregateType::Task,
            aggregate_id: task.to_string(),
            sequence: 1,
            event_type: "task_created".into(),
            schema_version: (1, 0),
            occurred_at: "2026-09-04T00:00:00Z".into(),
            actor: Actor {
                actor_type: ActorType::User,
                actor_id: "user-mohsin".into(),
            },
            causation_id: None,
            correlation_id: None,
            payload: DomainEvent::TaskCreated {
                session_id: session,
                title: "t".into(),
                prompt: "p".into(),
            },
            payload_object_hash: None,
            integrity_hash: String::new(),
        };
        e.seal();
        e
    }

    #[test]
    fn integrity_hash_detects_tampering() {
        let mut e = sample();
        assert!(e.verify_integrity());
        if let DomainEvent::TaskCreated { title, .. } = &mut e.payload {
            *title = "tampered".into();
        }
        assert!(!e.verify_integrity());
    }

    #[test]
    fn integrity_hash_is_deterministic() {
        let a = sample();
        let b = sample();
        // Same inputs except event_id/ids differ; hash must differ. But the
        // same envelope hashed twice agrees.
        assert_eq!(a.compute_integrity_hash(), a.compute_integrity_hash());
        assert_ne!(a.integrity_hash, b.integrity_hash);
    }
}
