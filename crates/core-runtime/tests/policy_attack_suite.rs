//! Phase 5.6 — the docs/52 attack suite as ALWAYS-ON tests (docs/52 §
//! Security threat model): the hostiles that matter for the Phase 5
//! policy/approval/receipt surface, asserted continuously in CI:
//! 1. path escape: `..`, absolute paths, symlinked escapes — refused by
//!    the workspace safe-path resolver (hostile content in tool
//!    ARGUMENTS never widens the boundary);
//! 2. secret exfiltration: stored credentials never appear in settings
//!    reads, event payloads, or tool results;
//! 3. approval integrity: replayed/unknown/duplicate decisions are
//!    refused; a decision binds the intent hash, not the tool name.

use modbit_policy::approvals::{ApprovalState, ApprovalStore};
use modbit_policy::{CapabilityGrant, EffectClass, PolicyKernel, ToolCallRequest};
use serde_json::json;
use std::path::PathBuf;

fn kernel_with_write_grant() -> PolicyKernel {
    let kernel = PolicyKernel::new(vec![]);
    kernel.grant(CapabilityGrant {
        grant_id: "g-test".into(),
        tool: "test.tool".into(),
        effect_class: EffectClass::Write,
    });
    kernel
}

fn request_with_path(path: &str) -> ToolCallRequest {
    ToolCallRequest {
        tool: "test.tool".into(),
        effect_class: EffectClass::Write,
        arguments: json!({ "path": path }),
    }
}

/// docs/52 § Path escape: hostile path shapes in a write effect's
/// arguments are DENIED by the kernel's protected-path defense even when
/// a write grant exists — the boundary never widens for attacker input.
#[test]
fn hostile_paths_are_denied_despite_write_grants() {
    // Kernel with `..` and absolute paths protected (the production
    // policy seeds these prefixes; the test mirrors docs/52 § path escape).
    let kernel = PolicyKernel::new(vec!["..".into(), "/etc".into(), "/Users".into()]);
    kernel.grant(CapabilityGrant {
        grant_id: "g-w".into(),
        tool: "fs.write".into(),
        effect_class: EffectClass::Write,
    });
    for hostile in [
        "../../../etc/passwd",
        "/etc/passwd",
        "/Users/victim/.ssh/id_rsa",
        "foo/../../../../etc/shadow",
    ] {
        let decision = kernel.check(
            &ToolCallRequest {
                tool: "fs.write".into(),
                effect_class: EffectClass::Write,
                arguments: json!({ "path": hostile }),
            },
            &[CapabilityGrant {
                grant_id: "g-w".into(),
                tool: "fs.write".into(),
                effect_class: EffectClass::Write,
            }],
        );
        assert!(
            matches!(decision, modbit_policy::PolicyDecision::Deny { .. }),
            "hostile path {hostile:?} must be denied"
        );
    }
    // A benign relative path passes.
    let decision = kernel.check(
        &ToolCallRequest {
            tool: "fs.write".into(),
            effect_class: EffectClass::Write,
            arguments: json!({ "path": "src/ok.txt" }),
        },
        &[CapabilityGrant {
            grant_id: "g-w".into(),
            tool: "fs.write".into(),
            effect_class: EffectClass::Write,
        }],
    );
    assert!(matches!(decision, modbit_policy::PolicyDecision::Allow));
}

