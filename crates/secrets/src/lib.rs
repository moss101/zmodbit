//! modbit-secrets — the credential broker (M8.6, REQ-EV-0288, docs/23 §
//! Secrets): registers tenant secrets under opaque references, issues
//! SHORT-LIVED, task/generation/scope-scoped credential leases, and
//! materializes lease handles into credential values only at execution
//! time — never into images, configs, durable events, logs or transcripts.
//!
//! Canonical owner subsystem: effects-security (docs/81).
//!
//! Invariants (each one is asserted by a test):
//! - durable evidence carries lease ids and references, never values;
//! - expired, revoked or unknown leases fail CLOSED (typed errors);
//! - a lease is bound to ONE task and ONE generation — after a generation
//!   advance the old lease is fenced (retry cannot resurrect authority);
//! - materialization cannot widen scope: a retry re-presents the SAME
//!   lease under the SAME or narrower scope;
//! - revoking one task's leases never exposes another's (blast radius of
//!   a compromised guest is its own leases);
//! - `redact` scrubs known secret fingerprints out of logs/errors/terminal
//!   text before persistence.
//!
//! Test fixtures use synthetic secret values only.

use std::collections::HashMap;
use std::fmt;
use std::time::{Duration, Instant};

use serde::Serialize;
use sha2::{Digest, Sha256};

/// An opaque reference to a registered secret. Carries no value.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
pub struct SecretRef {
    pub id: String,
    pub name: String,
}

/// What a lease may be materialized FOR. A materialization request with a
/// scope outside the lease's grant is denied (no widening on retry).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum CredScope {
    /// Environment injection for a process execution.
    ProcEnv,
    /// HTTP egress authorization header construction.
    HttpHeader,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CredentialLease {
    pub lease_id: String,
    /// Opaque handle given to callers — NOT the secret.
    pub handle: String,
    pub task: String,
    pub generation: u64,
    pub secret_ref_id: String,
    pub scopes: Vec<CredScope>,
    pub expires_at: Instant,
}

#[derive(Debug)]
pub enum CredentialError {
    UnknownLease,
    Expired,
    Revoked,
    TaskMismatch {
        lease_task: String,
    },
    /// Lease generation is older than the current work generation — the
    /// authority was fenced; retry must obtain a fresh lease.
    Fenced {
        lease_generation: u64,
    },
    ScopeDenied {
        requested: CredScope,
    },
}

impl fmt::Display for CredentialError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CredentialError::UnknownLease => write!(f, "unknown credential lease (fail closed)"),
            CredentialError::Expired => write!(f, "credential lease expired"),
            CredentialError::Revoked => write!(f, "credential lease revoked"),
            CredentialError::TaskMismatch { lease_task } => {
                write!(f, "lease belongs to task {lease_task:?}, not this task")
            }
            CredentialError::Fenced { lease_generation } => {
                write!(
                    f,
                    "lease generation {lease_generation} fenced by generation advance"
                )
            }
            CredentialError::ScopeDenied { requested } => {
                write!(f, "scope {requested:?} not granted by this lease")
            }
        }
    }
}

impl std::error::Error for CredentialError {}

/// Audit event — references only, never values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CredentialEvent {
    pub kind: &'static str,
    pub lease_id: String,
    pub secret_ref_id: String,
    pub task: String,
}

enum LeaseState {
    Live,
    Revoked,
}

struct RegisteredSecret {
    value: Vec<u8>,
    #[allow(dead_code)]
    name: String,
}

/// The broker. Values live only in process memory; everything that
/// leaves (leases, audit) carries references.
#[derive(Default)]
pub struct CredentialBroker {
    secrets: HashMap<String, RegisteredSecret>,
    leases: HashMap<String, (CredentialLease, LeaseState)>,
    audit: Vec<CredentialEvent>,
    counter: u64,
}

impl CredentialBroker {
    pub fn new() -> Self {
        Default::default()
    }

