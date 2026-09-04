//! Prompt Compiler (M2.7, docs/15 § Prompt cache economics).
//!
//! Assembles the canonical prompt as five STABLE, ORDERED segments:
//!   1. system/policy
//!   2. stable workspace rules / skill manifests
//!   3. compaction epoch summary
//!   4. task context pack
//!   5. recent events
//!
//! Cache keys = model + provider + compiler version + per-segment hashes.
//! Segment content is byte-stable for identical inputs, so provider prompt
//! caches hit on unchanged prefixes. Deterministic: same inputs → same
//! compiled prompt, same keys, always.
//!
//! Canonical owner subsystem: skills (docs/81 — prompt-compiler crate).
//! Layout: docs/12.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const COMPILER_VERSION: &str = "0.1.0";

/// The five canonical segments in compile order (docs/15).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PromptSegments {
    pub system_policy: String,
    pub workspace_rules: String,
    pub compaction_epoch: String,
    pub task_context_pack: String,
    pub recent_events: String,
}

/// A compiled prompt: ordered segments plus cache key material.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CompiledPrompt {
    pub cache_key: String,
    pub segment_hashes: [String; 5],
    pub compiled: String,
}

/// Inputs the host gathers; the compiler owns ordering and hashing.
#[derive(Clone, Debug, Default)]
pub struct CompilerInputs {
    pub model: String,
    pub provider: String,
    pub system_policy: String,
    pub workspace_rules: String,
    pub compaction_epoch: Option<String>,
    pub task_context_pack: String,
    pub recent_events: String,
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn segment_hash(content: &str) -> String {
    sha256_hex(content.as_bytes())
}

/// Compiles the canonical prompt. Deterministic: identical inputs yield a
/// byte-identical compiled prompt and identical segment hashes — that is
/// what makes provider prompt caches hit.
pub fn compile(inputs: &CompilerInputs) -> CompiledPrompt {
    let segments: [(&str, String); 5] = [
        ("1-system-policy", inputs.system_policy.clone()),
        ("2-workspace-rules", inputs.workspace_rules.clone()),
        (
            "3-compaction-epoch",
            inputs.compaction_epoch.clone().unwrap_or_default(),
        ),
        ("4-task-context-pack", inputs.task_context_pack.clone()),
        ("5-recent-events", inputs.recent_events.clone()),
    ];

    let mut compiled = String::new();
    let mut hashes: [String; 5] = Default::default();
    for (index, (name, content)) in segments.iter().enumerate() {
        hashes[index] = segment_hash(content);
        compiled.push_str(&format!("=== {name} ===\n{content}\n"));
    }

    // Cache key binds model/provider/compiler version and the chain of
    // segment hashes — a change anywhere invalidates the prefix.
    let mut hasher = Sha256::new();
    hasher.update(inputs.model.as_bytes());
    hasher.update(inputs.provider.as_bytes());
    hasher.update(COMPILER_VERSION.as_bytes());
    for h in &hashes {
        hasher.update(h.as_bytes());
    }
    let cache_key = format!("{:x}", hasher.finalize());

    CompiledPrompt {
        cache_key,
        segment_hashes: hashes,
        compiled,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs() -> CompilerInputs {
        CompilerInputs {
            model: "test-model".into(),
            provider: "openai".into(),
            system_policy: "policy text".into(),
            workspace_rules: "workspace rules".into(),
            compaction_epoch: Some("epoch summary".into()),
            task_context_pack: "context pack".into(),
            recent_events: "recent events".into(),
        }
    }

    #[test]
    fn compile_is_deterministic() {
        let a = compile(&inputs());
        let b = compile(&inputs());
        assert_eq!(a, b, "identical inputs must compile identically");
    }

    #[test]
    fn changing_any_segment_changes_the_cache_key() {
        let base = compile(&inputs());
        for (index, replacement) in ["policy v2", "rules v2", "epoch v2", "pack v2", "events v2"]
            .iter()
            .enumerate()
        {
            let mut modified = inputs();
            match index {
                0 => modified.system_policy = replacement.to_string(),
                1 => modified.workspace_rules = replacement.to_string(),
                2 => modified.compaction_epoch = Some(replacement.to_string()),
                3 => modified.task_context_pack = replacement.to_string(),
                4 => modified.recent_events = replacement.to_string(),
                _ => unreachable!(),
            }
            let compiled = compile(&modified);
            assert_ne!(
                base.cache_key, compiled.cache_key,
                "segment {index} change must invalidate the cache key"
            );
        }
    }

    #[test]
    fn compaction_epoch_is_optional() {
        let mut modified = inputs();
        modified.compaction_epoch = None;
        let compiled = compile(&modified);
        assert!(compiled.compiled.contains("=== 3-compaction-epoch ==="));
    }

    #[test]
    fn segments_appear_in_canonical_order() {
        let compiled = compile(&inputs());
        let order: Vec<usize> = [
            "1-system-policy",
            "2-workspace-rules",
            "3-compaction-epoch",
            "4-task-context-pack",
            "5-recent-events",
        ]
        .iter()
        .map(|name| compiled.compiled.find(&format!("=== {name} ===")).unwrap())
        .collect();
        let mut sorted = order.clone();
        sorted.sort();
        assert_eq!(order, sorted, "segments must appear in canonical order");
    }

    #[test]
    fn model_or_provider_change_invalidates_the_cache_key() {
        let base = compile(&inputs());
        let mut other = inputs();
        other.model = "other-model".into();
        assert_ne!(base.cache_key, compile(&other).cache_key);
        other.provider = "anthropic".into();
        assert_ne!(base.cache_key, compile(&other).cache_key);
    }
}
