//! Protocol-state persistence (M4, REQ-EV-0055): the full protocol
//! lifecycle — tool calls, approvals, questions, terminal/browser cursors,
//! subagents — is persisted durably so a crash at ANY point (including
//! while a tool awaits approval) reconstructs the EXACT pending state on
//! restart.
//!
//! Canonical owner subsystem: protocol-state (docs/81). Layout: docs/12.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

/// The protocol record kinds that must survive a crash.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProtocolRecord {
    ToolCall {
        call_id: String,
        name: String,
        arguments: String,
        status: ToolCallStatus,
    },
    ApprovalRequest {
        approval_id: String,
        tool: String,
        scope: String,
        status: ApprovalStatus,
    },
    Question {
        question_id: String,
        text: String,
        answered: bool,
    },
    TerminalCursor {
        run_id: String,
        offset: u64,
    },
    Subagent {
        subagent_id: String,
        task: String,
        state: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallStatus {
    AwaitingApproval,
    Approved,
    Running,
    Completed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalStatus {
    Pending,
    Granted,
    Denied,
}

#[derive(Debug)]
pub enum ProtocolStateError {
    Io(std::io::Error),
    Serialization(serde_json::Error),
}

impl fmt::Display for ProtocolStateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProtocolStateError::Io(e) => write!(f, "protocol state io: {e}"),
            ProtocolStateError::Serialization(e) => write!(f, "protocol state serialization: {e}"),
        }
    }
}

impl std::error::Error for ProtocolStateError {}

/// The durable protocol-state store: an append-only JSONL journal. Every
/// append is flushed to disk, so records survive process death by
/// construction.
pub struct ProtocolStateStore {
    path: PathBuf,
    pub records: Vec<ProtocolRecord>,
}

impl ProtocolStateStore {
    /// Opens (or initializes) the store at `path`, replaying any existing
    /// journal.
    pub fn open(path: &Path) -> Result<Self, ProtocolStateError> {
        let mut records = Vec::new();
        if path.exists() {
            let text = std::fs::read_to_string(path).map_err(ProtocolStateError::Io)?;
            let lines: Vec<&str> = text.lines().filter(|l| !l.is_empty()).collect();
            let last = lines.len().saturating_sub(1);
            for (index, line) in lines.iter().enumerate() {
                match serde_json::from_str::<ProtocolRecord>(line) {
                    Ok(record) => records.push(record),
                    Err(e) => {
                        // A torn FINAL line is a crash mid-append: the
                        // journal is prefix-consistent, so skip the tail.
                        // Corruption mid-journal is a real error.
                        if index == last {
                            break;
                        }
                        return Err(ProtocolStateError::Serialization(e));
                    }
                }
            }
        }
        Ok(Self {
            path: path.to_path_buf(),
            records,
        })
    }

    /// Appends a record and DURABLY persists it (append + flush).
    pub fn append(&mut self, record: ProtocolRecord) -> Result<(), ProtocolStateError> {
        use std::io::Write;
        let mut line = serde_json::to_string(&record).map_err(ProtocolStateError::Serialization)?;
        line.push('\n');
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(ProtocolStateError::Io)?;
        file.write_all(line.as_bytes())
            .map_err(ProtocolStateError::Io)?;
        file.flush().map_err(ProtocolStateError::Io)?;
        self.records.push(record);
        Ok(())
    }

    /// The exact pending approval, if any (QUAL-EV-0055 probe). The
    /// LATEST record per approval id wins — a later grant/deny supersedes
    /// an earlier pending request.
    pub fn pending_approval(&self) -> Option<(&str, &str, ApprovalStatus)> {
        let mut latest: BTreeMap<&str, (&str, ApprovalStatus)> = BTreeMap::new();
        for r in &self.records {
            if let ProtocolRecord::ApprovalRequest {
                approval_id,
                tool,
                status,
                ..
            } = r
            {
                latest.insert(approval_id.as_str(), (tool.as_str(), *status));
            }
        }
        latest
            .into_iter()
            .find(|(_, (_, status))| *status == ApprovalStatus::Pending)
            .map(|(id, (tool, status))| (id, tool, status))
    }

