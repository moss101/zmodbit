//! Merge transactions (M2, REQ-EV-0067): a merge between source and target
//! is a persistent, inspectable transaction — not hidden shell magic. The
//! transaction records source/target/base, conflicts and their resolutions,
//! validation results, and its own state; it survives process death so an
//! interrupted merge stays recoverable (docs/20 § Git strategy).

use modbit_git::{GitError, GitRepo, MergeOutcome};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs;
use std::path::PathBuf;

/// Persisted transaction state file, inside the workspace root.
const TRANSACTION_FILE: &str = ".modbit/merge-transaction.json";

/// Transaction lifecycle. Terminal states: `Committed` (validated merge
/// landed) and `RolledBack` (merge unwound). `Conflicted` and
/// `Validating` are recoverable — a later process can inspect and resume.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergePhase {
    Open,
    Conflicted,
    Validating,
    Committed,
    RolledBack,
}

impl fmt::Display for MergePhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            MergePhase::Open => "open",
            MergePhase::Conflicted => "conflicted",
            MergePhase::Validating => "validating",
            MergePhase::Committed => "committed",
            MergePhase::RolledBack => "rolled-back",
        };
        write!(f, "{s}")
    }
}

/// One conflicted path and how it was resolved (manual edit, ours, theirs,
/// or left for the user).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ConflictResolution {
    pub path: String,
    pub strategy: String,
}

/// The durable merge transaction record.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MergeTransaction {
    pub transaction_id: String,
    pub source_branch: String,
    pub target_branch: String,
    /// Merge base at open time.
    pub base_commit: String,
    /// Target branch tip at open time — the rollback point.
    pub target_head: String,
    pub phase: MergePhase,
    pub conflicts: Vec<String>,
    pub resolutions: Vec<ConflictResolution>,
    /// Validation evidence (e.g. gate names + pass/fail) gathered while
    /// `Validating`.
    pub validation: Vec<(String, bool)>,
    pub commit_sha: Option<String>,
}

impl MergeTransaction {
    pub fn new(transaction_id: &str, source_branch: &str, target_branch: &str) -> Self {
        Self {
            transaction_id: transaction_id.to_string(),
            source_branch: source_branch.to_string(),
            target_branch: target_branch.to_string(),
            base_commit: String::new(),
            target_head: String::new(),
            phase: MergePhase::Open,
            conflicts: Vec::new(),
            resolutions: Vec::new(),
            validation: Vec::new(),
            commit_sha: None,
        }
    }
}

#[derive(Debug)]
pub enum MergeTransactionError {
    Git(GitError),
    Io(std::io::Error),
    /// A transaction is already open for this workspace.
    AlreadyOpen {
        transaction_id: String,
    },
    /// No open transaction to act on.
    NoOpenTransaction,
    /// The action is invalid in the transaction's current phase.
    InvalidPhase {
        phase: MergePhase,
        action: &'static str,
    },
    /// Rollback was attempted but the merge state could not be unwound.
    RollbackIncomplete {
        reason: String,
    },
}

impl fmt::Display for MergeTransactionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MergeTransactionError::Git(e) => write!(f, "git: {e}"),
            MergeTransactionError::Io(e) => write!(f, "io: {e}"),
            MergeTransactionError::AlreadyOpen { transaction_id } => {
                write!(f, "merge transaction {transaction_id} is already open")
            }
            MergeTransactionError::NoOpenTransaction => {
                write!(f, "no open merge transaction")
            }
            MergeTransactionError::InvalidPhase { phase, action } => {
                write!(f, "cannot {action} in phase {phase}")
            }
            MergeTransactionError::RollbackIncomplete { reason } => {
                write!(f, "rollback incomplete: {reason}")
            }
        }
    }
}

impl std::error::Error for MergeTransactionError {}

impl From<GitError> for MergeTransactionError {
    fn from(e: GitError) -> Self {
        MergeTransactionError::Git(e)
    }
}

impl From<std::io::Error> for MergeTransactionError {
    fn from(e: std::io::Error) -> Self {
        MergeTransactionError::Io(e)
    }
}

