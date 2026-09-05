//! Client for the `modbit-execd` process broker (docs/21 § Durable
//! modbit-execd). Speaks the JSON-line TCP protocol; every command shell
//! execution routes through this boundary so output is durable, offset-
//! addressable and survives client restarts — no direct `Command` spawns
//! from tool runtime code.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::{RunState, TerminalError};

/// One request per connection: execd answers each JSON line and keeps the
/// connection open for more, but a fresh connection per call keeps the
/// client trivially thread-safe.
#[derive(Clone, Debug)]
pub struct ExecdClient {
    addr: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SpawnStatus {
    pub state: RunState,
    pub argv: Vec<String>,
}

impl ExecdClient {
    pub fn connect(addr: &str) -> Result<Self, TerminalError> {
        let client = ExecdClient {
            addr: addr.to_string(),
        };
        // Fail fast: prove the broker answers before returning the handle.
        client.call(&serde_json::json!({ "op": "list" }))?;
        Ok(client)
    }

    pub fn addr(&self) -> &str {
        &self.addr
    }

    /// Spawns a run pinned to `cwd` (REQ-EV-0100: honored exactly).
    pub fn spawn(
        &self,
        run_id: &str,
        argv: &[String],
        cwd: Option<&Path>,
    ) -> Result<(), TerminalError> {
        let mut request = serde_json::json!({ "op": "spawn", "id": run_id, "argv": argv });
        if let Some(cwd) = cwd {
            request["cwd"] = Value::String(cwd.display().to_string());
        }
        self.expect_ok(&request)
    }

    pub fn status(&self, run_id: &str) -> Result<SpawnStatus, TerminalError> {
        let response = self.call(&serde_json::json!({ "op": "status", "id": run_id }))?;
        if !response.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            return Err(broker_error(&response));
        }
        let state = response
            .get("state")
            .and_then(|v| v.as_str())
            .ok_or_else(|| TerminalError::UnknownRun(run_id.to_string()))?;
        let state = parse_state(state)?;
        let argv = response
            .get("argv")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        Ok(SpawnStatus { state, argv })
    }

    /// Reads output bytes from `offset`, returning (bytes, new_offset).
    pub fn read_output(
        &self,
        run_id: &str,
        offset: u64,
        max: usize,
    ) -> Result<(Vec<u8>, u64), TerminalError> {
        let response = self.call(&serde_json::json!({
            "op": "read", "id": run_id, "offset": offset, "max": max
        }))?;
        if !response.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            return Err(broker_error(&response));
        }
        let encoded = response
            .get("data")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        use base64::Engine as _;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|e| TerminalError::Io(std::io::Error::other(e)))?;
        let new_offset = response.get("offset").and_then(|v| v.as_u64()).unwrap_or(offset);
        Ok((bytes, new_offset))
    }

    pub fn stop(&self, run_id: &str) -> Result<(), TerminalError> {
        self.expect_ok(&serde_json::json!({ "op": "stop", "id": run_id }))
    }

    /// Waits until the run leaves the Running state (bounded polling).
    pub fn wait(&self, run_id: &str, timeout: Duration) -> Result<SpawnStatus, TerminalError> {
        let deadline = Instant::now() + timeout;
        loop {
            let status = self.status(run_id)?;
            if status.state != RunState::Running {
                return Ok(status);
            }
            if Instant::now() >= deadline {
                // Bounded wait: stop the run rather than hang the caller.
                self.stop(run_id)?;
                return Err(TerminalError::Timeout(run_id.to_string()));
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    /// Convenience: spawn, wait, and collect all output (bounded).
    pub fn run_capture(
        &self,
        run_id: &str,
        argv: &[String],
        cwd: Option<&Path>,
        timeout: Duration,
        max_output: usize,
    ) -> Result<(SpawnStatus, Vec<u8>), TerminalError> {
        self.spawn(run_id, argv, cwd)?;
        let status = self.wait(run_id, timeout)?;
        let (bytes, _) = self.read_output(run_id, 0, max_output)?;
        Ok((status, bytes))
    }

    /// Cancellable capture (Phase 2.3): while the run is executing, the
    /// supplied flag is polled; when it flips the broker run is STOPPED
    /// (killed, no orphan process) and the typed `Cancelled` error
    /// returns — never a hang, never an unbounded wait.
    pub fn run_capture_cancellable(
        &self,
        run_id: &str,
        argv: &[String],
        cwd: Option<&Path>,
        timeout: Duration,
        max_output: usize,
        cancelled: &std::sync::atomic::AtomicBool,
    ) -> Result<(SpawnStatus, Vec<u8>), TerminalError> {
        self.spawn(run_id, argv, cwd)?;
        let deadline = Instant::now() + timeout;
        loop {
            if cancelled.load(std::sync::atomic::Ordering::SeqCst) {
                self.stop(run_id)?;
                return Err(TerminalError::Cancelled(run_id.to_string()));
            }
            let status = self.status(run_id)?;
            if status.state != RunState::Running {
                let (bytes, _) = self.read_output(run_id, 0, max_output)?;
                return Ok((status, bytes));
            }
            if Instant::now() >= deadline {
                self.stop(run_id)?;
                return Err(TerminalError::Timeout(run_id.to_string()));
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    fn expect_ok(&self, request: &Value) -> Result<(), TerminalError> {
        let response = self.call(request)?;
        if response.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            Ok(())
        } else {
            Err(broker_error(&response))
        }
    }

    fn call(&self, request: &Value) -> Result<Value, TerminalError> {
        let mut stream = TcpStream::connect(&self.addr)
            .map_err(|e| TerminalError::Io(std::io::Error::other(e)))?;
        let line = serde_json::to_string(request)
            .map_err(|e| TerminalError::Io(std::io::Error::other(e)))?;
        stream
            .write_all(line.as_bytes())
            .and_then(|_| stream.write_all(b"\n"))
            .map_err(TerminalError::Io)?;
        let mut reader = BufReader::new(stream);
        let mut response = String::new();
        reader
            .read_line(&mut response)
            .map_err(TerminalError::Io)?;
        serde_json::from_str(&response)
            .map_err(|e| TerminalError::Io(std::io::Error::other(e)))
    }
}

fn parse_state(state: &str) -> Result<RunState, TerminalError> {
    match state {
        "running" => Ok(RunState::Running),
        s if s.starts_with("exited(") && s.ends_with(')') => s[7..s.len() - 1]
            .parse::<i64>()
            .map(RunState::Exited)
            .map_err(|_| TerminalError::UnknownRun(state.to_string())),
        "killed" => Ok(RunState::Killed),
        "interrupted" => Ok(RunState::Interrupted),
        other => Err(TerminalError::UnknownRun(other.to_string())),
    }
}

fn broker_error(response: &Value) -> TerminalError {
    TerminalError::UnknownRun(
        response
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown broker error")
            .to_string(),
    )
}
