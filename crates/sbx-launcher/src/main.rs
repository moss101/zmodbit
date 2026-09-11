//! sbx-launcher — applies a Landlock FS restriction profile to itself and
//! then runs the target command as a child, mirroring its exit code.
//!
//! Contract (Phase 6 item 2, docs/21 § sandbox):
//!   sbx-launcher --cwd <dir> --write <dir> ... -- <argv...>
//! Reads stay unrestricted; writes are allowed only under the --cwd and
//! --write roots. On kernels without Landlock this is a documented no-op
//! (best-effort, never blocks the launch). On non-Linux platforms the
//! launcher runs the command directly (the OS sandbox there is applied by
//! other means — Seatbelt wrapper on macOS, restricted token on Windows).

fn apply(cwd: Option<&std::path::Path>, writes: &[std::path::PathBuf]) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        use landlock::{Access as _, AccessFs, PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreatedAttr as _};
        let abi = landlock::ABI::V1;
        let mut created = Ruleset::default()
            .handle_access(AccessFs::from_all(abi))
            .map_err(|e| e.to_string())?
            .create()
            .map_err(|e| e.to_string())?;
        // Reads everywhere.
        created = created
            .add_rule(PathBeneath::new(
                PathFd::new("/")?,
                AccessFs::from_read(abi),
            ))
            .map_err(|e| e.to_string())?;
        // Writes: cwd + every --write root.
        if let Some(dir) = cwd {
            created = created
                .add_rule(PathBeneath::new(
                    PathFd::new(dir).map_err(|e| e.to_string())?,
                    AccessFs::from_all(abi),
                ))
                .map_err(|e| e.to_string())?;
        }
        for w in writes {
            created = created
                .add_rule(PathBeneath::new(PathFd::new(w).map_err(|e| e.to_string())?, AccessFs::from_all(abi)))
                .map_err(|e| e.to_string())?;
        }
        created
            .restrict_self()
            .map_err(|e| e.to_string())
            .map(|_| ())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (cwd, writes);
        Ok(())
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mut cwd: Option<std::path::PathBuf> = None;
    let mut writes: Vec<std::path::PathBuf> = Vec::new();
    let mut target: Vec<String> = Vec::new();
    let mut after_dashdash = false;
    while let Some(a) = args.next() {
        if after_dashdash {
            target.push(a);
            continue;
        }
        match a.as_str() {
            "--cwd" => cwd = args.next().map(std::path::PathBuf::from),
            "--write" => writes.push(args.next().map(std::path::PathBuf::from).unwrap_or_default()),
            "--" => after_dashdash = true,
            other => target.push(other.to_string()),
        }
    }
    if target.is_empty() {
        eprintln!("sbx-launcher: no command given");
        std::process::exit(125);
    }

    if let Err(e) = apply(cwd.as_deref(), &writes) {
        eprintln!("sbx-launcher: landlock apply failed (best-effort): {e}");
    }
    if let Some(dir) = cwd {
        let _ = std::env::set_current_dir(&dir);
    }

    let status = std::process::Command::new(&target[0])
        .args(&target[1..])
        .status()
        .unwrap_or_else(|e| {
            eprintln!("sbx-launcher: exec {:?}: {e}", target[0]);
            std::process::exit(127);
        });
    std::process::exit(status.code().unwrap_or(1));
}
