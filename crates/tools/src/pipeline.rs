//! Typed tool pipeline (M2, REQ-EV-0239, ADAPT): every tool invocation
//! flows through canonical stages — VALIDATE → POLICY → PRE_HOOKS →
//! EXECUTE → POST_HOOKS → EVIDENCE — with MONOTONIC guards: once policy
//! denies, no later stage (hook or otherwise) can flip the decision back
//! to allow. Hooks decorate; they never override policy.

use crate::schema::ToolSchema;
use modbit_policy::PolicyDecision;
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::sync::Arc;

/// A hook: observes/transforms the invocation envelope. It CANNOT change
/// the policy decision — only enrich context or veto (veto is monotone:
/// deny-only).
pub type Hook = Arc<dyn Fn(&Value, &mut HookContext) -> Result<(), String> + Send + Sync>;

/// The typed executor of a pipelined tool.
pub type Executor = Arc<dyn Fn(&Value) -> Result<Value, String> + Send + Sync>;

/// Mutable hook context: hooks may annotate evidence.
#[derive(Default, Serialize, Clone, Debug, PartialEq)]
pub struct HookContext {
    pub annotations: Vec<String>,
}

/// The typed pipeline stage outcomes.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub enum PipelineOutcome {
    /// Denied — by policy, by validation, or by hook veto. FINAL.
    Denied { stage: &'static str, reason: String },
    /// Executed with the evidence envelope.
    Executed {
        tool: String,
        normalized_arguments_hash: String,
        result: Value,
        evidence: HookContext,
    },
}

impl PipelineOutcome {
    pub fn is_denied(&self) -> bool {
        matches!(self, PipelineOutcome::Denied { .. })
    }
}

/// A registered pipeline for one tool.
pub struct PipelinedTool {
    pub name: String,
    pub schema: ToolSchema,
    pub effect_class: modbit_policy::EffectClass,
    pub execute: Executor,
    pub pre_hooks: Vec<Hook>,
    pub post_hooks: Vec<Hook>,
}

