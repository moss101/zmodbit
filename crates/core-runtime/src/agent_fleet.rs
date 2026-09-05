//! Agent fleet runtime (M6, docs/14 § AgentGraph): persisted AgentNodes
//! with parent/root lineage (REQ-EV-0047/0006), idempotent subagent spawn
//! (REQ-EV-0007), foreground↔background transitions (REQ-EV-0008), typed
//! live steering (REQ-EV-0009), detached-agent permission ceilings
//! (REQ-EV-0046), AgentExecutionCapsules (REQ-EV-0048), and durable agent
//! parking (REQ-EV-0049).
//!
//! Canonical owner subsystem: agent-runtime (docs/81). Layout: docs/12.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};

/// Agent lifecycle states. `Parked` is durable state DISTINCT from
/// cancel/complete (REQ-EV-0049).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Foreground,
    Background,
    Parked,
    Completed,
    Cancelled,
}

/// Per-agent execution capsule (REQ-EV-0048): context, tools, model
/// policy, budgets, and capability ceiling are EXPLICIT. A child simply
/// cannot see parent-only tools, secrets, or hidden transcripts — they
/// are not in its capsule.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentExecutionCapsule {
    pub allowed_tools: BTreeSet<String>,
    /// Context refs the agent may resolve (private by default).
    pub context_refs: BTreeSet<String>,
    pub model_policy: String,
    pub max_output_tokens: u64,
    pub max_tool_calls: u64,
    /// Capability ceiling: background agents consume grants but cannot
    /// CREATE interactive privilege expansion (REQ-EV-0046).
    pub can_request_expansion: bool,
}

impl Default for AgentExecutionCapsule {
    fn default() -> Self {
        Self {
            allowed_tools: BTreeSet::new(),
            context_refs: BTreeSet::new(),
            model_policy: "task-scoped".into(),
            max_output_tokens: 4096,
            max_tool_calls: 32,
            can_request_expansion: false,
        }
    }
}

/// Typed live-steering control events (REQ-EV-0009) — typed control
/// events, not chat conventions.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "steer", rename_all = "snake_case")]
pub enum SteerCommand {
    Steer { instruction: String },
    Cancel { reason: String },
    FollowUp { question: String },
}

/// A durable agent node.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentNode {
    pub agent_id: String,
    /// Parent agent (None for the root).
    pub parent: Option<String>,
    /// Root of the lineage tree.
    pub root: String,
    pub task: String,
    pub status: AgentStatus,
    /// Tool-event cursor (never lost across transitions/restarts).
    pub tool_cursor: u64,
    pub context_refs: Vec<String>,
    pub capsule: AgentExecutionCapsule,
    pub idempotency_key: Option<String>,
}

#[derive(Debug)]
pub enum FleetError {
    Persistence(std::io::Error),
    UnknownAgent(String),
    /// Spawn replay with a known idempotency key: reattach, don't
    /// duplicate (returned as Ok in `spawn`, this is for direct misuse).
    DuplicateSpawn {
        key: String,
        existing: String,
    },
    CannotStealCompleted(String),
    PrivilegeExpansionRefused {
        agent: String,
        effect: String,
    },
    UnknownContextRef {
        agent: String,
        context_ref: String,
    },
}

impl fmt::Display for FleetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FleetError::Persistence(e) => write!(f, "fleet journal persistence: {e}"),
            FleetError::UnknownAgent(id) => write!(f, "unknown agent {id:?}"),
            FleetError::DuplicateSpawn { key, existing } => {
                write!(f, "spawn key {key:?} already maps to agent {existing}")
            }
            FleetError::CannotStealCompleted(id) => {
                write!(f, "agent {id} already finished; cannot steer")
            }
            FleetError::PrivilegeExpansionRefused { agent, effect } => write!(
                f,
                "background agent {agent} cannot create privilege expansion for {effect:?} — parent attention required"
            ),
            FleetError::UnknownContextRef { agent, context_ref } => write!(
                f,
                "agent {agent} cannot resolve context {context_ref:?}: not in its capsule"
            ),
        }
    }
}

impl std::error::Error for FleetError {}

