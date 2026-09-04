//! Patch policy gate (IMP-EV-0071): validates patch hunks against the
//! policy kernel before applying — fail-closed on forbidden paths.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PatchRequest {
    pub path: String,
    pub old_content: String,
    pub new_content: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PatchVerdict {
    Allow,
    Deny { reason: String },
}

/// Validates a patch request against the policy rules.
pub fn check_patch(
    request: &PatchRequest,
    protected_prefixes: &[String],
    max_change_ratio: f64,
) -> PatchVerdict {
    for prefix in protected_prefixes {
        if request.path.starts_with(prefix.as_str()) {
            return PatchVerdict::Deny {
                reason: format!("path {:?} is protected", request.path),
            };
        }
    }
    if request.old_content.is_empty() && request.new_content.is_empty() {
        return PatchVerdict::Deny {
            reason: "empty patch".into(),
        };
    }
    let old_len = request.old_content.len();
    let new_len = request.new_content.len();
    let total = old_len.max(new_len).max(1);
    let changed = if old_len == new_len {
        request
            .old_content
            .bytes()
            .zip(request.new_content.bytes())
            .filter(|(a, b)| a != b)
            .count()
    } else {
        old_len.abs_diff(new_len)
    };
    let ratio = changed as f64 / total as f64;
    if ratio > max_change_ratio {
        return PatchVerdict::Deny {
            reason: format!("change ratio {ratio:.2} exceeds {max_change_ratio}"),
        };
    }
    PatchVerdict::Allow
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allow_normal_patch() {
        let req = PatchRequest {
            path: "src/main.rs".into(),
            old_content: "old".into(),
            new_content: "new".into(),
        };
        let verdict = check_patch(&req, &["/protected"], 1.0);
        assert_eq!(verdict, PatchVerdict::Allow);
    }

    #[test]
    fn deny_protected_path() {
        let req = PatchRequest {
            path: "/protected/secret".into(),
            old_content: "old".into(),
            new_content: "new".into(),
        };
        let verdict = check_patch(&req, &["/protected"], 1.0);
        assert!(matches!(verdict, PatchVerdict::Deny { .. }));
    }
}