    /// Registers one secret under an opaque reference. The reference id is
    /// derived from the value's fingerprint (so evidence can link leases
    /// to a SECRET CLASS without exposing it).
    pub fn register(&mut self, task: &str, name: &str, value: &[u8]) -> SecretRef {
        let id = {
            let mut h = Sha256::new();
            h.update(value);
            h.update(task.as_bytes());
            h.update(name.as_bytes());
            format!(
                "secr-{:016x}",
                &h.finalize()[..8]
                    .iter()
                    .fold(0u64, |a, b| (a << 8) | *b as u64)
            )
        };
        self.secrets.insert(
            id.clone(),
            RegisteredSecret {
                value: value.to_vec(),
                name: name.to_string(),
            },
        );
        SecretRef {
            id,
            name: name.to_string(),
        }
    }

    /// Issues a short-lived lease. `ttl` bounds its life; generation binds
    /// it to the current work generation so a fenced generation invalidates
    /// outstanding credentials.
    pub fn issue_lease(
        &mut self,
        secret_ref: &SecretRef,
        task: &str,
        generation: u64,
        scopes: Vec<CredScope>,
        ttl: Duration,
    ) -> Result<CredentialLease, CredentialError> {
        if !self.secrets.contains_key(&secret_ref.id) {
            return Err(CredentialError::UnknownLease);
        }
        self.counter += 1;
        let lease_id = format!("lease-{:08}", self.counter);
        let handle = {
            let mut h = Sha256::new();
            h.update(lease_id.as_bytes());
            h.update(task.as_bytes());
            format!(
                "cred-{:016x}",
                &h.finalize()[..8]
                    .iter()
                    .fold(0u64, |a, b| (a << 8) | *b as u64)
            )
        };
        let lease = CredentialLease {
            lease_id: lease_id.clone(),
            handle,
            task: task.to_string(),
            generation,
            secret_ref_id: secret_ref.id.clone(),
            scopes,
            expires_at: Instant::now() + ttl,
        };
        self.leases
            .insert(lease_id.clone(), (lease.clone(), LeaseState::Live));
        self.audit.push(CredentialEvent {
            kind: "issue",
            lease_id,
            secret_ref_id: lease.secret_ref_id.clone(),
            task: lease.task.clone(),
        });
        Ok(lease)
    }

    /// Materializes a lease handle into the credential value for ONE
    /// execution. Every authority dimension is checked before any value
    /// leaves the broker.
    pub fn materialize(
        &mut self,
        handle: &str,
        task: &str,
        current_generation: u64,
        scope: CredScope,
    ) -> Result<Vec<u8>, CredentialError> {
        let found = self
            .leases
            .values()
            .find(|(l, _)| l.handle == handle)
            .map(|(l, s)| (l.clone(), matches!(s, LeaseState::Live)));
        let Some((lease, live)) = found else {
            self.audit.push(CredentialEvent {
                kind: "denied-unknown",
                lease_id: String::new(),
                secret_ref_id: String::new(),
                task: task.to_string(),
            });
            return Err(CredentialError::UnknownLease);
        };
        let deny =
            |kind: &'static str, e: CredentialError, broker: &mut Self, l: &CredentialLease| {
                broker.audit.push(CredentialEvent {
                    kind,
                    lease_id: l.lease_id.clone(),
                    secret_ref_id: l.secret_ref_id.clone(),
                    task: l.task.clone(),
                });
                e
            };
        if !live {
            return Err(deny(
                "denied-revoked",
                CredentialError::Revoked,
                self,
                &lease,
            ));
        }
        if Instant::now() >= lease.expires_at {
            return Err(deny(
                "denied-expired",
                CredentialError::Expired,
                self,
                &lease,
            ));
        }
        if lease.task != task {
            return Err(deny(
                "denied-task-mismatch",
                CredentialError::TaskMismatch {
                    lease_task: lease.task.clone(),
                },
                self,
                &lease,
            ));
        }
        if lease.generation < current_generation {
            return Err(deny(
                "denied-fenced",
                CredentialError::Fenced {
                    lease_generation: lease.generation,
                },
                self,
                &lease,
            ));
        }
        if !lease.scopes.contains(&scope) {
            return Err(deny(
                "denied-scope",
                CredentialError::ScopeDenied { requested: scope },
                self,
                &lease,
            ));
        }
        let value = self
            .secrets
            .get(&lease.secret_ref_id)
            .map(|s| s.value.clone())
            .ok_or(CredentialError::UnknownLease)?;
        self.audit.push(CredentialEvent {
            kind: "materialize",
            lease_id: lease.lease_id,
            secret_ref_id: lease.secret_ref_id.clone(),
            task: task.to_string(),
        });
        Ok(value)
    }

    /// Revokes one lease immediately (fail closed from that instant).
    pub fn revoke(&mut self, lease_id: &str) {
        if let Some((lease, state)) = self.leases.get_mut(lease_id) {
            *state = LeaseState::Revoked;
            self.audit.push(CredentialEvent {
                kind: "revoke",
                lease_id: lease.lease_id.clone(),
                secret_ref_id: lease.secret_ref_id.clone(),
                task: lease.task.clone(),
            });
        }
    }

    /// Emergency-stop path: revokes EVERY lease of one task. A compromised
    /// guest loses only its own authority; other tasks keep theirs.
    pub fn revoke_task(&mut self, task: &str) {
        let ids: Vec<String> = self
            .leases
            .iter()
            .filter(|(_, (l, _))| l.task == task)
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            self.revoke(&id);
        }
    }

    /// Audit trail — references only (tests assert no secret bytes).
    pub fn audit(&self) -> &[CredentialEvent] {
        &self.audit
    }
}

