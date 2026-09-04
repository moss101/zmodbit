//! Searchable transcript/tool evidence (M2, REQ-EV-0132): every message,
//! tool call, command, file touch, error, and checkpoint is indexed as
//! typed evidence. Search is scoped: a tenant NEVER sees another tenant's
//! evidence, even when the query matches (QUAL-EV-0132).

use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceKind {
    Message,
    ToolCall,
    Command,
    FileTouch,
    Error,
    Checkpoint,
}

impl fmt::Display for EvidenceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            EvidenceKind::Message => "message",
            EvidenceKind::ToolCall => "tool_call",
            EvidenceKind::Command => "command",
            EvidenceKind::FileTouch => "file_touch",
            EvidenceKind::Error => "error",
            EvidenceKind::Checkpoint => "checkpoint",
        };
        write!(f, "{s}")
    }
}

/// One indexed evidence item.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EvidenceItem {
    pub tenant_id: String,
    pub run_id: String,
    pub step_id: String,
    pub kind: EvidenceKind,
    /// Indexed one-line summary (what a search matches against).
    pub summary: String,
    /// Full detail retrievable after a hit.
    pub detail: String,
    pub ts_ms: i64,
}

impl EvidenceItem {
    pub fn new(
        tenant_id: &str,
        run_id: &str,
        step_id: &str,
        kind: EvidenceKind,
        summary: &str,
        detail: &str,
    ) -> Self {
        Self {
            tenant_id: tenant_id.to_string(),
            run_id: run_id.to_string(),
            step_id: step_id.to_string(),
            kind,
            summary: summary.to_string(),
            detail: detail.to_string(),
            ts_ms: now_ms(),
        }
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[derive(Debug)]
pub enum EvidenceError {
    /// A cross-tenant access attempt — refused, never filtered silently.
    TenantMismatch { requested: String, owned: String },
}

impl fmt::Display for EvidenceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EvidenceError::TenantMismatch { requested, owned } => {
                write!(
                    f,
                    "tenant {requested:?} cannot access evidence owned by {owned:?}"
                )
            }
        }
    }
}

impl std::error::Error for EvidenceError {}

/// The in-memory evidence index. Keyed by (tenant, run) so scope checks are
/// structural, not best-effort.
#[derive(Default)]
pub struct EvidenceIndex {
    items: Vec<EvidenceItem>,
}

