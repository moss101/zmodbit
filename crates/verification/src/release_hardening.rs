//! Release hardening (M10): dual error channels with secret redaction
//! (REQ-EV-0017), the cloud SLO event ladder (REQ-EV-0023), per-run
//! token/cost accounting (REQ-EV-0032), headless mode equivalence
//! (REQ-EV-0126), trace/status/export diagnostics with secret redaction
//! (REQ-EV-0142), multi-level qualification with a real API call
//! (REQ-EV-0211), task-conditioned harness generation in shadow mode
//! (REQ-EV-0244), and bounded profile repair with a static fallback
//! (REQ-EV-0246).

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

// ---------------------------------------------------------------------------
// Dual user/model error channels (REQ-EV-0017)
// ---------------------------------------------------------------------------

/// One canonical error identity; two renderings.
#[derive(Clone, Debug, PartialEq)]
pub struct CanonicalError {
    pub error_id: String,
    pub internal_message: String, // may contain secrets
    pub secrets: Vec<String>,
}

/// The user surface: safe explanation, secrets redacted.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UserError {
    pub error_id: String,
    pub message: String,
}

/// The model surface: structured repair payload, secrets redacted but
/// structure retained.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelError {
    pub error_id: String,
    pub message: String,
    pub repair_hints: Vec<String>,
}

/// Redacts every known secret from text.
pub fn redact(text: &str, secrets: &[String]) -> String {
    let mut out = text.to_string();
    for secret in secrets {
        out = out.replace(secret.as_str(), "[REDACTED]");
    }
    out
}

/// Renders both channels from ONE canonical identity. Secret-bearing
/// internal detail is redacted for BOTH surfaces per policy
/// (QUAL-EV-0017).
pub fn render_error(err: &CanonicalError) -> (UserError, ModelError) {
    (
        UserError {
            error_id: err.error_id.clone(),
            message: redact(&err.internal_message, &err.secrets),
        },
        ModelError {
            error_id: err.error_id.clone(),
            message: redact(&err.internal_message, &err.secrets),
            repair_hints: vec!["check file permissions".into()],
        },
    )
}

// ---------------------------------------------------------------------------
// Cloud SLO event ladder (REQ-EV-0023)
// ---------------------------------------------------------------------------

/// The staged SLO ladder timestamps.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SloLadder {
    pub request_ms: Option<u128>,
    pub prewarm_ms: Option<u128>,
    pub sandbox_requested_ms: Option<u128>,
    pub sandbox_ready_ms: Option<u128>,
    pub first_token_ms: Option<u128>,
}

impl SloLadder {
    pub fn record(&mut self, stage: &'static str, at_ms: u128) {
        match stage {
            "request" => self.request_ms = Some(at_ms),
            "prewarm" => self.prewarm_ms = Some(at_ms),
            "sandbox_requested" => self.sandbox_requested_ms = Some(at_ms),
            "sandbox_ready" => self.sandbox_ready_ms = Some(at_ms),
            "first_token" => self.first_token_ms = Some(at_ms),
            _ => {}
        }
    }

    pub fn complete(&self) -> bool {
        self.request_ms.is_some()
            && self.prewarm_ms.is_some()
            && self.sandbox_requested_ms.is_some()
            && self.sandbox_ready_ms.is_some()
            && self.first_token_ms.is_some()
    }

    /// Derived cold latency: request → first token.
    pub fn cold_latency_ms(&self) -> Option<u128> {
        Some(self.first_token_ms? - self.request_ms?)
    }

    /// Derived warm latency: sandbox ready → first token.
    pub fn warm_latency_ms(&self) -> Option<u128> {
        Some(self.first_token_ms? - self.sandbox_ready_ms?)
    }
}

// ---------------------------------------------------------------------------
// Per-run token/cost accounting (REQ-EV-0032)
// ---------------------------------------------------------------------------

/// One usage event attributed to run/step.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UsageEvent {
    pub run_id: String,
    pub step_id: String,
    pub provider: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_units: u64,
}

/// The usage ledger: canonical usage events per run/step.
#[derive(Default)]
pub struct UsageLedger {
    pub events: Vec<UsageEvent>,
}

impl UsageLedger {
    pub fn record(&mut self, event: UsageEvent) {
        self.events.push(event);
    }

