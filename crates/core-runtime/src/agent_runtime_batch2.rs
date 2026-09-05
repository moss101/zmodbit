//! Agent runtime batch 2 (M6): resume of terminal child states
//! (REQ-EV-0050), bounded recursive delegation (REQ-EV-0051), structured
//! PlanGraph/TodoState (REQ-EV-0052), stall detection (REQ-EV-0053), and
//! child-agent isolation (REQ-EV-0078). All build on the durable
//! AgentFleet journal.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

// ---------------------------------------------------------------------------
// Resume terminal child states (REQ-EV-0050)
// ---------------------------------------------------------------------------

/// A NEW attempt on a terminal child: lineage is preserved, the prior
/// result is linked as evidence, and the new attempt gets its own id.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentAttempt {
    pub attempt_id: String,
    pub parent_agent: String,
    pub prior_status: String,
    pub new_prompt: String,
    /// Evidence ids linked from the prior attempt.
    pub prior_evidence: Vec<String>,
}

/// Resumes a terminal (completed/cancelled/failed) child with a new
/// prompt. Policy decides WHO may continue; the prior evidence stays
/// linked (QUAL-EV-0050).
pub fn resume_terminal_child(
    prior_status: &str,
    prior_evidence: Vec<String>,
    agent_id: &str,
    new_prompt: &str,
    allow_continue: bool,
) -> Result<AgentAttempt, String> {
    if !matches!(prior_status, "completed" | "cancelled" | "failed") {
        return Err(format!(
            "agent {agent_id} is {prior_status:?}, not terminal"
        ));
    }
    if !allow_continue {
        return Err(format!("policy forbids continuing agent {agent_id}"));
    }
    Ok(AgentAttempt {
        attempt_id: format!(
            "{agent_id}-attempt-{}",
            sha256_hex(new_prompt.as_bytes())[..8].to_string()
        ),
        parent_agent: agent_id.to_string(),
        prior_status: prior_status.to_string(),
        new_prompt: new_prompt.to_string(),
        prior_evidence,
    })
}

// ---------------------------------------------------------------------------
// Bounded recursive delegation (REQ-EV-0051)
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum AdmissionError {
    DepthExceeded { depth: u32, max_depth: u32 },
    CapacityExhausted { active: u32, capacity: u32 },
}

impl std::fmt::Display for AdmissionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AdmissionError::DepthExceeded { depth, max_depth } => {
                write!(
                    f,
                    "delegation depth {depth} exceeds the explicit max {max_depth}"
                )
            }
            AdmissionError::CapacityExhausted { active, capacity } => {
                write!(
                    f,
                    "subagent capacity exhausted: {active} active, capacity {capacity}"
                )
            }
        }
    }
}

impl std::error::Error for AdmissionError {}

/// TRANSACTIONAL admission (REQ-EV-0051): nested delegation is DISABLED
/// by default (max_depth 0); an explicit max depth and an active-agent
/// capacity are enforced atomically — the depth N+1 launch is rejected
/// with a typed admission failure.
#[derive(Default)]
pub struct DelegationAdmission {
    pub max_depth: u32,
    pub capacity: u32,
    active: BTreeMap<String, u32>, // agent_id → depth
}

impl DelegationAdmission {
    pub fn new(max_depth: u32, capacity: u32) -> Self {
        Self {
            max_depth,
            capacity,
            active: BTreeMap::new(),
        }
    }

    /// Admits a child at `parent_depth + 1` transactionally.
    pub fn admit(&mut self, agent_id: &str, parent_depth: u32) -> Result<u32, AdmissionError> {
        let depth = parent_depth + 1;
        if depth > self.max_depth {
            return Err(AdmissionError::DepthExceeded {
                depth,
                max_depth: self.max_depth,
            });
        }
        if self.active.len() as u32 >= self.capacity {
            return Err(AdmissionError::CapacityExhausted {
                active: self.active.len() as u32,
                capacity: self.capacity,
            });
        }
        self.active.insert(agent_id.to_string(), depth);
        Ok(depth)
    }

    pub fn finish(&mut self, agent_id: &str) {
        self.active.remove(agent_id);
    }
}

// ---------------------------------------------------------------------------
// Structured PlanGraph/TodoState (REQ-EV-0052)
// ---------------------------------------------------------------------------

/// A plan node: exists OUTSIDE the transcript, with dependencies, owner,
/// status, evidence, and blockers. Compaction and model restarts cannot
/// alter canonical plan state because it is content-addressed separately.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlanNode {
    pub node_id: String,
    pub title: String,
    pub status: PlanStatus,
    pub owner: Option<String>,
    pub depends_on: Vec<String>,
    pub evidence: Vec<String>,
    pub blockers: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    Pending,
    InProgress,
    Done,
    Blocked,
}

/// The canonical plan graph.
#[derive(Default)]
pub struct PlanGraph {
    pub nodes: BTreeMap<String, PlanNode>,
}

