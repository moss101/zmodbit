//! modbit-git — branches/worktrees/diff/commit (M2.2, docs/20 § Git
//! strategy).
//!
//! Typed wrapper over the git CLI. Every operation is explicit and returns
//! typed results: merge conflicts surface as structured evidence
//! (`MergeOutcome::Conflict` with the conflicted file list) — merge/rebase
//! is never hidden shell magic (docs/20). Coding tasks default to a
//! dedicated branch + worktree; concurrent builders use separate worktrees.
//!
//! Canonical owner subsystem: workspace-git (docs/81). Layout: docs/12.

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

pub mod snapshot;
pub use snapshot::{SnapshotHandle, SnapshotProvenance, SNAPSHOT_NAMESPACE};

/// A typed file diff entry (`git diff --numstat`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileDiff {
    pub path: String,
    pub additions: u64,
    pub deletions: u64,
}

/// Typed merge outcome with conflict evidence (docs/20: conflicts produce
/// evidence, never silent failure).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MergeOutcome {
    Merged,
    Conflict { conflicted_files: Vec<String> },
}

#[derive(Debug)]
pub enum GitError {
    Git { operation: String, message: String },
    Io(std::io::Error),
}

impl fmt::Display for GitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GitError::Git { operation, message } => {
                write!(f, "git {operation} failed: {message}")
            }
            GitError::Io(e) => write!(f, "git io: {e}"),
        }
    }
}

impl std::error::Error for GitError {}

pub struct GitRepo {
    root: PathBuf,
}

impl GitRepo {
    fn git(&self, operation: &str, args: &[&str]) -> Result<Output, GitError> {
        let mut command = Command::new("git");
        command.arg("-C").arg(&self.root).arg(operation);
        command.args(args);
        command.env("GIT_AUTHOR_NAME", "modbit-core");
        command.env("GIT_AUTHOR_EMAIL", "core@modbit.local");
        command.env("GIT_COMMITTER_NAME", "modbit-core");
        command.env("GIT_COMMITTER_EMAIL", "core@modbit.local");
        command.output().map_err(GitError::Io).and_then(|out| {
            if out.status.success() {
                Ok(out)
            } else {
                Err(GitError::Git {
                    operation: format!("{operation} {}", args.join(" ")),
                    message: String::from_utf8_lossy(&out.stderr).trim().to_string(),
                })
            }
        })
    }