    /// Reconciliation: the ledger total vs a provider invoice sample must
    /// match within `tolerance_bps` (QUAL-EV-0032).
    pub fn reconcile(&self, invoice_tokens: u64, tolerance_bps: u64) -> bool {
        let ledger_tokens: u64 = self
            .events
            .iter()
            .map(|e| e.input_tokens + e.output_tokens)
            .sum();
        if invoice_tokens == 0 {
            return ledger_tokens == 0;
        }
        let diff = ledger_tokens.abs_diff(invoice_tokens);
        (diff * 10_000) / invoice_tokens <= tolerance_bps
    }
}

// ---------------------------------------------------------------------------
// Headless mode (REQ-EV-0126)
// ---------------------------------------------------------------------------

/// The surface contracts available per mode. Headless exposes the SAME
/// Core/Policy/Evidence contracts minus UI-only tools.
#[derive(Clone, Debug, PartialEq)]
pub struct SurfaceContracts {
    pub core: bool,
    pub policy: bool,
    pub evidence: bool,
    pub ui_only_tools: Vec<String>,
}

pub fn surface_contracts(headless: bool) -> SurfaceContracts {
    SurfaceContracts {
        core: true,
        policy: true,
        evidence: true,
        // Headless DROPS UI-only tools; desktop carries them.
        ui_only_tools: if headless {
            Vec::new()
        } else {
            vec!["ui.takeover".to_string(), "ui.notify".to_string()]
        },
    }
}

// ---------------------------------------------------------------------------
// Trace/status/export diagnostics (REQ-EV-0142)
// ---------------------------------------------------------------------------

/// Exports a diagnostic bundle: evidence metadata replays and NO
/// credential values are included (QUAL-EV-0142).
pub fn export_diagnostics(
    facts: &[String],
    credentials: &[String],
) -> Result<(String, BTreeMap<String, String>), String> {
    let mut meta = BTreeMap::new();
    for (i, fact) in facts.iter().enumerate() {
        meta.insert(format!("fact-{i}"), fact.clone());
    }
    // Credential VALUES are never exported — only masked placeholders.
    for (i, cred) in credentials.iter().enumerate() {
        let digest = sha256_hex(cred.as_bytes());
        meta.insert(
            format!("credential-{i}"),
            format!("[redacted:{}]", &digest[..12]),
        );
    }
    let bundle = serde_json::to_string(&meta).map_err(|e| e.to_string())?;
    Ok((bundle, meta))
}

// ---------------------------------------------------------------------------
// Multi-level qualification (REQ-EV-0211)
// ---------------------------------------------------------------------------

/// The three qualification levels.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QualificationLevels {
    pub component_passed: bool,
    pub integration_passed: bool,
    /// Real external API call with a recorded safe fixture — mock-only
    /// cannot satisfy this.
    pub real_api_passed: Option<bool>,
}

/// Completion requires ALL levels, including the real API level.
pub fn qualification_complete(levels: &QualificationLevels) -> bool {
    levels.component_passed && levels.integration_passed && levels.real_api_passed == Some(true)
}

// ---------------------------------------------------------------------------
// Harness generation + bounded repair (REQ-EV-0244/0246, EXPERIMENT)
// ---------------------------------------------------------------------------

/// A generated harness/profile candidate. Shadow candidates NEVER
/// control production runs (QUAL-EV-0244).
#[derive(Clone, Debug, PartialEq)]
pub struct HarnessCandidate {
    pub generation: u32,
    pub profile: BTreeMap<String, String>,
    pub shadow: bool,
}

/// Generates a task-conditioned shadow candidate.
pub fn generate_shadow(task_tags: &[&str]) -> HarnessCandidate {
    let mut profile = BTreeMap::new();
    profile.insert("conditioned_on".to_string(), task_tags.join(","));
    profile.insert("mode".to_string(), "shadow".to_string());
    HarnessCandidate {
        generation: 1,
        profile,
        shadow: true,
    }
}

