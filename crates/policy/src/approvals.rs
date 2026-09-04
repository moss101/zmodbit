//! Approval flow (M2.5, docs/23 § Approval policy, docs/13 § Approval state
//! machine: `Requested → Approved | Denied | Expired | Superseded`).
//!
//! An approval binds the NORMALIZED EFFECT INTENT HASH (not merely a tool
//! name) plus scope and expiry — changing parameters invalidates the
//! approval. Expired approvals are rejected and transitioned to `Expired`.

use std::fmt;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::ToolCallRequest;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalState {
    Pending,
    Approved,
    Denied,
    Expired,
    Superseded,
}

/// One approval record bound to a normalized intent hash.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Approval {
    pub approval_id: String,
    pub intent_hash: String,
    pub tool: String,
    pub scope: String,
    pub state: ApprovalState,
    pub created_at_ms: u128,
    pub expires_at_ms: Option<u128>,
    pub resolved_by: Option<String>,
}

#[derive(Debug)]
pub enum ApprovalError {
    Unknown(String),
    NotPending(String),
}

impl fmt::Display for ApprovalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ApprovalError::Unknown(id) => write!(f, "unknown approval {id}"),
            ApprovalError::NotPending(id) => {
                write!(f, "approval {id} is not pending")
            }
        }
    }
}

impl std::error::Error for ApprovalError {}

/// The normalized effect intent hash (docs/23: approval binds the hash, not
/// merely a tool name — changing parameters changes the hash and invalidates
/// prior approvals).
pub fn intent_hash(request: &ToolCallRequest) -> String {
    // Canonical serialization: sorted-key JSON of tool + class + arguments.
    let canonical = serde_json::json!({
        "arguments": sort_value(&request.arguments),
        "effect_class": request.effect_class,
        "tool": request.tool,
    });
    let mut hasher = Sha256::new();
    hasher.update(serde_json::to_vec(&canonical).unwrap_or_default());
    format!("{:x}", hasher.finalize())
}

fn sort_value(value: &serde_json::Value) -> serde_json::Value {
    use serde_json::Value as V;
    match value {
        V::Object(map) => {
            let sorted: serde_json::Map<String, V> = map
                .iter()
                .map(|(k, v)| (k.clone(), sort_value(v)))
                .collect::<serde_json::Map<String, V>>();
            // BTreeMap-backed construction sorts keys.
            V::Object(sorted)
        }
        V::Array(items) => V::Array(items.iter().map(sort_value).collect()),
        other => other.clone(),
    }
}

/// The durable approval store with expiry and supersession.
pub struct ApprovalStore {
    approvals: Mutex<Vec<Approval>>,
}

impl Default for ApprovalStore {
    fn default() -> Self {
        Self::new()
    }
}

impl ApprovalStore {
    pub fn new() -> Self {
        Self {
            approvals: Mutex::new(Vec::new()),
        }
    }

    /// Requests an approval for an effect: binds the normalized intent hash
    /// with an optional expiry.
    pub fn request(
        &self,
        approval_id: &str,
        request: &ToolCallRequest,
        scope: &str,
        expires_at_ms: Option<u128>,
        now_ms: u128,
    ) -> Result<Approval, ApprovalError> {
        let approval = Approval {
            approval_id: approval_id.to_string(),
            intent_hash: intent_hash(request),
            tool: request.tool.clone(),
            scope: scope.to_string(),
            state: ApprovalState::Pending,
            created_at_ms: now_ms,
            expires_at_ms,
            resolved_by: None,
        };
        self.approvals
            .lock()
            .expect("approvals mutex poisoned")
            .push(approval.clone());
        Ok(approval)
    }

