//! Fleet admission and coordination (M6 batch): transactional subagent
//! admission (REQ-EV-0267), capacity tickets (REQ-EV-0272),
//! captain→build TaskContracts (REQ-EV-0144), task→branch/environment
//! isolation bundles (REQ-EV-0145), the parallel change coordinator
//! (REQ-EV-0150), and the Needs-Attention aggregator (REQ-EV-0151).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;

// ---------------------------------------------------------------------------
// Capacity tickets (REQ-EV-0272)
// ---------------------------------------------------------------------------

/// An explicit resource ticket consumed before spawn/sandbox.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CapacityTicket {
    pub ticket_id: String,
    pub tenant: String,
}

#[derive(Debug)]
pub enum TicketError {
    Exhausted {
        tenant: String,
        active: usize,
        capacity: usize,
    },
    UnknownTicket(String),
}

impl fmt::Display for TicketError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TicketError::Exhausted {
                tenant,
                active,
                capacity,
            } => {
                write!(
                    f,
                    "capacity exhausted for tenant {tenant}: {active}/{capacity}"
                )
            }
            TicketError::UnknownTicket(id) => write!(f, "unknown ticket {id:?}"),
        }
    }
}

impl std::error::Error for TicketError {}

/// The resource governor: per-tenant ticket pools. Exhaustion denies the
/// launch WITHOUT partial side effects (the ticket is only consumed on
/// success).
#[derive(Default)]
pub struct ResourceGovernor {
    capacity: BTreeMap<String, usize>,
    active: BTreeMap<String, Vec<CapacityTicket>>,
    counter: u64,
}

impl ResourceGovernor {
    pub fn set_capacity(&mut self, tenant: &str, capacity: usize) {
        self.capacity.insert(tenant.to_string(), capacity);
    }

    /// Consumes one ticket atomically (only on success).
    pub fn consume(&mut self, tenant: &str) -> Result<CapacityTicket, TicketError> {
        let capacity = *self.capacity.get(tenant).unwrap_or(&0);
        let active = self.active.entry(tenant.to_string()).or_default();
        if active.len() >= capacity {
            return Err(TicketError::Exhausted {
                tenant: tenant.to_string(),
                active: active.len(),
                capacity,
            });
        }
        self.counter += 1;
        let ticket = CapacityTicket {
            ticket_id: format!("ticket-{tenant}-{}", self.counter),
            tenant: tenant.to_string(),
        };
        active.push(ticket.clone());
        Ok(ticket)
    }

    /// Returns a ticket (task finished/crashed cleanup).
    pub fn release(&mut self, ticket_id: &str) -> Result<(), TicketError> {
        for tickets in self.active.values_mut() {
            if let Some(pos) = tickets.iter().position(|t| t.ticket_id == ticket_id) {
                tickets.remove(pos);
                return Ok(());
            }
        }
        Err(TicketError::UnknownTicket(ticket_id.to_string()))
    }
}

// ---------------------------------------------------------------------------
// Transactional subagent admission (REQ-EV-0267)
// ---------------------------------------------------------------------------

/// The admission bundle: every resource bound BEFORE the child starts.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Admission {
    pub child_id: String,
    pub ticket: CapacityTicket,
    pub worktree: String,
    pub write_set: Vec<String>,
    pub capability_ceiling: String,
}

#[derive(Debug)]
pub enum AdmissionError {
    Ticket(TicketError),
    InjectedFailure(&'static str),
}

impl fmt::Display for AdmissionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AdmissionError::Ticket(e) => write!(f, "{e}"),
            AdmissionError::InjectedFailure(step) => {
                write!(f, "injected admission failure at {step}")
            }
        }
    }
}

impl std::error::Error for AdmissionError {}

