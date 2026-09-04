//! Transcript compaction with recoverability (M3, REQ-EV-0092): the
//! model-visible transcript may be compacted repeatedly, but the
//! CANONICAL event log is lossless and append-only. Restart after any
//! number of compactions reconstructs the exact task/protocol state by
//! replaying canonical events — compaction never loses truth.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// One canonical, durable event. Immutable, append-only, seq-ordered.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CanonicalEvent {
    pub seq: u64,
    pub kind: String,
    pub payload: String,
}

/// A compaction applied to the model-visible view: covers canonical
/// events through `through_seq`, replaced by a summary. Canonical log is
/// NOT mutated.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TranscriptCompaction {
    pub through_seq: u64,
    pub summary: String,
}

/// The session transcript: durable canonical log + compacted view.
#[derive(Default)]
pub struct Transcript {
    pub canonical: Vec<CanonicalEvent>,
    pub compactions: Vec<TranscriptCompaction>,
}

impl Transcript {
    /// Appends a canonical event (the ONLY way events enter).
    pub fn append(&mut self, kind: &str, payload: &str) -> u64 {
        let seq = self.canonical.len() as u64;
        self.canonical.push(CanonicalEvent {
            seq,
            kind: kind.to_string(),
            payload: payload.to_string(),
        });
        seq
    }

    /// Applies a model-visible compaction (does not touch canonical).
    pub fn compact(&mut self, through_seq: u64, summary: &str) {
        self.compactions.push(TranscriptCompaction {
            through_seq,
            summary: summary.to_string(),
        });
    }

    /// The state digest of the CANONICAL log — identical before and after
    /// any number of compactions, and after a restart+replay.
    pub fn canonical_digest(&self) -> String {
        let mut hasher = Sha256::new();
        for event in &self.canonical {
            hasher.update(event.seq.to_le_bytes());
            hasher.update(event.kind.as_bytes());
            hasher.update(event.payload.as_bytes());
        }
        format!("{:x}", hasher.finalize())
    }

    /// RESTART: rebuilds the transcript from durable canonical events
    /// alone (compactions are re-derivable, not required for truth).
    pub fn replay(canonical: Vec<CanonicalEvent>) -> Self {
        Self {
            canonical,
            compactions: Vec::new(),
        }
    }

    /// The model-visible view: summaries for compacted ranges + verbatim
    /// events after the last compaction boundary.
    pub fn visible(&self) -> Vec<String> {
        let mut view = Vec::new();
        let mut next_seq = 0u64;
        for compaction in &self.compactions {
            view.push(format!(
                "[summary through seq {}] {}",
                compaction.through_seq, compaction.summary
            ));
            next_seq = compaction.through_seq + 1;
        }
        for event in &self.canonical {
            if event.seq >= next_seq {
                view.push(format!("{}: {}", event.kind, event.payload));
            }
        }
        view
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_transcript() -> Transcript {
        let mut t = Transcript::default();
        for i in 0..20 {
            t.append("tool_call", &format!("call-{i}"));
        }
        t
    }

    /// QUAL-EV-0092: restart after MULTIPLE compactions reconstructs the
    /// exact task/protocol state — canonical truth is lossless.
    #[test]
    fn restart_after_multiple_compactions_reconstructs_exact_state() {
        let mut transcript = build_transcript();
        let digest_before = transcript.canonical_digest();

        // Compact twice (model-visible only).
        transcript.compact(7, "early tool calls compressed");
        transcript.compact(14, "middle tool calls compressed");
        assert_eq!(transcript.compactions.len(), 2);

        // Canonical digest UNCHANGED by compaction.
        assert_eq!(
            transcript.canonical_digest(),
            digest_before,
            "compaction must not touch canonical truth"
        );

        // RESTART: durable log replays into an identical state.
        let durable = transcript.canonical.clone();
        let restarted = Transcript::replay(durable);
        assert_eq!(
            restarted.canonical_digest(),
            digest_before,
            "restart reconstructs exact state"
        );
        assert_eq!(restarted.canonical.len(), 20);
        assert_eq!(
            restarted.canonical[13],
            CanonicalEvent {
                seq: 13,
                kind: "tool_call".into(),
                payload: "call-13".into()
            }
        );

        // The pre-restart model-visible view reflects both compactions.
        let view = transcript.visible();
        assert!(view[0].contains("early tool calls"));
        assert!(view[1].contains("middle tool calls"));
        assert!(view.last().unwrap().contains("call-19"));

        // The restarted view is full verbatim (compactions are a
        // re-derivable projection, not part of truth): nothing is lost.
        let restarted_view = restarted.visible();
        assert_eq!(restarted_view.len(), 20);
        assert!(restarted_view[0].contains("call-0"));
    }
}