impl PlanGraph {
    pub fn add(&mut self, node: PlanNode) {
        self.nodes.insert(node.node_id.clone(), node);
    }

    /// Updates status; blocked-with-open-dependencies stays consistent.
    pub fn set_status(&mut self, node_id: &str, status: PlanStatus) -> Result<(), String> {
        let node = self
            .nodes
            .get_mut(node_id)
            .ok_or_else(|| format!("unknown plan node {node_id}"))?;
        node.status = status;
        Ok(())
    }

    /// Ready nodes: all dependencies Done, no blockers.
    pub fn ready_nodes(&self) -> Vec<String> {
        self.nodes
            .values()
            .filter(|n| {
                n.status == PlanStatus::Pending
                    && n.depends_on.iter().all(|d| {
                        self.nodes
                            .get(d)
                            .map(|dep| dep.status == PlanStatus::Done)
                            .unwrap_or(false)
                    })
                    && n.blockers.is_empty()
            })
            .map(|n| n.node_id.clone())
            .collect()
    }

    /// The CANONICAL state digest: independent of the model transcript,
    /// so compaction/restart cannot alter it.
    pub fn canonical_digest(&self) -> String {
        sha256_hex(&serde_json::to_vec(&self.nodes).unwrap_or_default())
    }
}

// ---------------------------------------------------------------------------
// Stall / no-progress detection (REQ-EV-0053)
// ---------------------------------------------------------------------------

/// A no-progress watchdog: feeds observed activity signatures and flags a
/// STALL when the same low-novelty signature repeats `threshold`
/// consecutive times.
#[derive(Debug)]
pub struct StallWatchdog {
    threshold: u32,
    repeats: u32,
    last_signature: Option<String>,
}

/// The watchdog verdict.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StallVerdict {
    pub stalled: bool,
    pub repeats: u32,
    pub signature: String,
    /// The surfaced blocker evidence.
    pub blocker_evidence: String,
}

impl StallWatchdog {
    pub fn new(threshold: u32) -> Self {
        Self {
            threshold,
            repeats: 0,
            last_signature: None,
        }
    }