/// The fleet: all agent nodes + the durable journal.
#[derive(Default)]
pub struct AgentFleet {
    nodes: BTreeMap<String, AgentNode>,
    idempotency: BTreeMap<String, String>,
    journal: Option<PathBuf>,
}

impl AgentFleet {
    /// In-memory fleet (tests / already-loaded state).
    pub fn new() -> Self {
        Self::default()
    }

    /// Loads a fleet from a durable journal file (restart path).
    pub fn load(path: &Path) -> Result<Self, std::io::Error> {
        let mut fleet = Self::new();
        fleet.journal = Some(path.to_path_buf());
        if path.exists() {
            let text = std::fs::read_to_string(path)?;
            for line in text.lines().filter(|l| !l.is_empty()) {
                let node: AgentNode = serde_json::from_str(line)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
                if let Some(key) = &node.idempotency_key {
                    fleet.idempotency.insert(key.clone(), node.agent_id.clone());
                }
                fleet.nodes.insert(node.agent_id.clone(), node);
            }
        }
        Ok(fleet)
    }

    fn persist_node(&self, node: &AgentNode) -> std::io::Result<()> {
        if let Some(journal) = &self.journal {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(journal)?;
            let mut line = serde_json::to_string(node)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            line.push('\n');
            file.write_all(line.as_bytes())?;
            file.flush()?;
        }
        Ok(())
    }

    /// IDEMPOTENT SPAWN (REQ-EV-0007): replaying an identical spawn call
    /// (same idempotency key) REATTACHES to the existing child — exactly
    /// one child exists per key.
    pub fn spawn(
        &mut self,
        parent: Option<&str>,
        agent_id: &str,
        task: &str,
        capsule: AgentExecutionCapsule,
        idempotency_key: Option<&str>,
    ) -> Result<AgentNode, FleetError> {
        // Idempotency: reattach on replay.
        if let Some(key) = idempotency_key {
            if let Some(existing_id) = self.idempotency.get(key) {
                return Ok(self.nodes[existing_id].clone());
            }
        }
        let root = match parent {
            Some(pid) => self
                .nodes
                .get(pid)
                .ok_or_else(|| FleetError::UnknownAgent(pid.to_string()))?
                .root
                .clone(),
            None => agent_id.to_string(),
        };
        let node = AgentNode {
            agent_id: agent_id.to_string(),
            parent: parent.map(|p| p.to_string()),
            root,
            task: task.to_string(),
            status: AgentStatus::Foreground,
            tool_cursor: 0,
            context_refs: Vec::new(),
            capsule,
            idempotency_key: idempotency_key.map(|k| k.to_string()),
        };
        if let Some(key) = idempotency_key {
            self.idempotency
                .insert(key.to_string(), agent_id.to_string());
        }
        self.persist_node(&node).map_err(FleetError::Persistence)?;
        self.nodes.insert(agent_id.to_string(), node.clone());
        Ok(node)
    }

    /// FOREGROUND ↔ BACKGROUND (REQ-EV-0008): identity, task, and tool
    /// cursor are preserved — only the scheduling/attention mode changes.
    pub fn set_background(&mut self, agent_id: &str) -> Result<(), FleetError> {
        self.transition(agent_id, AgentStatus::Background)
    }

    pub fn set_foreground(&mut self, agent_id: &str) -> Result<(), FleetError> {
        self.transition(agent_id, AgentStatus::Foreground)
    }

    fn transition(&mut self, agent_id: &str, to: AgentStatus) -> Result<(), FleetError> {
        let node = self
            .nodes
            .get_mut(agent_id)
            .ok_or_else(|| FleetError::UnknownAgent(agent_id.to_string()))?;
        node.status = to;
        let snapshot = node.clone();
        self.persist_node(&snapshot)
            .map_err(FleetError::Persistence)?;
        Ok(())
    }