/// Runs the canonical pipeline. `decision` comes from the Capability
/// Kernel — the pipeline consumes it and it is IRREVERSIBLE inside the
/// pipeline (QUAL-EV-0239).
pub fn run_pipeline(
    tool: &PipelinedTool,
    raw_arguments: &Value,
    decision: &PolicyDecision,
) -> PipelineOutcome {
    // Stage 1: VALIDATE — schema normalization before anything else.
    let normalized = match tool.schema.normalize(raw_arguments) {
        Ok(v) => v,
        Err(e) => {
            return PipelineOutcome::Denied {
                stage: "validate",
                reason: e.to_string(),
            };
        }
    };

    // Stage 2: POLICY — the kernel's decision is final.
    let deny = match decision {
        PolicyDecision::Allow => None,
        PolicyDecision::Deny { reason } => Some(reason.clone()),
    };
    if let Some(reason) = deny {
        // Even if hooks "approve", policy deny is monotone: we return now.
        return PipelineOutcome::Denied {
            stage: "policy",
            reason: format!("kernel denied: {reason}"),
        };
    }

    // Stage 3: PRE_HOOKS — may veto (deny-only), may annotate.
    let mut ctx = HookContext::default();
    for hook in &tool.pre_hooks {
        let mut hook_ctx = HookContext::default();
        if let Err(veto) = hook(&normalized, &mut hook_ctx) {
            ctx.annotations.push(format!("pre-hook veto: {veto}"));
            return PipelineOutcome::Denied {
                stage: "pre_hooks",
                reason: veto,
            };
        }
        ctx.annotations.extend(hook_ctx.annotations);
    }

    // Stage 4: EXECUTE.
    let result = match (tool.execute)(&normalized) {
        Ok(v) => v,
        Err(e) => {
            return PipelineOutcome::Denied {
                stage: "execute",
                reason: e,
            };
        }
    };

    // Stage 5: POST_HOOKS — annotate only; failures are recorded, never
    // turned into allows.
    for hook in &tool.post_hooks {
        let mut hook_ctx = HookContext::default();
        if let Err(e) = hook(&result, &mut hook_ctx) {
            ctx.annotations.push(format!("post-hook error: {e}"));
        } else {
            ctx.annotations.extend(hook_ctx.annotations);
        }
    }

    // Stage 6: EVIDENCE — hash the normalized arguments.
    let mut hasher = Sha256::new();
    hasher.update(serde_json::to_vec(&normalized).unwrap_or_default());
    PipelineOutcome::Executed {
        tool: tool.name.clone(),
        normalized_arguments_hash: format!("{:x}", hasher.finalize()),
        result,
        evidence: ctx,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use modbit_policy::EffectClass;
    use std::collections::BTreeMap;

    fn tool_with_hooks(pre: Vec<Hook>, post: Vec<Hook>) -> PipelinedTool {
        let mut parameters = BTreeMap::new();
        parameters.insert(
            "path".to_string(),
            crate::schema::ParamSpec {
                param_type: crate::schema::ParamType::Str,
                required: true,
                default: None,
                description: "path".into(),
            },
        );
        PipelinedTool {
            name: "modbit.file.read".into(),
            schema: ToolSchema {
                aliases: BTreeMap::new(),
                parameters,
            },
            effect_class: EffectClass::ReadOnly,
            execute: Arc::new(|args| Ok(serde_json::json!({"read": args["path"]}))),
            pre_hooks: pre,
            post_hooks: post,
        }
    }

    fn allow() -> PolicyDecision {
        PolicyDecision::Allow
    }

    /// QUAL-EV-0239: a hook tries to override a deny after the guard; the
    /// execution REMAINS denied.
    #[test]
    fn hook_cannot_override_deny_after_guard() {
        // Even a hook that claims to "force-allow" runs AFTER policy: the
        // pipeline returns at the policy stage and hooks never execute.
        let force_allow_hook: Hook = Arc::new(|_args, ctx| {
            ctx.annotations.push("FORCE ALLOW ATTEMPT".into());
            Ok(())
        });
        let tool = tool_with_hooks(vec![force_allow_hook], vec![]);
        let deny = PolicyDecision::Deny {
            reason: "session not granted file access".into(),
        };

        let outcome = run_pipeline(&tool, &serde_json::json!({"path": "src/main.rs"}), &deny);
        match outcome {
            PipelineOutcome::Denied { stage, reason } => {
                assert_eq!(stage, "policy");
                assert!(reason.contains("kernel denied"));
            }
            PipelineOutcome::Executed { .. } => {
                panic!("a hook overrode the deny — monotonicity violated")
            }
        }
    }

    #[test]
    fn happy_path_runs_all_stages_in_order() {
        let pre: Hook = Arc::new(|_args, ctx| {
            ctx.annotations.push("pre-ok".into());
            Ok(())
        });
        let post: Hook = Arc::new(|_result, ctx| {
            ctx.annotations.push("post-ok".into());
            Ok(())
        });
        let tool = tool_with_hooks(vec![pre], vec![post]);
        let outcome = run_pipeline(&tool, &serde_json::json!({"path": "src/lib.rs"}), &allow());
        match outcome {
            PipelineOutcome::Executed {
                tool: name,
                result,
                evidence,
                ..
            } => {
                assert_eq!(name, "modbit.file.read");
                assert_eq!(result["read"], "src/lib.rs");
                assert_eq!(evidence.annotations, vec!["pre-ok", "post-ok"]);
            }
            _ => panic!("expected execution"),
        }
    }

    /// A pre-hook veto denies even under an allow decision (deny-only).
    #[test]
    fn pre_hook_veto_is_monotone_deny() {
        let veto: Hook = Arc::new(|args, _ctx| {
            if args["path"].as_str().unwrap_or("").starts_with(".git") {
                Err("git internals are off-limits".into())
            } else {
                Ok(())
            }
        });
        let tool = tool_with_hooks(vec![veto], vec![]);
        let denied = run_pipeline(&tool, &serde_json::json!({"path": ".git/config"}), &allow());
        assert!(denied.is_denied());
        assert!(matches!(
            run_pipeline(&tool, &serde_json::json!({"path": "ok.txt"}), &allow()),
            PipelineOutcome::Executed { .. }
        ));
    }
}
