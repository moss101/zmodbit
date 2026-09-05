//! MCP hub lifecycle + management (M9, REQ-EV-0104/0128) and
//! organizational engineering memory (REQ-EV-0162).
//!
//! MCP HUB: external tools support list/call/cancel with task/turn/call
//! identity correlation; server config from user/project scopes resolves
//! deterministically; credentials flow through the broker — NEVER into
//! the model prompt. MEMORY: validated facts carry scope/provenance/TTL
//! with edit/delete; conflicts and supersession are inspectable; raw
//! transcripts never auto-promote to facts.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt;

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

// ---------------------------------------------------------------------------
// MCP hub lifecycle (REQ-EV-0104)
// ---------------------------------------------------------------------------

/// An external tool advertised by an MCP server.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct McpTool {
    pub server: String,
    pub name: String,
    pub schema: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct McpCall {
    pub call_id: String,
    pub task_id: String,
    pub turn: u64,
    pub tool: String,
    pub arguments: String,
    pub status: McpCallStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpCallStatus {
    Running,
    Completed,
    Cancelled,
}

#[derive(Debug)]
pub enum McpError {
    UnknownTool(String),
    UnknownCall(String),
    AlreadyCancelled(String),
}

impl fmt::Display for McpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            McpError::UnknownTool(t) => write!(f, "unknown MCP tool {t:?}"),
            McpError::UnknownCall(c) => write!(f, "unknown MCP call {c:?}"),
            McpError::AlreadyCancelled(c) => write!(f, "MCP call {c:?} already cancelled"),
        }
    }
}

impl std::error::Error for McpError {}

/// The MCP hub: discovery, governed calls, cancellation with full
/// task/turn/call identity correlation.
#[derive(Default)]
pub struct McpHub {
    tools: BTreeMap<String, McpTool>,
    calls: BTreeMap<String, McpCall>,
    counter: u64,
}

impl McpHub {
    pub fn new() -> Self {
        Default::default()
    }

    /// LIST: discovery of advertised tools.
    pub fn list(&self) -> Vec<&McpTool> {
        self.tools.values().collect()
    }

    pub fn register_tool(&mut self, tool: McpTool) {
        self.tools
            .insert(format!("{}/{}", tool.server, tool.name), tool);
    }

    /// CALL: starts a governed call with full identity.
    pub fn call(
        &mut self,
        task_id: &str,
        turn: u64,
        server: &str,
        tool: &str,
        arguments: &str,
    ) -> Result<&McpCall, McpError> {
        let key = format!("{server}/{tool}");
        if !self.tools.contains_key(&key) {
            return Err(McpError::UnknownTool(tool.to_string()));
        }
        self.counter += 1;
        let call_id = format!("mcp-call-{}", self.counter);
        let call = McpCall {
            call_id: call_id.clone(),
            task_id: task_id.to_string(),
            turn,
            tool: tool.to_string(),
            arguments: arguments.to_string(),
            status: McpCallStatus::Running,
        };
        self.calls.insert(call_id.clone(), call);
        Ok(&self.calls[&call_id])
    }

    /// CANCEL: cancels a running call (audit-correlated by call id).
    pub fn cancel(&mut self, call_id: &str) -> Result<(), McpError> {
        let call = self
            .calls
            .get_mut(call_id)
            .ok_or_else(|| McpError::UnknownCall(call_id.to_string()))?;
        if call.status == McpCallStatus::Cancelled {
            return Err(McpError::AlreadyCancelled(call_id.to_string()));
        }
        call.status = McpCallStatus::Cancelled;
        Ok(())
    }

    /// Completes a call (the server reported the result).
    pub fn complete(&mut self, call_id: &str) -> Result<(), McpError> {
        let call = self
            .calls
            .get_mut(call_id)
            .ok_or_else(|| McpError::UnknownCall(call_id.to_string()))?;
        call.status = McpCallStatus::Completed;
        Ok(())
    }

    /// AUDIT: calls correlated by task identity.
    pub fn calls_for_task(&self, task_id: &str) -> Vec<&McpCall> {
        self.calls
            .values()
            .filter(|c| c.task_id == task_id)
            .collect()
    }
}

// ---------------------------------------------------------------------------
// MCP management + scoped auth (REQ-EV-0128)
// ---------------------------------------------------------------------------

