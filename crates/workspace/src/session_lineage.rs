//! Session lineage (M2, REQ-EV-0124): cloning a branch into a new session
//! binds the new task/run to an ISOLATED branch + linked worktree and
//! records the lineage (parent session → child session). Isolation is
//! provable: two sessions edit the same file independently and a merge
//! transaction detects the conflict instead of silently overwriting.

use crate::merge_transaction::{self, MergePhase, MergeTransactionError};
use modbit_git::{GitError, GitRepo, MergeOutcome};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::{Path, PathBuf};

/// One session's isolated workspace binding plus its lineage.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionBinding {
    pub session_id: String,
    /// Parent session this was cloned from (empty for a root session).
    pub parent_session: Option<String>,
    pub branch: String,
    /// Linked worktree root for this session's isolated writes.
    pub worktree_root: PathBuf,
    /// Commit the session branched from (lineage anchor).
    pub fork_commit: String,
}

#[derive(Debug)]
pub enum SessionError {
    Git(GitError),
    Merge(MergeTransactionError),
    Io(std::io::Error),
    UnknownSession(String),
}

impl fmt::Display for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SessionError::Git(e) => write!(f, "git: {e}"),
            SessionError::Merge(e) => write!(f, "merge transaction: {e}"),
            SessionError::Io(e) => write!(f, "io: {e}"),
            SessionError::UnknownSession(s) => write!(f, "unknown session {s:?}"),
        }
    }
}

impl std::error::Error for SessionError {}

impl From<GitError> for SessionError {
    fn from(e: GitError) -> Self {
        SessionError::Git(e)
    }
}
impl From<MergeTransactionError> for SessionError {
    fn from(e: MergeTransactionError) -> Self {
        SessionError::Merge(e)
    }
}
impl From<std::io::Error> for SessionError {
    fn from(e: std::io::Error) -> Self {
        SessionError::Io(e)
    }
}

/// Creates a root session binding: a dedicated branch off HEAD.
pub fn open_root_session(
    repo: &GitRepo,
    session_id: &str,
    branch: &str,
) -> Result<SessionBinding, SessionError> {
    repo.create_branch(branch, None)?;
    repo.checkout(branch)?;
    Ok(SessionBinding {
        session_id: session_id.to_string(),
        parent_session: None,
        branch: branch.to_string(),
        worktree_root: repo.path().to_path_buf(),
        fork_commit: repo.head()?,
    })
}

/// Clones a session into a NEW session bound to an isolated linked
/// worktree on its own branch, forked from the parent session's current
/// commit. Lineage is recorded in the binding (REQ-EV-0124).
pub fn clone_session(
    repo: &GitRepo,
    parent: &SessionBinding,
    child_session_id: &str,
    child_branch: &str,
    child_worktree: &Path,
) -> Result<SessionBinding, SessionError> {
    let wt = repo.worktree_add(child_worktree, child_branch)?;
    let fork_commit = wt.head()?;
    Ok(SessionBinding {
        session_id: child_session_id.to_string(),
        parent_session: Some(parent.session_id.clone()),
        branch: child_branch.to_string(),
        worktree_root: child_worktree.to_path_buf(),
        fork_commit,
    })
}

/// Commits the session's worktree state onto its branch (the session's
/// isolated write becoming mergeable work).
pub fn commit_session_work(
    binding: &SessionBinding,
    message: &str,
) -> Result<String, SessionError> {
    let wt = GitRepo::open(&binding.worktree_root)?;
    let sha = wt.commit_all(message)?;
    Ok(sha)
}

/// Merges a session's branch back into the target branch through a
/// persistent merge transaction — conflicts are DETECTED and recorded,
/// never silently resolved (REQ-EV-0124 QUAL).
pub fn merge_session(
    repo: &GitRepo,
    binding: &SessionBinding,
    target_branch: &str,
    transaction_id: &str,
) -> Result<(crate::merge_transaction::MergeTransaction, MergeOutcome), SessionError> {
    let (tx, outcome) =
        merge_transaction::open_and_merge(repo, transaction_id, &binding.branch, target_branch)?;
    Ok((tx, outcome))
}

