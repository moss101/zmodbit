//! Provider-neutral request envelope (M2, REQ-EV-0112): the caller states
//! what it WANTS (model, reasoning effort, service tier); the gateway
//! resolves what it WILL use under enterprise policy — and the routing
//! record keeps both sides plus the policy reason, so every dispatch is
//! auditable (requested vs resolved never silently diverge).

use crate::routing::{EnterprisePolicy, ModelCapability};
use serde::{Deserialize, Serialize};
use std::fmt;

/// Reasoning effort a caller requests from a reasoning model.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    Minimal,
    Low,
    Medium,
    High,
}

impl fmt::Display for ReasoningEffort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            ReasoningEffort::Minimal => "minimal",
            ReasoningEffort::Low => "low",
            ReasoningEffort::Medium => "medium",
            ReasoningEffort::High => "high",
        };
        write!(f, "{s}")
    }
}

/// Cost/latency tier of a request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceTier {
    Standard,
    Flex,
    Priority,
}

impl fmt::Display for ServiceTier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            ServiceTier::Standard => "standard",
            ServiceTier::Flex => "flex",
            ServiceTier::Priority => "priority",
        };
        write!(f, "{s}")
    }
}

/// What the caller asked for (provider-neutral).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RequestedDispatch {
    pub model: String,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub service_tier: Option<ServiceTier>,
}

/// What the gateway will actually use after policy resolution.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ResolvedDispatch {
    pub model: String,
    pub provider: String,
    pub reasoning_effort: ReasoningEffort,
    pub service_tier: ServiceTier,
}

/// The auditable routing record: requested vs resolved + the policy reason
/// for any difference (REQ-EV-0112 QUAL).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RoutingRecord {
    pub requested: RequestedDispatch,
    pub resolved: ResolvedDispatch,
    pub policy_reason: String,
}