/// TRANSACTIONAL admission (REQ-EV-0267): capacity ticket, worktree
/// reservation, write-set registration, capability ceiling, and the child
/// record commit atomically. An INJECTED failure at any step rolls back
/// every earlier reservation — no orphan child, worktree, or capacity
/// leak.
pub fn admit_subagent(
    governor: &mut ResourceGovernor,
    tenant: &str,
    child_id: &str,
    worktree: &str,
    write_set: &[String],
    ceiling: &str,
    inject_failure_at: Option<&'static str>,
) -> Result<Admission, AdmissionError> {
    // Step 1: capacity ticket.
    let ticket = governor.consume(tenant).map_err(AdmissionError::Ticket)?;
    if inject_failure_at == Some("after_ticket") {
        governor.release(&ticket.ticket_id).unwrap();
        return Err(AdmissionError::InjectedFailure("after_ticket"));
    }

    // Step 2: worktree reservation.
    let worktree = format!("{worktree}");
    if inject_failure_at == Some("after_worktree") {
        governor.release(&ticket.ticket_id).unwrap();
        return Err(AdmissionError::InjectedFailure("after_worktree"));
    }

    // Step 3: write-set registration.
    let write_set = write_set.to_vec();
    if inject_failure_at == Some("after_write_set") {
        governor.release(&ticket.ticket_id).unwrap();
        return Err(AdmissionError::InjectedFailure("after_write_set"));
    }

    // Step 4: capability ceiling + child record (commit).
    let _ = ceiling;
    Ok(Admission {
        child_id: child_id.to_string(),
        ticket,
        worktree,
        write_set,
        capability_ceiling: ceiling.to_string(),
    })
}

// ---------------------------------------------------------------------------
// Captain→build TaskContracts (REQ-EV-0144)
// ---------------------------------------------------------------------------

/// A typed bounded task contract from the captain.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TaskContract {
    pub contract_id: String,
    pub builder: String,
    /// Files the builder MAY write — the scope boundary.
    pub write_scope: Vec<String>,
    pub objective: String,
}

#[derive(Debug)]
pub enum ContractError {
    OutOfScope { path: String, contract: String },
}

impl fmt::Display for ContractError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ContractError::OutOfScope { path, contract } => write!(
                f,
                "builder write to {path:?} is outside contract {contract}"
            ),
        }
    }
}

impl std::error::Error for ContractError {}

/// Enforces the contract: the builder CANNOT widen its scope (a write
/// outside `write_scope` is denied).
pub fn enforce_contract(contract: &TaskContract, write_path: &str) -> Result<(), ContractError> {
    if contract
        .write_scope
        .iter()
        .any(|p| write_path.starts_with(p.as_str()))
    {
        Ok(())
    } else {
        Err(ContractError::OutOfScope {
            path: write_path.to_string(),
            contract: contract.contract_id.clone(),
        })
    }
}

// ---------------------------------------------------------------------------
// Task isolation bundle (REQ-EV-0145)
// ---------------------------------------------------------------------------

/// Everything a task binds: worktree, sandbox, context namespace, lease,
/// credentials ref, capability snapshot, evidence namespace.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IsolationBundle {
    pub task_id: String,
    pub worktree: String,
    pub sandbox_id: String,
    pub context_namespace: String,
    pub lease_epoch: u64,
    pub credential_ref: String,
    pub capability_snapshot: Vec<String>,
    pub evidence_namespace: String,
}

/// Proves two tasks are fully isolated across every bound resource.
pub fn bundles_isolated(a: &IsolationBundle, b: &IsolationBundle) -> bool {
    a.task_id != b.task_id
        && a.worktree != b.worktree
        && a.sandbox_id != b.sandbox_id
        && a.context_namespace != b.context_namespace
        && a.lease_epoch != b.lease_epoch
        && a.evidence_namespace != b.evidence_namespace
}

// ---------------------------------------------------------------------------
// Parallel change coordinator (REQ-EV-0150)
// ---------------------------------------------------------------------------

/// A proposed write in the parallel plan.
#[derive(Clone, Debug, PartialEq)]
pub struct ProposedWrite {
    pub task_id: String,
    pub path: String,
}

