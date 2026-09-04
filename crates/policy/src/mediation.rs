//! Multi-client permission mediation (M2, REQ-EV-0194, ADAPT): one
//! canonical approval record owns state. Multiple clients (desktop UI,
//! mobile companion, terminal) may vote according to the configured
//! policy — designated owner or consensus. THE MODEL NEVER RESOLVES
//! VOTES: a vote submitted from a model-controlled actor is refused.
//! Every vote and the resolution are auditable.

use crate::approvals::{Approval, ApprovalState};
use serde::{Deserialize, Serialize};
use std::fmt;

/// Who may resolve approvals, and how.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum MediationPolicy {
    /// Exactly one designated client decides.
    Designated { client_id: String },
    /// At least `min_approvals` distinct clients must approve, and no
    /// client may both approve and deny.
    Consensus { min_approvals: u32 },
}

impl fmt::Display for MediationPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MediationPolicy::Designated { client_id } => write!(f, "designated({client_id})"),
            MediationPolicy::Consensus { min_approvals } => {
                write!(f, "consensus(min={min_approvals})")
            }
        }
    }
}

/// One client's recorded stance on an approval.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ClientVote {
    pub client_id: String,
    pub approve: bool,
    pub ts_ms: i64,
}

#[derive(Debug)]
pub enum MediationError {
    /// A model-controlled actor tried to vote: policy refuses — the model
    /// is the SUBJECT of approvals, never a resolver.
    ModelVoteRefused { actor: String },
    /// The voting client is not enrolled for this session.
    ClientNotEnrolled { client_id: String },
    /// A client changed its stance — conflicting votes are resolved by
    /// policy, not silently.
    ConflictingVotes { client_id: String },
}

impl fmt::Display for MediationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MediationError::ModelVoteRefused { actor } => {
                write!(
                    f,
                    "model-controlled actor {actor:?} cannot resolve approvals"
                )
            }
            MediationError::ClientNotEnrolled { client_id } => {
                write!(f, "client {client_id:?} is not enrolled")
            }
            MediationError::ConflictingVotes { client_id } => {
                write!(
                    f,
                    "client {client_id:?} voted both ways — policy resolves, not silence"
                )
            }
        }
    }
}

impl std::error::Error for MediationError {}

/// The canonical mediation record for ONE approval. Owns all state.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ApprovalMediation {
    pub approval_id: String,
    pub policy: MediationPolicy,
    pub enrolled_clients: Vec<String>,
    pub votes: Vec<ClientVote>,
    /// Audit trail: every submitted vote, accepted or not.
    pub audit: Vec<String>,
    pub resolved: Option<bool>,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

impl ApprovalMediation {
    /// Opens a canonical mediation for an approval.
    pub fn open(approval: &Approval, policy: MediationPolicy, enrolled_clients: &[String]) -> Self {
        let audit_open = format!(
            "opened under policy {:?} with {} enrolled clients",
            policy.to_string(),
            enrolled_clients.len()
        );
        Self {
            approval_id: approval.approval_id.clone(),
            policy,
            enrolled_clients: enrolled_clients.to_vec(),
            votes: Vec::new(),
            audit: vec![audit_open],
            resolved: None,
        }
    }

    /// Records a client's vote under the configured policy. The MODEL
    /// cannot vote (REQ-EV-0194: it never resolves votes).
    pub fn submit_vote(
        &mut self,
        client_id: &str,
        approve: bool,
        is_model_actor: bool,
    ) -> Result<Option<bool>, MediationError> {
        self.audit.push(format!(
            "vote from {client_id}: {} (model_actor={is_model_actor})",
            if approve { "approve" } else { "deny" }
        ));
        if is_model_actor {
            return Err(MediationError::ModelVoteRefused {
                actor: client_id.to_string(),
            });
        }
        if !self.enrolled_clients.iter().any(|c| c == client_id) {
            return Err(MediationError::ClientNotEnrolled {
                client_id: client_id.to_string(),
            });
        }
        // A client reversing its own vote is a conflict: policy keeps the
        // FIRST stance and refuses the reversal (deterministic, auditable).
        if let Some(existing) = self.votes.iter().find(|v| v.client_id == client_id) {
            if existing.approve != approve {
                return Err(MediationError::ConflictingVotes {
                    client_id: client_id.to_string(),
                });
            }
            return Ok(self.resolved);
        }
        self.votes.push(ClientVote {
            client_id: client_id.to_string(),
            approve,
            ts_ms: now_ms(),
        });
        self.recompute();
        Ok(self.resolved)
    }

