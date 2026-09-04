//! Model routing (M2.6/M2.8-0031, docs/15 § Routing + § Health and
//! failover): capability catalog, hard policy/capability filters, bounded
//! fallback chains, enterprise policy, and decision records.
//!
//! Pure deterministic logic over a catalog — no live calls. The streaming
//! client (gateway.rs) executes the routed choice when credentials exist.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Typed capability catalog entry (REQ-EV-0028): context/output/tool/
/// parallel/vision/reasoning/structured-output/cost/latency/health.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelCapability {
    pub model: String,
    pub provider: String,
    pub context_window: u64,
    pub max_output_tokens: u64,
    pub supports_tools: bool,
    pub supports_parallel_tools: bool,
    pub supports_vision: bool,
    pub supports_reasoning: bool,
    pub supports_structured_output: bool,
    pub cost_per_1k_input: f64,
    pub cost_per_1k_output: f64,
    pub success_rate: f64,
    pub latency_class: String,
}

/// Hard requirements a task fingerprint places on the model.
#[derive(Clone, Debug, Default)]
pub struct TaskFingerprint {
    pub requires_tools: bool,
    pub requires_vision: bool,
    pub requires_reasoning: bool,
    pub requires_structured_output: bool,
    pub min_context_tokens: u64,
    pub min_output_tokens: u64,
    pub blocked_models: Vec<String>,
    pub blocked_providers: Vec<String>,
    pub required_model: Option<String>,
}

/// Enterprise policy constraints that cannot be weakened downstream
/// (REQ-EV-0031).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct EnterprisePolicy {
    pub blocked_models: Vec<String>,
    pub blocked_providers: Vec<String>,
    pub required_model: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RouteDecision {
    pub model: String,
    pub provider: String,
    pub exclusion_reasons: Vec<String>,
    pub decision_record: String,
}

/// Routes a task fingerprint to the best available model from the catalog.
/// Hard exclusions first (deterministic), then cost optimization.
/// Returns `Err` with the exclusion reasons when no model qualifies.
pub fn route(
    fingerprint: &TaskFingerprint,
    policy: &EnterprisePolicy,
    catalog: &[ModelCapability],
) -> Result<RouteDecision, Vec<String>> {
    let mut exclusions: Vec<String> = Vec::new();
    let mut candidates: Vec<&ModelCapability> = catalog.iter().collect();

    // Enterprise blocks (cannot be weakened downstream — REQ-EV-0031).
    candidates.retain(|c| {
        let blocked_model = policy.blocked_models.contains(&c.model);
        let blocked_provider = policy.blocked_providers.contains(&c.provider);
        if blocked_model {
            exclusions.push(format!("{}: blocked by enterprise policy (model)", c.model));
        }
        if blocked_provider {
            exclusions.push(format!(
                "{}: blocked by enterprise policy (provider)",
                c.provider
            ));
        }
        !blocked_model && !blocked_provider
    });

    // Context window.
    candidates.retain(|c| {
        let ok = c.context_window >= fingerprint.min_context_tokens;
        if !ok {
            exclusions.push(format!(
                "{}: context {} < required {}",
                c.model, c.context_window, fingerprint.min_context_tokens
            ));
        }
        ok
    });

    // Output tokens.
    candidates.retain(|c| {
        let ok = c.max_output_tokens >= fingerprint.min_output_tokens;
        if !ok {
            exclusions.push(format!(
                "{}: output {} < required {}",
                c.model, c.max_output_tokens, fingerprint.min_output_tokens
            ));
        }
        ok
    });

    if fingerprint.requires_tools {
        candidates.retain(|c| {
            let ok = c.supports_tools;
            if !ok {
                exclusions.push(format!("{}: no tool support", c.model));
            }
            ok
        });
    }

    if fingerprint.requires_vision {
        candidates.retain(|c| {
            let ok = c.supports_vision;
            if !ok {
                exclusions.push(format!("{}: no vision support", c.model));
            }
            ok
        });
    }

    if fingerprint.requires_reasoning {
        candidates.retain(|c| {
            let ok = c.supports_reasoning;
            if !ok {
                exclusions.push(format!("{}: no reasoning support", c.model));
            }
            ok
        });
    }

    if fingerprint.requires_structured_output {
        candidates.retain(|c| {
            let ok = c.supports_structured_output;
            if !ok {
                exclusions.push(format!("{}: no structured output", c.model));
            }
            ok
        });
    }

    // Enterprise: required model — only that model survives.
    if let Some(required) = &policy.required_model {
        candidates.retain(|c| {
            let ok = &c.model == required;
            if !ok {
                exclusions.push(format!(
                    "{}: not the enterprise-required model {required}",
                    c.model
                ));
            }
            ok
        });
    }

    if candidates.is_empty() {
        return Err(exclusions.clone());
    }

    // Deterministic: cheapest qualifying input cost, tie-break by name.
    candidates.sort_by(|a, b| {
        a.cost_per_1k_input
            .partial_cmp(&b.cost_per_1k_input)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.model.cmp(&b.model))
    });
    let excluded_count = exclusions.len();
    let winner = &candidates[0];
    Ok(RouteDecision {
        model: winner.model.clone(),
        provider: winner.provider.clone(),
        exclusion_reasons: exclusions,
        decision_record: format!(
            "routed to {} (provider {}) — cost-optimized from {} candidates, {} excluded",
            winner.model,
            winner.provider,
            catalog.len(),
            excluded_count
        ),
    })
}

