//! Per-model request settings (Future-tasks Phase 2 item 2): the hot
//! path's `max_output_tokens` / `temperature` / reasoning-effort defaults
//! resolved from the model name, overridable by the scheduler's env
//! configuration. This is the seed of the production model profile
//! table; when IMP-EV-0028's `ModelCapability` catalog gains a production
//! roster these defaults move there (same shape, richer data).

use crate::envelope::ReasoningEffort;

/// Request knobs the one-agent loop needs per model.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ModelSettings {
    pub max_output_tokens: u32,
    pub temperature: f32,
    /// None = the reasoning parameter is never sent (models without
    /// reasoning support reject unknown parameters).
    pub reasoning_effort: Option<ReasoningEffort>,
}

impl Default for ModelSettings {
    fn default() -> Self {
        Self::BASE
    }
}

impl ModelSettings {
    pub const BASE: ModelSettings = ModelSettings {
        max_output_tokens: 4096,
        temperature: 0.2,
        reasoning_effort: None,
    };
}

/// Resolves per-model defaults by name pattern. Conservative: unknown
/// models get BASE; effort is never defaulted on (opt-in only).
pub fn resolve_model_settings(model: &str) -> ModelSettings {
    let lower = model.to_ascii_lowercase();
    let large_output = lower.contains("gpt-5")
        || lower.contains("o3")
        || lower.contains("o4")
        || lower.contains("claude")
        || lower.contains("glm-4")
        || lower.contains("glm-5");
    if large_output {
        ModelSettings {
            max_output_tokens: 8192,
            ..ModelSettings::BASE
        }
    } else {
        ModelSettings::BASE
    }
}

/// Parses a reasoning-effort env value (minimal|low|medium|high).
pub fn parse_reasoning_effort(value: &str) -> Option<ReasoningEffort> {
    match value.trim().to_ascii_lowercase().as_str() {
        "minimal" => Some(ReasoningEffort::Minimal),
        "low" => Some(ReasoningEffort::Low),
        "medium" => Some(ReasoningEffort::Medium),
        "high" => Some(ReasoningEffort::High),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_settings_are_conservative() {
        let s = resolve_model_settings("gpt-4o-mini");
        assert_eq!(s, ModelSettings::BASE);
        assert_eq!(s.max_output_tokens, 4096);
        assert_eq!(s.reasoning_effort, None, "effort is opt-in only");
    }

    #[test]
    fn reasoning_tier_models_get_larger_output_budgets() {
        for model in ["gpt-5", "gpt-5-mini", "o3-mini", "claude-sonnet-4", "GLM-5.3-Flash"] {
            assert_eq!(
                resolve_model_settings(model).max_output_tokens,
                8192,
                "{model}"
            );
        }
    }

    #[test]
    fn effort_parsing_covers_the_documented_values() {
        assert_eq!(
            parse_reasoning_effort("high"),
            Some(ReasoningEffort::High)
        );
        assert_eq!(parse_reasoning_effort(" Medium "), Some(ReasoningEffort::Medium));
        assert_eq!(parse_reasoning_effort("bogus"), None);
        assert_eq!(parse_reasoning_effort(""), None);
    }
}
