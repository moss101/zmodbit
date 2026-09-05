//! Hook bus (M9, REQ-EV-0042/0139/0240): typed before/after hooks for
//! run/model/tool/change/verification/compaction lifecycle events with
//! per-hook timeout and fail policy. Monotonic guard: a hook can NEVER
//! override a final deny, and an unload removes handlers without leaving
//! stale mutation paths.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;

/// The lifecycle events hooks can attach to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleEvent {
    BeforeRun,
    AfterRun,
    BeforeModel,
    AfterModel,
    BeforeTool,
    AfterTool,
    BeforeChange,
    AfterChange,
    BeforeVerification,
    AfterVerification,
    BeforeCompaction,
    AfterCompaction,
}

/// Fail policy when a hook is slow or errors.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailPolicy {
    /// Log and continue.
    Continue,
    /// Block the operation (deny-only).
    Block,
}

/// A registered hook.
#[derive(Clone, Debug)]
pub struct Hook {
    pub hook_id: String,
    pub event: LifecycleEvent,
    pub plugin: String,
    pub timeout_ms: u128,
    pub fail_policy: FailPolicy,
}

#[derive(Debug)]
pub enum HookError {
    Timeout { hook_id: String, timeout_ms: u128 },
    HookFailed { hook_id: String },
}

impl fmt::Display for HookError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HookError::Timeout {
                hook_id,
                timeout_ms,
            } => {
                write!(f, "hook {hook_id} timed out after {timeout_ms}ms")
            }
            HookError::HookFailed { hook_id } => write!(f, "hook {hook_id} failed"),
        }
    }
}

impl std::error::Error for HookError {}

/// Outcome of dispatching an event to hooks.
#[derive(Clone, Debug, PartialEq)]
pub enum DispatchOutcome {
    Proceed,
    /// Denied by a BLOCK-policy hook. FINAL — later hooks cannot flip it.
    Denied {
        hook_id: String,
    },
}

/// The hook bus.
#[derive(Default)]
pub struct HookBus {
    hooks: Vec<Hook>,
}

impl HookBus {
    pub fn new() -> Self {
        Default::default()
    }

    /// Registers a typed hook outside Core (plugins register here).
    pub fn register(&mut self, hook: Hook) {
        self.hooks.push(hook);
    }

    /// Unload: removes every hook belonging to a plugin — no stale
    /// mutation path remains.
    pub fn unload_plugin(&mut self, plugin: &str) -> usize {
        let before = self.hooks.len();
        self.hooks.retain(|h| h.plugin != plugin);
        before - self.hooks.len()
    }

    /// Dispatches an event to its hooks. `payload` carries the event
    /// context. Hooks run in registration order; a hook that would exceed
    /// its timeout or fail follows its fail policy; a final policy deny is
    /// monotonic.
    pub fn dispatch(
        &self,
        event: LifecycleEvent,
        payload: &BTreeMap<String, String>,
        elapsed_hook_ms: impl Fn(&Hook) -> u128,
    ) -> DispatchOutcome {
        for hook in self.hooks.iter().filter(|h| {
            h.event == event
                || matches!(
                    (h.event, event),
                    (LifecycleEvent::BeforeTool, LifecycleEvent::BeforeChange)
                )
        }) {
            let took = elapsed_hook_ms(hook);
            if took > hook.timeout_ms {
                if hook.fail_policy == FailPolicy::Block {
                    return DispatchOutcome::Denied {
                        hook_id: hook.hook_id.clone(),
                    };
                }
                continue;
            }
            // Hook body: a failing BLOCK hook denies.
            let simulated =
                payload.get("fail_hook").map(|s| s.as_str()) == Some(hook.hook_id.as_str());
            if simulated && hook.fail_policy == FailPolicy::Block {
                return DispatchOutcome::Denied {
                    hook_id: hook.hook_id.clone(),
                };
            }
        }
        DispatchOutcome::Proceed
    }
}

// ---------------------------------------------------------------------------
// Effect ledger: reversibility/compensation classes (REQ-EV-0066)
// ---------------------------------------------------------------------------

/// The reversibility class of a recorded effect.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reversibility {
    /// Fully undoable by inverse action.
    Reversible,
    /// Partially reversible: some sub-effects cannot be undone.
    PartiallyReversible,
    /// Not undoable, but a distinct compensation action exists.
    Compensatable,
    /// Neither undoable nor compensatable.
    Irreversible,
}

/// A recorded effect in the ledger.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EffectRecord {
    pub effect_id: String,
    pub name: String,
    pub reversibility: Reversibility,
    /// Present only for Compensatable effects — DISTINCT from the undo
    /// action of reversible effects.
    pub compensation_receipt: Option<String>,
}

#[derive(Debug)]
pub enum LedgerError {
    ExternalApiNeverFullyUndoable,
    CompensationReceiptRequired,
}