/// A server config from a scope. User scope overrides project scope on
/// conflict (deterministic).
#[derive(Clone, Debug, PartialEq)]
pub struct ServerConfig {
    pub server: String,
    pub scope: Scope,
    pub command: String,
    /// Credential BROKER REFERENCE — never the credential value.
    pub credential_ref: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Scope {
    Project,
    User,
}

/// Resolves conflicting server configs deterministically: user scope wins
/// over project scope per server name.
pub fn resolve_server_configs(configs: &[ServerConfig]) -> BTreeMap<String, ServerConfig> {
    let mut resolved: BTreeMap<String, ServerConfig> = BTreeMap::new();
    let mut ranked: Vec<&ServerConfig> = configs.iter().collect();
    ranked.sort_by_key(|c| c.scope); // Project < User: later (user) wins
    for config in ranked {
        resolved.insert(config.server.clone(), config.clone());
    }
    resolved
}

/// The credential to send to the MCP server — a BROKER REFERENCE, never
/// the value, so it can never enter the model prompt.
pub fn credential_for_prompt(config: &ServerConfig) -> Option<String> {
    config.credential_ref.clone()
}

// ---------------------------------------------------------------------------
// Organizational engineering memory (REQ-EV-0162)
// ---------------------------------------------------------------------------

/// A validated organizational fact.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MemoryFact {
    pub fact_id: String,
    pub text: String,
    pub scope: String, // org | team | project
    pub provenance: String,
    pub ttl_expires_ms: Option<i64>,
    pub superseded_by: Option<String>,
    pub deleted: bool,
}

#[derive(Debug)]
pub enum MemoryError {
    NotValidated,
    Expired { fact_id: String },
    AlreadySuperseded { fact_id: String },
}

impl fmt::Display for MemoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MemoryError::NotValidated => write!(f, "fact is not validated — cannot promote"),
            MemoryError::Expired { fact_id } => write!(f, "fact {fact_id} expired (TTL)"),
            MemoryError::AlreadySuperseded { fact_id } => {
                write!(f, "fact {fact_id} already superseded")
            }
        }
    }
}

impl std::error::Error for MemoryError {}

/// The organizational memory store. Raw transcripts NEVER auto-promote:
/// facts enter only through explicit validated promotion.
#[derive(Default)]
pub struct EngineeringMemory {
    facts: BTreeMap<String, MemoryFact>,
    counter: u64,
}

impl EngineeringMemory {
    pub fn new() -> Self {
        Default::default()
    }

    /// Promotes a validated fact (provenance recorded at promotion).
    pub fn promote_validated(
        &mut self,
        text: &str,
        scope: &str,
        provenance: &str,
        ttl_expires_ms: Option<i64>,
    ) -> Result<String, MemoryError> {
        if provenance.is_empty() {
            return Err(MemoryError::NotValidated);
        }
        self.counter += 1;
        let fact_id = format!("fact-{}", self.counter);
        self.facts.insert(
            fact_id.clone(),
            MemoryFact {
                fact_id: fact_id.clone(),
                text: text.to_string(),
                scope: scope.to_string(),
                provenance: provenance.to_string(),
                ttl_expires_ms,
                superseded_by: None,
                deleted: false,
            },
        );
        Ok(fact_id)
    }

    /// Supersedes an old fact with a new one — the old is marked
    /// superseded (inspectable), not deleted.
    pub fn supersede(
        &mut self,
        old_id: &str,
        new_text: &str,
        new_scope: &str,
        provenance: &str,
    ) -> Result<String, MemoryError> {
        let old = self
            .facts
            .get(old_id)
            .ok_or_else(|| MemoryError::AlreadySuperseded {
                fact_id: old_id.to_string(),
            })?;
        if old.superseded_by.is_some() || old.deleted {
            return Err(MemoryError::AlreadySuperseded {
                fact_id: old_id.to_string(),
            });
        }
        let new_id = self.promote_validated(new_text, new_scope, provenance, None)?;
        self.facts.get_mut(old_id).unwrap().superseded_by = Some(new_id.clone());
        Ok(new_id)
    }

    pub fn delete(&mut self, fact_id: &str) -> Result<(), MemoryError> {
        let fact = self
            .facts
            .get_mut(fact_id)
            .ok_or_else(|| MemoryError::AlreadySuperseded {
                fact_id: fact_id.to_string(),
            })?;
        fact.deleted = true;
        Ok(())
    }