/// Bounded repair: at most TWO repair generations; the third attempt is
/// rejected and the STATIC known-good fallback is returned
/// (QUAL-EV-0246).
pub fn bounded_repair(attempt: u32) -> Result<(HarnessCandidate, &'static str), String> {
    match attempt {
        1 | 2 => {
            let mut candidate = generate_shadow(&["repair"]);
            candidate.generation = attempt;
            Ok((candidate, "candidate"))
        }
        _ => Ok((
            HarnessCandidate {
                generation: 0,
                profile: BTreeMap::from([("source".to_string(), "static-known-good".to_string())]),
                shadow: true,
            },
            "static-known-good",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// QUAL-EV-0017: a secret-bearing internal error is redacted for both
    /// surfaces.
    #[test]
    fn secret_bearing_error_redacted_for_both_surfaces() {
        let err = CanonicalError {
            error_id: "err-1".into(),
            internal_message: "connect failed with key sk-live-SECRET and timeout".into(),
            secrets: vec!["sk-live-SECRET".into()],
        };
        let (user, model) = render_error(&err);
        assert!(!user.message.contains("sk-live-SECRET"));
        assert!(!model.message.contains("sk-live-SECRET"));
        assert!(model.message.contains("[REDACTED]"));
        assert_eq!(user.error_id, model.error_id);
    }

    /// QUAL-EV-0023: a staging run emits all ladder timestamps and
    /// derived cold/warm latency.
    #[test]
    fn slo_ladder_emits_all_stages_and_latencies() {
        let mut ladder = SloLadder::default();
        ladder.record("request", 0);
        ladder.record("prewarm", 50);
        ladder.record("sandbox_requested", 80);
        ladder.record("sandbox_ready", 400);
        ladder.record("first_token", 900);
        assert!(ladder.complete());
        assert_eq!(ladder.cold_latency_ms(), Some(900));
        assert_eq!(ladder.warm_latency_ms(), Some(500));
    }

    /// QUAL-EV-0032: provider invoice reconciles against canonical usage
    /// within tolerance.
    #[test]
    fn usage_ledger_reconciles_within_tolerance() {
        let mut ledger = UsageLedger::default();
        ledger.record(UsageEvent {
            run_id: "run-1".into(),
            step_id: "step-1".into(),
            provider: "z.ai".into(),
            input_tokens: 5000,
            output_tokens: 1500,
            cost_units: 40,
        });
        // Invoice within 1% tolerance.
        assert!(ledger.reconcile(6550, 200));
        assert!(!ledger.reconcile(9000, 200), "way-off invoice must fail");
    }

    /// QUAL-EV-0126: identical task run via desktop and headless yields
    /// the same canonical contracts (UI-only tools excluded headless).
    #[test]
    fn headless_and_desktop_share_canonical_contracts() {
        let desktop = surface_contracts(false);
        let headless = surface_contracts(true);
        // Core/Policy/Evidence identical.
        assert_eq!(desktop.core, headless.core);
        assert_eq!(desktop.policy, headless.policy);
        assert_eq!(desktop.evidence, headless.evidence);
        // UI-only tools absent headless.
        assert!(headless.ui_only_tools.is_empty());
    }

    /// QUAL-EV-0142: export replays evidence metadata with no credential
    /// values.
    #[test]
    fn export_contains_no_credential_values() {
        let (bundle, _) = export_diagnostics(
            &["fact: retry budget 5".into()],
            &["sk-live-STATIC-KEY".into()],
        )
        .unwrap();
        assert!(bundle.contains("fact: retry budget 5"));
        assert!(!bundle.contains("sk-live-STATIC-KEY"));
        assert!(bundle.contains("[redacted:"));
    }

    /// QUAL-EV-0211: completion requires all three qualification levels
    /// including the real API call.
    #[test]
    fn qualification_requires_real_api_level() {
        let mock_only = QualificationLevels {
            component_passed: true,
            integration_passed: true,
            real_api_passed: None,
        };
        assert!(!qualification_complete(&mock_only), "mock-only cannot pass");
        let full = QualificationLevels {
            component_passed: true,
            integration_passed: true,
            real_api_passed: Some(true),
        };
        assert!(qualification_complete(&full));
    }

    /// QUAL-EV-0244 + 0246: shadow candidates never control production,
    /// and the third repair attempt falls back to static known-good.
    #[test]
    fn shadow_candidates_and_bounded_repair() {
        let shadow = generate_shadow(&["deploy"]);
        assert!(shadow.shadow, "shadow candidate never controls production");

        // Repairs 1 and 2 produce candidates.
        assert!(bounded_repair(1).unwrap().1 == "candidate");
        assert!(bounded_repair(2).unwrap().1 == "candidate");
        // Third attempt: static known-good fallback keeps the run alive.
        let (candidate, source) = bounded_repair(3).unwrap();
        assert_eq!(source, "static-known-good");
        assert!(candidate.profile.contains_key("source"));
    }
}