    fn stdout_text(out: &Output) -> String {
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Initializes a new repository with the `main` branch and a local commit
    /// identity (CI runners have no global git config).
    pub fn init(path: &Path) -> Result<Self, GitError> {
        std::fs::create_dir_all(path).map_err(GitError::Io)?;
        let repo = Self {
            root: path.to_path_buf(),
        };
        let init = Command::new("git")
            .arg("init")
            .arg("-b")
            .arg("main")
            .arg(path)
            .output()
            .map_err(GitError::Io)?;
        if !init.status.success() {
            return Err(GitError::Git {
                operation: "init".into(),
                message: String::from_utf8_lossy(&init.stderr).trim().to_string(),
            });
        }
        repo.git("config", &["user.name", "modbit-core"])?;
        repo.git("config", &["user.email", "core@modbit.local"])?;
        Ok(repo)
    }

    /// Opens an existing repository.
    pub fn open(path: &Path) -> Result<Self, GitError> {
        let repo = Self {
            root: path.to_path_buf(),
        };
        repo.git("rev-parse", &["--git-dir"])?;
        Ok(repo)
    }

    /// Repository root directory.
    pub fn path(&self) -> &Path {
        &self.root
    }

    /// Merge base of two refs (empty string if none).
    pub fn merge_base(&self, a: &str, b: &str) -> Result<String, GitError> {
        let out = self.git("merge-base", &[a, b])?;
        Ok(Self::stdout_text(&out))
    }

    /// Stages one path (resolution bookkeeping for merge transactions).
    pub fn stage_path(&self, path: &str) -> Result<(), GitError> {
        self.git("add", &["--", path]).map(|_| ())
    }

    /// Concludes an in-progress merge with a commit message (MERGE_HEAD is
    /// consumed by the commit).
    pub fn conclude_merge(&self, message: &str) -> Result<(), GitError> {
        self.git("commit", &["-m", message]).map(|_| ())
    }

    /// Aborts a live merge, restoring the pre-merge worktree.
    pub fn abort_merge_state(&self) -> Result<(), GitError> {
        self.git("merge", &["--abort"]).map(|_| ())
    }

    /// Hard-resets the branch and worktree to a commit (transaction
    /// rollback of a merge commit we created).
    pub fn reset_hard(&self, target: &str) -> Result<(), GitError> {
        self.git("reset", &["--hard", target]).map(|_| ())
    }

    /// Commits all changes; returns the new HEAD hash.
    pub fn commit_all(&self, message: &str) -> Result<String, GitError> {
        self.git("add", &["-A"])?;
        self.git("commit", &["-m", message])?;
        let out = self.git("rev-parse", &["HEAD"])?;
        Ok(Self::stdout_text(&out))
    }

    pub fn create_branch(&self, name: &str, from: Option<&str>) -> Result<(), GitError> {
        match from {
            Some(start) => self.git("branch", &[name, start]).map(|_| ()),
            None => self.git("branch", &[name]).map(|_| ()),
        }
    }

    pub fn checkout(&self, name: &str) -> Result<(), GitError> {
        self.git("checkout", &[name]).map(|_| ())
    }

    pub fn current_branch(&self) -> Result<String, GitError> {
        let out = self.git("rev-parse", &["--abbrev-ref", "HEAD"])?;
        Ok(Self::stdout_text(&out))
    }

    /// Adds a linked worktree on a new branch (docs/20: concurrent builders
    /// use separate worktrees). Returns a handle rooted at the worktree.
    pub fn worktree_add(&self, worktree_path: &Path, branch: &str) -> Result<GitRepo, GitError> {
        self.git(
            "worktree",
            &["add", "-b", branch, &worktree_path.display().to_string()],
        )?;
        Ok(GitRepo {
            root: worktree_path.to_path_buf(),
        })
    }

    pub fn worktree_remove(&self, worktree_path: &Path) -> Result<(), GitError> {
        self.git(
            "worktree",
            &["remove", &worktree_path.display().to_string()],
        )
        .map(|_| ())
    }

    /// Numstat diff between two revisions: per-file add/del counts.
    pub fn diff_numstat(&self, from: &str, to: &str) -> Result<Vec<FileDiff>, GitError> {
        let out = self.git("diff", &["--numstat", &format!("{from}..{to}")])?;
        let text = Self::stdout_text(&out);
        let mut diffs = Vec::new();
        for line in text.lines() {
            let mut parts = line.split('\t');
            if let (Some(add), Some(del), Some(path)) = (parts.next(), parts.next(), parts.next()) {
                let parse = |v: &str| v.parse::<u64>().unwrap_or(0);
                diffs.push(FileDiff {
                    path: path.to_string(),
                    additions: parse(add),
                    deletions: parse(del),
                });
            }
        }
        Ok(diffs)
    }

    /// HEAD commit hash.
    pub fn head(&self) -> Result<String, GitError> {
        let out = self.git("rev-parse", &["HEAD"])?;
        Ok(Self::stdout_text(&out))
    }

    /// Starts a merge without committing and WITHOUT aborting on conflict:
    /// the merge state stays live so a merge transaction can resolve and
    /// conclude (two-parent commit) later. Conflicts surface as typed
    /// evidence; the caller owns abort vs. conclude.
    pub fn start_merge(&self, branch: &str) -> Result<MergeOutcome, GitError> {
        match self.git("merge", &["--no-ff", "--no-commit", branch]) {
            Ok(_) => Ok(MergeOutcome::Merged),
            Err(_) => {
                let diff = self.git("diff", &["--name-only", "--diff-filter=U"]);
                let files = match diff {
                    Ok(out) => Self::stdout_text(&out)
                        .lines()
                        .filter(|l| !l.is_empty())
                        .map(String::from)
                        .collect(),
                    Err(_) => Vec::new(),
                };
                Ok(MergeOutcome::Conflict {
                    conflicted_files: files,
                })
            }
        }
    }

    /// Typed merge (docs/20 § Git strategy): `Merged` on success; `Conflict`
    /// with the conflicted file list as evidence. On conflict the merge is
    /// aborted so the worktree stays clean — the caller decides how to
    /// proceed (merge / export patch / discard).
    pub fn merge(&self, branch: &str) -> Result<MergeOutcome, GitError> {
        match self.git("merge", &["--no-ff", "--no-commit", branch]) {
            Ok(_) => {
                self.git("commit", &["-m", &format!("merge {branch}")])?;
                Ok(MergeOutcome::Merged)
            }
            Err(_) => {
                // Collect the conflicted-file evidence while the merge state
                // is live, then abort so the worktree is left clean.
                let diff = self.git("diff", &["--name-only", "--diff-filter=U"]);
                let files = match diff {
                    Ok(out) => Self::stdout_text(&out)
                        .lines()
                        .filter(|l| !l.is_empty())
                        .map(String::from)
                        .collect(),
                    Err(_) => Vec::new(),
                };
                self.abort_merge();
                Ok(MergeOutcome::Conflict {
                    conflicted_files: files,
                })
            }
        }
    }

    fn abort_merge(&self) {
        let _ = self.git("merge", &["--abort"]);
    }
}