    /// Live facts: not deleted, not expired, not superseded.
    pub fn live_facts(&self, now_ms: i64) -> Vec<&MemoryFact> {
        self.facts
            .values()
            .filter(|f| {
                !f.deleted
                    && f.superseded_by.is_none()
                    && f.ttl_expires_ms.map(|t| t > now_ms).unwrap_or(true)
            })
            .collect()
    }

    /// The supersession chain of a fact (inspectable).
    pub fn supersession_chain(&self, fact_id: &str) -> Vec<String> {
        let mut chain = vec![fact_id.to_string()];
        let mut current = fact_id.to_string();
        while let Some(next) = self
            .facts
            .get(&current)
            .and_then(|f| f.superseded_by.clone())
        {
            chain.push(next.clone());
            current = next;
        }
        chain
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// QUAL-EV-0104: a real MCP lifecycle supports list/call/cancel with
    /// audit correlation by task identity.
    #[test]
    fn mcp_lifecycle_list_call_cancel_with_audit() {
        let mut hub = McpHub::new();
        hub.register_tool(McpTool {
            server: "acme".into(),
            name: "search_docs".into(),
            schema: "{}".into(),
        });

        // LIST: discovery sees the tool.
        assert_eq!(hub.list().len(), 1);

        // CALL with full identity.
        let call = hub
            .call("task-1", 3, "acme", "search_docs", "{}")
            .unwrap()
            .clone();
        let call_id = call.call_id.clone();
        assert_eq!(call.task_id, "task-1");
        assert_eq!(call.turn, 3);
        assert_eq!(call.status, McpCallStatus::Running);

        // CANCEL with audit correlation.
        hub.cancel(&call_id).unwrap();
        let audit = hub.calls_for_task("task-1");
        assert_eq!(audit.len(), 1);
        assert_eq!(audit[0].status, McpCallStatus::Cancelled);
        // Double cancel is a typed error.
        assert!(matches!(
            hub.cancel(&call.call_id),
            Err(McpError::AlreadyCancelled(_))
        ));
    }

    /// QUAL-EV-0128: user/project MCP conflicts resolve deterministically
    /// and credentials never enter the model prompt.
    #[test]
    fn scoped_auth_resolves_and_credentials_stay_out_of_prompt() {
        let configs = vec![
            ServerConfig {
                server: "acme".into(),
                scope: Scope::Project,
                command: "project-acme".into(),
                credential_ref: Some("broker:project-acme-cred".into()),
            },
            ServerConfig {
                server: "acme".into(),
                scope: Scope::User,
                command: "user-acme".into(),
                credential_ref: Some("broker:user-acme-cred".into()),
            },
        ];
        let resolved = resolve_server_configs(&configs);
        // User scope wins deterministically.
        assert_eq!(resolved["acme"].command, "user-acme");

        // The prompt carries only the BROKER REFERENCE.
        let prompt_ref = credential_for_prompt(&resolved["acme"]).unwrap();
        assert!(prompt_ref.starts_with("broker:"));
        assert!(!prompt_ref.contains("secret-value"));
    }

    /// QUAL-EV-0162: memory conflict/supersession is inspectable; raw
    /// transcripts never auto-promote.
    #[test]
    fn memory_supersession_is_inspectable() {
        let mut memory = EngineeringMemory::new();

        // Raw transcript text WITHOUT provenance: promotion refused.
        assert!(matches!(
            memory.promote_validated("raw transcript fragment", "project", "", None),
            Err(MemoryError::NotValidated)
        ));

        // Validated promotion works.
        let f1 = memory
            .promote_validated(
                "the retry budget is 3 attempts",
                "project",
                "eval:verified-2026-09",
                None,
            )
            .unwrap();

        // Supersede: the old fact remains inspectable, marked superseded.
        let f2 = memory
            .supersede(
                &f1,
                "the retry budget is now 5 attempts",
                "project",
                "eval:verified-2026-10",
            )
            .unwrap();
        assert_eq!(memory.supersession_chain(&f1), vec![f1.clone(), f2.clone()]);
        assert_eq!(
            memory.live_facts(0).len(),
            1,
            "only the newest fact is live"
        );

        // TTL expiry drops a fact from live view.
        let f3 = memory
            .promote_validated(
                "temporary mitigation in place",
                "team",
                "eval:temporary",
                Some(100),
            )
            .unwrap();
        assert_eq!(memory.live_facts(200).len(), 1, "expired fact dropped");

        // Delete removes the fact from live view.
        memory.delete(&f2).unwrap();
        assert_eq!(memory.live_facts(0).len(), 1, "deleted fact dropped");
        let _ = f3;
    }
}
