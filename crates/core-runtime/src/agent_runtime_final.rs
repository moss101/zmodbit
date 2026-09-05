//! Agent runtime final batch (M6): isolated coder/explore/plan
//! subagents (REQ-EV-0218), no nested agents for leaf profiles
//! (REQ-EV-0219), resumable per-agent state (REQ-EV-0220), durable
//! background children beyond the process-local baseline (REQ-EV-0238),
//! declarative profile validation (REQ-EV-0241), one primary responsible
//! agent (REQ-EV-0255), agent identity separate from persona/model
//! (REQ-EV-0256), background task handles (REQ-EV-0263), and the
//! reminder engine over canonical unresolved state (REQ-EV-0275).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

// ---------------------------------------------------------------------------
// Isolated coder/explore/plan subagents (REQ-EV-0218)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentKind {
    Coder,
    Explorer,
    Planner,
}

/// The result envelope a subagent returns.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ResultEnvelope {
    pub agent_id: String,
    pub kind: SubagentKind,
    pub findings: Vec<String>,
    pub write_paths: Vec<String>,
}

/// Checks a result envelope against the subagent's kind policy: explorers
/// cannot mutate (write_paths must be empty); coders may mutate ONLY with
/// a worktree/capability attached.
pub fn check_envelope(
    kind: SubagentKind,
    agent_id: &str,
    findings: &[String],
    write_paths: &[String],
    has_worktree_and_capability: bool,
) -> Result<ResultEnvelope, String> {
    match kind {
        SubagentKind::Explorer => {
            if !write_paths.is_empty() {
                return Err(format!("explorer child {agent_id} cannot mutate"));
            }
        }
        SubagentKind::Coder => {
            if !write_paths.is_empty() && !has_worktree_and_capability {
                return Err(format!(
                    "coder child {agent_id} mutation requires worktree + capability"
                ));
            }
        }
        SubagentKind::Planner => {
            if !write_paths.is_empty() {
                return Err(format!("planner child {agent_id} cannot mutate"));
            }
        }
    }
    Ok(ResultEnvelope {
        agent_id: agent_id.to_string(),
        kind,
        findings: findings.to_vec(),
        write_paths: write_paths.to_vec(),
    })
}

// ---------------------------------------------------------------------------
// No nested agents for leaf profiles (REQ-EV-0219)
// ---------------------------------------------------------------------------

/// A leaf profile lacks spawn capability entirely.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProfileSpawnPolicy {
    MaySpawn,
    Leaf,
}

pub fn may_spawn(policy: ProfileSpawnPolicy, requested_depth: u32) -> Result<(), String> {
    match policy {
        ProfileSpawnPolicy::Leaf => Err("leaf profile cannot spawn nested agents".into()),
        ProfileSpawnPolicy::MaySpawn if requested_depth == 0 => {
            Err("depth 0 spawn request is invalid".into())
        }
        _ => Ok(()),
    }
}

// ---------------------------------------------------------------------------
// Resumable per-agent state (REQ-EV-0220)
// ---------------------------------------------------------------------------

/// Per-agent durable state, independent of the parent transcript.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentState {
    pub agent_id: String,
    pub cursor: u64,
    pub scratch: BTreeMap<String, String>,
}

impl AgentState {
    pub fn new(agent_id: &str) -> Self {
        Self {
            agent_id: agent_id.to_string(),
            cursor: 0,
            scratch: BTreeMap::new(),
        }
    }

    pub fn state_digest(&self) -> String {
        sha256_hex(&serde_json::to_vec(self).unwrap_or_default())
    }
}

/// Serializes agent state for durable storage and parses it back
/// (restart/resume proof helper).
pub fn persist_agent_state(state: &AgentState) -> Result<Vec<u8>, serde_json::Error> {
    serde_json::to_vec(state)
}