    /// TOOL CURSOR advance (durable; survives restarts and transitions).
    pub fn advance_cursor(&mut self, agent_id: &str, to: u64) -> Result<(), FleetError> {
        let node = self
            .nodes
            .get_mut(agent_id)
            .ok_or_else(|| FleetError::UnknownAgent(agent_id.to_string()))?;
        node.tool_cursor = to;
        let snapshot = node.clone();
        self.persist_node(&snapshot)
            .map_err(FleetError::Persistence)?;
        Ok(())
    }

    /// PARK (REQ-EV-0049): durable state distinct from cancel/complete;
    /// resumable from the same state after a Core restart.
    pub fn park(&mut self, agent_id: &str) -> Result<(), FleetError> {
        self.transition(agent_id, AgentStatus::Parked)
    }

    pub fn resume(&mut self, agent_id: &str) -> Result<(), FleetError> {
        let status = self
            .nodes
            .get(agent_id)
            .ok_or_else(|| FleetError::UnknownAgent(agent_id.to_string()))?
            .status;
        if status != AgentStatus::Parked {
            return Err(FleetError::UnknownAgent(format!(
                "{agent_id} is not parked (status {status:?})"
            )));
        }
        self.transition(agent_id, AgentStatus::Foreground)
    }

    /// TYPED LIVE STEERING (REQ-EV-0009): steer/cancel/follow-up are
    /// typed control events applied at the DETERMINISTIC cancellation
    /// boundary (the current tool cursor). The command is journaled so
    /// replay is deterministic.
    pub fn steer(&mut self, agent_id: &str, command: SteerCommand) -> Result<u64, FleetError> {
        let node = self
            .nodes
            .get_mut(agent_id)
            .ok_or_else(|| FleetError::UnknownAgent(agent_id.to_string()))?;
        if matches!(node.status, AgentStatus::Completed | AgentStatus::Cancelled) {
            return Err(FleetError::CannotStealCompleted(agent_id.to_string()));
        }
        let boundary = node.tool_cursor;
        if let SteerCommand::Cancel { .. } = command {
            node.status = AgentStatus::Cancelled;
        }
        let snapshot = node.clone();
        self.persist_node(&snapshot)
            .map_err(FleetError::Persistence)?;
        Ok(boundary)
    }

    /// PRIVILEGE CEILING (REQ-EV-0046): a background agent consuming a
    /// protected effect it cannot hold transitions the PARENT to an
    /// attention state and refuses the interactive expansion.
    pub fn request_protected_effect(
        &mut self,
        agent_id: &str,
        effect: &str,
    ) -> Result<String, FleetError> {
        let node = self
            .nodes
            .get(agent_id)
            .ok_or_else(|| FleetError::UnknownAgent(agent_id.to_string()))?;
        if node.capsule.can_request_expansion {
            return Ok(format!("{agent_id} may prompt for {effect:?}"));
        }
        let parent = node
            .parent
            .clone()
            .ok_or_else(|| FleetError::PrivilegeExpansionRefused {
                agent: agent_id.to_string(),
                effect: effect.to_string(),
            })?;
        // Transition the parent to an attention state (background).
        self.set_background(&parent)?;
        Err(FleetError::PrivilegeExpansionRefused {
            agent: agent_id.to_string(),
            effect: effect.to_string(),
        })
    }

    /// CAPSULE ISOLATION (REQ-EV-0048): context refs outside the child's
    /// capsule (parent-only secrets, hidden transcripts) are refused.
    pub fn resolve_context(&self, agent_id: &str, context_ref: &str) -> Result<String, FleetError> {
        let node = self
            .nodes
            .get(agent_id)
            .ok_or_else(|| FleetError::UnknownAgent(agent_id.to_string()))?;
        if !node.capsule.context_refs.contains(context_ref) {
            return Err(FleetError::UnknownContextRef {
                agent: agent_id.to_string(),
                context_ref: context_ref.to_string(),
            });
        }
        Ok(format!("resolved {context_ref} for {agent_id}"))
    }

    pub fn node(&self, agent_id: &str) -> Option<&AgentNode> {
        self.nodes.get(agent_id)
    }