    /// Resolves a pending approval (host/user decision).
    pub fn resolve(
        &self,
        approval_id: &str,
        state: ApprovalState,
        resolved_by: &str,
        _now_ms: u128,
    ) -> Result<Approval, ApprovalError> {
        let mut approvals = self.approvals.lock().expect("approvals mutex poisoned");
        let approval = approvals
            .iter_mut()
            .find(|a| a.approval_id == approval_id)
            .ok_or_else(|| ApprovalError::Unknown(approval_id.to_string()))?;
        let is_supersede_from_approved =
            approval.state == ApprovalState::Approved && state == ApprovalState::Superseded;
        if approval.state != ApprovalState::Pending && !is_supersede_from_approved {
            return Err(ApprovalError::NotPending(approval_id.to_string()));
        }
        match state {
            ApprovalState::Approved | ApprovalState::Denied | ApprovalState::Superseded => {
                // Superseded is legal from Approved (a newer approval for the
                // same intent replaces this one — docs/13 state machine).
                approval.state = state;
            }
            _ => return Err(ApprovalError::NotPending(approval_id.to_string())),
        }
        approval.resolved_by = Some(resolved_by.to_string());
        Ok(approval.clone())
    }

    /// True when a live APPROVED approval exists for the intent hash. Expired
    /// approvals transition to `Expired` on sight.
    pub fn has_live_approval(&self, intent_hash: &str, now_ms: u128) -> bool {
        let mut approvals = self.approvals.lock().expect("approvals mutex poisoned");
        for a in approvals.iter_mut() {
            if a.intent_hash != intent_hash {
                continue;
            }
            if a.state == ApprovalState::Approved {
                if let Some(expires) = a.expires_at_ms {
                    if now_ms > expires {
                        a.state = ApprovalState::Expired;
                    } else {
                        return true;
                    }
                } else {
                    return true;
                }
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EffectClass;
    use serde_json::json;

    fn request(args: serde_json::Value) -> ToolCallRequest {
        ToolCallRequest {
            tool: "fs.write".into(),
            effect_class: EffectClass::Write,
            arguments: args,
        }
    }

    #[test]
    fn changing_parameters_changes_the_intent_hash() {
        let h1 = intent_hash(&request(json!({ "path": "/a", "content": "x" })));
        let h2 = intent_hash(&request(json!({ "path": "/a", "content": "y" })));
        let h3 = intent_hash(&request(json!({ "content": "x", "path": "/a" })));
        assert_ne!(h1, h2, "changed parameters must change the hash");
        assert_eq!(h1, h3, "key order must not change the hash");
    }

    #[test]
    fn denied_approval_never_allows() {
        let store = ApprovalStore::new();
        store
            .request("a1", &request(json!({"path": "/a"})), "/a", None, 0)
            .unwrap();
        store
            .resolve("a1", ApprovalState::Denied, "mohsin", 1)
            .unwrap();
        assert!(!store.has_live_approval(&intent_hash(&request(json!({"path": "/a"}))), 5));
    }

    #[test]
    fn expired_approval_is_rejected_on_sight() {
        let store = ApprovalStore::new();
        store
            .request("a2", &request(json!({"path": "/a"})), "/a", Some(100), 50)
            .unwrap();
        store
            .resolve("a2", ApprovalState::Approved, "mohsin", 60)
            .unwrap();
        // At t=50 still valid; at t=200 expired.
        assert!(store.has_live_approval(&intent_hash(&request(json!({"path": "/a"}))), 90));
        assert!(!store.has_live_approval(&intent_hash(&request(json!({"path": "/a"}))), 200));
        // Expired transition recorded.
        let expired = store
            .approvals
            .lock()
            .unwrap()
            .iter()
            .find(|a| a.state == ApprovalState::Expired)
            .is_some();
        assert!(expired);
    }

    #[test]
    fn superseded_approval_is_rejected() {
        let store = ApprovalStore::new();
        store
            .request("a3", &request(json!({"path": "/a"})), "/a", None, 0)
            .unwrap();
        store
            .resolve("a3", ApprovalState::Approved, "mohsin", 1)
            .unwrap();
        store
            .resolve("a3", ApprovalState::Superseded, "policy", 2)
            .unwrap();
        assert!(!store.has_live_approval(&intent_hash(&request(json!({"path": "/a"}))), 3));
    }
}
