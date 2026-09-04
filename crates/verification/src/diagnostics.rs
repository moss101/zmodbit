//! Diagnostics comparison — regression-only attribution (M2, REQ-EV-0018,
//! IMP-EV-0018, docs/18). Compares pre-edit and post-edit diagnostic sets
//! and attributes ONLY the newly-introduced issues to the change. Fixture
//! with pre-existing errors proves baseline issues are not blamed on the
//! change (docs/50).

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub file: String,
    pub line: usize,
    pub column: usize,
    pub severity: String,
    pub message: String,
    pub code: Option<String>,
}

impl Diagnostic {
    /// Stable identity for matching pre/post pairs.
    fn key(&self) -> String {
        format!(
            "{}:{}:{}:{}:{}",
            self.file, self.line, self.column, self.severity, self.message
        )
    }
}

/// Result of comparing two diagnostic snapshots.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DiagnosticDiff {
    /// New issues introduced by the change — these are regressions.
    pub regressions: Vec<Diagnostic>,
    /// Issues that existed before and are now gone — improvements.
    pub resolved: Vec<Diagnostic>,
    /// Issues that existed before and still exist — not the change's fault.
    pub preexisting: Vec<Diagnostic>,
}

/// Compares pre-edit and post-edit diagnostic sets, attributing only
/// introduced regressions.
pub fn compare(pre: &[Diagnostic], post: &[Diagnostic]) -> DiagnosticDiff {
    let pre_keys: std::collections::HashSet<String> = pre.iter().map(|d| d.key()).collect();
    let post_keys: std::collections::HashSet<String> = post.iter().map(|d| d.key()).collect();

    let regressions: Vec<Diagnostic> = post
        .iter()
        .filter(|d| !pre_keys.contains(&d.key()))
        .cloned()
        .collect();
    let resolved: Vec<Diagnostic> = pre
        .iter()
        .filter(|d| !post_keys.contains(&d.key()))
        .cloned()
        .collect();
    let preexisting: Vec<Diagnostic> = post
        .iter()
        .filter(|d| pre_keys.contains(&d.key()))
        .cloned()
        .collect();

    DiagnosticDiff {
        regressions,
        resolved,
        preexisting,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diag(file: &str, line: usize, msg: &str) -> Diagnostic {
        Diagnostic {
            file: file.into(),
            line,
            column: 1,
            severity: "error".into(),
            message: msg.into(),
            code: None,
        }
    }

    /// QUAL-EV-0018: fixture with pre-existing errors proves baseline issues
    /// are not blamed on the change.
    #[test]
    fn only_introduced_regressions_are_attributed() {
        let pre = vec![
            diag("lib.rs", 5, "unused variable"),
            diag("lib.rs", 10, "type mismatch"),
        ];
        let post = vec![
            diag("lib.rs", 10, "type mismatch"),
            diag("new.rs", 3, "missing semicolon"),
        ];

        let diff = compare(&pre, &post);
        assert_eq!(diff.regressions.len(), 1);
        assert_eq!(diff.regressions[0].file, "new.rs");
        assert_eq!(diff.preexisting.len(), 1);
        assert_eq!(diff.resolved.len(), 1);
    }

    #[test]
    fn no_change_means_no_regressions() {
        let diags = vec![diag("lib.rs", 5, "unused variable")];
        let diff = compare(&diags, &diags);
        assert!(diff.regressions.is_empty());
        assert!(diff.resolved.is_empty());
        assert_eq!(diff.preexisting.len(), 1);
    }

    #[test]
    fn clean_baseline_all_regressions() {
        let pre: Vec<Diagnostic> = vec![];
        let post = vec![
            diag("main.rs", 1, "borrow checker"),
            diag("util.rs", 7, "overflow"),
        ];
        let diff = compare(&pre, &post);
        assert_eq!(diff.regressions.len(), 2);
        assert!(diff.preexisting.is_empty());
    }
}