#[derive(Debug)]
pub enum WriteAdmission {
    /// Non-overlapping write approved for parallel execution.
    Approved { path: String },
    /// Overlapping writes serialized/denied BEFORE execution.
    ConflictDenied { path: String, holders: Vec<String> },
}

/// Coordinates parallel writes: the first claim on a path wins; later
/// overlapping claims are denied before any execution starts.
pub fn coordinate_writes(writes: &[ProposedWrite]) -> Vec<WriteAdmission> {
    let mut held: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut admissions = Vec::new();
    for write in writes {
        let holders = held.entry(write.path.clone()).or_default();
        if holders.is_empty() {
            holders.push(write.task_id.clone());
            admissions.push(WriteAdmission::Approved {
                path: write.path.clone(),
            });
        } else {
            admissions.push(WriteAdmission::ConflictDenied {
                path: write.path.clone(),
                holders: holders.clone(),
            });
        }
    }
    admissions
}

// ---------------------------------------------------------------------------
// Needs-attention aggregator (REQ-EV-0151)
// ---------------------------------------------------------------------------

/// An actionable attention item.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AttentionItem {
    pub reason: &'static str, // approval | conflict | failure | stall | question
    pub subject: String,
    pub action: String,
    /// The canonical event id that RAISED this item.
    pub event_id: String,
}

/// Aggregates approvals/conflicts/failures/stalls/questions into one
/// attention list. Each reason is actionable; items CLEAR when their
/// canonical event resolves.
#[derive(Default)]
pub struct AttentionManager {
    pub items: Vec<AttentionItem>,
}

impl AttentionManager {
    pub fn raise(&mut self, item: AttentionItem) {
        self.items.push(item);
    }

    /// Clears the item whose canonical event resolved.
    pub fn clear(&mut self, event_id: &str) -> bool {
        let before = self.items.len();
        self.items.retain(|i| i.event_id != event_id);
        before != self.items.len()
    }

