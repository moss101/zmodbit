//! Kernel lease/session fencing (M4, M4.4): exactly one session may hold
//! the write lease for a scope at a time. Leases carry a monotonically
//! increasing fencing epoch; a session whose lease epoch is superseded
//! cannot commit — checkpoint fencing, applied to sessions.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;

/// A session lease with its fencing epoch.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionLease {
    pub scope: String,
    pub session_id: String,
    pub epoch: u64,
}

#[derive(Debug)]
pub enum LeaseError {
    HeldByOther {
        scope: String,
        holder: String,
    },
    StaleEpoch {
        scope: String,
        session: String,
        lease_epoch: u64,
        current_epoch: u64,
    },
}

impl fmt::Display for LeaseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LeaseError::HeldByOther { scope, holder } => {
                write!(f, "lease {scope:?} held by session {holder:?}")
            }
            LeaseError::StaleEpoch {
                scope,
                session,
                lease_epoch,
                current_epoch,
            } => write!(
                f,
                "stale lease for {scope:?}: session {session} holds epoch {lease_epoch}, current is {current_epoch}"
            ),
        }
    }
}

impl std::error::Error for LeaseError {}

/// The lease registry. `fencing` is the scope's monotonic epoch counter;
/// it is bumped PAST a released lease so the released session's epoch can
/// never validate again.
#[derive(Default)]
pub struct LeaseFencer {
    /// scope → (holder session, lease epoch) for ACTIVE leases.
    active: BTreeMap<String, (String, u64)>,
    /// scope → current fencing epoch (monotonic).
    fencing: BTreeMap<String, u64>,
}

impl LeaseFencer {
    pub fn new() -> Self {
        Default::default()
    }

    fn next_epoch(&self, scope: &str) -> u64 {
        self.fencing.get(scope).copied().unwrap_or(0) + 1
    }

    /// Acquires the lease for a scope (fails while another session holds
    /// it). The lease's epoch supersedes any earlier lease.
    pub fn acquire(&mut self, scope: &str, session_id: &str) -> Result<SessionLease, LeaseError> {
        if let Some((holder, _)) = self.active.get(scope) {
            return Err(LeaseError::HeldByOther {
                scope: scope.to_string(),
                holder: holder.clone(),
            });
        }
        let epoch = self.next_epoch(scope);
        self.fencing.insert(scope.to_string(), epoch);
        self.active
            .insert(scope.to_string(), (session_id.to_string(), epoch));
        Ok(SessionLease {
            scope: scope.to_string(),
            session_id: session_id.to_string(),
            epoch,
        })
    }

    /// Releases the lease, bumping the fencing epoch past the released
    /// lease so its epoch is permanently stale.
    pub fn release(&mut self, scope: &str) {
        if let Some((_, epoch)) = self.active.remove(scope) {
            self.fencing
                .insert(scope.to_string(), self.next_epoch(scope).max(epoch + 1));
        }
    }

    /// Validates a commit: the session must be the ACTIVE holder and its
    /// lease epoch must equal the current fencing epoch. Superseded
    /// epochs are refused — stale sessions cannot commit.
    pub fn validate_commit(
        &self,
        scope: &str,
        session_id: &str,
        lease_epoch: u64,
    ) -> Result<(), LeaseError> {
        let current_epoch = self.fencing.get(scope).copied().unwrap_or(0);
        match self.active.get(scope) {
            Some((holder, epoch)) if holder == session_id && *epoch == lease_epoch => Ok(()),
            _ => Err(LeaseError::StaleEpoch {
                scope: scope.to_string(),
                session: session_id.to_string(),
                lease_epoch,
                current_epoch,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Kernel lease fencing: only the current holder with the current
    /// epoch commits; released/superseded sessions are fenced out.
    #[test]
    fn stale_session_epochs_cannot_commit() {
        let mut fencer = LeaseFencer::new();

        // Session A acquires and commits fine.
        let lease_a = fencer.acquire("workspace-1", "session-A").unwrap();
        assert!(fencer
            .validate_commit("workspace-1", "session-A", lease_a.epoch)
            .is_ok());

        // B cannot acquire while A holds it.
        assert!(matches!(
            fencer.acquire("workspace-1", "session-B"),
            Err(LeaseError::HeldByOther { .. })
        ));

        // A releases (crash/recovery): B acquires with a NEW epoch.
        fencer.release("workspace-1");
        let lease_b = fencer.acquire("workspace-1", "session-B").unwrap();
        assert!(lease_b.epoch > lease_a.epoch, "fencing epoch is monotonic");

        // A's recovered process still holds its OLD epoch: commits are
        // FENCED OUT even though A once owned the lease.
        assert!(matches!(
            fencer.validate_commit("workspace-1", "session-A", lease_a.epoch),
            Err(LeaseError::StaleEpoch { .. })
        ));
        // A retrying with B's epoch under A's name: also refused.
        assert!(fencer
            .validate_commit("workspace-1", "session-A", lease_b.epoch)
            .is_err());
        // B commits fine.
        assert!(fencer
            .validate_commit("workspace-1", "session-B", lease_b.epoch)
            .is_ok());
    }
}
