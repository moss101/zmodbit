//! Dynamic context query planning (M3, REQ-EV-0001) and signature-only
//! retrieval stubs (REQ-EV-0003). The planner classifies query intent and
//! escalates through retrieval levels ONLY when required:
//!   L0 exact → L1 hybrid (exact+path) → L2 structural → L3 engineering.
//! Lower-ranked candidates contribute signature stubs, hydrated lazily
//! under an explicit byte budget, with provenance preserved throughout.

use serde::{Deserialize, Serialize};

/// Retrieval escalation levels (docs/18 § query planning).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetrievalLevel {
    /// L0: exact term/identifier match.
    Exact,
    /// L1: hybrid — exact terms + path signals.
    Hybrid,
    /// L2: structural — symbols, definitions, call shapes.
    Structural,
    /// L3: engineering — architectural/behavioral, spans many files.
    Engineering,
}

/// Observed signals from a cheap pre-scan of the query.
#[derive(Clone, Debug, Default)]
pub struct QuerySignals {
    /// The query contains a verbatim identifier found in the index.
    pub exact_hit: bool,
    /// Multiple distinct search terms.
    pub multi_term: bool,
    /// Query names a structural shape (fn/struct/trait/impl/call).
    pub structural: bool,
    /// Query is architectural/behavioral ("how does X flow", "why").
    pub engineering: bool,
    /// Exact-level recall was insufficient (few/empty hits).
    pub exact_recall_insufficient: bool,
}

/// The compiled retrieval plan.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RetrievalPlan {
    pub level: RetrievalLevel,
    pub rationale: String,
}

/// Classifies the query intent into the MINIMUM sufficient level —
/// escalation happens only when lower levels cannot serve the query
/// (REQ-EV-0001).
pub fn plan(signals: &QuerySignals) -> RetrievalPlan {
    let (level, rationale) = if signals.engineering || signals.exact_recall_insufficient {
        (
            RetrievalLevel::Engineering,
            "architectural intent or insufficient exact recall — engineering-level context required",
        )
    } else if signals.structural {
        (
            RetrievalLevel::Structural,
            "query names a structural shape — symbol/definition retrieval",
        )
    } else if signals.multi_term || !signals.exact_hit {
        (
            RetrievalLevel::Hybrid,
            "multiple terms or unconfirmed identifier — hybrid exact+path search",
        )
    } else {
        (
            RetrievalLevel::Exact,
            "verbatim identifier hit — exact retrieval suffices",
        )
    };
    RetrievalPlan {
        level,
        rationale: rationale.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Signature-only stubs (REQ-EV-0003)
// ---------------------------------------------------------------------------

/// A signature stub for a lower-ranked candidate: symbol shape without the
/// body, with full provenance.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SignatureStub {
    pub path: String,
    pub line_no: usize,
    pub signature: String,
    /// Content digest of the source the signature was extracted from.
    pub sha256: String,
    pub revision: u64,
}

/// A candidate provisioned into a context pack.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Provision {
    /// Full bytes hydrated (top-ranked within budget).
    Hydrated {
        path: String,
        sha256: String,
        bytes: String,
    },
    /// Signature-only stub (lower-ranked; lazy hydration available).
    Stub(SignatureStub),
}

/// The byte budget for a context pack.
#[derive(Clone, Copy, Debug)]
pub struct ContextBudget {
    pub max_bytes: usize,
    /// Bytes one stub hydration costs when promoted.
    pub stub_hydration_bytes: usize,
}

/// Packs candidates under a budget: full hydration for as many top-ranked
/// candidates as fit, signature stubs for the rest. Hydration happens ONLY
/// at pack time for top candidates — stubs hydrate on explicit request.
/// Every provision carries provenance (path + digest).
pub fn pack(
    ranked: &[(String, String, u64)], // (path, bytes, revision) in rank order
    digests: &[String],               // sha256 per candidate, same order
    budget: ContextBudget,
) -> Vec<Provision> {
    let mut provisions = Vec::new();
    let mut used = 0usize;
    for ((path, bytes, revision), digest) in ranked.iter().zip(digests.iter()) {
        if used + bytes.len() <= budget.max_bytes {
            used += bytes.len();
            provisions.push(Provision::Hydrated {
                path: path.clone(),
                sha256: digest.clone(),
                bytes: bytes.clone(),
            });
        } else {
            // Stub: signature line only, one hydration unit when promoted.
            let signature: String = bytes
                .lines()
                .find(|l| l.starts_with("fn ") || l.starts_with("pub ") || l.starts_with("struct "))
                .unwrap_or("")
                .to_string();
            used += budget.stub_hydration_bytes;
            provisions.push(Provision::Stub(SignatureStub {
                path: path.clone(),
                line_no: 1,
                signature,
                sha256: digest.clone(),
                revision: *revision,
            }));
        }
    }
    provisions
}

#[cfg(test)]
mod tests {
    use super::*;

    /// QUAL-EV-0001: the planner escalates only when required.
    #[test]
    fn planner_escalates_only_when_required() {
        // Verbatim identifier hit: L0.
        let result = plan(&QuerySignals {
            exact_hit: true,
            ..Default::default()
        });
        assert_eq!(result.level, RetrievalLevel::Exact);

        // Multiple terms: L1.
        let result = plan(&QuerySignals {
            exact_hit: false,
            multi_term: true,
            ..Default::default()
        });
        assert_eq!(result.level, RetrievalLevel::Hybrid);

        // Structural shape: L2.
        let result = plan(&QuerySignals {
            structural: true,
            ..Default::default()
        });
        assert_eq!(result.level, RetrievalLevel::Structural);

        // Engineering intent or failed exact recall: L3.
        for signals in [
            QuerySignals {
                engineering: true,
                ..Default::default()
            },
            QuerySignals {
                exact_hit: true,
                exact_recall_insufficient: true,
                ..Default::default()
            },
        ] {
            let result = plan(&signals);
            assert_eq!(result.level, RetrievalLevel::Engineering);
        }
    }

    /// QUAL-EV-0003: the context budget proves hydration occurs only when
    /// requested, and provenance survives for stubs.
    #[test]
    fn budget_limits_hydration_and_provenance_survives() {
        let ranked = vec![
            (
                "src/big.rs".to_string(),
                "fn big() {\n    body();\n}".to_string(),
                9,
            ),
            (
                "src/small_a.rs".to_string(),
                "pub fn small_a() {}".to_string(),
                9,
            ),
            (
                "src/small_b.rs".to_string(),
                "struct SmallB;".to_string(),
                9,
            ),
        ];
        let digests = vec!["d1".into(), "d2".into(), "d3".into()];
        let budget = ContextBudget {
            max_bytes: "fn big() {\n    body();\n}".len() + 4,
            stub_hydration_bytes: 32,
        };

        let provisions = pack(&ranked, &digests, budget);
        assert!(matches!(&provisions[0], Provision::Hydrated { path, .. } if path == "src/big.rs"));
        // Lower-ranked candidates stayed STUBS (no full bytes in context).
        assert!(
            matches!(&provisions[1], Provision::Stub(s) if s.path == "src/small_a.rs" && !s.signature.is_empty())
        );
        // Provenance survived for every provision.
        for (i, provision) in provisions.iter().enumerate() {
            let sha = match provision {
                Provision::Hydrated { sha256, .. } => sha256,
                Provision::Stub(s) => &s.sha256,
            };
            assert_eq!(sha, &digests[i], "provenance digest survived");
        }

        // Hydrating a stub is an explicit act — modeled by the caller
        // swapping the stub for hydrated bytes; the pack never did it.
        assert!(provisions.len() == 3);
    }
}
