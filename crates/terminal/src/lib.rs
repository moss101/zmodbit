//! modbit-terminal — durable exec broker: structured argv, offset-addressed
//! output replay, typed exit results, stop.
//!
//! Implements the M2 terminal slice of docs/21 § Structured command contract:
//! argv[] only (no shell strings), a durable append-only output log per run,
//! typed exit results (MOD-EXEC-001), offset replay for reconnect, and stop.
//!
//! Canonical owner subsystem: terminal (docs/81). Layout: docs/12.

use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;

pub mod broker_ext;
pub mod client;
pub mod command_contract;
pub mod replay;

use serde::{Deserialize, Serialize};

/// Per-read cap so clients bound their own buffers (docs/33 bounded queue).
pub const READ_CHUNK_MAX: usize = 512 * 1024;

#[derive(Debug)]
pub enum TerminalError {
    UnknownRun(String),
    /// A bounded wait elapsed before the run left the Running state; the
    /// caller stopped it (no orphan processes).
    Timeout(String),
    EmptyArgv,
    AlreadyExists(String),
    Io(std::io::Error),
    Serialization(serde_json::Error),
}

impl fmt::Display for TerminalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TerminalError::UnknownRun(id) => write!(f, "unknown run {id}"),
            TerminalError::Timeout(id) => write!(f, "timed out waiting for run {id} (stopped)"),
            TerminalError::EmptyArgv => write!(f, "empty argv"),
            TerminalError::AlreadyExists(p) => write!(f, "already exists: {p}"),
            TerminalError::Io(e) => write!(f, "io: {e}"),
            TerminalError::Serialization(e) => write!(f, "serde: {e}"),
        }
    }
}

impl std::error::Error for TerminalError {}

impl From<std::io::Error> for TerminalError {
    fn from(e: std::io::Error) -> Self {
        TerminalError::Io(e)
    }
}

