//! modbit-terminal — durable exec broker: structured argv, offset-addressed
//! output replay, typed exit results, stop.
//!
//! Implements the M2 terminal slice of docs/21 § Structured command contract:
//! argv[] only (no shell strings), a durable append-only output log per run,
//! typed exit results (MOD-EXEC-001: command failure is a typed outcome, not
//! a turn failure), offset-addressed replay for reconnect, and durable stop.
//!
//! Canonical owner subsystem: terminal (docs/81). Layout: docs/12.

use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// Per-read cap so clients bound their own buffers (docs/33 bounded queue).
pub const READ_CHUNK_MAX: usize = 512 * 1024;

#[derive(Debug)]
pub enum TerminalError {
    UnknownRun(String),
    EmptyArgv,
    AlreadyExists(String),
    Io(std::io::Error),
    Serialization(serde_json::Error),
}

impl fmt::Display for TerminalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TerminalError::UnknownRun(id) => write!(f, "unknown run {id}"),
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
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunMeta {
    pub run_id: String,
    pub argv: Vec<String>,
    pub state: RunState,
    pub started_at_ms: u128,
    pub ended_at_ms: Option<u128>,
}

/// The durable exec broker: every run gets a directory with an append-only
/// `output.log` and a `status.json`; output is read by offset for reconnect.
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

    /// Spawns a structured argv command: stdout and stderr append into the
    /// run's durable `output.log`; the child is tracked for typed exit and
    /// stop.
    pub fn spawn(&self, run_id: &str, argv: &[String]) -> Result<(), TerminalError> {
        if argv.is_empty() {
            return Err(TerminalError::EmptyArgv);
        }
        let dir = self.run_dir(run_id);
        fs::create_dir_all(&dir)?;
        let output_log = dir.join("output.log");
        let log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&output_log)?;
        let log_err = log.try_clone()?;

        let child = Command::new(&argv[0])
            .args(&argv[1..])
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(log_err))
            .spawn()?;

        let mut children = self.children.lock().expect("children mutex poisoned");
        children.insert(run_id.to_string(), child);
        self.persist_meta(
            run_id,
            &RunMeta {
                run_id: run_id.to_string(),
                argv: argv.to_vec(),
                state: RunState::Running,
                started_at_ms: now_ms(),
                ended_at_ms: None,
            },
        )?;
        Ok(())
    }

    /// Waits for the child and records the typed exit code durably.
    pub fn wait_and_record(&self, run_id: &str) -> Result<RunState, TerminalError> {
        let mut child = self
            .children
            .lock()
            .expect("children mutex poisoned")
            .remove(run_id)
            .ok_or_else(|| TerminalError::UnknownRun(run_id.to_string()))?;
        let status = child.wait()?;
        let state = RunState::Exited(status.code().unwrap_or(-1) as i64);
        self.record_state(run_id, &state)?;
        Ok(state)
    }

    /// Typed status from the durable record. A still-Running entry whose
    /// child has exited is reconciled here: the typed exit is recorded
    /// durably before it is reported.
    pub fn status(&self, run_id: &str) -> Result<RunMeta, TerminalError> {
        if !self.run_dir(run_id).exists() {
            return Err(TerminalError::UnknownRun(run_id.to_string()));
        }
        let path = self.run_dir(run_id).join("status.json");
        let mut meta: RunMeta = serde_json::from_slice(&fs::read(path)?)?;
        if meta.state == RunState::Running {
            let reaped = self
                .children
                .lock()
                .expect("children mutex poisoned")
                .remove(run_id)
                .and_then(|mut child| child.try_wait().ok().flatten())
                .map(|status| status.code().unwrap_or(-1) as i64);
            if let Some(code) = reaped {
                let state = RunState::Exited(code);
                self.record_state(run_id, &state)?;
                meta.state = state;
                meta.ended_at_ms = Some(now_ms());
            }
        }
        Ok(meta)
    }

    /// Offset-addressed output replay: up to `max` bytes from `offset`;
    /// returns the bytes and the new offset. Reconnect-safe — the log is
    /// durable and append-only.
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

    /// Stops a running process (typed kill) and records it durably.
    pub fn stop(&self, run_id: &str) -> Result<(), TerminalError> {
        let mut child = self
            .children
            .lock()
            .expect("children mutex poisoned")
            .remove(run_id)
            .ok_or_else(|| TerminalError::UnknownRun(run_id.to_string()))?;
        child.kill()?;
        let _ = child.wait();
        self.record_state(run_id, &RunState::Killed)?;
        Ok(())
    }

    fn record_state(&self, run_id: &str, state: &RunState) -> Result<(), TerminalError> {
        let mut meta = self.status(run_id)?;
        meta.state = state.clone();
        if matches!(state, RunState::Exited(_) | RunState::Killed) {
            meta.ended_at_ms = Some(now_ms());
        }
        self.persist_meta(run_id, &meta)
    }

    fn persist_meta(&self, run_id: &str, meta: &RunMeta) -> Result<(), TerminalError> {
        let path = self.run_dir(run_id).join("status.json");
        fs::write(
            path,
            serde_json::to_vec(meta).map_err(TerminalError::Serialization)?,
        )?;
        Ok(())
    }

    /// Lists all run directories' metadata (durable across broker restarts).
    pub fn list(&self) -> Result<Vec<RunMeta>, TerminalError> {
        let mut out = Vec::new();
        for entry in fs::read_dir(&self.runs_dir)? {
            let entry = entry?;
            let status = entry.path().join("status.json");
            if status.exists() {
                let meta: RunMeta = serde_json::from_slice(&fs::read(status)?)
                    .map_err(TerminalError::Serialization)?;
                out.push(meta);
            }
        }
        Ok(out)
    }
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}