    fn recompute(&mut self) {
        let approvals = self.votes.iter().filter(|v| v.approve).count() as u32;
        let denials = self.votes.iter().filter(|v| !v.approve).count() as u32;
        self.resolved = match &self.policy {
            MediationPolicy::Designated { client_id } => self
                .votes
                .iter()
                .find(|v| v.client_id == *client_id)
                .map(|v| v.approve),
            MediationPolicy::Consensus { min_approvals } => {
                if denials > 0 {
                    Some(false)
                } else if approvals >= *min_approvals {
                    Some(true)
                } else {
                    None
                }
            }
        };
        if let Some(decision) = self.resolved {
            self.audit.push(format!(
                "RESOLVED: {}",
                if decision { "approved" } else { "denied" }
            ));
        }
    }

    /// The canonical state: pending until policy resolves.
    pub fn state(&self) -> ApprovalState {
        match self.resolved {
            Some(true) => ApprovalState::Approved,
            Some(false) => ApprovalState::Denied,
            None => ApprovalState::Pending,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approvals::{Approval, ApprovalState};

    fn approval() -> Approval {
        Approval {
            approval_id: "apr-1".to_string(),
            intent_hash: "deadbeef".to_string(),
            tool: "fs.write".to_string(),
            scope: "/workspace/src/lib.rs".to_string(),
            state: ApprovalState::Pending,
            created_at_ms: 0,
            expires_at_ms: None,
            resolved_by: None,
        }
    }

    /// QUAL-EV-0194: conflicting client votes follow the configured policy
    /// and every step is auditable.
    #[test]
    fn conflicting_client_approvals_follow_policy_and_are_auditable() {
        // Consensus(2): desktop and mobile must both approve; terminal's
        // denial kills it.
        let mut m = ApprovalMediation::open(
            &approval(),
            MediationPolicy::Consensus { min_approvals: 2 },
            &["desktop".into(), "mobile".into(), "terminal".into()],
        );
        assert_eq!(m.state(), ApprovalState::Pending);
        assert_eq!(
            m.submit_vote("desktop", true, false).unwrap(),
            None,
            "1/2 approvals: still pending"
        );
        // The model itself tries to push it over the line: REFUSED.
        assert!(matches!(
            m.submit_vote("model-agent", true, true),
            Err(MediationError::ModelVoteRefused { .. })
        ));
        // An unenrolled client cannot vote either.
        assert!(matches!(
            m.submit_vote("pager", true, false),
            Err(MediationError::ClientNotEnrolled { .. })
        ));
        // Terminal denies: consensus with any denial resolves DENIED even
        // though desktop approved.
        let resolved = m.submit_vote("terminal", false, false).unwrap();
        assert_eq!(resolved, Some(false));
        assert_eq!(m.state(), ApprovalState::Denied);

        // The audit trail shows every attempt, including the refusals.
        let audit = m.audit.join("\n");
        assert!(audit.contains("model-agent"));
        assert!(audit.contains("pager"));
        assert!(audit.contains("RESOLVED: denied"));
    }

    #[test]
    fn designated_policy_only_the_owner_counts() {
        let mut m = ApprovalMediation::open(
            &approval(),
            MediationPolicy::Designated {
                client_id: "desktop".into(),
            },
            &["desktop".into(), "mobile".into()],
        );
        // Mobile approves first — irrelevant under designated policy.
        assert_eq!(m.submit_vote("mobile", true, false).unwrap(), None);
        assert_eq!(m.state(), ApprovalState::Pending);
        // Desktop's approval resolves it.
        assert_eq!(m.submit_vote("desktop", true, false).unwrap(), Some(true));
        assert_eq!(m.state(), ApprovalState::Approved);

        // A client reversing its stance is a typed conflict, kept auditable.
        let mut m2 = ApprovalMediation::open(
            &approval(),
            MediationPolicy::Consensus { min_approvals: 1 },
            &["desktop".into()],
        );
        m2.submit_vote("desktop", true, false).unwrap();
        assert!(matches!(
            m2.submit_vote("desktop", false, false),
            Err(MediationError::ConflictingVotes { .. })
        ));
    }
}