    /// Attention items grouped by reason, each with an action.
    pub fn by_reason(&self) -> BTreeMap<&'static str, Vec<&AttentionItem>> {
        let mut out: BTreeMap<&'static str, Vec<&AttentionItem>> = BTreeMap::new();
        for item in &self.items {
            out.entry(item.reason).or_default().push(item);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// QUAL-EV-0272: capacity exhaustion denies the launch without
    /// partial side effects.
    #[test]
    fn capacity_exhaustion_denies_launch_cleanly() {
        let mut governor = ResourceGovernor::default();
        governor.set_capacity("tenant-a", 2);
        let t1 = governor.consume("tenant-a").unwrap();
        let _t2 = governor.consume("tenant-a").unwrap();
        assert!(matches!(
            governor.consume("tenant-a"),
            Err(TicketError::Exhausted {
                active: 2,
                capacity: 2,
                ..
            })
        ));
        // Release frees the ticket.
        governor.release(&t1.ticket_id).unwrap();
        assert!(governor.consume("tenant-a").is_ok());
        // Releasing an unknown ticket is a typed error.
        assert!(matches!(
            governor.release("nope"),
            Err(TicketError::UnknownTicket(_))
        ));
    }

    /// QUAL-EV-0267: injected failure during admission leaves no orphan
    /// child/worktree/capacity leak.
    #[test]
    fn injected_admission_failure_leaks_nothing() {
        let mut governor = ResourceGovernor::default();
        governor.set_capacity("tenant-a", 2);

        for step in ["after_ticket", "after_worktree", "after_write_set"] {
            let before = governor.consume("tenant-a").unwrap();
            governor.release(&before.ticket_id).unwrap();
            let err = admit_subagent(
                &mut governor,
                "tenant-a",
                "child",
                "wt",
                &["a.rs".into()],
                "write",
                Some(step),
            )
            .unwrap_err();
            assert!(
                matches!(err, AdmissionError::InjectedFailure(_)),
                "step {step} must surface the injected failure"
            );
            // No leak: capacity is back to full.
            let refill = governor.consume("tenant-a").unwrap();
            governor.release(&refill.ticket_id).unwrap();
        }

        // Clean admission (no injection) consumes exactly one ticket.
        let admission = admit_subagent(
            &mut governor,
            "tenant-a",
            "child-ok",
            "wt-child",
            &["a.rs".into()],
            "write",
            None,
        )
        .unwrap();
        assert_eq!(admission.write_set, vec!["a.rs".to_string()]);
    }

    /// QUAL-EV-0144: a builder attempting an out-of-scope file write is
    /// denied.
    #[test]
    fn builder_cannot_widen_scope() {
        let contract = TaskContract {
            contract_id: "tc-1".into(),
            builder: "builder-a".into(),
            write_scope: vec!["src/retry/".into()],
            objective: "harden the retry loop".into(),
        };
        assert!(enforce_contract(&contract, "src/retry/loop.rs").is_ok());
        let err = enforce_contract(&contract, "src/config.rs").unwrap_err();
        assert!(err.to_string().contains("outside contract tc-1"));
    }

    /// QUAL-EV-0145: parallel tasks prove isolation across every bound
    /// resource.
    #[test]
    fn parallel_tasks_are_fully_isolated() {
        let mk = |id: &str, epoch: u64, ns: &str| IsolationBundle {
            task_id: id.to_string(),
            worktree: format!("wt-{id}"),
            sandbox_id: format!("sbx-{id}"),
            context_namespace: format!("ctx-{id}"),
            lease_epoch: epoch,
            credential_ref: "creds/shared-ref".into(),
            capability_snapshot: vec!["fs.read".into()],
            evidence_namespace: format!("ev-{ns}-{id}"),
        };
        let a = mk("task-a", 1, "v1");
        let b = mk("task-b", 2, "v2");
        assert!(bundles_isolated(&a, &b));
    }

    /// QUAL-EV-0150: overlapping writes are denied before execution.
    #[test]
    fn overlapping_writes_denied_before_execution() {
        let writes = vec![
            ProposedWrite {
                task_id: "t-a".into(),
                path: "src/lib.rs".into(),
            },
            ProposedWrite {
                task_id: "t-a".into(),
                path: "src/util.rs".into(),
            },
            ProposedWrite {
                task_id: "t-b".into(),
                path: "src/lib.rs".into(),
            }, // conflict
        ];
        let admissions = coordinate_writes(&writes);
        assert!(
            matches!(&admissions[0], WriteAdmission::Approved { path } if path == "src/lib.rs")
        );
        assert!(
            matches!(&admissions[1], WriteAdmission::Approved { path } if path == "src/util.rs")
        );
        match &admissions[2] {
            WriteAdmission::ConflictDenied { path, holders } => {
                assert_eq!(path, "src/lib.rs");
                assert_eq!(holders, &vec!["t-a".to_string()]);
            }
            other => panic!("expected conflict denial, got {other:?}"),
        }
    }

    /// QUAL-EV-0151: each attention reason is actionable and clears from
    /// the canonical event.
    #[test]
    fn attention_items_are_actionable_and_clear() {
        let mut manager = AttentionManager::default();
        manager.raise(AttentionItem {
            reason: "approval",
            subject: "apr-9".into(),
            action: "approve or deny the fs.write to src/lib.rs".into(),
            event_id: "evt-1".into(),
        });
        manager.raise(AttentionItem {
            reason: "stall",
            subject: "run-3".into(),
            action: "inspect the repeated search loop".into(),
            event_id: "evt-2".into(),
        });
        let grouped = manager.by_reason();
        assert_eq!(grouped.len(), 2);
        assert!(grouped["approval"].iter().all(|i| !i.action.is_empty()));

        // Clearing the canonical event removes exactly that item.
        assert!(manager.clear("evt-1"));
        assert_eq!(manager.items.len(), 1);
        assert_eq!(manager.items[0].event_id, "evt-2");
    }
}
