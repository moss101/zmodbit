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
use std::io::Write as _;
use std::process::{Child, Command, Output};

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

/// One review hunk from a `-U0` diff (Phase 5 item 5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffHunk {
    pub path: String,
    /// 1-based start line in the NEW file.
    pub new_start: usize,
    /// Raw +/- content lines of the hunk (header excluded).
    pub lines: Vec<String>,
}

/// Extracts the new-file start line from a hunk header. `-U0` headers
/// carry no count (`@@ -2 +2 @@ context`), so the number is the digit run
/// immediately after `+`, before the optional `,count`.
fn hunk_new_start(header: &str) -> Option<usize> {
    let plus = header.split('+').nth(1)?;
    let digits: String = plus.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
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
                let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
                // Some failures (lock contention, early exit) print to
                // stdout only; keep the error diagnosable either way.
                let message = if stderr.is_empty() {
                    String::from_utf8_lossy(&out.stdout).trim().to_string()
                } else {
                    stderr
                };
                Err(GitError::Git {
                    operation: format!("{operation} {}", args.join(" ")),
                    message,
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

    /// Parses `git diff -U0` into review hunks (Phase 5 item 5): one entry
    /// per changed range with the raw +/- content lines. `base` selects the
    /// comparison revision (task review uses the worktree base, not HEAD —
    /// agents may commit on their branch).
    pub fn diff_hunks_from(&self, base: &str) -> Result<Vec<DiffHunk>, GitError> {
        let out = self.git("diff", &["-U0", base])?;
        Ok(Self::parse_hunks(&Self::stdout_text(&out)))
    }

    /// Working-tree hunks against HEAD.
    pub fn diff_hunks_unrated(&self) -> Result<Vec<DiffHunk>, GitError> {
        self.diff_hunks_from("HEAD")
    }

    /// Parses a unified diff with no context lines into hunks.
    pub fn parse_hunks(diff_text: &str) -> Vec<DiffHunk> {
        let mut hunks: Vec<DiffHunk> = Vec::new();
        let mut path = String::new();
        let mut new_start = 0usize;
        let mut lines: Vec<String> = Vec::new();
        let mut in_hunk = false;
        for line in diff_text.lines() {
            if line.starts_with("diff --git ") {
                if in_hunk && !path.is_empty() && !lines.is_empty() {
                    hunks.push(DiffHunk {
                        path: path.clone(),
                        new_start,
                        lines: std::mem::take(&mut lines),
                    });
                }
                in_hunk = false;
                path.clear();
                continue;
            }
            if let Some(rest) = line.strip_prefix("+++ b/") {
                path = rest.to_string();
                continue;
            }
            if line.starts_with("@@ -") {
                if in_hunk && !path.is_empty() && !lines.is_empty() {
                    hunks.push(DiffHunk {
                        path: path.clone(),
                        new_start,
                        lines: std::mem::take(&mut lines),
                    });
                }
                new_start = hunk_new_start(line).unwrap_or(0);
                in_hunk = true;
                continue;
            }
            if in_hunk && (line.starts_with('+') || line.starts_with('-')) {
                lines.push(line.to_string());
            }
        }
        if in_hunk && !path.is_empty() && !lines.is_empty() {
            hunks.push(DiffHunk {
                path: path.clone(),
                new_start,
                lines,
            });
        }
        hunks
    }

    /// Hunk-level reject (Phase 5 item 5): captures the raw `-U0` diff,
    /// selects the hunk whose new-file start is `new_start`, and applies
    /// only that hunk in reverse. The file header + hunk header are
    /// reused verbatim from git's own output, so the constructed patch
    /// always applies.
    pub fn reject_hunk(&self, path: &str, new_start: usize, base: &str) -> Result<(), GitError> {
        let diff = self.git("diff", &["-U0", base, "--", path])?;
        let text = Self::stdout_text(&diff);
        if text.is_empty() {
            return Ok(());
        }
        let mut file_header = String::new();
        let mut blocks: Vec<(String, Vec<String>)> = Vec::new();
        for line in text.lines() {
            if line.starts_with("diff --git ")
                || line.starts_with("index ")
                || line.starts_with("--- ")
                || line.starts_with("+++ ")
            {
                if blocks.is_empty() {
                    file_header.push_str(line);
                    file_header.push('\n');
                }
                continue;
            }
            if line.starts_with("@@") {
                blocks.push((line.to_string(), Vec::new()));
                continue;
            }
            if let Some((_, body)) = blocks.last_mut() {
                body.push(line.to_string());
            }
        }
        let selected = blocks.iter().find(|(h, _)| hunk_new_start(h) == Some(new_start));
        let Some((hunk_header, body)) = selected else {
            return Err(GitError::Git {
                operation: "apply --reverse".into(),
                message: format!("no hunk starts at line {new_start} in {path}"),
            });
        };
        let patch = format!("{file_header}{hunk_header}\n{}\n", body.join("\n"));
        let mut child = Command::new("git")
            .arg("-C")
            .arg(&self.root)
            .args(["apply", "--reverse", "--unidiff-zero", "-"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(GitError::Io)?;
        {
            let stdin = child.stdin.as_mut().expect("stdin piped");
            stdin
                .write_all(patch.as_bytes())
                .map_err(|e| GitError::Git {
                    operation: "apply --reverse".into(),
                    message: e.to_string(),
                })?;
        }
        let out = child.wait_with_output().map_err(GitError::Io)?;
        if !out.status.success() {
            return Err(GitError::Git {
                operation: "apply --reverse".into(),
                message: String::from_utf8_lossy(&out.stderr).trim().to_string(),
            });
        }
        Ok(())
    }

    /// Rejects the worktree changes for one path (Phase 5 item 5): the
    /// diff against HEAD is captured and applied IN REVERSE, restoring
    /// the reviewed-hunk file to its committed state byte-exactly.
    pub fn reject_worktree_changes(&self, path: &str) -> Result<(), GitError> {
        let diff = self.git("diff", &["HEAD", "--", path])?;
        if diff.stdout.is_empty() {
            return Ok(()); // nothing to reject
        }
        let mut apply = Command::new("git");
        apply
            .arg("-C")
            .arg(&self.root)
            .args(["apply", "--reverse", "--unidiff-zero", "-"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut child = apply.spawn().map_err(GitError::Io)?;
        {
            let stdin = child.stdin.as_mut().expect("stdin piped");
            stdin
                .write_all(&diff.stdout)
                .map_err(|e| GitError::Git {
                    operation: "apply --reverse".into(),
                    message: e.to_string(),
                })?;
        }
        let out = child.wait_with_output().map_err(GitError::Io)?;
        if out.status.success() {
            Ok(())
        } else {
            Err(GitError::Git {
                operation: "apply --reverse".into(),
                message: String::from_utf8_lossy(&out.stderr).trim().to_string(),
            })
        }
    }

    /// Sets a repo-local config value (e.g. core.autocrlf for byte-exact
    /// tests or line-ending-sensitive workflows).
    pub fn set_config(&self, key: &str, value: &str) -> Result<(), GitError> {
        self.git("config", &[key, value]).map(|_| ())
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
        // One bounded retry: transient index.lock contention under
        // concurrent operations must not fail the commit.
        let commit = self.git("commit", &["-m", message]).or_else(|_| {
            std::thread::sleep(std::time::Duration::from_millis(150));
            self.git("commit", &["-m", message])
        });
        commit?;
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

    /// Adds a linked worktree on a new branch starting FROM `start_point`
    /// (a branch, tag, or commit) instead of HEAD — the per-task base
    /// branch selection (Phase 4.1).
    pub fn worktree_add_from(
        &self,
        worktree_path: &Path,
        branch: &str,
        start_point: &str,
    ) -> Result<GitRepo, GitError> {
        self.git(
            "worktree",
            &[
                "add",
                "-b",
                branch,
                &worktree_path.display().to_string(),
                start_point,
            ],
        )?;
        Ok(GitRepo {
            root: worktree_path.to_path_buf(),
        })
    }

    /// Clones a repository URL into `into` (Phase 4.1: register-by-URL).
    /// Returns a handle rooted at the clone.
    pub fn clone(url: &str, into: &Path) -> Result<Self, GitError> {
        if into.exists() && std::fs::read_dir(into).map(|mut d| d.next().is_some()).unwrap_or(false) {
            return Err(GitError::Git {
                operation: "clone".into(),
                message: format!("target {} already exists and is not empty", into.display()),
            });
        }
        let out = Command::new("git")
            .arg("clone")
            .arg(url)
            .arg(into)
            .env("GIT_AUTHOR_NAME", "modbit-core")
            .env("GIT_AUTHOR_EMAIL", "core@modbit.local")
            .env("GIT_COMMITTER_NAME", "modbit-core")
            .env("GIT_COMMITTER_EMAIL", "core@modbit.local")
            .output()
            .map_err(GitError::Io)?;
        if !out.status.success() {
            return Err(GitError::Git {
                operation: format!("clone {url}"),
                message: String::from_utf8_lossy(&out.stderr).trim().to_string(),
            });
        }
        Ok(GitRepo {
            root: into.to_path_buf(),
        })
    }

    /// The repository's default branch: the symbolic ref of HEAD.
    pub fn default_branch(&self) -> Result<String, GitError> {
        self.current_branch()
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

    /// Working-tree + index status as porcelain entries (git.status tool;
    /// `xy` is the two-letter porcelain code, path is repo-relative).
    pub fn status_porcelain(&self) -> Result<Vec<(String, String)>, GitError> {
        let out = self.git("status", &["--porcelain=v1"])?;
        let text = Self::stdout_text(&out);
        let mut entries = Vec::new();
        for line in text.lines() {
            if line.len() < 4 {
                continue;
            }
            let xy = line[..2].to_string();
            let path = line[3..].to_string();
            entries.push((xy, path));
        }
        Ok(entries)
    }

    /// Numstat diff of the working tree against a base revision
    /// (git.diff tool: uncommitted changes bound to the current worktree).
    pub fn diff_workdir_numstat(&self, base: &str) -> Result<Vec<FileDiff>, GitError> {
        let out = self.git("diff", &["--numstat", base])?;
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

#[cfg(test)]
mod hunk_tests {
    use super::*;

    fn repo_with(path: &str, content: &str) -> GitRepo {
        let dir = std::env::temp_dir().join(format!(
            "git-hunks-{}-{}",
            std::process::id(),
            uuid::Uuid::now_v7().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let repo = GitRepo::init(&dir).unwrap();
        repo.set_config("user.email", "t@t").unwrap();
        repo.set_config("user.name", "T").unwrap();
        repo.set_config("core.autocrlf", "false").unwrap();
        let full = dir.join("greet.js");
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(&full, content).unwrap();
        repo.stage_path("greet.js").unwrap();
        repo.commit_all("base").unwrap();
        let _ = path;
        repo
    }

    /// diff_hunks_unrated parses a -U0 diff into per-path hunks.
    #[test]
    fn diff_hunks_parse() {
        let dir = std::env::temp_dir().join(format!(
            "git-hunks-parse-{}",
            uuid::Uuid::now_v7().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let diff_text = "\
diff --git a/greet.js b/greet.js
--- a/greet.js
+++ b/greet.js
@@ -1,3 +1,3 @@
 function greet() {
-  return 'hi';
+  return 'howdy';
 }
";
        let hunks = GitRepo::parse_hunks(diff_text);
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].path, "greet.js");
        assert_eq!(hunks[0].new_start, 1);
        assert_eq!(hunks[0].lines.len(), 2);
        let _ = dir;
    }

    /// Hunk-level reject: only the selected hunk is reversed.
    #[test]
    fn reject_hunk_reverses_only_the_selected_hunk() {
        let root = std::env::temp_dir().join(format!(
            "git-hunk-rej-{}-{}",
            std::process::id(),
            uuid::Uuid::now_v7().simple()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let repo = GitRepo::init(&root).unwrap();
        repo.set_config("user.email", "t@t").unwrap();
        repo.set_config("user.name", "T").unwrap();
        repo.set_config("core.autocrlf", "false").unwrap();
        let file = root.join("multi.js");
        std::fs::write(&file, "line1\nline2\nline3\nline4\nline5\n").unwrap();
        repo.stage_path("multi.js").unwrap();
        repo.commit_all("base").unwrap();

        // Two separated single-line edits: lines 2 and 4.
        std::fs::write(&file, "line1\nline2-EDIT\nline3\nline4-EDIT\nline5\n").unwrap();

        // Hunk 1 starts at line 2 (new file coords).
        repo.reject_hunk("multi.js", 2, "HEAD").expect("hunk reject");

        let after = std::fs::read_to_string(&file).unwrap();
        // Line 2 reverted, line 4 edit preserved.
        assert!(
            after.contains("line2") && after.contains("line4-EDIT"),
            "after reject: {after:?}"
        );
        assert!(!after.contains("line2-EDIT"), "the selected hunk was reversed");
        assert!(after.contains("line4-EDIT"), "the other hunk is untouched");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Phase 5 item 5: reject = inverse-apply through git apply. A REAL
    /// repo edit is reverted byte-exactly by feeding the inverse hunk.
    #[test]
    fn reject_applies_the_inverse_patch() {
        let root = std::env::temp_dir().join(format!(
            "git-hunks-rej-{}-{}",
            std::process::id(),
            uuid::Uuid::now_v7().simple()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let repo = GitRepo::init(&root).unwrap();
        repo.set_config("user.email", "t@t").unwrap();
        repo.set_config("user.name", "T").unwrap();
        repo.set_config("core.autocrlf", "false").unwrap();
        let file = root.join("greet.js");
        std::fs::write(&file, "function greet() {\n  return 'hi';\n}\n").unwrap();
        repo.stage_path("greet.js").unwrap();
        repo.commit_all("base").unwrap();

        // The agent edits the file (worktree change, not committed).
        std::fs::write(&file, "function greet() {\n  return 'howdy';\n}\n").unwrap();

        // The hunk the reviewer sees (parsed from a -U0 diff).
        let hunks = repo.diff_hunks_unrated().unwrap();
        assert_eq!(hunks.len(), 1, "{hunks:?}");
        let hunk = &hunks[0];
        assert_eq!(hunk.path, "greet.js");

        // Reject: the diff against HEAD is applied in reverse.
        repo.reject_worktree_changes("greet.js").expect("reverse apply");
        let after = std::fs::read_to_string(&file).unwrap();
        assert_eq!(
            after, "function greet() {\n  return 'hi';\n}\n",
            "the rejected edit is reverted byte-exactly"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
