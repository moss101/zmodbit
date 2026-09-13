//! Guest loss recovery (M8.9, E2E-018, docs/54 fault catalog: sandbox
//! loss mid-task): when a guest dies with an operation in flight, the
//! operation's OUTCOME IS UNKNOWN — the request may or may not have taken
//! effect before the process vanished. Recovery NEVER blindly replays:
//!
//! - fs.write is reconciled by READ-BACK against the fresh (checkpoint-
//!   restored) sandbox: content present and identical → already applied
//!   (no duplicate effect); absent → re-executed once (content-identical
//!   write is idempotent); divergent content → unresolved (the task must
//!   restore from checkpoint).
//! - process execution is NEVER reconciled (a re-run is a duplicate
//!   external effect by definition) — the plan marks it ambiguous and the
//!   task resumes from checkpoint.
//!
//! Canonical owner subsystem: sandbox-cloud (docs/81).

use crate::guest_client::{GuestClient, GuestClientError};
use modbit_protocol::guest::GuestOp;

/// What happened to one lost operation after reconciliation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LostOutcome {
    /// The fresh sandbox already holds the exact effect — nothing was
    /// re-executed.
    AlreadyApplied,
    /// The effect was absent; the content-identical operation was
    /// executed exactly once against the fresh sandbox.
    Reexecuted,
    /// The sandbox state diverges (different bytes at the target) — the
    /// operation cannot be reconciled without a checkpoint restore.
    Unresolved(String),
}

#[derive(Debug)]
pub enum RecoveryError {
    /// The fresh guest refused or failed the reconciliation I/O.
    Guest(String),
    /// Reconciliation is not defined for this operation class.
    NotReconcilable(&'static str),
}

impl std::fmt::Display for RecoveryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RecoveryError::Guest(e) => write!(f, "recovery against fresh guest failed: {e}"),
            RecoveryError::NotReconcilable(class) => {
                write!(
                    f,
                    "operation class {class} is not reconcilable — restore from checkpoint"
                )
            }
        }
    }
}

impl std::error::Error for RecoveryError {}

/// True when a client error means the operation's OUTCOME IS UNKNOWN and
/// reconciliation is required: the connection dropped mid-conversation,
/// or the bounded wait expired without an answer (a wedged or dead guest
/// is indistinguishable from the caller's side — both leave the effect
/// unverified). Typed guest refusals and protocol garbage are NOT losses:
/// the guest answered, so the outcome is known.
pub fn is_guest_lost(err: &GuestClientError) -> bool {
    match err {
        GuestClientError::Transport(e) => {
            let e = e.to_lowercase();
            e.contains("connection reset")
                || e.contains("closed")
                || e.contains("eof")
                || e.contains("broken pipe")
                || e.contains("unexpected end")
                || e.contains("temporarily unavailable")
                || e.contains("timed out")
                // Windows socket phrasing (WSAETIMEDOUT/WSAECONNABORTED):
                || e.contains("did not properly respond")
                || e.contains("connection attempt failed")
                || e.contains("connection aborted")
                || e.contains("software caused connection abort")
        }
        _ => false,
    }
}