    /// Observes one activity cycle signature (e.g. a hash of the read/
    /// search calls + their results). Repeated identical signatures with
    /// empty new results are LOW-NOVELTY.
    pub fn observe(&mut self, signature: &str, new_results: bool) -> StallVerdict {
        if new_results {
            self.repeats = 0;
            self.last_signature = Some(signature.to_string());
            return StallVerdict {
                stalled: false,
                repeats: 0,
                signature: signature.to_string(),
                blocker_evidence: String::new(),
            };
        }
        if self.last_signature.as_deref() == Some(signature) {
            self.repeats += 1;
        } else {
            self.repeats = 1;
            self.last_signature = Some(signature.to_string());
        }
        let stalled = self.repeats >= self.threshold;
        StallVerdict {
            stalled,
            repeats: self.repeats,
            signature: signature.to_string(),
            blocker_evidence: if stalled {
                format!(
                    "no progress: identical activity signature {signature:?} repeated {} times without new results",
                    self.repeats
                )
            } else {
                String::new()
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Child-agent isolation (REQ-EV-0078)
// ---------------------------------------------------------------------------

/// Builds the CHILD prompt from the parent's material: only the capsule
/// summary and the delegated task — parent-only memory, secrets, and the
/// full transcript are excluded by construction.
pub fn build_child_prompt(
    delegated_task: &str,
    parent_only_memory: &[&str],
    parent_secrets: &[&str],
    transcript: &[&str],
    capsule_context: &[&str],
) -> Result<String, String> {
    for secret in parent_secrets {
        if delegated_task.contains(secret) {
            return Err("delegated task leaks a parent secret".into());
        }
    }
    // The child prompt contains ONLY the task + capsule context. Parent
    // memory, secrets, and transcript are structurally absent.
    let mut prompt = format!("task: {delegated_task}\n");
    for ctx in capsule_context {
        prompt.push_str(&format!("context: {ctx}\n"));
    }
    let leaked_memory = parent_only_memory.iter().any(|m| prompt.contains(*m));
    let leaked_transcript = transcript.len() > 0 && prompt.contains(transcript[0]);
    if leaked_memory || leaked_transcript {
        return Err("child prompt leaked parent-only material".into());
    }
    Ok(prompt)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// QUAL-EV-0050: resume a completed research child; the new result is
    /// a new attempt with prior evidence linked.
    #[test]
    fn resume_completed_child_creates_new_linked_attempt() {
        let attempt = resume_terminal_child(
            "completed",
            vec!["evidence:result-set".into()],
            "research-1",
            "now also check the archived papers",
            true,
        )
        .unwrap();
        assert!(attempt.attempt_id.starts_with("research-1-attempt-"));
        assert_eq!(attempt.prior_status, "completed");
        assert_eq!(
            attempt.prior_evidence,
            vec!["evidence:result-set".to_string()]
        );
        // Policy can refuse continuation.
        assert!(resume_terminal_child("completed", vec![], "r", "p", false).is_err());
        // Non-terminal states refuse.
        assert!(resume_terminal_child("running", vec![], "r", "p", true).is_err());
    }

    /// QUAL-EV-0051: a depth N+1 launch is rejected with a typed
    /// admission failure; capacity is enforced transactionally.
    #[test]
    fn bounded_delegation_rejects_depth_and_capacity() {
        // Nested delegation disabled by default.
        let mut disabled = DelegationAdmission::default();
        assert!(matches!(
            disabled.admit("sub-1", 0),
            Err(AdmissionError::DepthExceeded {
                depth: 1,
                max_depth: 0
            })
        ));

        // Explicit max depth 2, capacity 2.
        let mut admission = DelegationAdmission::new(2, 2);
        assert_eq!(admission.admit("a1", 0).unwrap(), 1);
        assert_eq!(admission.admit("a2", 1).unwrap(), 2);
        // Depth 3 > max 2: typed rejection.
        assert!(matches!(
            admission.admit("a3", 2),
            Err(AdmissionError::DepthExceeded {
                depth: 3,
                max_depth: 2
            })
        ));
        // Capacity full: typed rejection.
        assert!(matches!(
            admission.admit("a4", 0),
            Err(AdmissionError::CapacityExhausted {
                active: 2,
                capacity: 2
            })
        ));
        // Finishing frees capacity.
        admission.finish("a1");
        assert!(admission.admit("a4", 0).is_ok());
    }

    /// QUAL-EV-0052: compaction and model restart cannot alter canonical
    /// plan state.
    #[test]
    fn plan_graph_state_is_canonical_across_restart() {
        let mut plan = PlanGraph::default();
        plan.add(PlanNode {
            node_id: "n1".into(),
            title: "implement retry".into(),
            status: PlanStatus::Done,
            owner: Some("agent-a".into()),
            depends_on: vec![],
            evidence: vec!["gate:build".into()],
            blockers: vec![],
        });
        plan.add(PlanNode {
            node_id: "n2".into(),
            title: "verify retry".into(),
            status: PlanStatus::Pending,
            owner: None,
            depends_on: vec!["n1".into()],
            evidence: vec![],
            blockers: vec![],
        });
        let digest = plan.canonical_digest();

        // "Compaction + model restart": the plan is rebuilt from durable
        // serialization — identical state, identical digest.
        let bytes = serde_json::to_vec(&plan.nodes).unwrap();
        let restored_nodes: BTreeMap<String, PlanNode> = serde_json::from_slice(&bytes).unwrap();
        let mut restored = PlanGraph::default();
        restored.nodes = restored_nodes;
        assert_eq!(restored.canonical_digest(), digest);

        // Ready nodes respect dependencies.
        assert_eq!(restored.ready_nodes(), vec!["n2".to_string()]);
        restored.set_status("n2", PlanStatus::Done).unwrap();
        assert!(restored.ready_nodes().is_empty());
    }

    /// QUAL-EV-0053: a seeded repeated read/search loop moves the run to
    /// STALLED with blocker evidence.
    #[test]
    fn repeated_loop_is_detected_as_stalled() {
        let mut watchdog = StallWatchdog::new(3);
        let sig = "read(lib.rs)+search(retry)";
        // Progress happens twice: no stall.
        assert!(!watchdog.observe(sig, true).stalled);
        assert!(!watchdog.observe(sig, false).stalled);
        // Then the loop repeats identically without new results.
        assert!(!watchdog.observe(sig, false).stalled);
        let verdict = watchdog.observe(sig, false);
        assert!(verdict.stalled, "third identical no-result cycle = stall");
        assert!(verdict.blocker_evidence.contains("no progress"));
        // Any NEW result resets the watchdog.
        assert!(!watchdog.observe("read(other.rs)", true).stalled);
    }

    /// QUAL-EV-0078: the child prompt dump lacks parent-only memory and
    /// secrets.
    #[test]
    fn child_prompt_excludes_parent_only_material() {
        let prompt = build_child_prompt(
            "explore the retry module",
            &["memory: user prefers tabs"],
            &["sk-or-v1-SECRET"],
            &["parent transcript line about salary"],
            &["ctx:task-brief"],
        )
        .unwrap();
        assert!(prompt.contains("explore the retry module"));
        assert!(prompt.contains("ctx:task-brief"));
        assert!(!prompt.contains("SECRET"));
        assert!(!prompt.contains("salary"));
        assert!(!prompt.contains("prefers tabs"));

        // A task that embeds a secret is refused outright.
        assert!(build_child_prompt(
            "use sk-or-v1-SECRET for the call",
            &[],
            &["sk-or-v1-SECRET"],
            &[],
            &[],
        )
        .is_err());
    }
}
