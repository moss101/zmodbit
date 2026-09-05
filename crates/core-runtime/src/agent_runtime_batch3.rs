//! Agent runtime batch 3 (M6): command failure ≠ turn failure
//! (REQ-EV-0099), bounded failure evidence for repair (REQ-EV-0107),
//! specialized subagents with tool/model profiles (REQ-EV-0178),
//! background follow-up continuation (REQ-EV-0179), and
//! extension-provided subagent profiles (REQ-EV-0182).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

// ---------------------------------------------------------------------------
// Command failure ≠ turn failure (REQ-EV-0099) + repair evidence
// (REQ-EV-0107)
// ---------------------------------------------------------------------------

/// A typed command result: nonzero exit is a RESULT, not a turn crash.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CommandResult {
    pub exec_id: String,
    pub exit_code: i64,
    /// Full raw output retained (raw log is never lost).
    pub raw_output: String,
    /// Bounded failure evidence delivered to the model for the repair
    /// round (tail of the raw log with provenance).
    pub bounded_failure: Option<BoundedFailure>,
}

/// Bounded failure evidence for the next repair round.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BoundedFailure {
    pub excerpt: String,
    pub raw_sha256: String,
    /// Raw byte length — proving the raw log still exists untruncated.
    pub raw_len: usize,
}

/// Runs a command result through the repair loop semantics: nonzero exit
/// produces bounded evidence and keeps the turn alive.
pub fn feed_failure_for_repair(exec_id: &str, exit_code: i64, raw_output: &str) -> CommandResult {
    let bounded_failure = if exit_code != 0 {
        let tail_start = raw_output.len().saturating_sub(2000);
        // Snap to a line boundary so the excerpt stays coherent.
        let excerpt = raw_output[tail_start..]
            .trim_start_matches(|c| c != '\n')
            .to_string();
        Some(BoundedFailure {
            excerpt,
            raw_sha256: crate::agent_runtime_batch2::sha256_hex(raw_output.as_bytes()),
            raw_len: raw_output.len(),
        })
    } else {
        None
    };
    CommandResult {
        exec_id: exec_id.to_string(),
        exit_code,
        raw_output: raw_output.to_string(),
        bounded_failure,
    }
}

// ---------------------------------------------------------------------------
// Specialized subagents (REQ-EV-0178)
// ---------------------------------------------------------------------------

/// The tool/model profile selecting a subagent's bounded surface.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentProfileSpec {
    pub kind: &'static str, // coder | explorer | reviewer
    pub tools: Vec<String>,
    pub model: String,
    pub domain_context: Vec<String>,
}

/// A launched specialized child: independent state, bounded tools.
#[derive(Clone, Debug, PartialEq)]
pub struct SpecializedChild {
    pub agent_id: String,
    pub kind: &'static str,
    pub model: String,
    pub allowed_tools: Vec<String>,
    pub state: BTreeMap<String, String>,
}

/// Launches a specialized child: its tool ceiling comes ONLY from the
/// profile — two children of the same parent have independent state and
/// disjoint ceilings.
pub fn launch_specialized(agent_id: &str, profile: &AgentProfileSpec) -> SpecializedChild {
    SpecializedChild {
        agent_id: agent_id.to_string(),
        kind: profile.kind,
        model: profile.model.clone(),
        allowed_tools: profile.tools.clone(),
        state: BTreeMap::new(),
    }
}

// ---------------------------------------------------------------------------
// Background follow-up continuation (REQ-EV-0179)
// ---------------------------------------------------------------------------

/// A completed background child remains addressable; a follow-up is a
/// typed continuation with prior output as evidence.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FollowUp {
    pub parent_agent_id: String,
    pub prior_output_sha256: String,
    pub instruction: String,
}

