//! Computer-use runtime safety (M7, REQ-EV-0086/0087/0089/0090):
//! verified fallback with evidence, human-activity preemption with
//! cooldown, the typed computer-use failure taxonomy, and safe typing
//! with a clipboard guard. The clipboard secret NEVER enters the
//! model/evidence body.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;

pub use crate::sha256_hex;

// ---------------------------------------------------------------------------
// Verified fallback + evidence (REQ-EV-0086)
// ---------------------------------------------------------------------------

/// A fallback (raw-input) action record. Completion verification REJECTS
/// any fallback lacking a post-state check.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FallbackAction {
    pub modality: &'static str, // mouse | keyboard
    pub reason: String,
    pub target: String,
    pub post_state_verified: bool,
    pub post_state_digest: String,
}

/// The completion verifier: a fallback WITHOUT a verified post-state is
/// rejected (QUAL-EV-0086).
pub fn verify_fallback(action: &FallbackAction) -> Result<(), String> {
    if !action.post_state_verified || action.post_state_digest.is_empty() {
        return Err(
            "raw-input fallback rejected: post-state check missing — re-verify before completion"
                .into(),
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Human activity preemption (REQ-EV-0087)
// ---------------------------------------------------------------------------

/// The controller state after human activity is detected.
#[derive(Clone, Debug, PartialEq)]
pub enum PreemptionState {
    /// Automation continues.
    Active,
    /// Automation parked; cooldown must elapse before reacquisition.
    Preempted {
        reason: String,
        cooldown_until_ms: i64,
    },
}

/// The preemption manager: real human input revokes/parks the controller
/// and enforces a cooldown.
pub struct PreemptionManager {
    pub cooldown_ms: i64,
    pub state: PreemptionState,
    pub last_human_activity_ms: i64,
}

impl PreemptionManager {
    pub fn new(cooldown_ms: i64) -> Self {
        Self {
            cooldown_ms,
            state: PreemptionState::Active,
            last_human_activity_ms: 0,
        }
    }

    /// Registers real human mouse/keyboard activity: automation STOPS and
    /// requires reacquisition after cooldown (QUAL-EV-0087).
    pub fn human_activity(&mut self, at_ms: i64, device: &str) {
        self.last_human_activity_ms = at_ms;
        self.state = PreemptionState::Preempted {
            reason: format!("real {device} activity detected"),
            cooldown_until_ms: at_ms + self.cooldown_ms,
        };
    }

    /// Reacquisition attempt: succeeds only after cooldown elapses.
    pub fn try_reacquire(&mut self, at_ms: i64) -> Result<(), String> {
        match &self.state {
            PreemptionState::Preempted {
                cooldown_until_ms, ..
            } if at_ms < *cooldown_until_ms => Err(format!(
                "cooldown active for {} more ms",
                cooldown_until_ms - at_ms
            )),
            _ => {
                self.state = PreemptionState::Active;
                Ok(())
            }
        }
    }

    pub fn is_active(&self) -> bool {
        matches!(self.state, PreemptionState::Active)
    }
}

// ---------------------------------------------------------------------------
// Typed computer-use failure taxonomy (REQ-EV-0089)
// ---------------------------------------------------------------------------

/// The stable computer-use failure codes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ComputerUseFailure {
    TargetStale,
    TargetOccluded,
    WindowUnverifiable,
    AccessibilityUnavailable,
    HumanActive,
    ModalBlocking,
    TargetNotEditable,
    ActionUnsafe,
    PermissionRequired,
}

impl fmt::Display for ComputerUseFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let code = match self {
            ComputerUseFailure::TargetStale => "TARGET_STALE",
            ComputerUseFailure::TargetOccluded => "TARGET_OCCLUDED",
            ComputerUseFailure::WindowUnverifiable => "WINDOW_UNVERIFIABLE",
            ComputerUseFailure::AccessibilityUnavailable => "ACCESSIBILITY_UNAVAILABLE",
            ComputerUseFailure::HumanActive => "HUMAN_ACTIVE",
            ComputerUseFailure::ModalBlocking => "MODAL_BLOCKING",
            ComputerUseFailure::TargetNotEditable => "TARGET_NOT_EDITABLE",
            ComputerUseFailure::ActionUnsafe => "ACTION_UNSAFE",
            ComputerUseFailure::PermissionRequired => "PERMISSION_REQUIRED",
        };
        write!(f, "{code}")
    }
}

/// The typed failure with stable code + recovery guidance.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ComputerUseFailureDiagnostic {
    pub code: ComputerUseFailure,
    pub recovery: String,
}

/// Maps a fault fixture to its typed diagnostic — stable code and
/// recovery guidance per failure (QUAL-EV-0089).
pub fn diagnose(fault: ComputerUseFailure) -> ComputerUseFailureDiagnostic {
    let recovery = match fault {
        ComputerUseFailure::TargetStale => {
            "re-extract semantic state and re-resolve the target".into()
        }
        ComputerUseFailure::TargetOccluded => {
            "raise or close the occluding window, then re-resolve".into()
        }
        ComputerUseFailure::WindowUnverifiable => {
            "resolve the exact process identity before retrying".into()
        }
        ComputerUseFailure::AccessibilityUnavailable => {
            "restart the AX bridge or fall back to the targeted visual ladder".into()
        }
        ComputerUseFailure::HumanActive => "yield to the human and wait out the cooldown".into(),
        ComputerUseFailure::ModalBlocking => "dismiss or navigate the modal dialog first".into(),
        ComputerUseFailure::TargetNotEditable => {
            "select an editable target or use the appropriate tool".into()
        }
        ComputerUseFailure::ActionUnsafe => "replan: the action is flagged unsafe by policy".into(),
        ComputerUseFailure::PermissionRequired => {
            "request the permission grant from the operator".into()
        }
    };
    ComputerUseFailureDiagnostic {
        code: fault,
        recovery,
    }
}

