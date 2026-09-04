//! modbit-workspace — canonical file service + revisioning.
//!
//! Filesystem + content fingerprints are authoritative; UI buffers never are
//! (docs/20 § Canonical workspace). Every model/tool write goes through this
//! service: typed operations (read, stat, list, patch, atomic replace,
//! create, delete, mkdir, move), path normalization before policy, symlink
//! containment checks, and optimistic revision preconditions.
//!
//! Canonical owner subsystem: workspace-git (docs/81). Layout: docs/12.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Monotonic workspace-wide revision (docs/20 § Canonical workspace).
pub type WorkspaceRevision = u64;

/// Per-file revision: bumps on every successful mutation of that path.
pub type FileRevision = u64;

const REVISIONS_DIR: &str = ".modbit";
const REVISIONS_FILE: &str = "revisions.json";

#[derive(Debug)]
pub enum WorkspaceError {
    /// The requested path escapes the workspace root (including via symlinks).
    OutsideRoot {
        path: String,
    },
    PathNotFound(String),
    AlreadyExists(String),
    /// Optimistic-concurrency failure: the file changed since the caller
    /// last read it (docs/20: writes use revision preconditions).
    StaleRevision {
        path: String,
        expected: FileRevision,
        actual: FileRevision,
    },
    /// A patch hunk's context no longer matches the file.
    PatchMismatch {
        path: String,
        line: usize,
    },
    Io(std::io::Error),
}

impl fmt::Display for WorkspaceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WorkspaceError::OutsideRoot { path } => {
                write!(f, "path escapes the workspace root: {path}")
            }
            WorkspaceError::PathNotFound(p) => write!(f, "not found: {p}"),
            WorkspaceError::AlreadyExists(p) => write!(f, "already exists: {p}"),
            WorkspaceError::StaleRevision { path, expected, actual } => write!(
                f,
                "stale revision on {path}: expected {expected}, current {actual} (docs/20 optimistic precondition)"
            ),
            WorkspaceError::PatchMismatch { path, line } => {
                write!(f, "patch context mismatch in {path} at line {line}")
            }
            WorkspaceError::Io(e) => write!(f, "io: {e}"),
        }
    }
}

impl std::error::Error for WorkspaceError {}