/// Recovers a session merge: abort/roll back a conflicted or failed merge
/// transaction so the target branch is clean again.
pub fn recover_merge(repo: &GitRepo) -> Result<(), SessionError> {
    let tx = merge_transaction::inspect(repo)?;
    let Some(tx) = tx else { return Ok(()) };
    match tx.phase {
        MergePhase::Conflicted | MergePhase::Validating => {
            merge_transaction::rollback(repo)?;
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let unique = uuid::Uuid::now_v7().simple().to_string();
            let path = std::env::temp_dir().join(format!("modbit-sess-{tag}-{unique}"));
            std::fs::create_dir_all(&path).unwrap();
            TempDir(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// QUAL-EV-0124: two sessions modify the same file separately; the
    /// merge transaction detects the conflict.
    #[test]
    fn two_sessions_editing_separately_conflict_on_merge() {
        let dir = TempDir::new("conflict");
        let repo = GitRepo::init(&dir.0).unwrap();
        std::fs::write(dir.0.join("doc.txt"), "original\n").unwrap();
        repo.commit_all("base").unwrap();
        repo.set_config("core.autocrlf", "false").unwrap();

        let root = open_root_session(&repo, "sess-main", "session/main").unwrap();
        assert_eq!(root.parent_session, None);

        // Clone two child sessions into isolated worktrees.
        let wt_a = TempDir::new("wt-a");
        let wt_b = TempDir::new("wt-b");
        let sess_a = clone_session(&repo, &root, "sess-a", "session/a", &wt_a.0).unwrap();
        let sess_b = clone_session(&repo, &root, "sess-b", "session/b", &wt_b.0).unwrap();
        assert_eq!(sess_a.parent_session.as_deref(), Some("sess-main"));
        assert_eq!(sess_b.parent_session.as_deref(), Some("sess-main"));
        assert_ne!(sess_a.worktree_root, sess_b.worktree_root);

        // Both sessions modify the SAME file independently.
        std::fs::write(wt_a.0.join("doc.txt"), "session A's version\n").unwrap();
        std::fs::write(wt_b.0.join("doc.txt"), "session B's version\n").unwrap();
        commit_session_work(&sess_a, "A edits doc").unwrap();
        commit_session_work(&sess_b, "B edits doc").unwrap();

        // Isolation: neither worktree sees the other's edit.
        assert_eq!(
            std::fs::read_to_string(wt_a.0.join("doc.txt")).unwrap(),
            "session A's version\n"
        );

        // Merge A back into the root session branch: clean.
        repo.checkout("session/main").unwrap();
        let (tx_a, outcome_a) = merge_session(&repo, &sess_a, "session/main", "mtx-a").unwrap();
        assert_eq!(outcome_a, MergeOutcome::Merged);
        assert_eq!(tx_a.phase, MergePhase::Validating);
        // Conclude A's transaction cleanly (validation passed).
        crate::merge_transaction::record_validation(&repo, "build", true).unwrap();
        crate::merge_transaction::commit(&repo).unwrap();

        // Merge B into the SAME branch: now CONFLICTS with A's landed edit.
        repo.checkout("session/main").unwrap();
        let (tx_b, outcome_b) = merge_session(&repo, &sess_b, "session/main", "mtx-b").unwrap();
        assert!(
            matches!(outcome_b, MergeOutcome::Conflict { .. }),
            "the transaction must DETECT the cross-session conflict"
        );
        assert_eq!(tx_b.phase, MergePhase::Conflicted);
        assert_eq!(tx_b.conflicts, vec!["doc.txt".to_string()]);
        // The transaction record is inspectable for resolution.
        let inspected = crate::merge_transaction::inspect(&repo).unwrap().unwrap();
        assert_eq!(inspected.transaction_id, "mtx-b");

        // Recovery: roll B back so the branch is clean at A's state.
        recover_merge(&repo).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.0.join("doc.txt")).unwrap(),
            "session A's version\n"
        );
    }
}