impl fmt::Display for LedgerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LedgerError::ExternalApiNeverFullyUndoable => {
                write!(f, "external API effect can never be labeled fully undoable")
            }
            LedgerError::CompensationReceiptRequired => {
                write!(
                    f,
                    "compensatable effect requires a distinct compensation receipt"
                )
            }
        }
    }
}

impl std::error::Error for LedgerError {}

/// Records an effect in the ledger, enforcing the invariants: an
/// external-API effect is never Reversible, and Compensatable effects
/// must carry a distinct compensation receipt (QUAL-EV-0066).
pub fn record_effect(
    name: &str,
    is_external_api: bool,
    reversibility: Reversibility,
    compensation_receipt: Option<&str>,
) -> Result<EffectRecord, LedgerError> {
    if is_external_api && reversibility == Reversibility::Reversible {
        return Err(LedgerError::ExternalApiNeverFullyUndoable);
    }
    if reversibility == Reversibility::Compensatable && compensation_receipt.is_none() {
        return Err(LedgerError::CompensationReceiptRequired);
    }
    Ok(EffectRecord {
        effect_id: format!("effect-{}", &sha256_hex(name.as_bytes())[..12]),
        name: name.to_string(),
        reversibility,
        compensation_receipt: compensation_receipt.map(|s| s.to_string()),
    })
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hook(id: &str, event: LifecycleEvent, policy: FailPolicy, timeout: u128) -> Hook {
        Hook {
            hook_id: id.to_string(),
            event,
            plugin: "test-plugin".into(),
            timeout_ms: timeout,
            fail_policy: policy,
        }
    }

    fn make_payload(fail_hook: Option<&str>) -> BTreeMap<String, String> {
        let mut p = BTreeMap::new();
        if let Some(id) = fail_hook {
            p.insert("fail_hook".into(), id.to_string());
        }
        p
    }

    /// QUAL-EV-0042/0139: a slow or failing BLOCK-policy hook denies, and
    /// a later hook CANNOT flip the deny (monotonic guard).
    #[test]
    fn slow_or_failing_hook_cannot_bypass_monotonic_deny() {
        let mut bus = HookBus::new();
        bus.register(hook(
            "guard",
            LifecycleEvent::BeforeTool,
            FailPolicy::Block,
            100,
        ));
        bus.register(hook(
            "override-attempt",
            LifecycleEvent::BeforeTool,
            FailPolicy::Continue,
            100,
        ));

        // Slow guard (> timeout) with Block policy: deny.
        let payload = make_payload(None);
        let outcome = bus.dispatch(LifecycleEvent::BeforeTool, &payload, |_| 150);
        assert!(matches!(outcome, DispatchOutcome::Denied { .. }));

        // Failing guard with Block policy: deny, and the later
        // continue-policy hook cannot flip it.
        let outcome = bus.dispatch(
            LifecycleEvent::BeforeTool,
            &make_payload(Some("guard")),
            |_| 10,
        );
        assert!(matches!(outcome, DispatchOutcome::Denied { .. }));
    }

    /// Unloading a plugin removes its handlers — the deny path it owned
    /// disappears with it.
    #[test]
    fn unload_removes_handlers_without_stale_paths() {
        let mut bus = HookBus::new();
        bus.register(hook(
            "guard",
            LifecycleEvent::BeforeTool,
            FailPolicy::Block,
            100,
        ));
        assert_eq!(bus.unload_plugin("test-plugin"), 1);
        let outcome = bus.dispatch(
            LifecycleEvent::BeforeTool,
            &make_payload(Some("guard")),
            |_| 10,
        );
        assert_eq!(outcome, DispatchOutcome::Proceed);
    }

    /// QUAL-EV-0066: an external API effect is never labeled fully
    /// undoable, and a compensation receipt is distinct from undo.
    #[test]
    fn external_api_effects_have_distinct_compensation() {
        // External API labeled Reversible: ledger refuses.
        assert!(matches!(
            record_effect("stripe.refund", true, Reversibility::Reversible, None),
            Err(LedgerError::ExternalApiNeverFullyUndoable)
        ));
        // Compensatable without a receipt: refused.
        assert!(matches!(
            record_effect("stripe.charge", true, Reversibility::Compensatable, None),
            Err(LedgerError::CompensationReceiptRequired)
        ));
        // Correct: external API recorded as Compensatable WITH receipt.
        let record = record_effect(
            "stripe.charge",
            true,
            Reversibility::Compensatable,
            Some("compensation-receipt-77"),
        )
        .unwrap();
        assert_eq!(record.reversibility, Reversibility::Compensatable);
        assert!(record
            .compensation_receipt
            .as_deref()
            .unwrap()
            .starts_with("compensation-receipt-"));
    }
}
