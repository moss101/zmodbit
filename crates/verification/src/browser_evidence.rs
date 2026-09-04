//! Browser/E2E evidence normalization (M2, REQ-EV-0147): a visual or
//! browser test artifact is only admissible when it is bound to the exact
//! workspace revision, run/step, and the verification claim it proves.
//! Unbound screenshots are not evidence — they are pictures.

use crate::evidence_index::EvidenceItem;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The claim a browser/E2E artifact is offered as evidence FOR.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VerificationClaim {
    pub claim_id: String,
    /// The gate whose passing this artifact substantiates.
    pub gate: String,
}

/// A normalized browser/E2E artifact.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BrowserEvidence {
    pub artifact_id: String,
    /// Exact workspace revision the artifact was captured against.
    pub workspace_revision: u64,
    /// Run/step the capture belongs to.
    pub run_id: String,
    pub step_id: String,
    /// The verification claim this artifact substantiates.
    pub claim: VerificationClaim,
    pub url: String,
    /// Digest of the captured bytes (screenshot/recording/HAR).
    pub artifact_sha256: String,
    pub byte_length: usize,
    pub ts_ms: i64,
}

#[derive(Debug)]
pub enum BrowserEvidenceError {
    Unbound(&'static str),
    DigestMismatch,
}

impl std::fmt::Display for BrowserEvidenceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BrowserEvidenceError::Unbound(field) => {
                write!(f, "browser artifact is not admissible: missing {field}")
            }
            BrowserEvidenceError::DigestMismatch => {
                write!(f, "artifact digest does not match captured bytes")
            }
        }
    }
}

impl std::error::Error for BrowserEvidenceError {}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Normalizes raw capture bytes + bindings into admissible evidence
/// (REQ-EV-0147). Every binding is required; the digest is computed over
/// the actual bytes, not trusted from the caller.
pub fn normalize_browser_artifact(
    artifact_id: &str,
    workspace_revision: u64,
    run_id: &str,
    step_id: &str,
    claim: VerificationClaim,
    url: &str,
    captured_bytes: &[u8],
) -> Result<BrowserEvidence, BrowserEvidenceError> {
    if workspace_revision == 0 {
        return Err(BrowserEvidenceError::Unbound("workspace revision"));
    }
    if run_id.is_empty() || step_id.is_empty() {
        return Err(BrowserEvidenceError::Unbound("run/step binding"));
    }
    if claim.claim_id.is_empty() || claim.gate.is_empty() {
        return Err(BrowserEvidenceError::Unbound("verification claim"));
    }
    if url.is_empty() {
        return Err(BrowserEvidenceError::Unbound("capture url"));
    }
    let mut hasher = Sha256::new();
    hasher.update(captured_bytes);
    let digest = format!("{:x}", hasher.finalize());
    Ok(BrowserEvidence {
        artifact_id: artifact_id.to_string(),
        workspace_revision,
        run_id: run_id.to_string(),
        step_id: step_id.to_string(),
        claim,
        url: url.to_string(),
        artifact_sha256: digest,
        byte_length: captured_bytes.len(),
        ts_ms: now_ms(),
    })
}

impl BrowserEvidence {
    /// The artifact's link line: revision + verification claim + artifact
    /// digest in one auditable reference (QUAL-EV-0147).
    pub fn evidence_link(&self) -> String {
        format!(
            "artifact {} (sha256:{}) rev {} run {}/step {} proves claim {} [gate {}]",
            self.artifact_id,
            &self.artifact_sha256[..12],
            self.workspace_revision,
            self.run_id,
            self.step_id,
            self.claim.claim_id,
            self.claim.gate
        )
    }

    /// Flattens into the evidence index so browser evidence is searchable
    /// under the same tenant-scoped index as everything else.
    pub fn to_index_item(&self, tenant_id: &str) -> EvidenceItem {
        EvidenceItem::new(
            tenant_id,
            &self.run_id,
            &self.step_id,
            crate::evidence_index::EvidenceKind::Checkpoint,
            &self.evidence_link(),
            &format!("url={} bytes={}", self.url, self.byte_length),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// QUAL-EV-0147: a visual/browser artifact links to the revision and
    /// the verification claim — and unbound artifacts are refused.
    #[test]
    fn artifact_links_to_revision_and_claim() {
        let png = vec![0x89u8, b'P', b'N', b'G', 1, 2, 3];
        let ev = normalize_browser_artifact(
            "shot-1",
            42,
            "run-9",
            "step-3",
            VerificationClaim {
                claim_id: "claim-e2e-login".into(),
                gate: "e2e".into(),
            },
            "http://localhost:3000/login",
            &png,
        )
        .unwrap();

        let link = ev.evidence_link();
        assert!(link.contains("rev 42"));
        assert!(link.contains("claim claim-e2e-login"));
        assert!(link.contains("gate e2e"));
        assert!(link.contains("sha256:"));

        // Searchable through the standard evidence index, tenant-scoped.
        let mut idx = crate::evidence_index::EvidenceIndex::new();
        idx.index(ev.to_index_item("tenant-a"));
        assert_eq!(idx.search("tenant-a", "claim-e2e-login").len(), 1);
        assert!(idx.search("tenant-b", "claim-e2e-login").is_empty());

        // Unbound: no revision, no claim — not evidence.
        assert!(matches!(
            normalize_browser_artifact(
                "shot-2",
                0,
                "run-9",
                "step-3",
                VerificationClaim {
                    claim_id: "c".into(),
                    gate: "e2e".into()
                },
                "http://x",
                &png
            ),
            Err(BrowserEvidenceError::Unbound("workspace revision"))
        ));
        assert!(matches!(
            normalize_browser_artifact(
                "shot-3",
                42,
                "run-9",
                "step-3",
                VerificationClaim {
                    claim_id: String::new(),
                    gate: String::new()
                },
                "http://x",
                &png
            ),
            Err(BrowserEvidenceError::Unbound("verification claim"))
        ));
    }
}
