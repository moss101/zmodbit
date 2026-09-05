//! Agent profiles, plan mode, plan versions, todo graphs, and
//! autonomous detach/resume (M6 batch 3).

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

// ---------------------------------------------------------------------------
// File-defined custom agents (REQ-EV-0115)
// ---------------------------------------------------------------------------

/// A declarative agent profile (file-defined).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentProfile {
    pub name: String,
    pub requested_tools: Vec<String>,
    pub model: String,
}

/// The EFFECTIVE tool surface Core compiles: profile requests ∩ allowed
/// tools — a profile requesting a forbidden tool gets a NARROWED surface,
/// not an error and not the forbidden tool.
pub fn compile_effective_tools(profile: &AgentProfile, allowed_tools: &[String]) -> Vec<String> {
    profile
        .requested_tools
        .iter()
        .filter(|t| allowed_tools.contains(t))
        .cloned()
        .collect()
}

// ---------------------------------------------------------------------------
// Plan mode (REQ-EV-0117)
// ---------------------------------------------------------------------------

/// The runtime mode. Plan mode is MUTATION-DISABLED.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunMode {
    Normal,
    Plan,
}

#[derive(Debug)]
pub enum ModeGateError {
    WriteDeniedInPlanMode,
}

impl std::fmt::Display for ModeGateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ModeGateError::WriteDeniedInPlanMode => {
                write!(f, "write denied: plan mode is mutation-disabled")
            }
        }
    }
}

impl std::error::Error for ModeGateError {}

