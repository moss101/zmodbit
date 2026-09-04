//! Policy before execution (IMP-EV-0080): the capability kernel check runs
//! BEFORE the tool handler — never after. Fail-closed by design.
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub tool: String,
    pub effect_class: String,
    pub path: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PolicyCheck {
    pub allowed: bool,
    pub reason: String,
}

/// Fail-closed: only explicit allow proceeds to execution.
pub fn check_policy(check: &PolicyCheck) -> Result<(), String> {
    if check.allowed {
        Ok(())
    } else {
        Err(format!("policy denied: {}", check.reason))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allow_proceeds() {
        let check = PolicyCheck {
            allowed: true,
            reason: "granted".into(),
        };
        assert!(check_policy(&check).is_ok());
    }

    #[test]
    fn deny_blocks() {
        let check = PolicyCheck {
            allowed: false,
            reason: "no grant".into(),
        };
        assert!(check_policy(&check).is_err());
    }
}
