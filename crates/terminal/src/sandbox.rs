//! OS sandbox for broker-spawned processes (Phase 6 item 2, docs/21 §
//! sandbox): wraps a spawned command so that
//! - macOS: `sandbox-exec` applies a Seatbelt profile — network denied
//!   by default, file writes restricted to the worktree (+ temp), reads
//!   allowed;
//! - Linux: Landlock (kernel LSM) restricts filesystem WRITES to the
//!   worktree (+ temp) via `pre_exec`; network denial on Linux requires
//!   seccomp and is a recorded follow-up (the Seatbelt path already
//!   denies network).
//! - Windows: restricted-token sandbox is a recorded follow-up (docs/21).
//!
//! The wrapper is opt-in per spawn (`sandbox: true` on the execd spawn
//! op / the scheduler's shell.run under approvals mode) and never
//! changes argv semantics for the caller: the wrapped command runs the
//! SAME argv with the SAME cwd/env.

use std::path::Path;

#[derive(Debug)]
pub struct SandboxError {
    pub message: String,
}

impl std::fmt::Display for SandboxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "sandbox: {}", self.message)
    }
}

impl std::error::Error for SandboxError {}

/// The Seatbelt profile: reads broadly (toolchains need their toolchains),
/// writes only under the worktree + temp, network DENIED by default.
const SEATBELT_TEMPLATE: &str = r#"(version 1)
(deny default)
; reads are allowed (toolchains, system libs); writes are not.
(allow file-read*)
(allow file-write*
  (subpath "<WORKTREE>")
  (subpath "/private/tmp")
  (subpath "/tmp"))
(allow process-exec)
(allow process-fork)
(allow sysctl-read)
(deny network*)
"#;

/// Returns the wrapped argv for the current platform, or None when no
/// sandbox applies (Windows — restricted-token sandbox is follow-up).
pub fn wrap_argv(
    argv: &[String],
    worktree: &Path,
) -> Result<Option<Vec<String>>, SandboxError> {
    if cfg!(target_os = "macos") {
        // Seatbelt evaluates REAL paths: /var/folders (macOS temp) is a
        // symlink to /private/var/folders, so the worktree must be
        // canonicalized or the write-scope rule never matches.
        let worktree = std::fs::canonicalize(worktree).unwrap_or_else(|_| worktree.to_path_buf());
        let profile = SEATBELT_TEMPLATE.replace(
            "<WORKTREE>",
            &worktree.display().to_string(),
        );
        let dir = std::env::temp_dir().join(format!(
            "modbit-sb-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        std::fs::create_dir_all(&dir).map_err(|e| SandboxError { message: e.to_string() })?;
        let profile_path = dir.join("seatbelt.sb");
        std::fs::write(&profile_path, profile)
            .map_err(|e| SandboxError { message: e.to_string() })?;
        let mut wrapped = vec![
            "sandbox-exec".to_string(),
            "-f".to_string(),
            profile_path.display().to_string(),
        ];
        wrapped.extend(argv.iter().cloned());
        Ok(Some(wrapped))
    } else if cfg!(target_os = "linux") {
        // Landlock is applied in-process via pre_exec (see
        // apply_landlock_pre_exec); argv is returned unchanged with a
        // marker the caller passes to CommandExt.
        Ok(Some(wrap_linux_marker(argv)))
    } else {
        Ok(None)
    }
}

fn wrap_linux_marker(argv: &[String]) -> Vec<String> {
    let mut wrapped = vec!["__MODBIT_LANDLOCK__".to_string()];
    wrapped.extend(argv.iter().cloned());
    wrapped
}

/// Applies Landlock FS rules to the current thread (called from
/// `pre_exec` so the spawned child inherits the restricted view):
/// writes allowed ONLY under `worktree` (+ /tmp), reads everywhere.
/// Best-effort: on a kernel without Landlock this is a documented no-op
/// (never blocks the spawn).
#[cfg(target_os = "linux")]
pub fn apply_landlock_pre_exec(worktree: &Path) -> Result<(), String> {
    use landlock::{
        Access as _, AccessFs, PathBeneath, PathFd, Ruleset, RulesetAttr,
        RulesetCreatedAttr as _,
    };
    let no_exec_write = {
        let mut a = AccessFs::from_all(landlock::ABI::V1);
        a.remove(AccessFs::Execute);
        a.remove(AccessFs::WriteFile);
        a
    };
    // Official crate pattern: handle accesses on the Ruleset, create() the
    // real kernel ruleset, add path rules, then restrict. Reads everywhere;
    // writes only under the worktree + /tmp. BestEffort on old kernels.
    let status = Ruleset::default()
        .handle_access(AccessFs::from_all(landlock::ABI::V1))
        .map_err(|e| e.to_string())?
        .create()
        .map_err(|e| e.to_string())?
        .add_rule(PathBeneath::new(
            PathFd::new("/").map_err(|e| e.to_string())?,
            AccessFs::from_read(landlock::ABI::V1),
        ))
        .map_err(|e| e.to_string())?
        .add_rule(PathBeneath::new(
            PathFd::new(worktree).map_err(|e| e.to_string())?,
            AccessFs::from_all(landlock::ABI::V1),
        ))
        .map_err(|e| e.to_string())?
        .add_rule(PathBeneath::new(
            PathFd::new("/tmp").map_err(|e| e.to_string())?,
            AccessFs::from_all(landlock::ABI::V1),
        ))
        .map_err(|e| e.to_string())?
        .restrict_self()
        .map_err(|e| e.to_string())?;
    let _ = status;
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn apply_landlock_pre_exec(_worktree: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The macOS wrapper produces a sandbox-exec invocation whose profile
    /// denies network and scopes writes to the worktree.
    #[test]
    fn macos_wrap_produces_sandbox_exec_argv() {
        if !cfg!(target_os = "macos") {
            println!("seatbelt is macOS-only; wrap test skipped");
            return;
        }
        let argv = vec!["sh".to_string(), "-c".to_string(), "echo hi".to_string()];
        let wrapped = wrap_argv(&argv, Path::new("/tmp/modbit-wt")).unwrap().unwrap();
        assert_eq!(wrapped[0], "sandbox-exec");
        assert_eq!(wrapped[1], "-f");
        let profile = std::fs::read_to_string(&wrapped[2]).unwrap();
        assert!(profile.contains("(deny default)"));
        assert!(profile.contains("(deny network*)"));
        assert!(profile.contains("/tmp/modbit-wt"));
        assert_eq!(&wrapped[3..], argv.as_slice(), "original argv preserved");
        let _ = std::fs::remove_dir_all(Path::new(&wrapped[2]).parent().unwrap());
    }

    /// Linux: the wrapper marks the argv for Landlock pre-exec.
    #[cfg(target_os = "linux")]
    #[test]
    fn linux_wrap_marks_landlock() {
        let argv = vec!["sh".to_string(), "-c".to_string(), "echo hi".to_string()];
        let wrapped = wrap_argv(&argv, Path::new("/tmp/modbit-wt")).unwrap().unwrap();
        assert_eq!(wrapped[0], "__MODBIT_LANDLOCK__");
        assert_eq!(&wrapped[1..], argv.as_slice());
    }
}