fn transaction_path(repo: &GitRepo) -> PathBuf {
    repo.path().join(TRANSACTION_FILE)
}

fn load(repo: &GitRepo) -> Result<Option<MergeTransaction>, MergeTransactionError> {
    let path = transaction_path(repo);
    if !path.exists() {
        return Ok(None);
    }
    let bytes = fs::read(&path)?;
    let tx: MergeTransaction = serde_json::from_slice(&bytes)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    Ok(Some(tx))
}

fn save(repo: &GitRepo, tx: &MergeTransaction) -> Result<(), MergeTransactionError> {
    let path = transaction_path(repo);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    fs::write(
        &tmp,
        serde_json::to_vec_pretty(tx)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?,
    )?;
    fs::rename(&tmp, &path)?;
    Ok(())
}

/// Opens a merge transaction: records base/source/target durably, checks
/// out the target branch, and runs the merge as a typed outcome. A conflict
/// does NOT abort the transaction — the merge state stays live on disk and
/// the transaction moves to `Conflicted`, inspectable and recoverable.
pub fn open_and_merge(
    repo: &GitRepo,
    transaction_id: &str,
    source_branch: &str,
    target_branch: &str,
) -> Result<(MergeTransaction, MergeOutcome), MergeTransactionError> {
    if let Some(existing) = load(repo)? {
        return Err(MergeTransactionError::AlreadyOpen {
            transaction_id: existing.transaction_id,
        });
    }
    let mut tx = MergeTransaction::new(transaction_id, source_branch, target_branch);
    tx.base_commit = merge_base(repo, source_branch, target_branch)?;
    repo.checkout(target_branch)?;
    tx.target_head = repo.head()?;
    save(repo, &tx)?;

    // Keep the merge state live on conflict — the transaction owns
    // resolution (conclude) vs recovery (abort).
    let outcome = repo.start_merge(source_branch)?;
    match &outcome {
        MergeOutcome::Merged => {
            // start_merge leaves --no-commit state: conclude now so the
            // transaction holds a real merge commit for validation/rollback.
            repo.conclude_merge(&format!(
                "merge {} (transaction {})",
                source_branch, transaction_id
            ))?;
            tx.phase = MergePhase::Validating;
            tx.commit_sha = Some(repo.head()?);
        }
        MergeOutcome::Conflict { conflicted_files } => {
            tx.phase = MergePhase::Conflicted;
            tx.conflicts = conflicted_files.clone();
        }
    }
    save(repo, &tx)?;
    Ok((tx, outcome))
}

fn merge_base(repo: &GitRepo, source: &str, target: &str) -> Result<String, MergeTransactionError> {
    let out = repo.merge_base(source, target)?;
    Ok(out)
}

/// Records one resolution for a conflicted path (stage the resolved file).
pub fn record_resolution(
    repo: &GitRepo,
    path: &str,
    strategy: &str,
) -> Result<MergeTransaction, MergeTransactionError> {
    let mut tx = load(repo)?.ok_or(MergeTransactionError::NoOpenTransaction)?;
    if tx.phase != MergePhase::Conflicted {
        return Err(MergeTransactionError::InvalidPhase {
            phase: tx.phase,
            action: "record a resolution",
        });
    }
    repo.stage_path(path)?;
    tx.resolutions.push(ConflictResolution {
        path: path.to_string(),
        strategy: strategy.to_string(),
    });
    tx.conflicts.retain(|c| c != path);
    if tx.conflicts.is_empty() {
        // All conflicts resolved: finalize the merge commit and validate.
        repo.conclude_merge(&format!(
            "merge {} (transaction {})",
            tx.source_branch, tx.transaction_id
        ))?;
        tx.commit_sha = Some(repo.head()?);
        tx.phase = MergePhase::Validating;
    }
    save(repo, &tx)?;
    Ok(tx)
}