pub fn send_follow_up(parent_agent_id: &str, prior_output: &str, instruction: &str) -> FollowUp {
    FollowUp {
        parent_agent_id: parent_agent_id.to_string(),
        prior_output_sha256: crate::agent_runtime_batch2::sha256_hex(prior_output.as_bytes()),
        instruction: instruction.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Extension-provided subagent profiles (REQ-EV-0182)
// ---------------------------------------------------------------------------

/// An imported declarative profile.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ImportedProfile {
    pub name: String,
    pub tools: Vec<String>,
    pub model: String,
}

#[derive(Debug)]
pub enum ImportOutcome {
    /// Valid profile imported as-is.
    Imported(ImportedProfile),
    /// Unsafe tool declaration: imported NARROWED to the safe subset.
    Narrowed {
        profile: ImportedProfile,
        removed: Vec<String>,
    },
}

/// Imports an extension profile into the canonical schema. Unsafe tool
/// declarations (`shell.*`, anything outside the allowlist) are narrowed
/// out; a profile left with no safe tools is REJECTED.
pub fn import_profile(
    name: &str,
    tools: &[String],
    model: &str,
    safe_allowlist: &[String],
) -> Result<ImportOutcome, String> {
    if name.trim().is_empty() {
        return Err("profile name must not be empty".into());
    }
    let mut removed = Vec::new();
    let mut safe = Vec::new();
    for tool in tools {
        if safe_allowlist.iter().any(|a| a == tool) {
            safe.push(tool.clone());
        } else {
            removed.push(tool.clone());
        }
    }
    if safe.is_empty() {
        return Err(format!(
            "profile {name:?} declares no safe tools — rejected"
        ));
    }
    let profile = ImportedProfile {
        name: name.to_string(),
        tools: safe,
        model: model.to_string(),
    };
    if removed.is_empty() {
        Ok(ImportOutcome::Imported(profile))
    } else {
        Ok(ImportOutcome::Narrowed { profile, removed })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// QUAL-EV-0099 + QUAL-EV-0107: a compile failure is a typed result
    /// that feeds the repair loop in the same run — raw log retained,
    /// model receives bounded evidence.
    #[test]
    fn command_failure_feeds_repair_without_crashing_turn() {
        let raw = "warning: unused\nerror[E0308]: mismatched types\n  --> src/lib.rs:42\n";
        let result = feed_failure_for_repair("exec-9", 101, raw);
        assert_eq!(result.exit_code, 101, "nonzero exit is a typed result");
        let failure = result.bounded_failure.as_ref().expect("failure evidence");
        assert!(failure.excerpt.contains("E0308"), "model sees the error");
        assert_eq!(failure.raw_len, raw.len(), "raw log length preserved");
        assert_eq!(failure.raw_sha256.len(), 64, "raw log digest provenance");

        // Success: no failure evidence.
        let ok = feed_failure_for_repair("exec-10", 0, "all good");
        assert!(ok.bounded_failure.is_none());
    }

    /// QUAL-EV-0178: two real specialized children with independent
    /// state and disjoint tool ceilings.
    #[test]
    fn specialized_children_have_independent_state_and_ceilings() {
        let coder = AgentProfileSpec {
            kind: "coder",
            tools: vec!["tools.fs.write".into(), "tools.git.commit".into()],
            model: "coder-model".into(),
            domain_context: vec!["repo layout".into()],
        };
        let explorer = AgentProfileSpec {
            kind: "explorer",
            tools: vec!["tools.fs.read".into()],
            model: "fast-model".into(),
            domain_context: vec!["module map".into()],
        };
        let c = launch_specialized("child-coder", &coder);
        let e = launch_specialized("child-explorer", &explorer);

        // Disjoint ceilings: the coder may write; the explorer may not.
        assert!(c.allowed_tools.contains(&"tools.fs.write".to_string()));
        assert!(!e.allowed_tools.contains(&"tools.fs.write".to_string()));
        // Independent models.
        assert_ne!(c.model, e.model);
        // Independent state: writing into one does not touch the other.
        let mut c2 = c.clone();
        c2.state.insert("owns".into(), "coder-state".into());
        assert!(e.state.is_empty());
        let _ = c2;
    }

    /// QUAL-EV-0179: complete a child, "restart" the parent, send a
    /// typed follow-up — prior output is bound by digest.
    #[test]
    fn follow_up_binds_prior_output_by_digest() {
        let prior_output = "research results: 42 findings";
        let follow = send_follow_up("research-1", prior_output, "summarize finding 7");
        assert_eq!(follow.parent_agent_id, "research-1");
        assert_eq!(
            follow.prior_output_sha256,
            crate::agent_runtime_batch2::sha256_hex(prior_output.as_bytes())
        );
        assert!(follow.instruction.contains("finding 7"));
    }

    /// QUAL-EV-0182: unsafe tool declarations are narrowed; fully unsafe
    /// profiles are rejected.
    #[test]
    fn extension_profiles_narrow_or_reject_unsafe_tools() {
        let allowlist = vec!["tools.fs.read".to_string(), "tools.web.fetch".to_string()];

        // Mixed: unsafe tool narrowed out, safe ones kept.
        let outcome = import_profile(
            "mixed-agent",
            &["tools.fs.read".into(), "tools.shell.run".into()],
            "m",
            &allowlist,
        )
        .unwrap();
        match outcome {
            ImportOutcome::Narrowed { profile, removed } => {
                assert_eq!(removed, vec!["tools.shell.run".to_string()]);
                assert_eq!(profile.tools, vec!["tools.fs.read".to_string()]);
            }
            other => panic!("expected narrowed, got {other:?}"),
        }

        // Fully unsafe: rejected.
        assert!(
            import_profile("dangerous", &["tools.shell.run".into()], "m", &allowlist,).is_err()
        );
        // Empty name: rejected.
        assert!(import_profile("", &["tools.fs.read".into()], "m", &allowlist).is_err());
    }
}