/// Redacts every known secret fingerprint out of arbitrary text (logs,
/// error strings, terminal persistence, event payloads). Secrets are
/// replaced with a typed marker that carries the reference CLASS, not
/// the value.
pub fn redact(text: &str, known_values: &[&[u8]]) -> String {
    let mut out = text.to_string();
    for (i, secret) in known_values.iter().enumerate() {
        if secret.is_empty() {
            continue;
        }
        if let Ok(needle) = std::str::from_utf8(secret) {
            if out.contains(needle) {
                out = out.replace(needle, &format!("[REDACTED:secret-{i}]"));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // Synthetic fixtures only — never real credentials (contract).
    const SYNTH_TOKEN: &[u8] = b"synthetic-token-ABCDEF-do-not-use";
    const OTHER_TOKEN: &[u8] = b"synthetic-token-OTHER-task";

    fn broker() -> CredentialBroker {
        CredentialBroker::new()
    }

    /// The full lease path: register → issue → materialize returns the
    /// value for the right task/generation/scope within TTL.
    #[test]
    fn lease_materializes_for_authorized_request() {
        let mut b = broker();
        let r = b.register("task-1", "provider-api", SYNTH_TOKEN);
        let lease = b
            .issue_lease(
                &r,
                "task-1",
                0,
                vec![CredScope::ProcEnv, CredScope::HttpHeader],
                Duration::from_secs(60),
            )
            .expect("lease");
        // The handle is opaque — not the secret.
        assert!(!lease.handle.contains("synthetic"));
        let v = b
            .materialize(&lease.handle, "task-1", 0, CredScope::ProcEnv)
            .expect("materialize");
        assert_eq!(v, SYNTH_TOKEN);
        // Audit references only.
        let audit_text = format!("{:?}", b.audit());
        assert!(!audit_text.contains("synthetic"), "audit leaked values");
    }

    /// Every authority violation fails closed with a typed error.
    #[test]
    fn violations_fail_closed() {
        let mut b = broker();
        let r1 = b.register("task-1", "provider-api", SYNTH_TOKEN);
        let r2 = b.register("task-2", "provider-api", OTHER_TOKEN);
        let lease = b
            .issue_lease(
                &r1,
                "task-1",
                3,
                vec![CredScope::ProcEnv],
                Duration::from_secs(60),
            )
            .unwrap();
        let other = b
            .issue_lease(
                &r2,
                "task-2",
                0,
                vec![CredScope::ProcEnv],
                Duration::from_secs(60),
            )
            .unwrap();

        // Wrong task.
        assert!(matches!(
            b.materialize(&lease.handle, "task-2", 3, CredScope::ProcEnv),
            Err(CredentialError::TaskMismatch { .. })
        ));
        // Fenced generation: lease issued under gen 3, work advanced to 4.
        assert!(matches!(
            b.materialize(&lease.handle, "task-1", 4, CredScope::ProcEnv),
            Err(CredentialError::Fenced {
                lease_generation: 3
            })
        ));
        // Scope widening attempt: lease grants ProcEnv only.
        assert!(matches!(
            b.materialize(&lease.handle, "task-1", 3, CredScope::HttpHeader),
            Err(CredentialError::ScopeDenied { .. })
        ));
        // Unknown handle.
        assert!(matches!(
            b.materialize("cred-does-not-exist", "task-1", 3, CredScope::ProcEnv),
            Err(CredentialError::UnknownLease)
        ));
        // Revocation (emergency stop for task-2) — task-1 unaffected.
        b.revoke_task("task-2");
        assert!(matches!(
            b.materialize(&other.handle, "task-2", 0, CredScope::ProcEnv),
            Err(CredentialError::Revoked)
        ));
        assert!(b
            .materialize(&lease.handle, "task-1", 3, CredScope::ProcEnv)
            .is_ok());
    }

    /// Expiry is enforced (short TTL lease fails closed after the
    /// deadline).
    #[test]
    fn expired_lease_fails_closed() {
        let mut b = broker();
        let r = b.register("task-1", "provider-api", SYNTH_TOKEN);
        let lease = b
            .issue_lease(
                &r,
                "task-1",
                0,
                vec![CredScope::ProcEnv],
                Duration::from_millis(10),
            )
            .unwrap();
        std::thread::sleep(Duration::from_millis(40));
        assert!(matches!(
            b.materialize(&lease.handle, "task-1", 0, CredScope::ProcEnv),
            Err(CredentialError::Expired)
        ));
    }

    /// Guest-compromise blast radius: revoking everything of task-1 never
    /// affects task-2's credential; the audit trail records the full chain.
    #[test]
    fn task_scoped_revocation_and_audit_chain() {
        let mut b = broker();
        let r1 = b.register("task-1", "provider-api", SYNTH_TOKEN);
        let r2 = b.register("task-2", "provider-api", OTHER_TOKEN);
        let l1 = b
            .issue_lease(
                &r1,
                "task-1",
                0,
                vec![CredScope::ProcEnv],
                Duration::from_secs(60),
            )
            .unwrap();
        let l2 = b
            .issue_lease(
                &r2,
                "task-2",
                0,
                vec![CredScope::ProcEnv],
                Duration::from_secs(60),
            )
            .unwrap();
        b.revoke_task("task-1");
        assert!(matches!(
            b.materialize(&l1.handle, "task-1", 0, CredScope::ProcEnv),
            Err(CredentialError::Revoked)
        ));
        assert!(b
            .materialize(&l2.handle, "task-2", 0, CredScope::ProcEnv)
            .is_ok());
        let kinds: Vec<&'static str> = b.audit().iter().map(|e| e.kind).collect();
        assert!(kinds.contains(&"revoke"));
        assert!(kinds.contains(&"materialize"));
        assert!(!format!("{:?}", b.audit()).contains("synthetic"));
    }

    /// Redaction scrubs known fingerprints from log-shaped text.
    #[test]
    fn redaction_scrubs_known_fingerprints() {
        let out = redact(
            "request failed: Authorization: Bearer synthetic-token-ABCDEF-do-not-use (task-1)",
            &[SYNTH_TOKEN],
        );
        assert!(!out.contains("synthetic-token-ABCDEF"));
        assert!(out.contains("[REDACTED:secret-0]"));
        assert!(out.contains("task-1"), "unrelated text survives");
    }
}