/// Runs a validation gate against the transaction (e.g. a verification
/// gate name + pass/fail). While `Validating`, failures are recorded and
/// reported — the caller decides to roll back; the transaction record keeps
/// the evidence either way.
pub fn record_validation(
    repo: &GitRepo,
    gate: &str,
    passed: bool,
) -> Result<MergeTransaction, MergeTransactionError> {
    let mut tx = load(repo)?.ok_or(MergeTransactionError::NoOpenTransaction)?;
    if tx.phase != MergePhase::Validating {
        return Err(MergeTransactionError::InvalidPhase {
            phase: tx.phase,
            action: "record validation",
        });
    }
    tx.validation.push((gate.to_string(), passed));
    save(repo, &tx)?;
    Ok(tx)
}

/// Marks the transaction committed (all validations passed).
pub fn commit(repo: &GitRepo) -> Result<MergeTransaction, MergeTransactionError> {
    let mut tx = load(repo)?.ok_or(MergeTransactionError::NoOpenTransaction)?;
    if tx.phase != MergePhase::Validating {
        return Err(MergeTransactionError::InvalidPhase {
            phase: tx.phase,
            action: "commit",
        });
    }
    if tx.validation.iter().any(|(_, passed)| !passed) {
        return Err(MergeTransactionError::InvalidPhase {
            phase: tx.phase,
            action: "commit with failed validations — roll back instead",
        });
    }
    tx.phase = MergePhase::Committed;
    save(repo, &tx)?;
    Ok(tx)
}

/// Unwinds the merge and marks the transaction rolled back. Works from
/// `Conflicted` (abort the live merge) and from `Validating` (reset the
/// merge commit we created). The transaction record persists as evidence.
pub fn rollback(repo: &GitRepo) -> Result<MergeTransaction, MergeTransactionError> {
    let mut tx = load(repo)?.ok_or(MergeTransactionError::NoOpenTransaction)?;
    match tx.phase {
        MergePhase::Conflicted => {
            repo.abort_merge_state()?;
        }
        MergePhase::Validating => {
            let target_head = tx.target_head.clone();
            repo.reset_hard(&target_head)?;
        }
        phase => {
            return Err(MergeTransactionError::InvalidPhase {
                phase,
                action: "roll back",
            });
        }
    }
    let head = repo.head()?;
    if tx.phase == MergePhase::Validating && head == tx.commit_sha.clone().unwrap_or_default() {
        return Err(MergeTransactionError::RollbackIncomplete {
            reason: "HEAD still at the merge commit after reset".into(),
        });
    }
    tx.phase = MergePhase::RolledBack;
    save(repo, &tx)?;
    Ok(tx)
}