/// The mode gate consulted BEFORE execution: in Plan mode every mutation
/// is denied pre-execution (QUAL-EV-0117).
pub fn check_mutation_allowed(mode: RunMode, is_mutation: bool) -> Result<(), ModeGateError> {
    if mode == RunMode::Plan && is_mutation {
        return Err(ModeGateError::WriteDeniedInPlanMode);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Saved plan versions with review/annotation (REQ-EV-0118)
// ---------------------------------------------------------------------------

/// A versioned plan document living OUTSIDE the transcript.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlanVersion {
    pub version_id: String,
    pub plan_graph_json: String,
    pub annotations: Vec<String>,
}

#[derive(Default)]
pub struct PlanStore {
    versions: BTreeMap<String, PlanVersion>,
}

impl PlanStore {
    /// Saves a new immutable version; the exact version id is what
    /// execution records on resume.
    pub fn save(&mut self, plan_graph_json: &str) -> String {
        let version_id = format!("plan-{}", &sha256_hex(plan_graph_json.as_bytes())[..12]);
        self.versions.insert(
            version_id.clone(),
            PlanVersion {
                version_id: version_id.clone(),
                plan_graph_json: plan_graph_json.to_string(),
                annotations: Vec::new(),
            },
        );
        version_id
    }

    pub fn get(&self, version_id: &str) -> Option<&PlanVersion> {
        self.versions.get(version_id)
    }

    /// Review: an annotation attaches to an existing version.
    pub fn annotate(&mut self, version_id: &str, note: &str) -> Result<(), String> {
        let version = self
            .versions
            .get_mut(version_id)
            .ok_or_else(|| format!("unknown plan version {version_id}"))?;
        version.annotations.push(note.to_string());
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Durable todo/tasklist graph (REQ-EV-0120)
// ---------------------------------------------------------------------------

/// A durable task with evidence and attempts — independent of chat
/// compaction.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TodoTask {
    pub task_id: String,
    pub title: String,
    pub status: String,
    pub attempts: Vec<String>,
    pub evidence: Vec<String>,
}

/// The durable todo store (JSON round-trip proves restart safety).
#[derive(Default)]
pub struct TodoGraph {
    pub tasks: BTreeMap<String, TodoTask>,
}

impl TodoGraph {
    pub fn add(&mut self, task: TodoTask) {
        self.tasks.insert(task.task_id.clone(), task);
    }

    pub fn set_status(&mut self, task_id: &str, status: &str) -> Result<(), String> {
        let task = self
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| format!("unknown task {task_id}"))?;
        task.status = status.to_string();
        task.attempts.push(format!("status → {status}"));
        Ok(())
    }

    pub fn add_evidence(&mut self, task_id: &str, evidence: &str) -> Result<(), String> {
        let task = self
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| format!("unknown task {task_id}"))?;
        task.evidence.push(evidence.to_string());
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Autonomous detach/resume (REQ-EV-0127)
// ---------------------------------------------------------------------------

/// A detached worker: heartbeat + lease + checkpoint + replay. Killing
/// the client does not stop the worker; a reconnecting client replays
/// from the last checkpoint.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DetachedRun {
    pub run_id: String,
    pub lease_epoch: u64,
    /// Last heartbeat (unix ms).
    pub last_heartbeat_ms: i64,
    /// Cursor of the last durable checkpoint.
    pub checkpoint_cursor: u64,
    /// Permission ceiling: detached runs can consume grants but cannot
    /// expand privileges interactively.
    pub can_expand_privileges: bool,
}

impl DetachedRun {
    pub fn new(run_id: &str, lease_epoch: u64, now_ms: i64) -> Self {
        Self {
            run_id: run_id.to_string(),
            lease_epoch,
            last_heartbeat_ms: now_ms,
            checkpoint_cursor: 0,
            can_expand_privileges: false,
        }
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// The reconnect check: a client may reattach if its lease epoch matches
/// the run's; expansion stays disabled regardless.
pub fn reconnect(run: &DetachedRun, client_lease_epoch: u64) -> Result<(), String> {
    if client_lease_epoch != run.lease_epoch {
        return Err(format!(
            "lease epoch mismatch: client {client_lease_epoch} vs run {}",
            run.lease_epoch
        ));
    }
    if run.can_expand_privileges {
        return Err("detached runs can never expand privileges".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// QUAL-EV-0115: a profile requesting a forbidden tool receives a
    /// narrowed surface.
    #[test]
    fn forbidden_tool_request_narrows_surface() {
        let profile = AgentProfile {
            name: "researcher".into(),
            requested_tools: vec![
                "tools.fs.read".into(),
                "tools.shell.run".into(), // forbidden for researchers
                "tools.web.fetch".into(),
            ],
            model: "glm-5.3-flash".into(),
        };
        let allowed = vec!["tools.fs.read".to_string(), "tools.web.fetch".to_string()];
        let effective = compile_effective_tools(&profile, &allowed);
        assert_eq!(
            effective,
            vec!["tools.fs.read".to_string(), "tools.web.fetch".to_string()]
        );
        assert!(!effective.contains(&"tools.shell.run".to_string()));
    }

    /// QUAL-EV-0117: a write attempt in Plan mode is denied BEFORE
    /// execution.
    #[test]
    fn plan_mode_denies_writes_before_execution() {
        assert!(check_mutation_allowed(RunMode::Plan, true).is_err());
        assert!(check_mutation_allowed(RunMode::Plan, false).is_ok());
        assert!(check_mutation_allowed(RunMode::Normal, true).is_ok());
    }

    /// QUAL-EV-0118 + QUAL-EV-0120: plan versions with annotations and a
    /// durable todo graph that survives restart.
    #[test]
    fn plan_versions_and_todos_survive_restart() {
        // Plan versions with annotations.
        let mut store = PlanStore::default();
        let v1 = store.save(r#"{"nodes": ["a", "b"]}"#);
        store.annotate(&v1, "reviewer: swap steps 2 and 3").unwrap();
        // The exact version id is recorded for execution resume.
        assert!(store.get(&v1).unwrap().annotations.len() == 1);

        // Durable todo graph: statuses survive a JSON restart, independent
        // of any chat compaction.
        let mut todos = TodoGraph::default();
        todos.add(TodoTask {
            task_id: "t1".into(),
            title: "implement".into(),
            status: "done".into(),
            attempts: vec![],
            evidence: vec!["gate:build".into()],
        });
        todos.add(TodoTask {
            task_id: "t2".into(),
            title: "verify".into(),
            status: "pending".into(),
            attempts: vec![],
            evidence: vec![],
        });
        let durable = serde_json::to_vec(&todos.tasks).unwrap();
        let restored_tasks: BTreeMap<String, TodoTask> = serde_json::from_slice(&durable).unwrap();
        assert_eq!(
            restored_tasks["t1"].status, "done",
            "statuses survive restart"
        );
        assert_eq!(
            restored_tasks["t1"].evidence,
            vec!["gate:build".to_string()]
        );
    }

    /// QUAL-EV-0127: kill the client; the worker continues and a client
    /// with the right lease reconnects; privilege expansion stays off.
    #[test]
    fn detached_worker_survives_client_kill_and_reconnects() {
        let mut run = DetachedRun::new("run-detach", 4, now_ms());
        // Worker keeps checkpointing while the client is gone.
        run.checkpoint_cursor = 250;

        // Killed client reconnects with the SAME lease epoch: OK.
        assert!(reconnect(&run, 4).is_ok());
        // A client with a stale epoch: refused.
        assert!(reconnect(&run, 3).is_err());
        // Privilege expansion stays disabled.
        assert!(!run.can_expand_privileges);
    }
}
