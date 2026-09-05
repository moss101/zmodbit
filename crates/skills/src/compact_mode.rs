//! Compact skill mode (M5, REQ-EV-0213): only task-relevant skill
//! instructions and resources enter the prompt; large references
//! lazy-load. The token benchmark compares the EAGER package (everything
//! inlined) against the COMPILED projection.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A skill package section.
#[derive(Clone, Debug)]
pub struct SkillSection {
    pub name: String,
    pub text: String,
    /// Tags matched against the task to decide relevance.
    pub tags: Vec<String>,
    /// Large reference material: never eager, lazy-load only.
    pub reference: bool,
}

/// Compiles the compact projection of a skill for a task.
pub struct SkillCompiler;

impl SkillCompiler {
    /// Eager cost: the whole package (what selective context avoids).
    pub fn eager_bytes(sections: &[SkillSection]) -> usize {
        sections.iter().map(|s| s.text.len()).sum()
    }

    /// Compiled projection: only task-relevant, non-reference sections.
    pub fn compile(sections: &[SkillSection], task_tags: &[&str]) -> Vec<(String, String)> {
        sections
            .iter()
            .filter(|s| !s.reference)
            .filter(|s| {
                task_tags.is_empty() || s.tags.iter().any(|t| task_tags.contains(&t.as_str()))
            })
            .map(|s| (s.name.clone(), s.text.clone()))
            .collect()
    }

    /// Lazy hydration of a large reference, on explicit request.
    pub fn hydrate_reference<'a>(sections: &'a [SkillSection], name: &str) -> Option<&'a str> {
        sections
            .iter()
            .find(|s| s.reference && s.name == name)
            .map(|s| s.text.as_str())
    }
}

/// The token benchmark (QUAL-EV-0213): compiled projection must cost far
/// less than the eager package on a realistically large skill.
pub fn token_benchmark(sections: &[SkillSection], task_tags: &[&str]) -> (usize, usize) {
    let eager = SkillCompiler::eager_bytes(sections);
    let compiled: usize = SkillCompiler::compile(sections, task_tags)
        .iter()
        .map(|(_, t)| t.len())
        .sum();
    (eager, compiled)
}

/// Resource-indexed storage for lazy references (digest keyed).
#[derive(Default)]
pub struct ResourceStore {
    pub resources: BTreeMap<String, String>,
}

impl ResourceStore {
    pub fn put(&mut self, name: &str, text: &str) -> String {
        let digest = {
            use sha2::{Digest, Sha256};
            let mut hasher = Sha256::new();
            hasher.update(text.as_bytes());
            format!("{:x}", hasher.finalize())
        };
        self.resources.insert(name.to_string(), text.to_string());
        digest
    }

    pub fn load(&self, name: &str) -> Option<&str> {
        self.resources.get(name).map(|s| s.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn large_skill() -> Vec<SkillSection> {
        vec![
            SkillSection {
                name: "overview".into(),
                text: "Deploy the service with bounded retries.".into(),
                tags: vec!["deploy".to_string()],
                reference: false,
            },
            SkillSection {
                name: "runbook".into(),
                text: "step ".repeat(400),
                tags: vec![],
                reference: true,
            },
            SkillSection {
                name: "api-reference".into(),
                text: "endpoint ".repeat(400),
                tags: vec![],
                reference: true,
            },
            SkillSection {
                name: "rollback".into(),
                text: "Roll back by redeploying the previous tag.".into(),
                tags: vec!["deploy".to_string(), "rollback".to_string()],
                reference: false,
            },
        ]
    }

    /// QUAL-EV-0213: the token benchmark compares the eager package vs
    /// the compiled skill projection.
    #[test]
    fn compiled_projection_beats_eager_package() {
        let sections = large_skill();
        let (eager, compiled) = token_benchmark(&sections, &["deploy"]);
        assert!(
            compiled * 4 < eager,
            "compiled {compiled}B must be far below eager {eager}B"
        );

        // Only deploy-relevant sections in the projection.
        let projected = SkillCompiler::compile(&sections, &["deploy"]);
        assert_eq!(projected.len(), 2, "overview + rollback only");
        assert!(projected.iter().all(|(n, _)| n != "runbook"));

        // The large references lazy-load on demand.
        let runbook = SkillCompiler::hydrate_reference(&sections, "runbook");
        assert!(runbook.unwrap().starts_with("step step"));
    }

    /// Resource store: put/load round trip.
    #[test]
    fn resource_store_round_trips() {
        let mut store = ResourceStore::default();
        let digest = store.put("runbook.md", "the runbook text");
        assert_eq!(store.load("runbook.md"), Some("the runbook text"));
        assert_eq!(digest.len(), 64);
    }
}