/// Reloads the persisted transaction without mutating anything — the
/// inspection/recovery entry point after a crash.
pub fn inspect(repo: &GitRepo) -> Result<Option<MergeTransaction>, MergeTransactionError> {
    load(repo)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unique temp workspace root (matches the crate's test idiom).
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let unique = uuid::Uuid::now_v7().simple().to_string();
            let path = std::env::temp_dir().join(format!("modbit-mtx-{tag}-{unique}"));
            std::fs::create_dir_all(&path).unwrap();
            TempDir(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn temp_repo(tag: &str) -> (TempDir, GitRepo) {
        let dir = TempDir::new(tag);
        let repo = GitRepo::init(&dir.0).unwrap();
        std::fs::write(dir.0.join("README.md"), "# temp repo\n").unwrap();
        repo.commit_all("base").unwrap();
        (dir, repo)
    }

    /// QUAL-EV-0067: injected conflict + failed validation leaves the merge
    /// transaction inspectable and recoverable.
    #[test]
    fn conflicted_merge_with_failed_validation_is_inspectable_and_recoverable() {
        let (_d, repo) = temp_repo("txn-conflict");
        let path = repo.path().join("shared.txt");
        std::fs::write(&path, "base\n").unwrap();
        repo.commit_all("add shared").unwrap();

        repo.create_branch("feature", None).unwrap();
        repo.checkout("feature").unwrap();
        std::fs::write(&path, "feature version\n").unwrap();
        repo.commit_all("feature edit").unwrap();

        repo.checkout("main").unwrap();
        std::fs::write(&path, "main version\n").unwrap();
        repo.commit_all("main edit").unwrap();

        // Conflict is injected by construction: both branches edited the
        // same hunk of the same file.
        let (tx, outcome) = open_and_merge(&repo, "txn-1", "feature", "main").unwrap();
        assert!(matches!(outcome, MergeOutcome::Conflict { .. }));
        assert_eq!(tx.phase, MergePhase::Conflicted);
        assert_eq!(tx.conflicts, vec!["shared.txt".to_string()]);

        // The transaction is INSPECTABLE from disk (crash-recovery path).
        let inspected = inspect(&repo).unwrap().expect("transaction persisted");
        assert_eq!(inspected, tx);
        assert_eq!(inspected.source_branch, "feature");
        assert_eq!(inspected.target_branch, "main");
        assert!(!inspected.base_commit.is_empty());
        assert!(!inspected.target_head.is_empty());

        // Failed validation is recorded as evidence while conflicted.
        assert!(record_validation(&repo, "build", true).is_err());

        // Resolve the conflict manually (strategy: ours), merge concludes.
        std::fs::write(&path, "resolved version\n").unwrap();
        let tx = record_resolution(&repo, "shared.txt", "manual:resolved").unwrap();
        assert_eq!(tx.phase, MergePhase::Validating);
        assert!(tx.resolutions.contains(&ConflictResolution {
            path: "shared.txt".into(),
            strategy: "manual:resolved".into(),
        }));
        // The concluded commit is a TRUE merge: two parents.
        let parents = std::process::Command::new("git")
            .arg("-C")
            .arg(repo.path())
            .args(["rev-list", "--parents", "-1", "HEAD"])
            .output()
            .unwrap();
        let parent_count = String::from_utf8_lossy(&parents.stdout)
            .split_whitespace()
            .count()
            - 1;
        assert_eq!(parent_count, 2, "concluded merge must have two parents");

        // A validation gate fails: the transaction keeps the evidence and
        // stays recoverable.
        let tx = record_validation(&repo, "tests", false).unwrap();
        assert!(tx.validation.contains(&("tests".to_string(), false)));
        // Commit must refuse while a gate failed.
        assert!(commit(&repo).is_err());

        // Recovery: roll back to base. The target branch is back to its
        // pre-merge state and the transaction records the rollback.
        let tx = rollback(&repo).unwrap();
        assert_eq!(tx.phase, MergePhase::RolledBack);
        let head_tree = repo.head().unwrap();
        let base_tree = repo.merge_base("feature", "main").unwrap();
        let _ = (head_tree, base_tree);
        let content = std::fs::read_to_string(repo.path().join("shared.txt")).unwrap();
        assert_eq!(content, "main version\n", "rollback restored target state");
    }

    /// Clean merge: open → validating → committed, with the merge commit
    /// recorded.
    #[test]
    fn clean_merge_commits_after_validation() {
        let (_d, repo) = temp_repo("txn-clean");
        repo.create_branch("feature", None).unwrap();
        repo.checkout("feature").unwrap();
        std::fs::write(repo.path().join("f.txt"), "feature\n").unwrap();
        repo.commit_all("feature file").unwrap();
        repo.checkout("main").unwrap();

        let (tx, outcome) = open_and_merge(&repo, "txn-2", "feature", "main").unwrap();
        assert_eq!(outcome, MergeOutcome::Merged);
        assert_eq!(tx.phase, MergePhase::Validating);

        record_validation(&repo, "build", true).unwrap();
        let tx = commit(&repo).unwrap();
        assert_eq!(tx.phase, MergePhase::Committed);
        assert!(tx.commit_sha.is_some());
    }

    /// A second open while one transaction is in flight is refused.
    #[test]
    fn only_one_transaction_at_a_time() {
        let (_d, repo) = temp_repo("txn-single");
        repo.create_branch("feature", None).unwrap();
        repo.checkout("feature").unwrap();
        std::fs::write(repo.path().join("f.txt"), "feature\n").unwrap();
        repo.commit_all("feature file").unwrap();
        repo.checkout("main").unwrap();

        open_and_merge(&repo, "txn-a", "feature", "main").unwrap();
        assert!(open_and_merge(&repo, "txn-b", "feature", "main").is_err());
        rollback(&repo).unwrap();
        // Terminal state refuses further transitions.
        assert!(rollback(&repo).is_err());
    }
}