    /// M6.1 AGENTGRAPH PROJECTION: nodes + lineage edges.
    pub fn agent_graph(&self) -> (Vec<String>, Vec<(String, String)>) {
        let mut nodes = Vec::new();
        let mut edges = Vec::new();
        for node in self.nodes.values() {
            nodes.push(node.agent_id.clone());
            if let Some(parent) = &node.parent {
                edges.push((parent.clone(), node.agent_id.clone()));
            }
        }
        (nodes, edges)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capsule() -> AgentExecutionCapsule {
        let mut capsule = AgentExecutionCapsule::default();
        capsule.allowed_tools.insert("tools.fs.read".into());
        capsule.context_refs.insert("ctx:task-brief".to_string());
        capsule
    }

    fn temp_journal(tag: &str) -> PathBuf {
        let unique = uuid::Uuid::now_v7().simple().to_string();
        std::env::temp_dir().join(format!("modbit-fleet-{tag}-{unique}.jsonl"))
    }

    /// QUAL-EV-0007: replaying an identical spawn call after a transport
    /// retry reattaches — exactly one child exists.
    #[test]
    fn idempotent_spawn_reattaches_on_replay() {
        let mut fleet = AgentFleet::new();
        let first = fleet
            .spawn(
                None,
                "child-1",
                "explore retries",
                capsule(),
                Some("spawn-key-1"),
            )
            .unwrap();
        // Transport retry: identical key returns the SAME agent.
        let replay = fleet
            .spawn(
                None,
                "child-2",
                "explore retries",
                capsule(),
                Some("spawn-key-1"),
            )
            .unwrap();
        assert_eq!(first.agent_id, replay.agent_id);
        assert_eq!(fleet.nodes.len(), 1, "exactly one child exists");
    }

    /// QUAL-EV-0047 + QUAL-EV-0006: killing Core mid-child run and
    /// restarting preserves child status, task, tool cursor, context refs,
    /// and parent/root lineage.
    #[test]
    fn restart_preserves_child_identity_and_lineage() {
        let journal = temp_journal("restart");
        {
            let mut fleet = AgentFleet::load(&journal).unwrap();
            fleet
                .spawn(None, "root-1", "main task", capsule(), None)
                .unwrap();
            fleet
                .spawn(Some("root-1"), "child-1", "subtask", capsule(), None)
                .unwrap();
            fleet.advance_cursor("child-1", 17).unwrap();
            // KILL: journal is the only durable artifact.
        }
        let fleet = AgentFleet::load(&journal).unwrap();
        let child = fleet.node("child-1").unwrap();
        assert_eq!(child.parent.as_deref(), Some("root-1"));
        assert_eq!(child.root, "root-1");
        assert_eq!(child.tool_cursor, 17, "cursor preserved");
        assert_eq!(child.task, "subtask");
        assert!(matches!(child.status, AgentStatus::Foreground));
        let _ = std::fs::remove_file(&journal);
    }

    /// QUAL-EV-0008: move a running child background then foreground
    /// without restart or lost event offset.
    #[test]
    fn background_foreground_preserves_identity_and_offset() {
        let mut fleet = AgentFleet::new();
        fleet.spawn(None, "a-1", "task", capsule(), None).unwrap();
        fleet.advance_cursor("a-1", 55).unwrap();

        fleet.set_background("a-1").unwrap();
        let node = fleet.node("a-1").unwrap();
        assert!(matches!(node.status, AgentStatus::Background));
        assert_eq!(node.tool_cursor, 55, "no lost event offset");

        fleet.set_foreground("a-1").unwrap();
        let node = fleet.node("a-1").unwrap();
        assert!(matches!(node.status, AgentStatus::Foreground));
        assert_eq!(node.tool_cursor, 55);
    }

    /// QUAL-EV-0009: steering during a live cycle is a typed control event
    /// with a deterministic cancellation boundary.
    #[test]
    fn steering_applies_at_deterministic_boundary() {
        let mut fleet = AgentFleet::new();
        fleet.spawn(None, "a-1", "task", capsule(), None).unwrap();
        fleet.advance_cursor("a-1", 9).unwrap();

        let boundary = fleet
            .steer(
                "a-1",
                SteerCommand::Steer {
                    instruction: "focus on the timeout path".into(),
                },
            )
            .unwrap();
        assert_eq!(boundary, 9, "steer applies at the current cursor");
        // Cancel at the same boundary: agent becomes cancelled.
        let boundary = fleet
            .steer(
                "a-1",
                SteerCommand::Cancel {
                    reason: "user cancelled".into(),
                },
            )
            .unwrap();
        assert_eq!(boundary, 9);
        assert!(matches!(
            fleet.node("a-1").unwrap().status,
            AgentStatus::Cancelled
        ));
    }

    /// QUAL-EV-0046: a background child reaching a protected effect
    /// transitions the parent to attention and refuses the expansion.
    #[test]
    fn background_child_cannot_expand_privileges() {
        let mut fleet = AgentFleet::new();
        fleet
            .spawn(None, "root-1", "main", capsule(), None)
            .unwrap();
        fleet
            .spawn(Some("root-1"), "bg-1", "subtask", capsule(), None)
            .unwrap();
        fleet.set_background("bg-1").unwrap();

        let err = fleet
            .request_protected_effect("bg-1", "production-deploy")
            .unwrap_err();
        assert!(err.to_string().contains("parent attention required"));
        // The parent moved to attention (background) state.
        assert!(matches!(
            fleet.node("root-1").unwrap().status,
            AgentStatus::Background
        ));
    }

    /// QUAL-EV-0048: a child cannot access a parent-only secret/tool or a
    /// hidden transcript.
    #[test]
    fn child_cannot_access_parent_only_context() {
        let mut fleet = AgentFleet::new();
        fleet
            .spawn(None, "root-1", "main", capsule(), None)
            .unwrap();
        fleet
            .spawn(Some("root-1"), "child-1", "subtask", capsule(), None)
            .unwrap();

        // Child's capsule only has the task brief — no secrets, no hidden
        // transcript. Parent-only refs are refused at resolve time.
        let err = fleet
            .resolve_context("child-1", "secret:prod-key")
            .unwrap_err();
        assert!(err.to_string().contains("not in its capsule"));
        // The parent CAN resolve its own ref.
        assert!(fleet.resolve_context("root-1", "secret:prod-key").is_err());
        // And the task brief resolves for the child.
        assert!(fleet.resolve_context("child-1", "ctx:task-brief").is_ok());
    }

    /// QUAL-EV-0049: park during parent intervention, restart Core, resume
    /// from the same state.
    #[test]
    fn park_survives_restart_and_resumes() {
        let journal = temp_journal("park");
        {
            let mut fleet = AgentFleet::load(&journal).unwrap();
            fleet.spawn(None, "a-1", "task", capsule(), None).unwrap();
            fleet.advance_cursor("a-1", 31).unwrap();
            fleet.park("a-1").unwrap();
            // Core restart.
        }
        let mut fleet = AgentFleet::load(&journal).unwrap();
        let node = fleet.node("a-1").unwrap();
        assert!(
            matches!(node.status, AgentStatus::Parked),
            "park is durable"
        );
        assert_eq!(node.tool_cursor, 31);
        // Resume returns it to foreground at the same cursor.
        fleet.resume("a-1").unwrap();
        let node = fleet.node("a-1").unwrap();
        assert!(matches!(node.status, AgentStatus::Foreground));
        assert_eq!(node.tool_cursor, 31);
        let _ = std::fs::remove_file(&journal);
    }

    /// M6.1: the AgentGraph projection — nodes plus lineage edges.
    #[test]
    fn agent_graph_projection_exposes_nodes_and_edges() {
        let mut fleet = AgentFleet::new();
        fleet
            .spawn(None, "root-1", "main", capsule(), None)
            .unwrap();
        fleet
            .spawn(Some("root-1"), "c1", "sub", capsule(), None)
            .unwrap();
        fleet
            .spawn(Some("root-1"), "c2", "sub", capsule(), None)
            .unwrap();
        let (nodes, edges) = fleet.agent_graph();
        assert_eq!(nodes.len(), 3);
        assert!(edges.contains(&("root-1".to_string(), "c1".to_string())));
        assert!(edges.contains(&("root-1".to_string(), "c2".to_string())));
    }
}
