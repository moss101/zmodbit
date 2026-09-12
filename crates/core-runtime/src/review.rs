//! Review checklist generation (Phase 5 residual): a DETERMINISTIC
//! function of the diff and its review state — never model output. Each
//! item is derived from the changed files, the hunk contents, and which
//! hunks already carry comments/decisions. Docs/14 § verification roles:
//! gates are deterministic where possible.

/// One changed hunk as the checklist sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HunkInput {
    pub path: String,
    pub new_start: usize,
    /// Raw -/+ lines of the hunk (with prefixes).
    pub lines: Vec<String>,
    /// A review decision was already recorded for this hunk.
    pub decided: bool,
    /// At least one inline comment is bound to this hunk.
    pub commented: bool,
}

/// One checklist item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChecklistItem {
    pub id: String,
    pub text: String,
    /// Done when the evidence the item asks for already exists.
    pub done: bool,
}

fn is_test_path(path: &str) -> bool {
    let lower = path.to_lowercase();
    lower.contains("test")
        || lower.contains("spec")
        || lower.ends_with("_test.go")
        || lower.starts_with("tests/")
}

/// Patterns that deserve a second look before merge (leftover debris,
/// loud failure paths, secret-shaped strings).
fn suspicious(line: &str) -> Option<&'static str> {
    let t = line.trim_start_matches(['+', '-']).trim();
    if t.contains("TODO") || t.contains("FIXME") {
        Some("leftover TODO/FIXME")
    } else if t.contains("unwrap()") {
        Some("unwrap() that may panic")
    } else if t.contains("dbg!") {
        Some("dbg! left in the diff")
    } else if t.to_lowercase().contains("api_key")
        || t.to_lowercase().contains("apikey")
        || t.contains("sk-")
    {
        Some("possible secret in the diff")
    } else {
        None
    }
}

/// Generates the checklist for one revision's diff. Order is stable:
/// per-file items first (file order), then per-hunk flags, then the two
/// always-on gates.
pub fn generate_checklist(hunks: &[HunkInput]) -> Vec<ChecklistItem> {
    let mut items: Vec<ChecklistItem> = Vec::new();
    let mut id = 0usize;
    let mut next_id = || {
        id += 1;
        format!("c{id:02}")
    };

    // Per-file: review + test-request items.
    let mut seen_files: Vec<&str> = Vec::new();
    for h in hunks {
        if !seen_files.contains(&h.path.as_str()) {
            seen_files.push(&h.path);
        }
    }
    for path in &seen_files {
        let count = hunks.iter().filter(|h| h.path == *path).count();
        items.push(ChecklistItem {
            id: next_id(),
            text: format!("Review {path} ({count} hunk{})", if count == 1 { "" } else { "s" }),
            done: false,
        });
    }
    for path in &seen_files {
        if is_test_path(path) {
            items.push(ChecklistItem {
                id: next_id(),
                text: format!("Run the tests touched by {path}"),
                done: false,
            });
        }
    }

    // Per-hunk: suspicious content + under-reviewed hunks.
    for h in hunks {
        for line in &h.lines {
            if let Some(why) = suspicious(line) {
                items.push(ChecklistItem {
                    id: next_id(),
                    text: format!("Check {} in {} @{}", why, h.path, h.new_start),
                    done: false,
                });
            }
        }
        if !h.commented && !h.decided {
            items.push(ChecklistItem {
                id: next_id(),
                text: format!(
                    "Comment or accept/reject {} @{} (no review recorded)",
                    h.path, h.new_start
                ),
                done: false,
            });
        }
    }

    // Always-on gates.
    items.push(ChecklistItem {
        id: next_id(),
        text: "Run the project test suite".into(),
        done: false,
    });
    items.push(ChecklistItem {
        id: next_id(),
        text: "No secrets in the diff".into(),
        done: false,
    });
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hunk(path: &str, new_start: usize, lines: &[&str]) -> HunkInput {
        HunkInput {
            path: path.into(),
            new_start,
            lines: lines.iter().map(|l| l.to_string()).collect(),
            decided: false,
            commented: false,
        }
    }

    #[test]
    fn checklist_is_deterministic_and_covers_the_rules() {
        let hunks = vec![
            hunk("src/api.rs", 2, &["-old", "+let v = x.unwrap();"]),
            hunk("src/api.rs", 40, &["+// TODO: tighten"]),
            hunk("tests/api_test.rs", 1, &["+assert!(true);"]),
        ];
        let a = generate_checklist(&hunks);
        let b = generate_checklist(&hunks);
        assert_eq!(a, b, "deterministic");

        let texts: Vec<&str> = a.iter().map(|i| i.text.as_str()).collect();
        assert!(texts.iter().any(|t| t.contains("Review src/api.rs (2 hunks)")));
        assert!(texts.iter().any(|t| t.contains("Review tests/api_test.rs")));
        assert!(texts.iter().any(|t| t.contains("Run the tests touched by tests/api_test.rs")));
        assert!(texts.iter().any(|t| t.contains("unwrap() that may panic")));
        assert!(texts.iter().any(|t| t.contains("leftover TODO/FIXME")));
        assert!(texts.iter().any(|t| t.starts_with("Comment or accept/reject src/api.rs @2")));
        assert!(texts.contains(&"Run the project test suite"));
        assert!(texts.contains(&"No secrets in the diff"));
    }

    #[test]
    fn decided_and_commented_hunks_need_no_review_nag() {
        let mut h = hunk("src/x.rs", 4, &["+ok"]);
        h.decided = true;
        let items = generate_checklist(&[h]);
        assert!(!items.iter().any(|i| i.text.contains("Comment or accept/reject")));

        let mut h2 = hunk("src/y.rs", 9, &["+ok"]);
        h2.commented = true;
        let items = generate_checklist(&[h2]);
        assert!(!items.iter().any(|i| i.text.contains("Comment or accept/reject")));
    }

    #[test]
    fn secret_shaped_lines_get_a_flag() {
        let h = hunk("src/config.rs", 3, &["+api_key: \"sk-abc123\""]);
        let items = generate_checklist(&[h]);
        assert!(items.iter().any(|i| i.text.contains("possible secret in the diff")));
    }
}