/// Bounded fallback chain (REQ-EV-0030): profiles define an ordered list of
/// preferred/fallback models. The router walks the chain in order; the first
/// available model wins. Every fallback attempt is recorded.
pub fn fallback_chain(
    chain: &[String],
    catalog: &BTreeMap<String, ModelCapability>,
) -> Result<RouteDecision, Vec<String>> {
    let mut attempts = Vec::new();
    for model in chain {
        match catalog.get(model) {
            Some(cap) => {
                return Ok(RouteDecision {
                    model: model.clone(),
                    provider: cap.provider.clone(),
                    exclusion_reasons: attempts.clone(),
                    decision_record: format!("fallback chain resolved to {model}"),
                });
            }
            None => attempts.push(format!("{model}: not in catalog")),
        }
    }
    Err(attempts)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cap(
        model: &str,
        provider: &str,
        ctx: u64,
        out: u64,
        tools: bool,
        cost: f64,
    ) -> ModelCapability {
        ModelCapability {
            model: model.into(),
            provider: provider.into(),
            context_window: ctx,
            max_output_tokens: out,
            supports_tools: tools,
            supports_parallel_tools: false,
            supports_vision: false,
            supports_reasoning: false,
            supports_structured_output: false,
            cost_per_1k_input: cost,
            cost_per_1k_output: cost,
            success_rate: 1.0,
            latency_class: "fast".into(),
        }
    }

    #[test]
    fn capability_mismatch_routes_away() {
        let catalog = vec![
            cap("no-tools", "prov-a", 128_000, 4_096, false, 1.0),
            cap("with-tools", "prov-a", 128_000, 4_096, true, 3.0),
        ];
        let fp = TaskFingerprint {
            requires_tools: true,
            ..Default::default()
        };
        let policy = EnterprisePolicy::default();
        let result = route(&fp, &policy, &catalog).unwrap();
        assert_eq!(result.model, "with-tools", "no-tools model excluded");
        assert!(
            result
                .exclusion_reasons
                .iter()
                .any(|r| r.contains("no tool support")),
            "exclusion reason recorded: {:?}",
            result.exclusion_reasons
        );
    }

    #[test]
    fn enterprise_blocked_provider_routes_away() {
        let catalog = vec![
            cap("openai-model", "openai", 128_000, 4_096, true, 1.0),
            cap("anthropic-model", "anthropic", 128_000, 4_096, true, 3.0),
        ];
        let policy = EnterprisePolicy {
            blocked_providers: vec!["openai".into()],
            ..Default::default()
        };
        let fp = TaskFingerprint {
            requires_tools: true,
            ..Default::default()
        };
        let result = route(&fp, &policy, &catalog).unwrap();
        assert_eq!(result.provider, "anthropic", "blocked provider excluded");
    }

    #[test]
    fn enterprise_required_model_pin_survives() {
        let catalog = vec![
            cap("cheap-a", "prov", 128_000, 4_096, true, 0.5),
            cap("expensive-b", "prov", 128_000, 4_096, true, 15.0),
        ];
        let policy = EnterprisePolicy {
            required_model: Some("expensive-b".into()),
            ..Default::default()
        };
        let fp = TaskFingerprint {
            requires_tools: true,
            ..Default::default()
        };
        let result = route(&fp, &policy, &catalog).unwrap();
        assert_eq!(
            result.model, "expensive-b",
            "required pin beats cost optimization"
        );
    }

    #[test]
    fn all_excluded_is_a_hard_error_with_reasons() {
        let catalog = vec![cap("small", "prov", 4_096, 4_096, true, 1.0)];
        let fp = TaskFingerprint {
            min_context_tokens: 200_000,
            ..Default::default()
        };
        let policy = EnterprisePolicy::default();
        let err = route(&fp, &policy, &catalog).unwrap_err();
        assert!(!err.is_empty(), "exclusion reasons must be returned");
    }

    #[test]
    fn cost_optimization_picks_cheapest_qualifying() {
        let catalog = vec![
            cap("cheap", "prov", 128_000, 4_096, true, 0.5),
            cap("expensive", "prov", 128_000, 4_096, true, 15.0),
        ];
        let fp = TaskFingerprint {
            requires_tools: true,
            ..Default::default()
        };
        let policy = EnterprisePolicy::default();
        let result = route(&fp, &policy, &catalog).unwrap();
        assert_eq!(result.model, "cheap");
    }

    #[test]
    fn fallback_chain_walks_in_order() {
        let mut catalog_map = std::collections::BTreeMap::new();
        catalog_map.insert(
            "primary".to_string(),
            cap("primary", "prov", 128_000, 4_096, true, 1.0),
        );
        let chain = vec!["primary".to_string()];
        let result = fallback_chain(&chain, &catalog_map);
        assert!(result.is_ok(), "primary in catalog resolves");
    }

    #[test]
    fn fallback_chain_reports_missing_models() {
        let catalog_map = std::collections::BTreeMap::new();
        let chain = vec!["ghost-1".to_string(), "ghost-2".to_string()];
        let result = fallback_chain(&chain, &catalog_map);
        assert!(result.is_err());
        let errors = result.unwrap_err();
        assert!(errors.iter().any(|e| e.contains("ghost-1")));
        assert!(errors.iter().any(|e| e.contains("ghost-2")));
    }
}