/// Resolves a requested dispatch against enterprise policy and the model
/// catalog. Resolution rules, in order:
/// 1. Requested model blocked by policy → nearest catalog model not blocked
///    (preserving the requested prefix when possible), reason recorded.
/// 2. Service tier above the policy ceiling → clamped DOWN, reason recorded.
/// 3. Otherwise resolved == requested ("honored as requested").
pub fn resolve_dispatch(
    requested: &RequestedDispatch,
    policy: &EnterprisePolicy,
    catalog: &[ModelCapability],
    default_effort: ReasoningEffort,
) -> Result<RoutingRecord, String> {
    let mut reasons: Vec<String> = Vec::new();

    // Model resolution.
    let (model, provider) = if policy.blocked_models.contains(&requested.model) {
        let fallback = catalog
            .iter()
            .filter(|c| !policy.blocked_models.contains(&c.model))
            .find(|c| {
                // Prefer the same family prefix (e.g. gpt-* → gpt-*).
                let prefix_ok = requested
                    .model
                    .split('-')
                    .next()
                    .map(|p| c.model.starts_with(p))
                    .unwrap_or(false);
                prefix_ok
            })
            .or_else(|| {
                catalog
                    .iter()
                    .find(|c| !policy.blocked_models.contains(&c.model))
            })
            .ok_or_else(|| "no unblocked model in catalog".to_string())?;
        reasons.push(format!(
            "model {} blocked by enterprise policy; resolved to {}",
            requested.model, fallback.model
        ));
        (fallback.model.clone(), fallback.provider.clone())
    } else {
        let entry = catalog
            .iter()
            .find(|c| c.model == requested.model)
            .ok_or_else(|| format!("requested model {} not in catalog", requested.model))?;
        (entry.model.clone(), entry.provider.clone())
    };

    // Service tier clamp (ceiling, never a bypass).
    let requested_tier = requested.service_tier.unwrap_or(ServiceTier::Standard);
    let service_tier = if let Some(allowed) = &policy.allowed_service_tiers {
        if !allowed.is_empty() && !allowed.contains(&requested_tier) {
            let clamped = allowed
                .iter()
                .copied()
                .max()
                .unwrap_or(ServiceTier::Standard);
            if clamped < requested_tier {
                reasons.push(format!(
                    "service tier {requested_tier} above policy ceiling; clamped to {clamped}"
                ));
            }
            clamped.min(requested_tier)
        } else {
            requested_tier
        }
    } else {
        requested_tier
    };

    let reasoning_effort = requested.reasoning_effort.unwrap_or(default_effort);

    let policy_reason = if reasons.is_empty() {
        "honored as requested".to_string()
    } else {
        reasons.join("; ")
    };

    Ok(RoutingRecord {
        requested: requested.clone(),
        resolved: ResolvedDispatch {
            model,
            provider,
            reasoning_effort,
            service_tier,
        },
        policy_reason,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> Vec<ModelCapability> {
        vec![
            cap("gpt-5", "openai"),
            cap("gpt-5-mini", "openai"),
            cap("claude-opus", "anthropic"),
        ]
    }

    fn cap(model: &str, provider: &str) -> ModelCapability {
        ModelCapability {
            model: model.to_string(),
            provider: provider.to_string(),
            context_window: 200_000,
            max_output_tokens: 32_000,
            supports_tools: true,
            supports_parallel_tools: true,
            supports_vision: true,
            supports_reasoning: true,
            supports_structured_output: true,
            cost_per_1k_input: 1.0,
            cost_per_1k_output: 2.0,
            latency_class: "fast".to_string(),
            success_rate: 0.99,
        }
    }

    /// QUAL-EV-0112: the routing record shows requested vs resolved values
    /// and the policy reason when they differ.
    #[test]
    fn routing_record_shows_requested_vs_resolved_and_reason() {
        let policy = EnterprisePolicy {
            blocked_models: vec!["gpt-5".to_string()],
            blocked_providers: vec![],
            required_model: None,
            allowed_service_tiers: Some(vec![ServiceTier::Standard, ServiceTier::Flex]),
        };
        let requested = RequestedDispatch {
            model: "gpt-5".into(),
            reasoning_effort: Some(ReasoningEffort::High),
            service_tier: Some(ServiceTier::Priority),
        };
        let record =
            resolve_dispatch(&requested, &policy, &catalog(), ReasoningEffort::Medium).unwrap();

        // Requested side is preserved verbatim for audit.
        assert_eq!(record.requested.model, "gpt-5");
        assert_eq!(record.requested.service_tier, Some(ServiceTier::Priority));
        // Resolved side is the policy-compliant dispatch.
        assert_eq!(record.resolved.model, "gpt-5-mini", "same family fallback");
        assert_eq!(record.resolved.provider, "openai");
        assert_eq!(
            record.resolved.service_tier,
            ServiceTier::Flex,
            "highest allowed tier at or below requested"
        );
        // The reason names BOTH corrections.
        assert!(record
            .policy_reason
            .contains("blocked by enterprise policy"));
        assert!(record.policy_reason.contains("clamped to flex"));
    }

    #[test]
    fn honored_requests_record_no_divergence() {
        let policy = EnterprisePolicy::default();
        let requested = RequestedDispatch {
            model: "claude-opus".into(),
            reasoning_effort: Some(ReasoningEffort::Low),
            service_tier: Some(ServiceTier::Flex),
        };
        let record =
            resolve_dispatch(&requested, &policy, &catalog(), ReasoningEffort::Medium).unwrap();
        assert_eq!(record.resolved.model, "claude-opus");
        assert_eq!(record.resolved.reasoning_effort, ReasoningEffort::Low);
        assert_eq!(record.policy_reason, "honored as requested");
    }

    #[test]
    fn missing_model_is_a_typed_error() {
        let policy = EnterprisePolicy::default();
        let requested = RequestedDispatch {
            model: "nonexistent".into(),
            reasoning_effort: None,
            service_tier: None,
        };
        assert!(
            resolve_dispatch(&requested, &policy, &catalog(), ReasoningEffort::Medium).is_err()
        );
    }
}
