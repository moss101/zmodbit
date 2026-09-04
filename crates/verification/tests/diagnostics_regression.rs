//! Regression-only diagnostics comparison tests (IMP-EV-0018, QUAL-EV-0018).
use modbit_verification::diagnostics::{compare, Diagnostic};

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

#[test]
fn preexisting_errors_not_blamed_on_change() {
    let pre = vec![
        diag("lib.rs", 5, "old error"),
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
fn clean_baseline_and_dirty_post() {
    let pre: Vec<Diagnostic> = vec![];
    let post = vec![
        diag("main.rs", 1, "borrow checker"),
        diag("util.rs", 7, "overflow"),
    ];
    let diff = compare(&pre, &post);
    assert_eq!(diff.regressions.len(), 2);
    assert!(diff.preexisting.is_empty());
}
