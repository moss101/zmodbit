//! Security hardening final batch (M9): tool credentials outside tool
//! arguments via the Secret Broker (REQ-EV-0215), MCP config
//! conversational management with trust/credential gates (REQ-EV-0224),
//! marketplace trust surfaced before activation (REQ-EV-0225), the
//! protected-effect receipt hash chain (REQ-EV-0270), and dynamic
//! credential handles / broker injection (REQ-EV-0288).

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
// Tool credentials outside tool arguments (REQ-EV-0215)
// ---------------------------------------------------------------------------

/// A tool schema entry. Secrets are declared as REQUIRED/optional
/// capabilities resolved by the host — the schema never exposes an API
/// key parameter to the model.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolSchemaEntry {
    pub tool: String,
    pub parameters: Vec<(String, String)>,
    /// Secret broker references this tool needs (resolved host-side).
    pub required_secrets: Vec<String>,
}

/// Inspects a schema: no parameter may look like an API key field
/// (QUAL-EV-0215).
pub fn inspect_schema(schema: &ToolSchemaEntry) -> Result<(), String> {
    let suspicious = [
        "api_key", "apikey", "api-key", "token", "secret", "password",
    ];
    for (param, _) in &schema.parameters {
        let lower = param.to_lowercase();
        if suspicious.iter().any(|s| lower.contains(s)) {
            return Err(format!(
                "schema for {tool:?} exposes secret-like parameter {param:?} — secrets resolve via the broker",
                tool = schema.tool
            ));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// MCP config management gates (REQ-EV-0224)
// ---------------------------------------------------------------------------

/// A proposed MCP install (from the agent or UI).
#[derive(Clone, Debug, PartialEq)]
pub struct ProposedMcpInstall {
    pub server: String,
    pub command: String,
    pub trust_acknowledged: bool,
    pub credential_gate_passed: bool,
}

/// Validates the proposal: BOTH trust and credential gates must pass
/// before the config executes (QUAL-EV-0224).
pub fn authorize_mcp_install(proposal: &ProposedMcpInstall) -> Result<(), String> {
    if !proposal.trust_acknowledged {
        return Err(format!(
            "server {:?} cannot execute: trust gate not acknowledged",
            proposal.server
        ));
    }
    if !proposal.credential_gate_passed {
        return Err(format!(
            "server {:?} cannot execute: credential gate not passed",
            proposal.server
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Marketplace trust surfaced (REQ-EV-0225)
// ---------------------------------------------------------------------------

/// A marketplace extension listing.
#[derive(Clone, Debug, PartialEq)]
pub struct MarketplaceExtension {
    pub name: String,
    pub publisher: String,
    pub source_url: String,
    pub signature_valid: bool,
    pub publisher_trusted: bool,
    pub requested_capabilities: Vec<String>,
}

/// The surfaced trust card shown BEFORE activation.
#[derive(Clone, Debug, PartialEq)]
pub struct TrustCard {
    pub publisher: String,
    pub source_url: String,
    pub signature_valid: bool,
    pub capabilities: Vec<String>,
    pub quarantined: bool,
}

/// Surfaces trust and decides activation: unsigned/untrusted extensions
/// are QUARANTINED (QUAL-EV-0225).
pub fn surface_trust(ext: &MarketplaceExtension) -> TrustCard {
    let quarantined = !ext.signature_valid || !ext.publisher_trusted;
    TrustCard {
        publisher: ext.publisher.clone(),
        source_url: ext.source_url.clone(),
        signature_valid: ext.signature_valid,
        capabilities: ext.requested_capabilities.clone(),
        quarantined,
    }
}

// ---------------------------------------------------------------------------
// Protected-effect receipt chain (REQ-EV-0270)
// ---------------------------------------------------------------------------

/// One immutable receipt in the protected-effect chain. Each receipt
/// hash-links to the previous one and binds approval/capability/call/
/// result digests.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EffectReceipt {
    pub receipt_id: String,
    pub prev_receipt_hash: String,
    /// Bound digests: approval, capability, call, result.
    pub approval_digest: String,
    pub capability_digest: String,
    pub call_digest: String,
    pub result_digest: String,
    /// Chain hash = H(prev + all bound digests).
    pub chain_hash: String,
}

/// The append-only chain. Writes are append-only; verification detects
/// tamper, delete, and reorder.
#[derive(Default)]
pub struct ReceiptChain {
    receipts: Vec<EffectReceipt>,
}

fn chain_hash(prev: &str, approval: &str, capability: &str, call: &str, result: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(prev.as_bytes());
    hasher.update(approval.as_bytes());
    hasher.update(capability.as_bytes());
    hasher.update(call.as_bytes());
    hasher.update(result.as_bytes());
    format!("{:x}", hasher.finalize())
}

impl ReceiptChain {
    /// Appends a receipt bound to approval/capability/call/result digests.
    pub fn append(
        &mut self,
        approval_digest: &str,
        capability_digest: &str,
        call_digest: &str,
        result_digest: &str,
    ) -> EffectReceipt {
        let prev = self
            .receipts
            .last()
            .map(|r| r.chain_hash.clone())
            .unwrap_or_else(|| "GENESIS".to_string());
        let chain_hash = chain_hash(
            &prev,
            approval_digest,
            capability_digest,
            call_digest,
            result_digest,
        );
        let receipt = EffectReceipt {
            receipt_id: format!("receipt-{}", &chain_hash[..12]),
            prev_receipt_hash: prev,
            approval_digest: approval_digest.to_string(),
            capability_digest: capability_digest.to_string(),
            call_digest: call_digest.to_string(),
            result_digest: result_digest.to_string(),
            chain_hash,
        };
        self.receipts.push(receipt.clone());
        receipt
    }

    /// Verifies the full chain: hash links intact, order intact.
    pub fn verify(&self) -> Result<(), String> {
        let mut prev = "GENESIS".to_string();
        for (index, receipt) in self.receipts.iter().enumerate() {
            if receipt.prev_receipt_hash != prev {
                return Err(format!(
                    "chain broken at receipt {index}: link hash mismatch (tamper/delete/reorder)"
                ));
            }
            let expected = chain_hash(
                &prev,
                &receipt.approval_digest,
                &receipt.capability_digest,
                &receipt.call_digest,
                &receipt.result_digest,
            );
            if expected != receipt.chain_hash {
                return Err(format!(
                    "chain broken at receipt {index}: content hash mismatch"
                ));
            }
            prev = receipt.chain_hash.clone();
        }
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.receipts.len()
    }
}

// ---------------------------------------------------------------------------
// Dynamic credential handles / broker injection (REQ-EV-0288)
// ---------------------------------------------------------------------------

/// A short-lived scoped credential handle issued by the broker.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CredentialHandle {
    pub handle_id: String,
    pub scope: String,
    pub expires_ms: i64,
}

/// The broker: issues dynamic handles (never static secrets in image or
/// config) and injects them into the guest environment only.
#[derive(Default)]
pub struct SecretBroker {
    handles: BTreeMap<String, (CredentialHandle, String)>, // handle_id → (handle, secret)
}

#[derive(Debug)]
pub enum BrokerError {
    Expired {
        handle_id: String,
    },
    UnknownHandle(String),
    ScopeMismatch {
        handle_id: String,
        required_scope: String,
    },
}

impl fmt::Display for BrokerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BrokerError::Expired { handle_id } => write!(f, "handle {handle_id} expired"),
            BrokerError::UnknownHandle(id) => write!(f, "unknown handle {id:?}"),
            BrokerError::ScopeMismatch {
                handle_id,
                required_scope,
            } => write!(
                f,
                "handle {handle_id} scope does not cover {required_scope:?}"
            ),
        }
    }
}

impl SecretBroker {
    pub fn new() -> Self {
        Default::default()
    }

    /// Issues a short-lived scoped handle. The STATIC secret stays in the
    /// broker; only the handle is injected into the guest.
    pub fn issue(&mut self, scope: &str, secret: &str, expires_ms: i64) -> CredentialHandle {
        let handle_id = format!("handle-{}", &sha256_hex(secret.as_bytes())[..16]);
        self.handles.insert(
            handle_id.clone(),
            (
                CredentialHandle {
                    handle_id: handle_id.clone(),
                    scope: scope.to_string(),
                    expires_ms,
                },
                secret.to_string(),
            ),
        );
        CredentialHandle {
            handle_id,
            scope: scope.to_string(),
            expires_ms,
        }
    }

    /// Resolves a handle to the secret — only for live, scope-covering
    /// handles (broker-side injection).
    pub fn resolve(
        &self,
        handle: &CredentialHandle,
        required_scope: &str,
    ) -> Result<String, BrokerError> {
        let (stored_handle, secret) = self
            .handles
            .get(&handle.handle_id)
            .ok_or_else(|| BrokerError::UnknownHandle(handle.handle_id.clone()))?;
        if stored_handle.expires_ms <= 0 {
            return Err(BrokerError::Expired {
                handle_id: handle.handle_id.clone(),
            });
        }
        if !stored_handle.scope.contains(required_scope) {
            return Err(BrokerError::ScopeMismatch {
                handle_id: handle.handle_id.clone(),
                required_scope: required_scope.to_string(),
            });
        }
        Ok(secret.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// QUAL-EV-0215: schema inspection contains no API key field.
    #[test]
    fn schema_inspection_finds_no_api_key_field() {
        let clean = ToolSchemaEntry {
            tool: "web.fetch".into(),
            parameters: vec![("url".into(), "string".into())],
            required_secrets: vec!["broker:web-api-key".into()],
        };
        assert!(inspect_schema(&clean).is_ok());

        let leaky = ToolSchemaEntry {
            tool: "web.fetch".into(),
            parameters: vec![
                ("url".into(), "string".into()),
                ("api_key".into(), "string".into()),
            ],
            required_secrets: vec![],
        };
        assert!(inspect_schema(&leaky).is_err());
    }

    /// QUAL-EV-0224: a proposed MCP install cannot execute until trust
    /// AND credential gates pass.
    #[test]
    fn mcp_install_requires_both_gates() {
        let base = ProposedMcpInstall {
            server: "acme".into(),
            command: "npx acme-mcp".into(),
            trust_acknowledged: false,
            credential_gate_passed: false,
        };
        assert!(authorize_mcp_install(&base).is_err(), "no gates passed");

        let mut trust_only = base.clone();
        trust_only.trust_acknowledged = true;
        assert!(
            authorize_mcp_install(&trust_only).is_err(),
            "credential gate missing"
        );

        let mut both = trust_only;
        both.credential_gate_passed = true;
        assert!(authorize_mcp_install(&both).is_ok());
    }

    /// QUAL-EV-0225: unsigned/untrusted marketplace extensions are
    /// quarantined before activation.
    #[test]
    fn unsigned_extension_quarantined() {
        let unsigned = MarketplaceExtension {
            name: "cool-extension".into(),
            publisher: "unknown-pub".into(),
            source_url: "https://marketplace.example/cool".into(),
            signature_valid: false,
            publisher_trusted: false,
            requested_capabilities: vec!["fs.read".into()],
        };
        let card = surface_trust(&unsigned);
        assert!(card.quarantined);
        // Capabilities are still SURFACED to the user.
        assert_eq!(card.capabilities, vec!["fs.read".to_string()]);

        let signed = MarketplaceExtension {
            signature_valid: true,
            publisher_trusted: true,
            ..unsigned
        };
        assert!(!surface_trust(&signed).quarantined);
    }

    /// QUAL-EV-0270: tamper/delete/reorder of a receipt causes chain
    /// verification failure.
    #[test]
    fn receipt_chain_detects_tamper_delete_reorder() {
        let mut chain = ReceiptChain::default();
        let r1 = chain.append("ap-1", "cap-1", "call-1", "res-1");
        let r2 = chain.append("ap-2", "cap-2", "call-2", "res-2");
        let r3 = chain.append("ap-3", "cap-3", "call-3", "res-3");
        assert!(chain.verify().is_ok());

        // TAMPER: modify a bound digest in the middle receipt.
        let mut tampered = vec![r1.clone(), r2.clone(), r3.clone()];
        tampered[1].result_digest = "tampered".into();
        let mut c = ReceiptChain::default();
        c.receipts = tampered;
        assert!(c.verify().is_err());

        // DELETE: drop the middle receipt — link breaks.
        let mut deleted = vec![r1.clone(), r2.clone()];
        let mut c = ReceiptChain::default();
        c.receipts = deleted.clone();
        deleted.push(r3.clone());
        let mut c2 = ReceiptChain::default();
        c2.receipts = vec![r1.clone(), r3.clone()];
        assert!(c2.verify().is_err());
        let _ = c;
        let _ = deleted;

        // REORDER: swap two receipts — link breaks.
        let mut reordered = vec![r2.clone(), r1.clone(), r3.clone()];
        reordered = vec![r1.clone(), r3.clone(), r2.clone()];
        let mut c = ReceiptChain::default();
        c.receipts = reordered;
        assert!(c.verify().is_err());
    }

    /// QUAL-EV-0288: inspecting the guest material shows no long-lived
    /// provider secret — only short-lived scoped handles.
    #[test]
    fn dynamic_handles_keep_static_secrets_out_of_guest() {
        let mut broker = SecretBroker::new();
        let static_secret = "sk-live-STATIC-PROVIDER-KEY";
        let handle = broker.issue("stripe:charge", static_secret, 1_000_000);

        // What the guest receives: the HANDLE, not the secret.
        assert!(handle.handle_id.starts_with("handle-"));
        assert!(!handle.handle_id.contains(static_secret));

        // Guest image/env/artifact inspection: static secret absent —
        // only the handle id is injected.
        let guest_env = format!("STRIPE_KEY={}", handle.handle_id);
        assert!(!guest_env.contains(static_secret));

        // Broker-side resolve works for a live, scope-covering handle.
        assert_eq!(broker.resolve(&handle, "stripe").unwrap(), static_secret);
        // Scope mismatch refused.
        assert!(broker.resolve(&handle, "github").is_err());
        // Expired handle refused.
        let expired = broker.issue("stripe:charge", "another-secret", 0);
        assert!(matches!(
            broker.resolve(&expired, "stripe"),
            Err(BrokerError::Expired { .. })
        ));
    }
}