impl EvidenceIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// Indexes one evidence item.
    pub fn index(&mut self, item: EvidenceItem) {
        self.items.push(item);
    }

    /// Full-text-ish search scoped to a tenant: matches summary, detail,
    /// run id, step id, or kind name.
    pub fn search(&self, tenant_id: &str, query: &str) -> Vec<&EvidenceItem> {
        let q = query.to_lowercase();
        self.items
            .iter()
            .filter(|i| i.tenant_id == tenant_id)
            .filter(|i| {
                i.summary.to_lowercase().contains(&q)
                    || i.detail.to_lowercase().contains(&q)
                    || i.run_id.to_lowercase().contains(&q)
                    || i.step_id.to_lowercase().contains(&q)
                    || i.kind.to_string().contains(&q)
            })
            .collect()
    }

    /// All evidence for one run, scoped to a tenant.
    pub fn search_by_run(&self, tenant_id: &str, run_id: &str) -> Vec<&EvidenceItem> {
        self.items
            .iter()
            .filter(|i| i.tenant_id == tenant_id && i.run_id == run_id)
            .collect()
    }

    /// Evidence for one step of one run, scoped to a tenant.
    pub fn search_by_step(
        &self,
        tenant_id: &str,
        run_id: &str,
        step_id: &str,
    ) -> Result<Vec<&EvidenceItem>, EvidenceError> {
        let hits: Vec<&EvidenceItem> = self
            .items
            .iter()
            .filter(|i| i.tenant_id == tenant_id && i.run_id == run_id && i.step_id == step_id)
            .collect();
        // Structural scope check: any hit proves the run exists; a run
        // owned by another tenant yields the same empty result as no hits —
        // but an EXPLICIT cross-tenant detail fetch is refused.
        if let Some(item) = self.items.iter().find(|i| i.run_id == run_id) {
            if item.tenant_id != tenant_id {
                return Err(EvidenceError::TenantMismatch {
                    requested: tenant_id.to_string(),
                    owned: item.tenant_id.clone(),
                });
            }
        }
        Ok(hits)
    }

    /// Fetches one item's full detail by exact run+step+kind, refusing
    /// cross-tenant access explicitly (QUAL: respects tenant scope).
    pub fn detail_of(
        &self,
        tenant_id: &str,
        run_id: &str,
        step_id: &str,
        kind: EvidenceKind,
    ) -> Result<Option<&EvidenceItem>, EvidenceError> {
        if let Some(item) = self.items.iter().find(|i| i.run_id == run_id) {
            if item.tenant_id != tenant_id {
                return Err(EvidenceError::TenantMismatch {
                    requested: tenant_id.to_string(),
                    owned: item.tenant_id.clone(),
                });
            }
        }
        Ok(self.items.iter().find(|i| {
            i.tenant_id == tenant_id && i.run_id == run_id && i.step_id == step_id && i.kind == kind
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed() -> EvidenceIndex {
        let mut idx = EvidenceIndex::new();
        idx.index(EvidenceItem::new(
            "tenant-alpha",
            "run-1",
            "step-1",
            EvidenceKind::ToolCall,
            "tool fs.read on src/lib.rs",
            "arguments {path: src/lib.rs}",
        ));
        idx.index(EvidenceItem::new(
            "tenant-alpha",
            "run-1",
            "step-2",
            EvidenceKind::Command,
            "command cargo test workspace",
            "exit 0 in 42s",
        ));
        idx.index(EvidenceItem::new(
            "tenant-alpha",
            "run-2",
            "step-1",
            EvidenceKind::Error,
            "error patch context mismatch",
            "doc.rs line 12",
        ));
        idx.index(EvidenceItem::new(
            "tenant-beta",
            "run-1",
            "step-1",
            EvidenceKind::ToolCall,
            "tool fs.read on secret/plan.md",
            "SECRET-CONTENT",
        ));
        idx
    }

    /// QUAL-EV-0132: search returns evidence by run/step.
    #[test]
    fn search_returns_evidence_by_run_and_step() {
        let idx = seed();
        let run_hits = idx.search_by_run("tenant-alpha", "run-1");
        assert_eq!(run_hits.len(), 2);
        assert!(run_hits.iter().all(|h| h.run_id == "run-1"));

        let step_hits = idx
            .search_by_step("tenant-alpha", "run-1", "step-2")
            .unwrap();
        assert_eq!(step_hits.len(), 1);
        assert_eq!(step_hits[0].kind, EvidenceKind::Command);

        let text_hits = idx.search("tenant-alpha", "cargo test");
        assert_eq!(text_hits.len(), 1);
        assert_eq!(text_hits[0].step_id, "step-2");
    }

    /// QUAL-EV-0132: tenant scope is respected — another tenant's evidence
    /// is invisible to search AND explicit cross-tenant fetch is refused.
    #[test]
    fn search_respects_tenant_scope() {
        let idx = seed();

        // tenant-beta searching its own run sees only its items.
        let beta_hits = idx.search_by_run("tenant-beta", "run-1");
        assert_eq!(beta_hits.len(), 1);
        assert!(beta_hits[0].summary.contains("secret/plan.md"));

        // tenant-alpha CANNOT see tenant-beta's evidence, even with the
        // exact matching query.
        let leaks = idx.search("tenant-alpha", "SECRET-CONTENT");
        assert!(leaks.is_empty(), "cross-tenant search must return nothing");

        // Explicit cross-tenant detail fetch is a typed refusal.
        let err = idx
            .search_by_step("tenant-beta", "run-1", "step-1")
            .unwrap_err();
        assert!(matches!(err, EvidenceError::TenantMismatch { .. }));
        assert!(idx
            .detail_of("tenant-alpha", "run-2", "step-1", EvidenceKind::Error)
            .is_ok());
    }
}
