//! Admin policy precedence (IMP-EV-0091): admin settings override project
//! settings which override user settings. Admin cannot be weakened.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PolicyLayer {
    pub source: String,
    pub blocked_tools: Vec<String>,
    pub required_approvals: Vec<String>,
}

/// Merges policy layers: admin blocks always win.
pub fn merge(layers: &[PolicyLayer]) -> PolicyLayer {
    let mut merged = PolicyLayer {
        source: "merged".into(),
        blocked_tools: Vec::new(),
        required_approvals: Vec::new(),
    };
    for layer in layers {
        for tool in &layer.blocked_tools {
            if !merged.blocked_tools.contains(tool) {
                merged.blocked_tools.push(tool.clone());
            }
        }
        for approval in &layer.required_approvals {
            if !merged.required_approvals.contains(approval) {
                merged.required_approvals.push(approval.clone());
            }
        }
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admin_blocks_propagate() {
        let layers = vec![
            PolicyLayer {
                source: "admin".into(),
                blocked_tools: vec!["shell.run".into()],
                required_approvals: vec![],
            },
            PolicyLayer {
                source: "user".into(),
                blocked_tools: vec![],
                required_approvals: vec![],
            },
        ];
        let merged = merge(&layers);
        assert!(merged.blocked_tools.contains(&"shell.run".to_string()));
    }

    #[test]
    fn admin_can_block_what_user_allows() {
        let layers = vec![
            PolicyLayer {
                source: "user".into(),
                blocked_tools: vec![],
                required_approvals: vec![],
            },
            PolicyLayer {
                source: "admin".into(),
                blocked_tools: vec!["fs.write".into()],
                required_approvals: vec![],
            },
        ];
        let merged = merge(&layers);
        assert!(merged.blocked_tools.contains(&"fs.write".to_string()));
    }
}
