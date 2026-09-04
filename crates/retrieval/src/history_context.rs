//! Subordinate historical context (M3, REQ-EV-0158) and freshness/relevance
//! ranking (REQ-EV-0159). Git history and blame are context, but CURRENT
//! source is the only authority: a historical note that contradicts the
//! active revision is marked subordinate and can never override it.
//! Knowledge items carry revision validity; deprecated documents are
//! downranked once their source has moved.

use serde::{Deserialize, Serialize};

/// A note derived from git history/blame.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HistoricalNote {
    pub path: String,
    pub commit: String,
    pub note: String,
    /// sha256 of the file AT THAT COMMIT.
    pub file_sha256_at_commit: String,
}

/// The blended context for one file: current truth plus subordinate
/// history.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ContextBlend {
    pub path: String,
    /// Current source bytes — AUTHORITY.
    pub current_sha256: String,
    pub current_excerpt: String,
    /// Historical notes, each marked with whether it still matches the
    /// current file.
    pub notes: Vec<(HistoricalNote, bool)>, // (note, still_accurate)
}

impl ContextBlend {
    /// Blends history with current source. CURRENT SOURCE ALWAYS WINS: a
    /// note whose file digest no longer matches is `still_accurate=false`
    /// (subordinate, never overriding).
    pub fn blend(
        path: &str,
        current_sha256: &str,
        current_excerpt: &str,
        notes: Vec<HistoricalNote>,
    ) -> Self {
        let notes = notes
            .into_iter()
            .map(|note| {
                let still_accurate = note.file_sha256_at_commit == current_sha256;
                (note, still_accurate)
            })
            .collect();
        Self {
            path: path.to_string(),
            current_sha256: current_sha256.to_string(),
            current_excerpt: current_excerpt.to_string(),
            notes,
        }
    }

    /// The authority statement for a consumer: current source, with notes
    /// explicitly subordinate.
    pub fn authority_line(&self) -> String {
        format!(
            "authority: current source ({:.12}); {} historical note(s) subordinate",
            self.current_sha256,
            self.notes.iter().filter(|(_, ok)| !ok).count()
        )
    }
}

// ---------------------------------------------------------------------------
// Freshness/relevance ranking (REQ-EV-0159)
// ---------------------------------------------------------------------------

/// A knowledge item with revision validity.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RankedKnowledge {
    pub path: String,
    pub base_score: f64,
    /// True when the item's revision matches the current head.
    pub revision_valid: bool,
    /// Effective score after freshness adjustment.
    pub score: f64,
}

/// Freshness-aware ranking: items whose revision lags the current head
/// are DOWNRANKED (penalized), so a deprecated document sinks below
/// current knowledge while remaining visible as history.
pub fn rank_with_freshness(
    items: &[(String, f64, bool)], // (path, base_score, revision_valid)
) -> Vec<RankedKnowledge> {
    const STALE_PENALTY: f64 = 0.4;
    let mut ranked: Vec<RankedKnowledge> = items
        .iter()
        .map(|(path, base_score, revision_valid)| {
            let score = if *revision_valid {
                *base_score
            } else {
                *base_score * STALE_PENALTY
            };
            RankedKnowledge {
                path: path.clone(),
                base_score: *base_score,
                revision_valid: *revision_valid,
                score,
            }
        })
        .collect();
    ranked.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    ranked
}

#[cfg(test)]
mod tests {
    use super::*;

    /// QUAL-EV-0158: an old commit cannot override current code truth.
    #[test]
    fn old_commit_never_overrides_current_source() {
        let note = HistoricalNote {
            path: "src/lib.rs".into(),
            commit: "abc1234".into(),
            note: "parse_config returned u32 at this commit".into(),
            file_sha256_at_commit: "sha-old".into(),
        };
        // Current source has since changed (different digest).
        let blend = ContextBlend::blend(
            "src/lib.rs",
            "sha-current",
            "pub fn parse_config() -> Config { /* new behavior */ }",
            vec![note],
        );
        // The historical note is subordinate...
        assert!(!blend.notes[0].1, "old commit marked not-accurate");
        // ...and the authority line names CURRENT source.
        let authority = blend.authority_line();
        assert!(authority.contains("current source"));
        assert!(authority.contains("subordinate"));
        // Current excerpt is carried as the authority content.
        assert!(blend.current_excerpt.contains("new behavior"));
    }

    /// QUAL-EV-0159: a deprecated document is downranked after its source
    /// changes.
    #[test]
    fn deprecated_doc_downranked_after_source_change() {
        // The deprecated doc had the HIGHEST base score.
        let items = vec![
            ("docs/deprecated-design.md".to_string(), 0.9, false),
            ("docs/current-design.md".to_string(), 0.7, true),
            ("src/lib.rs".to_string(), 0.5, true),
        ];
        let ranked = rank_with_freshness(&items);
        // The stale doc sinks below the current ones despite base score.
        assert_eq!(ranked[0].path, "docs/current-design.md");
        assert_eq!(ranked[1].path, "src/lib.rs");
        assert_eq!(ranked.last().unwrap().path, "docs/deprecated-design.md");
        assert!(!ranked.last().unwrap().revision_valid);
        assert_eq!(
            ranked.last().unwrap().score,
            0.9 * 0.4,
            "stale penalty applied to base score"
        );
    }
}