/// Reconciles ONE lost `fs.write` against a fresh sandbox: read back the
/// exact bytes; decide AlreadyApplied / Reexecuted / Unresolved. The
/// guest-level idempotency cache is NOT relied on — a fresh process has
/// none; the read-back is the source of truth.
pub fn reconcile_fs_write(
    fresh: &mut GuestClient,
    capability_token: &str,
    generation: u64,
    path: &str,
    expected_base64: &str,
) -> Result<LostOutcome, RecoveryError> {
    use base64::Engine as _;
    let expected = base64::engine::general_purpose::STANDARD
        .decode(expected_base64)
        .map_err(|e| RecoveryError::Guest(format!("bad expected payload: {e}")))?;
    let read = fresh.call(
        capability_token,
        generation,
        GuestOp::FsRead {
            path: path.to_string(),
        },
    );
    match read {
        Ok(modbit_protocol::guest::GuestPayload::Data {
            bytes_base64,
            sha256,
        }) => {
            let got = base64::engine::general_purpose::STANDARD
                .decode(&bytes_base64)
                .map_err(|e| RecoveryError::Guest(format!("bad read payload: {e}")))?;
            if got == expected {
                use sha2::{Digest, Sha256};
                let mut h = Sha256::new();
                h.update(&got);
                debug_assert_eq!(format!("{:x}", h.finalize()), sha256);
                return Ok(LostOutcome::AlreadyApplied);
            }
            Ok(LostOutcome::Unresolved(format!(
                "path {path:?} holds different bytes — restore from checkpoint"
            )))
        }
        Ok(_) => Err(RecoveryError::Guest("unexpected read payload".into())),
        Err(GuestClientError::Guest(e)) if e.code == "not_found" => {
            // Absent → the lost write never landed. Re-execute exactly
            // once (content-identical fs write is idempotent).
            fresh
                .call(
                    capability_token,
                    generation,
                    GuestOp::FsWrite {
                        path: path.to_string(),
                        bytes_base64: expected_base64.to_string(),
                    },
                )
                .map_err(|e| RecoveryError::Guest(e.to_string()))?;
            // Prove the re-execution landed.
            match fresh.call(
                capability_token,
                generation,
                GuestOp::FsRead {
                    path: path.to_string(),
                },
            ) {
                Ok(modbit_protocol::guest::GuestPayload::Data { bytes_base64, .. }) => {
                    let got = base64::engine::general_purpose::STANDARD
                        .decode(&bytes_base64)
                        .map_err(|e| RecoveryError::Guest(format!("bad read payload: {e}")))?;
                    if got == expected {
                        Ok(LostOutcome::Reexecuted)
                    } else {
                        Ok(LostOutcome::Unresolved(format!(
                            "re-execution diverged at {path:?}"
                        )))
                    }
                }
                other => Err(RecoveryError::Guest(format!("verify read: {other:?}"))),
            }
        }
        Err(e) => Err(RecoveryError::Guest(e.to_string())),
    }
}

/// Process execution is never reconcilable: re-running a lost proc is a
/// duplicate external effect by definition. The typed answer steers the
/// caller to checkpoint restore.
pub fn reconcile_proc_exec() -> Result<LostOutcome, RecoveryError> {
    Err(RecoveryError::NotReconcilable("proc.exec"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The classification surface: transport losses are typed; guest
    /// refusals and protocol garbage are not.
    #[test]
    fn loss_classification() {
        assert!(is_guest_lost(&GuestClientError::Transport(
            "connection reset by peer".into()
        )));
        assert!(is_guest_lost(&GuestClientError::Transport(
            "broken pipe".into()
        )));
        assert!(is_guest_lost(&GuestClientError::Transport(
            "transport io: A connection attempt failed because the connected party did not properly respond after a period of time".into()
        )));
        assert!(is_guest_lost(&GuestClientError::Transport(
            "transport io: A connection attempt failed because the connected party did not properly respond after a period of time or established connection failed because connected host has failed to respond".into()
        )));
        assert!(!is_guest_lost(&GuestClientError::Guest(
            modbit_protocol::guest::GuestError::new("fenced", "stale generation")
        )));
        assert!(!is_guest_lost(&GuestClientError::Identity(
            modbit_protocol::guest::GuestIdentityError::BadSignature
        )));
    }

    /// proc.exec reconciliation is refused BY DESIGN (duplicate external
    /// effect) — the caller is steered to checkpoint restore.
    #[test]
    fn proc_exec_not_reconcilable() {
        match reconcile_proc_exec() {
            Err(RecoveryError::NotReconcilable(class)) => assert_eq!(class, "proc.exec"),
            other => panic!("expected NotReconcilable, got {other:?}"),
        }
    }
}