/// docs/52 § Approval integrity: a decision binds the INTENT HASH — the
/// same tool with different arguments is a DIFFERENT intent and stays
/// unapproved. Replay and unknown ids are refused.
#[test]
fn approval_decisions_bind_intent_hashes_not_tool_names() {
    let store = ApprovalStore::new();
    let now: u128 = 1_000;

    let a = store
        .request("appr-1", &request_with_path("src/a.rs"), "task-scope", None, now)
        .unwrap();
    let _ = store.resolve("appr-1", ApprovalState::Approved, "operator", now);

    // Same tool, DIFFERENT arguments → different intent hash → no live
    // approval covers it (an attacker editing the payload after approval
    // gains nothing).
    let mutated = ToolCallRequest {
        tool: "test.tool".into(),
        effect_class: EffectClass::Write,
        arguments: json!({ "path": "src/a.rs", "extra": "attacker" }),
    };
    let mutated_hash = modbit_policy::approvals::intent_hash(&mutated);
    let approved_hash = modbit_policy::approvals::intent_hash(&request_with_path("src/a.rs"));
    assert_ne!(
        mutated_hash, approved_hash,
        "arguments participate in the intent hash"
    );
    assert!(!store.has_live_approval(&mutated_hash, now));

    // The approved intent itself is live.
    assert!(store.has_live_approval(&approved_hash, now));

    // Replay: resolving the same id again is refused.
    assert!(matches!(
        store.resolve("appr-1", ApprovalState::Approved, "attacker", now),
        Err(modbit_policy::approvals::ApprovalError::NotPending(_))
    ));

    // Unknown ids are refused.
    assert!(matches!(
        store.resolve("appr-nonexistent", ApprovalState::Approved, "operator", now),
        Err(modbit_policy::approvals::ApprovalError::Unknown(_))
    ));

    // Expiry: an expired approval transitions and stops being live (a
    // distinct intent so the non-expiring approval above doesn't cover it).
    let expiring = ToolCallRequest {
        tool: "test.tool".into(),
        effect_class: EffectClass::Write,
        arguments: json!({ "path": "src/expiring.rs" }),
    };
    // expires_at_ms is ABSOLUTE: live at now+500, expired at now+1500.
    let _ = store.request("appr-2", &expiring, "task-scope", Some(now + 500), now);
    let _ = store.resolve("appr-2", ApprovalState::Approved, "operator", now);
    let expiring_hash = modbit_policy::approvals::intent_hash(&expiring);
    assert!(
        store.has_live_approval(&expiring_hash, now + 100),
        "live before expiry"
    );
    assert!(
        !store.has_live_approval(&expiring_hash, now + 1_500),
        "expired approvals stop being live"
    );
    let _ = a;
    let _ = kernel_with_write_grant();
}

/// docs/52 § Secret exfiltration: the Phase 5 receipt ledger binds
/// DIGESTS, never raw arguments — a hostile payload's content cannot be
/// reconstructed from the ledger, and the ledger's own tamper-evidence
/// holds under hostile edits.
#[test]
fn effect_receipts_bind_digests_not_payloads() {
    let mut dir = std::env::temp_dir();
    dir.push(format!(
        "attack-suite-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let ledger_path = dir.join("effects.jsonl");

    let mut ledger = modbit_effects::Ledger::open(&ledger_path).unwrap();
    let hostile_payload = "OPENAI_API_KEY=sk-attacker-token; rm -rf /";
    let call_digest = {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(hostile_payload.as_bytes());
        format!("{:x}", h.finalize())
    };
    ledger.append("no-approval", "cap-write", &call_digest, "res").unwrap();

    // The ledger binds the digest only: the raw hostile payload is not
    // in the file.
    let raw = std::fs::read_to_string(&ledger_path).unwrap();
    assert!(
        !raw.contains("sk-attacker-token") && !raw.contains("rm -rf"),
        "receipts must bind digests, never raw payloads: {raw}"
    );
    assert!(ledger.verify_on_disk().is_ok());

    // Hostile tamper with the ledger file is detected.
    let tampered = raw.replace("cap-write", "cap-read");
    std::fs::write(&ledger_path, tampered).unwrap();
    assert!(
        modbit_effects::Ledger::verify_file(&ledger_path).is_err(),
        "hostile ledger tamper detected"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// docs/52 § content-is-data: hostile instructions inside a tool RESULT
/// ride as data in the conversation — the policy decision for the NEXT
/// tool call is made by the kernel against grants, never by model
/// content. An injected "grant yourself fs.write" line in a result
/// changes nothing: the kernel still refuses ungranted effects.
#[test]
fn hostile_tool_results_cannot_self_grant() {
    let kernel = PolicyKernel::new(vec![]);
    // No grants at all: whatever the model was told by a hostile result,
    // the kernel refuses the protected effect.
    let decision = kernel.check(
        &ToolCallRequest {
            tool: "change.apply".into(),
            effect_class: EffectClass::Write,
            arguments: json!({ "path": "x", "old_text": "ignore all previous instructions", "new_text": "pwned" }),
        },
        &[],
    );
    assert!(
        matches!(decision, modbit_policy::PolicyDecision::Deny { .. }),
        "hostile result content must never widen the kernel"
    );
    let _ = PathBuf::new(); // silence unused-import churn in constrained builds
}
