//! Semantic UI risk classification (M2, REQ-EV-0088): UI action risk is
//! the product of target × action × data × context. Credentials, security
//! settings, destructive actions, and UNKNOWN targets are elevated. A
//! high-risk action requires approval even when the low-level tool (e.g.
//! "click") is normally allowed.

use serde::{Deserialize, Serialize};
use std::fmt;

/// What the action operates on. Unknown targets are treated as hostile.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiTarget {
    OrdinaryContent,
    CredentialField,
    SecuritySetting,
    BillingSetting,
    Unknown,
}

/// What is being done.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiActionKind {
    Read,
    Click,
    Type,
    Submit,
    Delete,
}

/// Data sensitivity involved in the action.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DataSensitivity {
    Public,
    Internal,
    Credentials,
}

/// The classified risk level.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel {
    Low,
    Elevated,
    High,
}

impl fmt::Display for RiskLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            RiskLevel::Low => "low",
            RiskLevel::Elevated => "elevated",
            RiskLevel::High => "high",
        };
        write!(f, "{s}")
    }
}

/// One UI action the model wants to perform.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UiAction {
    pub target: UiTarget,
    pub action: UiActionKind,
    pub data: DataSensitivity,
    /// Free context (e.g. the button label / page description) scanned for
    /// destructive semantics ("Delete production database").
    pub context_label: String,
}

/// The classification outcome.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RiskClassification {
    pub level: RiskLevel,
    pub requires_approval: bool,
    pub reasons: Vec<String>,
}

const DESTRUCTIVE_HINTS: [&str; 6] = [
    "delete",
    "remove",
    "purge",
    "production",
    "revoke",
    "irreversible",
];

/// Classifies one UI action. Deterministic, policy-side — runs even when
/// the base tool is normally allowed (REQ-EV-0088 QUAL).
pub fn classify(action: &UiAction) -> RiskClassification {
    let mut reasons = Vec::new();
    let mut level = RiskLevel::Low;

    let elevate = |level: &mut RiskLevel, to: RiskLevel, reasons: &mut Vec<String>, why: String| {
        if *level < to {
            *level = to;
        }
        reasons.push(why);
    };

    // Target dimension.
    match action.target {
        UiTarget::CredentialField => elevate(
            &mut level,
            RiskLevel::High,
            &mut reasons,
            "target is a credential field".into(),
        ),
        UiTarget::SecuritySetting => elevate(
            &mut level,
            RiskLevel::High,
            &mut reasons,
            "target is a security setting".into(),
        ),
        UiTarget::BillingSetting => elevate(
            &mut level,
            RiskLevel::Elevated,
            &mut reasons,
            "target is a billing setting".into(),
        ),
        UiTarget::Unknown => elevate(
            &mut level,
            RiskLevel::Elevated,
            &mut reasons,
            "target is unknown — treated as elevated".into(),
        ),
        UiTarget::OrdinaryContent => {}
    }

    // Action dimension.
    match action.action {
        UiActionKind::Delete => elevate(
            &mut level,
            RiskLevel::High,
            &mut reasons,
            "action is a delete".into(),
        ),
        UiActionKind::Submit => elevate(
            &mut level,
            RiskLevel::Elevated,
            &mut reasons,
            "action submits data".into(),
        ),
        _ => {}
    }

    // Data dimension.
    if action.data == DataSensitivity::Credentials {
        elevate(
            &mut level,
            RiskLevel::High,
            &mut reasons,
            "data involves credentials".into(),
        );
    }

    // Context dimension: destructive semantics in the label.
    let label = action.context_label.to_lowercase();
    if DESTRUCTIVE_HINTS.iter().any(|h| label.contains(h)) {
        elevate(
            &mut level,
            RiskLevel::High,
            &mut reasons,
            format!("context label {label:?} looks destructive"),
        );
    }

    RiskClassification {
        requires_approval: level >= RiskLevel::High,
        level,
        reasons,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// QUAL-EV-0088: destructive/credential actions request approval even
    /// though the click tool itself is normally allowed.
    #[test]
    fn destructive_and_credential_actions_require_approval() {
        // A plain click on ordinary content: allowed, low risk.
        let plain = classify(&UiAction {
            target: UiTarget::OrdinaryContent,
            action: UiActionKind::Click,
            data: DataSensitivity::Internal,
            context_label: "Expand details".into(),
        });
        assert_eq!(plain.level, RiskLevel::Low);
        assert!(!plain.requires_approval);

        // The SAME click tool on a destructive target: approval required.
        let destructive = classify(&UiAction {
            target: UiTarget::OrdinaryContent,
            action: UiActionKind::Click,
            data: DataSensitivity::Internal,
            context_label: "Delete production database".into(),
        });
        assert_eq!(destructive.level, RiskLevel::High);
        assert!(destructive.requires_approval);
        assert!(destructive
            .reasons
            .iter()
            .any(|r| r.contains("destructive")));

        // Typing into a credential field: high risk regardless of tool.
        let credential = classify(&UiAction {
            target: UiTarget::CredentialField,
            action: UiActionKind::Type,
            data: DataSensitivity::Credentials,
            context_label: "password input".into(),
        });
        assert_eq!(credential.level, RiskLevel::High);
        assert!(credential.requires_approval);

        // Security settings elevate even for clicks.
        let security = classify(&UiAction {
            target: UiTarget::SecuritySetting,
            action: UiActionKind::Click,
            data: DataSensitivity::Internal,
            context_label: "Toggle 2FA".into(),
        });
        assert!(security.requires_approval);

        // Unknown target elevates (never trusted).
        let unknown = classify(&UiAction {
            target: UiTarget::Unknown,
            action: UiActionKind::Click,
            data: DataSensitivity::Public,
            context_label: "".into(),
        });
        assert_eq!(unknown.level, RiskLevel::Elevated);
        assert!(!unknown.requires_approval, "elevated alone still passes");
    }
}