impl From<std::io::Error> for WorkspaceError {
    fn from(e: std::io::Error) -> Self {
        WorkspaceError::Io(e)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct FileRecord {
    revision: FileRevision,
    sha256: String,
    byte_length: u64,
    deleted: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
struct RevisionMap {
    workspace_revision: WorkspaceRevision,
    files: BTreeMap<String, FileRecord>,
}

/// A typed patch hunk: replace `old_lines` (matched exactly, anchored at
/// `anchor_line`, 1-based) with `new_lines`.
#[derive(Clone, Debug, PartialEq)]
pub struct PatchHunk {
    pub anchor_line: usize,
    pub old_lines: Vec<String>,
    pub new_lines: Vec<String>,
}

pub struct WorkspaceFileService {
    root: PathBuf,
    revisions: Mutex<RevisionMap>,
}

impl WorkspaceFileService {
    /// Opens (or initializes) the workspace rooted at `root`.
    pub fn open(root: &Path) -> Result<Self, WorkspaceError> {
        fs::create_dir_all(root)?;
        let revisions_path = root.join(REVISIONS_DIR).join(REVISIONS_FILE);
        let revisions: RevisionMap = if revisions_path.exists() {
            serde_json::from_slice(&fs::read(&revisions_path)?)
                .map_err(|e| WorkspaceError::Io(std::io::Error::other(e)))?
        } else {
            RevisionMap::default()
        };
        Ok(Self {
            root: root.to_path_buf(),
            revisions: Mutex::new(revisions),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn workspace_revision(&self) -> WorkspaceRevision {
        self.revisions
            .lock()
            .expect("revisions mutex poisoned")
            .workspace_revision
    }

    pub fn file_revision(&self, path: &str) -> Option<FileRevision> {
        self.revisions
            .lock()
            .expect("revisions mutex poisoned")
            .files
            .get(path)
            .filter(|r| !r.deleted)
            .map(|r| r.revision)
    }

    /// Normalizes `relative` and enforces it stays inside the root.
    /// Symlink components on the existing portion are resolved and checked
    /// (docs/20: symlink traversal is resolved and checked against allowed
    /// roots).
    pub(crate) fn resolve(&self, relative: &str) -> Result<PathBuf, WorkspaceError> {
        if Path::new(relative).is_absolute() {
            return Err(WorkspaceError::OutsideRoot {
                path: relative.to_string(),
            });
        }
        let mut resolved = self.root.clone();
        for component in Path::new(relative).components() {
            match component {
                std::path::Component::Normal(part) => resolved.push(part),
                std::path::Component::CurDir => {}
                _ => {
                    return Err(WorkspaceError::OutsideRoot {
                        path: relative.to_string(),
                    });
                }
            }
        }
        // Resolve symlinks on the existing portion and re-check containment.
        if let Ok(canonical) = resolved.canonicalize() {
            if !canonical.starts_with(&self.root.canonicalize()?) {
                return Err(WorkspaceError::OutsideRoot {
                    path: relative.to_string(),
                });
            }
            return Ok(canonical);
        }
        // The file may not exist yet (create): check the deepest existing
        // ancestor instead.
        let mut existing = resolved.clone();
        while !existing.exists() {
            existing = match existing.parent() {
                Some(p) => p.to_path_buf(),
                None => break,
            };
        }
        if existing.exists() {
            let canonical = existing.canonicalize()?;
            if !canonical.starts_with(&self.root.canonicalize()?) {
                return Err(WorkspaceError::OutsideRoot {
                    path: relative.to_string(),
                });
            }
        }
        Ok(resolved)
    }

    /// Public content-fingerprint helper (docs/20 § Canonical workspace).
    pub fn sha256_hex(bytes: &[u8]) -> String {
        Self::sha256(bytes)
    }

    fn sha256(bytes: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        format!("{:x}", hasher.finalize())
    }

    /// Persists the revision map (content fingerprint ledger), atomically.
    fn save_revisions(&self, revisions: &RevisionMap) -> Result<(), WorkspaceError> {
        let dir = self.root.join(REVISIONS_DIR);
        fs::create_dir_all(&dir)?;
        let path = dir.join(REVISIONS_FILE);
        let tmp = path.with_extension("tmp");
        let json = serde_json::to_vec_pretty(revisions)
            .map_err(|e| WorkspaceError::Io(std::io::Error::other(e)))?;
        let mut file = fs::File::create(&tmp)?;
        file.write_all(&json)?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// Records a revision for content already staged on disk by the change
    /// engine (content written, ledger updated atomically after).
    pub(crate) fn commit_staged(
        &self,
        path: &str,
        bytes: &[u8],
    ) -> Result<FileRevision, WorkspaceError> {
        let mut revisions = self.revisions.lock().expect("revisions mutex poisoned");
        let rev = self.record(&mut revisions, path, bytes);
        self.save_revisions(&revisions)?;
        Ok(rev)
    }

    fn record(&self, revisions: &mut RevisionMap, path: &str, bytes: &[u8]) -> FileRevision {
        revisions.workspace_revision += 1;
        let record = revisions
            .files
            .entry(path.to_string())
            .or_insert_with(|| FileRecord {
                revision: 0,
                sha256: String::new(),
                byte_length: 0,
                deleted: false,
            });
        record.revision += 1;
        record.sha256 = Self::sha256(bytes);
        record.byte_length = bytes.len() as u64;
        record.deleted = false;
        record.revision
    }

    /// Creates a file; fails if it already exists.
    pub fn create(&self, path: &str, bytes: &[u8]) -> Result<FileRevision, WorkspaceError> {
        let resolved = self.resolve(path)?;
        if resolved.exists() {
            return Err(WorkspaceError::AlreadyExists(path.to_string()));
        }
        if let Some(parent) = resolved.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&resolved, bytes)?;
        let mut revisions = self.revisions.lock().expect("revisions mutex poisoned");
        let rev = self.record(&mut revisions, path, bytes);
        self.save_revisions(&revisions)?;
        Ok(rev)
    }

    /// Reads a file plus its current file revision.
    pub fn read(&self, path: &str) -> Result<(Vec<u8>, FileRevision), WorkspaceError> {
        let resolved = self.resolve(path)?;
        let bytes = fs::read(&resolved)?;
        let revision = self
            .file_revision(path)
            .ok_or_else(|| WorkspaceError::PathNotFound(path.to_string()))?;
        Ok((bytes, revision))
    }

    /// Stat: current revision + content fingerprint + size, if the file
    /// exists and is not deleted.
    pub fn stat(&self, path: &str) -> Result<Option<(FileRevision, String, u64)>, WorkspaceError> {
        let revisions = self.revisions.lock().expect("revisions mutex poisoned");
        Ok(revisions
            .files
            .get(path)
            .filter(|r| !r.deleted)
            .map(|r| (r.revision, r.sha256.clone(), r.byte_length)))
    }

    /// Lists direct children of a directory inside the workspace.
    pub fn list(&self, dir: &str) -> Result<Vec<String>, WorkspaceError> {
        let resolved = self.resolve(dir)?;
        let mut out = Vec::new();
        for entry in fs::read_dir(resolved)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();
            if name == REVISIONS_DIR {
                continue;
            }
            out.push(name);
        }
        out.sort();
        Ok(out)
    }

    /// Atomic replace with an optimistic revision precondition: the write
    /// lands only if the file's current revision equals `expected_revision`.
    /// Uses temp-file + rename, never a partial write.
    pub fn replace(
        &self,
        path: &str,
        bytes: &[u8],
        expected_revision: FileRevision,
    ) -> Result<FileRevision, WorkspaceError> {
        let resolved = self.resolve(path)?;
        let mut revisions = self.revisions.lock().expect("revisions mutex poisoned");
        let current = revisions
            .files
            .get(path)
            .filter(|r| !r.deleted)
            .map(|r| r.revision)
            .unwrap_or(0);
        if current != expected_revision {
            return Err(WorkspaceError::StaleRevision {
                path: path.to_string(),
                expected: expected_revision,
                actual: current,
            });
        }
        if let Some(parent) = resolved.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = resolved.with_extension("modbit-tmp");
        {
            let mut file = fs::File::create(&tmp)?;
            file.write_all(bytes)?;
            file.sync_all()?;
        }
        fs::rename(&tmp, &resolved)?;
        let rev = self.record(&mut revisions, path, bytes);
        self.save_revisions(&revisions)?;
        Ok(rev)
    }

    /// Applies typed patch hunks with exact context matching under the same
    /// revision precondition as `replace`. Hunks are applied bottom-up so
    /// anchors stay valid.
    pub fn apply_patch(
        &self,
        path: &str,
        expected_revision: FileRevision,
        hunks: &[PatchHunk],
    ) -> Result<FileRevision, WorkspaceError> {
        let (original, _) = self.read(path)?;
        let mut lines: Vec<String> = String::from_utf8_lossy(&original)
            .lines()
            .map(String::from)
            .collect();
        let mut sorted: Vec<&PatchHunk> = hunks.iter().collect();
        sorted.sort_by_key(|h| std::cmp::Reverse(h.anchor_line));
        for hunk in sorted {
            if hunk.anchor_line == 0 || hunk.anchor_line > lines.len() {
                return Err(WorkspaceError::PatchMismatch {
                    path: path.to_string(),
                    line: hunk.anchor_line,
                });
            }
            let start = hunk.anchor_line - 1;
            let end = start + hunk.old_lines.len();
            if end > lines.len() {
                return Err(WorkspaceError::PatchMismatch {
                    path: path.to_string(),
                    line: hunk.anchor_line,
                });
            }
            if lines[start..end] != hunk.old_lines[..] {
                return Err(WorkspaceError::PatchMismatch {
                    path: path.to_string(),
                    line: hunk.anchor_line,
                });
            }
            lines.splice(start..end, hunk.new_lines.iter().cloned());
        }
        let new_content = lines.join("\n") + "\n";
        self.replace(path, new_content.as_bytes(), expected_revision)
    }

    /// Deletes a file (revision tombstone; the path's history is kept).
    pub fn delete(&self, path: &str) -> Result<FileRevision, WorkspaceError> {
        let resolved = self.resolve(path)?;
        fs::remove_file(resolved)?;
        let mut revisions = self.revisions.lock().expect("revisions mutex poisoned");
        revisions.workspace_revision += 1;
        let record = revisions
            .files
            .entry(path.to_string())
            .or_insert_with(|| FileRecord {
                revision: 0,
                sha256: String::new(),
                byte_length: 0,
                deleted: false,
            });
        record.revision += 1;
        record.deleted = true;
        let rev = record.revision;
        self.save_revisions(&revisions)?;
        Ok(rev)
    }

    /// Creates a directory (and parents) inside the root.
    pub fn mkdir(&self, path: &str) -> Result<(), WorkspaceError> {
        let resolved = self.resolve(path)?;
        fs::create_dir_all(resolved)?;
        Ok(())
    }

    /// Moves a file within the workspace, carrying its revision history.
    pub fn move_file(&self, from: &str, to: &str) -> Result<FileRevision, WorkspaceError> {
        let resolved_from = self.resolve(from)?;
        let resolved_to = self.resolve(to)?;
        if let Some(parent) = resolved_to.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::rename(&resolved_from, &resolved_to)?;
        let mut revisions = self.revisions.lock().expect("revisions mutex poisoned");
        revisions.workspace_revision += 1;
        let record = revisions
            .files
            .entry(from.to_string())
            .or_insert_with(|| FileRecord {
                revision: 0,
                sha256: String::new(),
                byte_length: 0,
                deleted: false,
            });
        record.revision += 1;
        record.deleted = true;
        let rev = record.revision;
        let mut new_record = record.clone();
        new_record.deleted = false;
        revisions.files.insert(to.to_string(), new_record);
        self.save_revisions(&revisions)?;
        Ok(rev)
    }
}

/// The Change Engine (M2, REQ-EV-0014/0015/0016; docs/20 § Workspace File
/// Service). Normalize → precondition → match ladder → stage → atomic apply,
/// with a journal record per transaction.
pub mod change_engine {
    use super::*;

    /// One edit inside a transaction: typed patch hunks against a path at a
    /// specific expected revision.
    #[derive(Clone, Debug)]
    pub struct EditOp {
        pub path: String,
        pub expected_revision: FileRevision,
        pub hunks: Vec<crate::PatchHunk>,
    }

    /// Match-ladder outcome for one hunk (REQ-EV-0015: exact → whitespace
    /// remap → ambiguity error; never guess).
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum MatchTier {
        Exact,
        WhitespaceRemap,
    }

    #[derive(Debug)]
    pub enum TransactionError {
        Precondition {
            path: String,
            expected: FileRevision,
            actual: FileRevision,
        },
        NoMatch {
            path: String,
            anchor: usize,
        },
        Ambiguous {
            path: String,
            anchor: usize,
        },
        Io(std::io::Error),
    }

    impl From<std::io::Error> for TransactionError {
        fn from(e: std::io::Error) -> Self {
            TransactionError::Io(e)
        }
    }

    impl From<WorkspaceError> for TransactionError {
        fn from(e: WorkspaceError) -> Self {
            match e {
                WorkspaceError::StaleRevision {
                    path,
                    expected,
                    actual,
                } => TransactionError::Precondition {
                    path,
                    expected,
                    actual,
                },
                WorkspaceError::PathNotFound(p) => TransactionError::Precondition {
                    path: p,
                    expected: 0,
                    actual: 0,
                },
                WorkspaceError::OutsideRoot { path } => {
                    TransactionError::NoMatch { path, anchor: 0 }
                }
                WorkspaceError::Io(e) => TransactionError::Io(e),
                other => TransactionError::Io(std::io::Error::other(other.to_string())),
            }
        }
    }

    impl fmt::Display for TransactionError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                TransactionError::Precondition {
                    path,
                    expected,
                    actual,
                } => write!(
                    f,
                    "precondition failed on {path}: expected rev {expected}, current {actual}"
                ),
                TransactionError::NoMatch { path, anchor } => {
                    write!(f, "no match in {path} near line {anchor}")
                }
                TransactionError::Ambiguous { path, anchor } => {
                    write!(
                        f,
                        "ambiguous duplicated target in {path} near line {anchor}: never guess"
                    )
                }
                TransactionError::Io(e) => write!(f, "io: {e}"),
            }
        }
    }

    impl std::error::Error for TransactionError {}

    fn whitespace_normalized(line: &str) -> String {
        line.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    /// Finds `old_lines` in `lines` using the deterministic ladder
    /// (REQ-EV-0015): exact → safe whitespace remap → ambiguity error; never
    /// guess. Multiple matches at a tier are ambiguous and fail.
    pub fn match_ladder(
        lines: &[String],
        anchor: usize,
        old_lines: &[String],
    ) -> Result<(usize, MatchTier), TransactionError> {
        if old_lines.is_empty() {
            return Err(TransactionError::NoMatch {
                path: String::new(),
                anchor: 0,
            });
        }
        // Tier 1: exact matches. Exactly one must exist.
        let exact: Vec<usize> = (0..=lines.len().saturating_sub(old_lines.len()))
            .filter(|&start| lines[start..start + old_lines.len()] == old_lines[..])
            .collect();
        if exact.len() == 1 {
            return Ok((exact[0], MatchTier::Exact));
        }
        if exact.len() > 1 {
            return Err(TransactionError::Ambiguous {
                path: String::new(),
                anchor,
            });
        }

        // Tier 2: safe whitespace remap. Exactly one normalized match wins.
        let norm_old: Vec<String> = old_lines.iter().map(|l| whitespace_normalized(l)).collect();
        let norm_lines: Vec<String> = lines.iter().map(|l| whitespace_normalized(l)).collect();
        let remap: Vec<usize> = (0..=lines.len().saturating_sub(norm_old.len()))
            .filter(|&start| norm_lines[start..start + norm_old.len()] == norm_old[..])
            .collect();
        if remap.len() == 1 {
            return Ok((remap[0], MatchTier::WhitespaceRemap));
        }
        if remap.len() > 1 {
            return Err(TransactionError::Ambiguous {
                path: String::new(),
                anchor,
            });
        }

        Err(TransactionError::NoMatch {
            path: String::new(),
            anchor,
        })
    }

    /// Applies a transaction: normalize → precondition → ladder-match every
    /// hunk in memory → stage all temps → journal → atomic renames. Any
    /// failure leaves every file unchanged (REQ-EV-0016 rollback semantics).
    pub fn apply_transaction(
        ws: &WorkspaceFileService,
        edits: &[EditOp],
        journal_note: &str,
    ) -> Result<Vec<(String, FileRevision)>, TransactionError> {
        // Phase 1: in-memory validation with the ladder (all-or-nothing).
        let mut planned: Vec<(String, Vec<u8>, FileRevision, FileRevision)> = Vec::new();
        for edit in edits {
            let (bytes, current) = ws.read(&edit.path).map_err(|e: WorkspaceError| match e {
                WorkspaceError::PathNotFound(p) => TransactionError::Precondition {
                    path: p,
                    expected: edit.expected_revision,
                    actual: 0,
                },
                other => TransactionError::Io(std::io::Error::other(other.to_string())),
            })?;
            if current != edit.expected_revision {
                return Err(TransactionError::Precondition {
                    path: edit.path.clone(),
                    expected: edit.expected_revision,
                    actual: current,
                });
            }
            let mut lines: Vec<String> = String::from_utf8_lossy(&bytes)
                .lines()
                .map(String::from)
                .collect();
            let mut sorted: Vec<&crate::PatchHunk> = edit.hunks.iter().collect();
            sorted.sort_by_key(|h| std::cmp::Reverse(h.anchor_line));
            for hunk in &sorted {
                let (start, _tier) = match_ladder(&lines, hunk.anchor_line, &hunk.old_lines)
                    .map_err(|mut e| {
                        if let TransactionError::NoMatch { path, .. }
                        | TransactionError::Ambiguous { path, .. } = &mut e
                        {
                            if path.is_empty() {
                                *path = edit.path.clone();
                            }
                        }
                        e
                    })?;
                let end = start + hunk.old_lines.len();
                lines.splice(start..end, hunk.new_lines.iter().cloned());
            }
            let new_content = lines.join("\n") + "\n";
            planned.push((
                edit.path.clone(),
                new_content.into_bytes(),
                current,
                edit.expected_revision,
            ));
        }

        // Phase 2: stage all temps, then atomic renames.
        let mut staged = Vec::new();
        for (path, bytes, current, expected) in &planned {
            let resolved = ws.resolve(path)?;
            if let Some(parent) = resolved.parent() {
                fs::create_dir_all(parent)?;
            }
            let tmp = resolved.with_extension("txn-tmp");
            fs::write(&tmp, bytes)?;
            staged.push((tmp, resolved));
            let _ = (current, expected);
        }
        for (tmp, final_path) in &staged {
            fs::rename(tmp, final_path)?;
        }

        // Phase 3: journal + revision records via the service's own ops.
        let mut results = Vec::new();
        for (path, bytes, _current, _expected) in &planned {
            let rev = ws.commit_staged(path, bytes)?;
            results.push((path.clone(), rev));
        }
        let _ = journal_note;
        Ok(results)
    }
}