    /// All tool calls in their persisted status.
    pub fn tool_calls(&self) -> Vec<(&str, ToolCallStatus)> {
        self.records
            .iter()
            .filter_map(|r| match r {
                ProtocolRecord::ToolCall { name, status, .. } => Some((name.as_str(), *status)),
                _ => None,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> PathBuf {
        let unique = uuid::Uuid::now_v7().simple().to_string();
        std::env::temp_dir().join(format!("modbit-prot-{tag}-{unique}.jsonl"))
    }

    /// QUAL-EV-0055: a crash while a tool awaits approval — restart
    /// reconstructs the EXACT pending state.
    #[test]
    fn crash_while_awaiting_approval_restarts_exact_pending_state() {
        let path = temp_path("approval");

        // Session 1: tool call -> approval request (pending) -> CRASH.
        {
            let mut store = ProtocolStateStore::open(&path).unwrap();
            store
                .append(ProtocolRecord::ToolCall {
                    call_id: "call-1".into(),
                    name: "fs.write".into(),
                    arguments: r#"{"path":"src/lib.rs"}"#.into(),
                    status: ToolCallStatus::AwaitingApproval,
                })
                .unwrap();
            store
                .append(ProtocolRecord::ApprovalRequest {
                    approval_id: "apr-1".into(),
                    tool: "fs.write".into(),
                    scope: "src/lib.rs".into(),
                    status: ApprovalStatus::Pending,
                })
                .unwrap();
            store
                .append(ProtocolRecord::TerminalCursor {
                    run_id: "run-1".into(),
                    offset: 512,
                })
                .unwrap();
            // drop(store) == the crash. Everything is already on disk.
        }

        // Session 2 (restart): the pending state is reconstructed EXACTLY.
        let store = ProtocolStateStore::open(&path).unwrap();
        assert_eq!(store.records.len(), 3);
        let (approval_id, tool, status) =
            store.pending_approval().expect("pending approval survives");
        assert_eq!(approval_id, "apr-1");
        assert_eq!(tool, "fs.write");
        assert_eq!(status, ApprovalStatus::Pending);
        assert_eq!(
            store.tool_calls(),
            vec![("fs.write", ToolCallStatus::AwaitingApproval)]
        );

        // Grant the approval durably; a restart still knows it.
        let mut store = ProtocolStateStore::open(&path).unwrap();
        store
            .append(ProtocolRecord::ApprovalRequest {
                approval_id: "apr-1".into(),
                tool: "fs.write".into(),
                scope: "src/lib.rs".into(),
                status: ApprovalStatus::Granted,
            })
            .unwrap();
        let reopened = ProtocolStateStore::open(&path).unwrap();
        // The LATEST approval state wins; no pending remains.
        assert!(reopened.pending_approval().is_none());
        assert!(std::fs::remove_file(&path).is_ok());
    }

    /// Questions and subagent records persist with the same durability.
    #[test]
    fn questions_and_subagents_persist() {
        let path = temp_path("mixed");
        {
            let mut store = ProtocolStateStore::open(&path).unwrap();
            store
                .append(ProtocolRecord::Question {
                    question_id: "q-1".into(),
                    text: "which database?".into(),
                    answered: false,
                })
                .unwrap();
            store
                .append(ProtocolRecord::Subagent {
                    subagent_id: "sub-1".into(),
                    task: "explore retrieval".into(),
                    state: "running".into(),
                })
                .unwrap();
        }
        let store = ProtocolStateStore::open(&path).unwrap();
        assert_eq!(store.records.len(), 2);
        assert!(matches!(
            store.records[0],
            ProtocolRecord::Question { ref question_id, answered: false, .. } if question_id == "q-1"
        ));
        let _ = std::fs::remove_file(&path);
    }
}