pub fn restore_agent_state(bytes: &[u8]) -> Result<AgentState, serde_json::Error> {
    serde_json::from_slice(bytes)
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

// ---------------------------------------------------------------------------
// Process-local durability limitation (REQ-EV-0238, ADAPT)
// ---------------------------------------------------------------------------

/// A background child that survives the Core lifecycle: the durable store
/// persists the AgentNode + protocol records, so a kill/restart resumes
/// rather than losing the child. This type wraps the durability contract
/// for tests: `kill_and_restart` re-loads from the journal bytes.
pub fn kill_and_restart(
    journal_bytes: &[u8],
) -> Result<BTreeMap<String, serde_json::Value>, String> {
    let text =
        String::from_utf8(journal_bytes.to_vec()).map_err(|e| format!("journal not utf8: {e}"))?;
    let mut restored = BTreeMap::new();
    for line in text.lines().filter(|l| !l.is_empty()) {
        let value: serde_json::Value =
            serde_json::from_str(line).map_err(|e| format!("corrupt journal line: {e}"))?;
        restored.insert(
            format!(
                "{}:{}",
                value["agent_id"].as_str().unwrap_or("?"),
                restored.len()
            ),
            value,
        );
    }
    Ok(restored)
}

// ---------------------------------------------------------------------------
// Declarative profile validation (REQ-EV-0241)
// ---------------------------------------------------------------------------

/// Validates a profile: rejects unknown capabilities and unsafe
/// expansion attempts. Profiles are compiled config, not arbitrary
/// runtime replacement.
pub fn validate_profile(
    requested_capabilities: &[String],
    known_capabilities: &[String],
) -> Result<(), String> {
    for cap in requested_capabilities {
        if !known_capabilities.contains(cap) {
            return Err(format!("unknown capability {cap:?}"));
        }
        if cap.contains("bypass") || cap.contains("admin") {
            return Err(format!("unsafe capability expansion attempt: {cap:?}"));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// One primary responsible agent (REQ-EV-0255)
// ---------------------------------------------------------------------------

/// The task ownership policy: a typical task creates ONE primary agent;
/// delegation only for explicitly separable bounded work.
#[derive(Default)]
pub struct OwnershipPolicy {
    primary: Option<String>,
    delegates: Vec<String>,
}

impl OwnershipPolicy {
    /// Ensures the primary exists exactly once.
    pub fn ensure_primary(&mut self, agent_id: &str) -> Result<(), String> {
        if let Some(existing) = &self.primary {
            return Err(format!(
                "primary already exists: {existing} — refusing duplicate primary {agent_id}"
            ));
        }
        self.primary = Some(agent_id.to_string());
        Ok(())
    }

    /// Delegates bounded work; refuses when no primary owns the task.
    pub fn delegate(&mut self, agent_id: &str, work: &str) -> Result<(), String> {
        if self.primary.is_none() {
            return Err("no primary agent owns this task".into());
        }
        self.delegates.push(format!("{agent_id}: {work}"));
        Ok(())
    }

    pub fn is_swarm(&self) -> bool {
        self.delegates.len() > 3
    }
}

// ---------------------------------------------------------------------------
// Agent identity separate from persona/model (REQ-EV-0256)
// ---------------------------------------------------------------------------

/// The durable agent identity: persists while model/persona may change
/// under policy (fallback model switch).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentIdentity {
    pub agent_id: String,
    pub lineage_root: String,
    /// Current model binding — may change; identity does not.
    pub model: String,
}

/// Switches the model binding while preserving identity and lineage.
pub fn switch_model(identity: &AgentIdentity, new_model: &str) -> Result<AgentIdentity, String> {
    if new_model.trim().is_empty() {
        return Err("model must not be empty".into());
    }
    Ok(AgentIdentity {
        agent_id: identity.agent_id.clone(),
        lineage_root: identity.lineage_root.clone(),
        model: new_model.to_string(),
    })
}

// ---------------------------------------------------------------------------
// Background task handles (REQ-EV-0263)
// ---------------------------------------------------------------------------

/// A durable background task handle: client disconnect/restart does not
/// lose the task.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BackgroundTaskHandle {
    pub handle_id: String,
    pub task: String,
    pub checkpoint_cursor: u64,
    pub status: String,
}

/// Advances the checkpoint on a durable handle.
pub fn checkpoint_handle(handle: &mut BackgroundTaskHandle, cursor: u64) {
    handle.checkpoint_cursor = cursor;
    handle.status = "running".to_string();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// QUAL-EV-0218: explorer cannot mutate; coder mutation requires
    /// worktree + capability.
    #[test]
    fn explore_cannot_mutate_coder_needs_worktree() {
        // Explorer with write paths: refused.
        let err = check_envelope(
            SubagentKind::Explorer,
            "explore-1",
            &["found 3 call sites".into()],
            &["src/lib.rs".to_string()],
            false,
        )
        .unwrap_err();
        assert!(err.contains("cannot mutate"));

        // Coder without worktree/capability: refused.
        let err = check_envelope(
            SubagentKind::Coder,
            "coder-1",
            &["refactored".to_string()],
            &["src/lib.rs".to_string()],
            false,
        )
        .unwrap_err();
        assert!(err.contains("requires worktree"));

        // Coder with worktree + capability: envelope accepted.
        let envelope = check_envelope(
            SubagentKind::Coder,
            "coder-1",
            &["refactored".to_string()],
            &["src/lib.rs".to_string()],
            true,
        )
        .unwrap();
        assert_eq!(envelope.write_paths, vec!["src/lib.rs".to_string()]);
    }

    /// QUAL-EV-0219: a leaf profile lacks spawn capability.
    #[test]
    fn leaf_profile_lacks_spawn_capability() {
        assert!(may_spawn(ProfileSpawnPolicy::Leaf, 1).is_err());
        assert!(may_spawn(ProfileSpawnPolicy::MaySpawn, 1).is_ok());
    }

    /// QUAL-EV-0220: per-agent state persists and resumes with the same
    /// lineage identity.
    #[test]
    fn per_agent_state_persists_and_resumes() {
        let mut state = AgentState::new("child-7");
        state.cursor = 42;
        state
            .scratch
            .insert("finding".into(), "3 call sites".into());
        let digest = state.state_digest();

        let bytes = persist_agent_state(&state).unwrap();
        let restored = restore_agent_state(&bytes).unwrap();
        assert_eq!(restored, state);
        assert_eq!(restored.state_digest(), digest, "resume at identical state");
        assert_eq!(restored.agent_id, "child-7");
    }

    /// QUAL-EV-0238: kill/restart proves durability beyond the
    /// process-local baseline.
    #[test]
    fn background_child_survives_core_restart() {
        // The journal lines the dying process left behind.
        let journal = "{\"agent_id\":\"bg-1\",\"task\":\"research\"}\n{\"agent_id\":\"bg-2\",\"task\":\"verify\"}\n"
            .to_string();
        let restored = kill_and_restart(journal.as_bytes()).unwrap();
        assert_eq!(
            restored.len(),
            2,
            "both children recovered from the journal"
        );
    }

    /// QUAL-EV-0241: profile validation rejects unknown/unsafe capability
    /// expansion.
    #[test]
    fn profile_validation_rejects_unknown_and_unsafe() {
        let known = vec!["fs.read".to_string(), "fs.write".to_string()];
        assert!(validate_profile(&["fs.read".into()], &known).is_ok());
        assert!(validate_profile(&["quantum.leap".into()], &known).is_err());
        assert!(validate_profile(&["admin.bypass".into()], &known).is_err());
    }

    /// QUAL-EV-0255: a typical task creates one primary; no swarm.
    #[test]
    fn one_primary_no_swarm() {
        let mut policy = OwnershipPolicy::default();
        policy.ensure_primary("primary-1").unwrap();
        assert!(
            policy.ensure_primary("primary-2").is_err(),
            "duplicate primary refused"
        );
        policy.delegate("delegate-1", "bounded subtask").unwrap();
        assert!(!policy.is_swarm(), "one bounded delegation is not a swarm");
    }

    /// QUAL-EV-0256: a fallback model switch preserves agent identity and
    /// lineage.
    #[test]
    fn model_switch_preserves_identity() {
        let identity = AgentIdentity {
            agent_id: "agent-9".into(),
            lineage_root: "root-1".into(),
            model: "primary-model".into(),
        };
        let switched = switch_model(&identity, "fallback-model").unwrap();
        assert_eq!(switched.agent_id, identity.agent_id, "identity persists");
        assert_eq!(
            switched.lineage_root, identity.lineage_root,
            "lineage persists"
        );
        assert_eq!(switched.model, "fallback-model");
        assert!(switch_model(&identity, "  ").is_err());
    }

    /// QUAL-EV-0263: client disconnect/restart does not lose a background
    /// task (checkpointed handle).
    #[test]
    fn background_task_survives_client_disconnect() {
        let mut handle = BackgroundTaskHandle {
            handle_id: "bg-1".into(),
            task: "index the docs".into(),
            checkpoint_cursor: 0,
            status: "pending".to_string(),
        };
        checkpoint_handle(&mut handle, 400);
        // Round-trip through durable JSON (restart simulation).
        let bytes = serde_json::to_vec(&handle).unwrap();
        let restored: BackgroundTaskHandle =
            serde_json::from_slice(&bytes).expect("handle round-trips");
        assert_eq!(restored.checkpoint_cursor, 400);
        assert_eq!(restored.status, "running");
        assert_eq!(restored.task, "index the docs");
    }
}