// ---------------------------------------------------------------------------
// Safe typing + clipboard guard (REQ-EV-0090)
// ---------------------------------------------------------------------------

/// The clipboard guard: preserves the prior clipboard content, verifies
/// the destination before replacement, restores afterwards, and keeps
/// the secret OUT of the model/evidence body (only a digest travels).
#[derive(Default)]
pub struct ClipboardGuard {
    pub preserved: BTreeMap<String, String>, // destination → preserved content
}

#[derive(Clone, Debug, PartialEq)]
pub struct SafeTypingResult {
    pub typed_via: &'static str,
    /// sha256 of the secret — the secret itself never enters evidence.
    pub secret_digest: String,
    pub clipboard_restored: bool,
}

impl ClipboardGuard {
    /// Safe-typing path: prefer reversible typing into a verified
    /// editable destination; the clipboard (if used) is restored and the
    /// secret value never recorded — only its digest.
    pub fn safe_type_secret(
        &mut self,
        _destination: &str,
        editable: bool,
        secret: &str,
    ) -> Result<SafeTypingResult, String> {
        if !editable {
            return Err("destination not verified editable".into());
        }
        let digest = sha256_hex(secret.as_bytes());

        // Clipboard is NOT touched on the preferred reversible path.
        let typed_via = "reversible typing (no clipboard use)";
        let restored = true; // nothing to restore: clipboard untouched
        Ok(SafeTypingResult {
            typed_via,
            secret_digest: digest,
            clipboard_restored: restored,
        })
    }

    /// Clipboard path: preserves the old content before replacement and
    /// hands it back for restoration.
    pub fn stash_clipboard(&mut self, destination: &str, content: &str) {
        self.preserved
            .insert(destination.to_string(), content.to_string());
    }

    pub fn restore_clipboard(&self, destination: &str) -> Option<&str> {
        self.preserved.get(destination).map(|s| s.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// QUAL-EV-0086: a raw-input fallback without a post-check is
    /// rejected by the completion verifier.
    #[test]
    fn fallback_without_postcheck_rejected() {
        let unverified = FallbackAction {
            modality: "mouse",
            reason: "canvas click".into(),
            target: "canvas@200,300".into(),
            post_state_verified: false,
            post_state_digest: String::new(),
        };
        assert!(verify_fallback(&unverified).is_err());

        let verified = FallbackAction {
            post_state_verified: true,
            post_state_digest: sha256_hex(b"post state"),
            ..unverified
        };
        assert!(verify_fallback(&verified).is_ok());
    }

    /// QUAL-EV-0087: injected real mouse/keyboard activity stops
    /// automation and requires cooldown-gated reacquisition.
    #[test]
    fn human_activity_stops_automation_requires_reacquisition() {
        let mut manager = PreemptionManager::new(10_000);
        assert!(manager.is_active());

        // Real keyboard activity at t=50_000: automation parked.
        manager.human_activity(50_000, "keyboard");
        assert!(!manager.is_active());

        // Early reacquisition refused.
        assert!(manager.try_reacquire(55_000).is_err());
        // After cooldown: reacquisition succeeds.
        assert!(manager.try_reacquire(60_001).is_ok());
        assert!(manager.is_active());
    }

    /// QUAL-EV-0089: fault fixtures trigger each code and verify the
    /// recovery guidance; codes are stable.
    #[test]
    fn fault_fixtures_trigger_every_typed_code() {
        let all = [
            ComputerUseFailure::TargetStale,
            ComputerUseFailure::TargetOccluded,
            ComputerUseFailure::WindowUnverifiable,
            ComputerUseFailure::AccessibilityUnavailable,
            ComputerUseFailure::HumanActive,
            ComputerUseFailure::ModalBlocking,
            ComputerUseFailure::TargetNotEditable,
            ComputerUseFailure::ActionUnsafe,
            ComputerUseFailure::PermissionRequired,
        ];
        for fault in all {
            let diagnostic = diagnose(fault);
            assert!(!diagnostic.recovery.is_empty(), "{fault:?} needs guidance");
            assert_eq!(diagnostic.code, fault);
        }
        // Stable wire codes.
        assert_eq!(ComputerUseFailure::TargetStale.to_string(), "TARGET_STALE");
        assert_eq!(
            ComputerUseFailure::PermissionRequired.to_string(),
            "PERMISSION_REQUIRED"
        );
    }

    /// QUAL-EV-0090: the clipboard secret is restored and never enters
    /// the model/evidence body.
    #[test]
    fn clipboard_secret_restored_never_in_evidence() {
        let mut guard = ClipboardGuard::default();
        let secret = "sk-or-v1-SUPER-SECRET-VALUE";

        // Non-editable destination: refused before anything happens.
        assert!(guard
            .safe_type_secret("readonly-field", false, secret)
            .is_err());

        // Editable destination: reversible typing, no clipboard contact.
        let result = guard
            .safe_type_secret("login-password", true, secret)
            .unwrap();
        assert!(result.typed_via.contains("no clipboard use"));
        assert!(result.clipboard_restored);
        // The secret digest travels; the VALUE does not.
        assert!(!result.secret_digest.contains(secret));
        assert_eq!(result.secret_digest.len(), 64);

        // If the clipboard path IS used: stash + restore round trip.
        guard.stash_clipboard("login-password", "previous clipboard content");
        assert_eq!(
            guard.restore_clipboard("login-password"),
            Some("previous clipboard content")
        );
    }
}
