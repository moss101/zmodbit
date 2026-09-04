//! Editor context bridge (M3, REQ-EV-0141): selected/open review
//! artifacts and active file/symbol context CONTRIBUTE to the model's
//! context pack, but the bridge is structurally READ-ONLY — a review
//! selection can shape context and can never mutate the canonical source.

use serde::{Deserialize, Serialize};

/// The user's selection in a review artifact (1-based, inclusive).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Selection {
    pub artifact_path: String,
    pub start_line: usize,
    pub end_line: usize,
}

/// One open artifact (review file, notes) contributing context.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OpenArtifact {
    pub path: String,
    pub content_sha256: String,
}

/// The read-only bridge between editor/review surface and context.
#[derive(Clone, Debug, Default)]
pub struct EditorBridge {
    pub selection: Option<Selection>,
    pub open_artifacts: Vec<OpenArtifact>,
    /// Active symbol the cursor rests on.
    pub active_symbol: Option<String>,
}

impl EditorBridge {
    /// Sets the review selection (affects context composition only).
    pub fn select(&mut self, selection: Selection) {
        self.selection = Some(selection);
    }

    /// A mutation attempt through the bridge: REFUSED, always. The bridge
    /// has no write path to canonical sources — this exists so callers can
    /// prove refusal is the behavior, not an omission.
    pub fn apply_edit(&self, _path: &str, _new_bytes: &[u8]) -> Result<(), String> {
        Err("editor bridge is read-only: mutations go through the Change Engine with policy + review".to_string())
    }
}

/// The context contribution derived from the bridge (read-only slices for
/// the context pack).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BridgeContribution {
    pub selection_slice: Option<(String, usize, usize)>,
    pub open_artifact_paths: Vec<String>,
    pub active_symbol: Option<String>,
}

/// Derives the contribution from the bridge (REQ-EV-0141: selection
/// affects context).
pub fn contribute(bridge: &EditorBridge) -> BridgeContribution {
    BridgeContribution {
        selection_slice: bridge
            .selection
            .as_ref()
            .map(|s| (s.artifact_path.clone(), s.start_line, s.end_line)),
        open_artifact_paths: bridge
            .open_artifacts
            .iter()
            .map(|a| a.path.clone())
            .collect(),
        active_symbol: bridge.active_symbol.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// QUAL-EV-0141: a review selection affects context but cannot mutate
    /// canonical source.
    #[test]
    fn selection_affects_context_but_cannot_mutate() {
        let mut bridge = EditorBridge::default();
        assert!(contribute(&bridge).selection_slice.is_none());

        bridge.select(Selection {
            artifact_path: "reviews/pr-42.md".into(),
            start_line: 10,
            end_line: 24,
        });
        bridge.open_artifacts.push(OpenArtifact {
            path: "reviews/pr-42.md".into(),
            content_sha256: "abc".into(),
        });
        bridge.active_symbol = Some("WorkspaceFileService::replace".into());

        // The selection shapes the context contribution.
        let contribution = contribute(&bridge);
        assert_eq!(
            contribution.selection_slice,
            Some(("reviews/pr-42.md".into(), 10, 24))
        );
        assert_eq!(contribution.open_artifact_paths, vec!["reviews/pr-42.md"]);
        assert_eq!(
            contribution.active_symbol.as_deref(),
            Some("WorkspaceFileService::replace")
        );

        // A mutation through the bridge is REFUSED — canonical sources are
        // only writable via the Change Engine under policy + review.
        let err = bridge
            .apply_edit("reviews/pr-42.md", b"tampered")
            .unwrap_err();
        assert!(err.contains("read-only"));
    }
}