impl From<serde_json::Error> for TerminalError {
    fn from(e: serde_json::Error) -> Self {
        TerminalError::Serialization(e)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum RunState {
    Running,
    Exited(i64),
    Killed,
    /// The run ENDED while detached from this broker (e.g. after a UI
    /// restart): we know it is gone, but the exit code is unobservable.
    /// Docs/21: surface the unobservable case explicitly — never guess.
    Interrupted,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunMeta {
    pub run_id: String,
    pub argv: Vec<String>,
    pub state: RunState,
    pub started_at_ms: u128,
    pub ended_at_ms: Option<u128>,
    /// OS process id, recorded so a RESTARTED UI can reattach to and
    /// cancel a detached run (REQ-EV-0027/0135). Absent in legacy files.
    #[serde(default)]
    pub pid: Option<u32>,
}

pub struct ExecBroker {
    runs_dir: PathBuf,
    children: Mutex<HashMap<String, Child>>,
}

impl ExecBroker {
    pub fn open(runs_dir: &Path) -> Result<Self, TerminalError> {
        fs::create_dir_all(runs_dir)?;
        Ok(Self {
            runs_dir: runs_dir.to_path_buf(),
            children: Mutex::new(HashMap::new()),
        })
    }

    fn run_dir(&self, run_id: &str) -> PathBuf {
        self.runs_dir.join(run_id)
    }

    pub fn spawn(&self, run_id: &str, argv: &[String]) -> Result<(), TerminalError> {
        self.spawn_full(run_id, argv, None, &[])
    }

    /// Spawn with explicit cwd and env additions (REQ-EV-0100 contract:
    /// these fields are honored exactly, never inherited by accident).
    pub fn spawn_full(
        &self,
        run_id: &str,
        argv: &[String],
        cwd: Option<&Path>,
        env: &[(String, String)],
    ) -> Result<(), TerminalError> {
        if argv.is_empty() {
            return Err(TerminalError::EmptyArgv);
        }
        let dir = self.run_dir(run_id);
        fs::create_dir_all(&dir)?;
        let log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("output.log"))?;
        let log_err = log.try_clone()?;
        let mut command = Command::new(&argv[0]);
        command.args(&argv[1..]);
        if let Some(dir_path) = cwd {
            command.current_dir(dir_path);
        }
        for (k, v) in env {
            command.env(k, v);
        }
        let child = command
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(log_err))
            .spawn()?;
        let mut children = self.children.lock().expect("poisoned");
        let pid = child.id();
        children.insert(run_id.to_string(), child);
        let meta = RunMeta {
            run_id: run_id.to_string(),
            argv: argv.to_vec(),
            state: RunState::Running,
            started_at_ms: now_ms(),
            ended_at_ms: None,
            pid: Some(pid),
        };
        self.persist_meta(run_id, &meta)?;
        Ok(())
    }

    /// Whether this broker holds an in-memory child handle for the run
    /// (diagnostics for the detached/reattach paths).
    pub fn has_child(&self, run_id: &str) -> bool {
        self.children.lock().expect("poisoned").contains_key(run_id)
    }

    pub fn status(&self, run_id: &str) -> Result<RunMeta, TerminalError> {
        if !self.run_dir(run_id).exists() {
            return Err(TerminalError::UnknownRun(run_id.to_string()));
        }
        let path = self.run_dir(run_id).join("status.json");
        let mut meta: RunMeta = serde_json::from_slice(&fs::read(path)?)?;
        if meta.state == RunState::Running {
            // A HELD child handle is the only truth while we own it:
            // try_wait(None) means running — never fall through to the
            // detached pid probe for a run this broker spawned (a just-exited
            // child can look dead to `ps` before the reaping happens here).
            // Option<io::Result<Option<i32>>>: None = no handle (detached);
            // Some(Ok(None)) = held and genuinely running.
            let held: Option<std::io::Result<Option<i32>>> = {
                let mut children = self.children.lock().expect("poisoned");
                match children.get_mut(run_id) {
                    Some(child) => match child.try_wait() {
                        Ok(Some(status)) => Some(Ok(Some(status.code().unwrap_or(-1)))),
                        Ok(None) => Some(Ok(None)),
                        Err(e) => Some(Err(e)),
                    },
                    None => None,
                }
            };
            match held {
                Some(Ok(Some(code))) => {
                    meta.state = RunState::Exited(code as i64);
                    self.persist_meta(run_id, &meta)?;
                }
                // Held and running, or reaped by a concurrent poll (the
                // terminal state is already persisted or arrives next).
                Some(Ok(None)) | Some(Err(_)) => {
                    // Reaped by a concurrent status; the persisted meta (or
                    // the next read) already carries the terminal state.
                    // Never downgrade a held run to Interrupted here.
                }
                None => {
                    // Detached run: this broker owns no child handle. Probe
                    // liveness by pid; if the process is gone, the run ended
                    // while detached — typed Interrupted, not stuck Running.
                    if let Some(pid) = meta.pid {
                        if !pid_alive(pid) {
                            meta.state = RunState::Interrupted;
                            meta.ended_at_ms = Some(now_ms());
                            self.persist_meta(run_id, &meta)?;
                        }
                    }
                }
            }
        }
        Ok(meta)
    }

    pub fn read_output(
        &self,
        run_id: &str,
        offset: u64,
        max: usize,
    ) -> Result<(Vec<u8>, u64), TerminalError> {
        let log = self.run_dir(run_id).join("output.log");
        let mut file = fs::File::open(&log)?;
        let total = file.metadata()?.len();
        let start = offset.min(total);
        file.seek(SeekFrom::Start(start))?;
        let take = ((READ_CHUNK_MAX.min(max)) as u64).min(total - start) as usize;
        let mut buf = vec![0u8; take];
        let mut filled = 0;
        while filled < take {
            let n = file.read(&mut buf[filled..])?;
            if n == 0 {
                break;
            }
            filled += n;
        }
        buf.truncate(filled);
        let new_offset = start + filled as u64;
        Ok((buf, new_offset))
    }

    /// Stops a run: through the in-memory child handle when this broker
    /// owns it, or by the recorded OS pid when the run was spawned by a
    /// previous (restarted) UI — detach/reattach never orphans a process
    /// (REQ-EV-0027/0135).
    pub fn stop(&self, run_id: &str) -> Result<(), TerminalError> {
        let in_memory = self.children.lock().expect("poisoned").remove(run_id);
        if let Some(mut child) = in_memory {
            child.kill()?;
            let _ = child.wait();
        } else {
            let meta_path = self.run_dir(run_id).join("status.json");
            let meta: RunMeta = serde_json::from_slice(&fs::read(&meta_path)?)?;
            let pid = meta
                .pid
                .ok_or_else(|| TerminalError::UnknownRun(run_id.to_string()))?;
            if !kill_pid(pid) {
                return Err(TerminalError::Io(std::io::Error::other(format!(
                    "failed to kill detached run {run_id} (pid {pid})"
                ))));
            }
        }
        let mut meta_path = self.run_dir(run_id);
        meta_path.push("status.json");
        let mut meta: RunMeta = serde_json::from_slice(&fs::read(&meta_path)?)?;
        meta.state = RunState::Killed;
        meta.ended_at_ms = Some(now_ms());
        fs::write(
            &meta_path,
            serde_json::to_vec(&meta).map_err(std::io::Error::other)?,
        )?;
        Ok(())
    }

    pub fn list(&self) -> Result<Vec<RunMeta>, TerminalError> {
        let mut out = Vec::new();
        for entry in fs::read_dir(&self.runs_dir)? {
            let entry = entry?;
            let status = entry.path().join("status.json");
            if status.exists() {
                let meta: RunMeta = serde_json::from_slice(&fs::read(&status)?)
                    .map_err(TerminalError::Serialization)?;
                out.push(meta);
            }
        }
        Ok(out)
    }

    fn persist_meta(&self, run_id: &str, meta: &RunMeta) -> Result<(), TerminalError> {
        let path = self.run_dir(run_id).join("status.json");
        fs::write(
            path,
            serde_json::to_vec(meta).map_err(TerminalError::Serialization)?,
        )?;
        Ok(())
    }

    pub fn wait_and_record(&self, run_id: &str) -> Result<RunState, TerminalError> {
        let mut child = self
            .children
            .lock()
            .expect("poisoned")
            .remove(run_id)
            .ok_or_else(|| TerminalError::UnknownRun(run_id.to_string()))?;
        let status = child.wait()?;
        let state = RunState::Exited(status.code().map(|c| c as i64).unwrap_or(-1));
        self.record_state(run_id, &state)?;
        Ok(state)
    }

    fn record_state(&self, run_id: &str, state: &RunState) -> Result<(), TerminalError> {
        let mut meta = self.status(run_id)?;
        meta.state = state.clone();
        if matches!(state, RunState::Exited(_) | RunState::Killed) {
            meta.ended_at_ms = Some(now_ms());
        }
        self.persist_meta(run_id, &meta)
    }
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Terminates an OS process by pid — the detach/reattach cancel path for
/// runs this broker process does not own.
fn kill_pid(pid: u32) -> bool {
    #[cfg(windows)]
    {
        Command::new("taskkill")
            .args(["/F", "/PID", &pid.to_string()])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
    #[cfg(not(windows))]
    {
        Command::new("kill")
            .arg(pid.to_string())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
}

/// Reports whether an OS pid is ALIVE RUNNING — a zombie counts as dead:
/// it no longer executes, it is merely an unreaped pid entry.
pub fn pid_alive(pid: u32) -> bool {
    #[cfg(windows)]
    {
        Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}")])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains(&pid.to_string()))
            .unwrap_or(false)
    }
    #[cfg(not(windows))]
    {
        Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .map(|o| {
                let stat = String::from_utf8_lossy(&o.stdout).trim().to_string();
                !stat.is_empty() && !stat.starts_with('Z')
            })
            .unwrap_or(false)
    }
}
