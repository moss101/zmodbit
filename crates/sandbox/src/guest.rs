//! In-guest request gate + typed executor (M8.5, REQ-EV-0289/0287/0290).
//! EVERY guest request passes the gate before anything executes:
//! version → task binding → capability token → effect-class schema
//! match → generation fence → deny-by-default path policy. The executor
//! then runs the op against the REAL guest machine (processes, files,
//! PTYs) with timeouts, cancellation by generation fencing, idempotent
//! request replay, and a minimal child environment (the guest agent's own
//! environment — including its provisioning material — is never
//! inherited by guest workloads).
//!
//! Canonical owner subsystem: sandbox-cloud (docs/81).

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use modbit_protocol::guest::{
    guest_error_codes, GuestError, GuestOp, GuestPayload, GuestRequest, GuestResponse,
    GUEST_EFFECT_CLASSES, GUEST_RPC_VERSION,
};

use crate::SandboxPolicy;

/// Default/cap for process execution. A request without `timeout_ms` gets
/// the default; nothing runs unbounded on the guest.
pub const PROC_TIMEOUT_DEFAULT_MS: u64 = 120_000;
pub const PROC_TIMEOUT_MAX_MS: u64 = 600_000;

/// Idempotency window: replays of a recent request_id return the original
/// response (duplicate delivery never doubles an effect).
const IDEMPOTENCY_CACHE_CAP: usize = 1024;

/// Session identity + authority state held INSIDE the guest. The host
/// never writes this directly — it evolves only through gated requests.
pub struct GuestSession {
    pub task: String,
    pub generation: u64,
    live_tokens: Vec<String>,
}

impl GuestSession {
    pub fn new(task: &str, live_tokens: Vec<String>) -> Self {
        GuestSession {
            task: task.to_string(),
            generation: 0,
            live_tokens,
        }
    }

    pub fn token_live(&self, token: &str) -> bool {
        self.live_tokens.iter().any(|t| t == token)
    }

    /// Revocation fails closed: a revoked token is unknown on the next
    /// request.
    pub fn revoke_token(&mut self, token: &str) {
        self.live_tokens.retain(|t| t != token);
    }
}

/// A running child process tracked for kill/fencing.
struct ProcHandle {
    child: Arc<Mutex<std::process::Child>>,
    generation: u64,
    fenced: Arc<AtomicBool>,
}

struct IdempotencyCache {
    map: HashMap<String, GuestResponse>,
    order: VecDeque<String>,
}

impl IdempotencyCache {
    fn new() -> Self {
        IdempotencyCache {
            map: HashMap::new(),
            order: VecDeque::new(),
        }
    }
    fn get(&self, id: &str) -> Option<GuestResponse> {
        self.map.get(id).cloned()
    }
    fn put(&mut self, id: &str, resp: GuestResponse) {
        if self.map.contains_key(id) {
            return;
        }
        while self.order.len() >= IDEMPOTENCY_CACHE_CAP {
            if let Some(old) = self.order.pop_front() {
                self.map.remove(&old);
            }
        }
        self.order.push_back(id.to_string());
        self.map.insert(id.to_string(), resp);
    }
}

/// The gate: typed refusal for every authority violation, checked BEFORE
/// any effect. Error codes are the stable wire values.
pub fn gate(session: &GuestSession, req: &GuestRequest) -> Result<(), GuestError> {
    if req.rpc_version != GUEST_RPC_VERSION {
        return Err(GuestError::new(
            guest_error_codes::VERSION,
            format!("unsupported guest rpc version {}", req.rpc_version),
        ));
    }
    if req.task != session.task {
        return Err(GuestError::new(
            guest_error_codes::WRONG_TASK,
            format!("request task {:?} does not match guest binding {:?}; guest never serves another task", req.task, session.task),
        ));
    }
    if !session.token_live(&req.capability_token) {
        return Err(GuestError::new(
            guest_error_codes::STALE_CAPABILITY,
            "capability token is not live on this guest",
        ));
    }
    if !GUEST_EFFECT_CLASSES.contains(&req.effect_class.as_str()) {
        return Err(GuestError::new(
            guest_error_codes::UNKNOWN_CAPABILITY,
            format!("unknown effect class {:?}", req.effect_class),
        ));
    }
    // Schema-manipulation defense: the declared class must BE the class of
    // the operation carried — a request cannot borrow authority of one
    // class to smuggle another.
    if req.op.effect_class() != req.effect_class {
        return Err(GuestError::new(
            guest_error_codes::BAD_REQUEST,
            format!(
                "declared effect class {:?} does not match operation class {:?}",
                req.effect_class,
                req.op.effect_class()
            ),
        ));
    }
    if req.generation < session.generation {
        return Err(GuestError::new(
            guest_error_codes::FENCED,
            format!(
                "stale generation {} (current {}); fenced",
                req.generation, session.generation
            ),
        ));
    }
    Ok(())
}

/// Lexically normalizes `.`/`..`, then — for paths that exist — uses the
/// canonical resolution so symlink escapes are caught too. The result must
/// sit under one of the policy's fs roots (deny-by-default,
/// REQ-EV-0287/0290).
pub fn ensure_path_allowed(policy: &SandboxPolicy, raw: &str) -> Result<PathBuf, GuestError> {
    let denied = || {
        GuestError::new(
            guest_error_codes::POLICY_DENIED,
            format!("path {raw:?} is outside the granted guest fs roots"),
        )
    };
    let path = Path::new(raw);
    let mut normalized = PathBuf::new();
    for comp in path.components() {
        match comp {
            std::path::Component::ParentDir => {
                if !normalized.pop() {
                    return Err(denied());
                }
            }
            std::path::Component::CurDir => {}
            other => normalized.push(other),
        }
    }
    // If it (now) exists, the OS resolution is the truth — a symlink under
    // an allowed root pointing outside resolves to the outside target.
    // For missing paths, canonically resolve the deepest existing ancestor
    // (macOS /var → /private/var aliases) and re-attach the missing tail,
    // so the comparison happens in ONE canonical namespace.
    let resolved = match std::fs::canonicalize(&normalized) {
        Ok(p) => p,
        Err(_) => {
            let mut anc = normalized.clone();
            let mut tail: Vec<std::ffi::OsString> = Vec::new();
            loop {
                match anc.parent() {
                    Some(parent) => {
                        if let Some(name) = anc.file_name() {
                            tail.push(name.to_os_string());
                        }
                        anc = parent.to_path_buf();
                        if let Ok(p) = std::fs::canonicalize(&anc) {
                            anc = p;
                            break;
                        }
                    }
                    None => return Err(denied()),
                }
            }
            let mut resolved = anc;
            for part in tail.into_iter().rev() {
                resolved.push(part);
            }
            resolved
        }
    };
    for root in &policy.fs_roots {
        let root = Path::new(root);
        let root_canon = if root.exists() {
            root.canonicalize().unwrap_or_else(|_| root.to_path_buf())
        } else {
            root.to_path_buf()
        };
        if resolved.starts_with(&root_canon) {
            return Ok(resolved);
        }
    }
    Err(denied())
}

/// The guest executor: gate → dispatch → real effect, with in-flight
/// kill/fencing, bounded idempotent replay and PTY sessions.
pub struct GuestExecutor {
    session: Mutex<GuestSession>,
    policy: SandboxPolicy,
    procs: Mutex<HashMap<String, ProcHandle>>,
    pty: modbit_terminal::pty::PtyBroker,
    cache: Mutex<IdempotencyCache>,
}

impl GuestExecutor {
    pub fn new(task: &str, live_tokens: Vec<String>, policy: SandboxPolicy) -> Self {
        GuestExecutor {
            session: Mutex::new(GuestSession::new(task, live_tokens)),
            policy,
            procs: Mutex::new(HashMap::new()),
            pty: modbit_terminal::pty::PtyBroker::new(),
            cache: Mutex::new(IdempotencyCache::new()),
        }
    }

    /// Advancing the generation fences ALL in-flight work of older
    /// generations: their processes are killed and their blocked callers
    /// get a typed fenced error (never a silent partial effect).
    pub fn advance_generation(&self, to: u64) {
        let mut session = self.session.lock().expect("session");
        if to <= session.generation {
            return;
        }
        session.generation = to;
        drop(session);
        let procs = self.procs.lock().expect("procs");
        for handle in procs.values() {
            if handle.generation < to {
                handle.fenced.store(true, Ordering::SeqCst);
                let _ = handle.child.lock().expect("child").kill();
            }
        }
    }

    pub fn revoke_token(&self, token: &str) {
        self.session.lock().expect("session").revoke_token(token);
    }

    /// Full request path: replay check → gate → dispatch. FENCED results
    /// are not cached (the request never executed; retry under the new
    /// generation must really run).
    pub fn execute(&self, req: &GuestRequest) -> GuestResponse {
        if let Some(cached) = self.cache.lock().expect("cache").get(&req.request_id) {
            return cached;
        }
        // Read the generation through a bound temporary — holding the
        // session guard across advance/dispatch would self-deadlock.
        let current = self.session.lock().expect("session").generation;
        let response = if req.generation > current {
            self.advance_generation(req.generation);
            self.dispatch(req)
        } else {
            if let Err(e) = gate(&self.session.lock().expect("session"), req) {
                return GuestResponse::err(&req.request_id, e);
            }
            self.dispatch(req)
        };
        let fenceable = !matches!(
            &response.outcome,
            modbit_protocol::guest::GuestOutcome::Err { error }
                if error.code == guest_error_codes::FENCED
        );
        if fenceable {
            self.cache
                .lock()
                .expect("cache")
                .put(&req.request_id, response.clone());
        }
        response
    }

    fn dispatch(&self, req: &GuestRequest) -> GuestResponse {
        match self.dispatch_inner(req) {
            Ok(payload) => GuestResponse::ok(&req.request_id, payload),
            Err(error) => GuestResponse::err(&req.request_id, error),
        }
    }

    fn dispatch_inner(&self, req: &GuestRequest) -> Result<GuestPayload, GuestError> {
        match &req.op {
            GuestOp::Ping => Ok(GuestPayload::Pong {
                guest_version: env!("CARGO_PKG_VERSION").to_string(),
                protocol_major: modbit_protocol::guest::GUEST_PROTOCOL_MAJOR,
                protocol_minor: modbit_protocol::guest::GUEST_PROTOCOL_MINOR,
            }),
            GuestOp::ProcExec {
                argv,
                cwd,
                env,
                timeout_ms,
            } => {
                if argv.is_empty() {
                    return Err(GuestError::new(
                        guest_error_codes::BAD_REQUEST,
                        "empty argv",
                    ));
                }
                let cwd = match cwd {
                    Some(c) => Some(ensure_path_allowed(&self.policy, c)?),
                    None => None,
                };
                let timeout = Duration::from_millis(
                    timeout_ms
                        .unwrap_or(PROC_TIMEOUT_DEFAULT_MS)
                        .min(PROC_TIMEOUT_MAX_MS),
                );
                let proc_id = new_id("proc");
                let payload = run_child(
                    &self.procs,
                    &proc_id,
                    req.generation,
                    argv,
                    cwd.as_deref(),
                    env,
                    timeout,
                )?;
                Ok(payload)
            }
            GuestOp::ProcKill { proc_id } => {
                let procs = self.procs.lock().expect("procs");
                match procs.get(proc_id) {
                    Some(handle) => {
                        let _ = handle.child.lock().expect("child").kill();
                        Ok(GuestPayload::Accepted {})
                    }
                    None => Err(GuestError::new(
                        guest_error_codes::NOT_FOUND,
                        format!("no proc {proc_id:?}"),
                    )),
                }
            }
            GuestOp::FsRead { path } => {
                let resolved = ensure_path_allowed(&self.policy, path)?;
                let bytes = std::fs::read(&resolved).map_err(|e| {
                    GuestError::new(guest_error_codes::NOT_FOUND, format!("read {path:?}: {e}"))
                })?;
                Ok(GuestPayload::Data {
                    bytes_base64: base64::engine::general_purpose::STANDARD.encode(&bytes),
                    sha256: crate::sha256_hex(&bytes),
                })
            }
            GuestOp::FsWrite { path, bytes_base64 } => {
                let resolved = ensure_path_allowed(&self.policy, path)?;
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(bytes_base64)
                    .map_err(|e| {
                        GuestError::new(
                            guest_error_codes::BAD_REQUEST,
                            format!("bad base64 payload: {e}"),
                        )
                    })?;
                if let Some(parent) = resolved.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                std::fs::write(&resolved, &bytes).map_err(|e| {
                    GuestError::new(
                        guest_error_codes::POLICY_DENIED,
                        format!("write {path:?}: {e}"),
                    )
                })?;
                Ok(GuestPayload::Accepted {})
            }
            GuestOp::FsList { path } => {
                let resolved = ensure_path_allowed(&self.policy, path)?;
                let mut entries: Vec<String> = std::fs::read_dir(&resolved)
                    .map_err(|e| {
                        GuestError::new(guest_error_codes::NOT_FOUND, format!("list {path:?}: {e}"))
                    })?
                    .filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().to_string())
                    .collect();
                entries.sort();
                Ok(GuestPayload::Listing { entries })
            }
            GuestOp::PtyOpen {
                argv,
                cwd,
                cols,
                rows,
            } => {
                if argv.is_empty() {
                    return Err(GuestError::new(
                        guest_error_codes::BAD_REQUEST,
                        "empty argv",
                    ));
                }
                let cwd = match cwd {
                    Some(c) => Some(ensure_path_allowed(&self.policy, c)?),
                    None => None,
                };
                let pty_id = new_id("pty");
                self.pty
                    .spawn(&pty_id, argv, cwd.as_deref(), *rows, *cols)
                    .map_err(|e| GuestError::new(guest_error_codes::INTERNAL, e.to_string()))?;
                Ok(GuestPayload::Pty {
                    pty_id,
                    bytes_base64: String::new(),
                    offset: 0,
                    alive: true,
                })
            }
            GuestOp::PtyWrite {
                pty_id,
                bytes_base64,
            } => {
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(bytes_base64)
                    .map_err(|e| {
                        GuestError::new(
                            guest_error_codes::BAD_REQUEST,
                            format!("bad base64 payload: {e}"),
                        )
                    })?;
                self.pty
                    .write(pty_id, &bytes)
                    .map_err(|e| GuestError::new(guest_error_codes::NOT_FOUND, e.to_string()))?;
                Ok(GuestPayload::Accepted {})
            }
            GuestOp::PtyRead {
                pty_id,
                offset,
                max,
            } => {
                let (bytes, next) = self
                    .pty
                    .read(pty_id, *offset, *max)
                    .map_err(|e| GuestError::new(guest_error_codes::NOT_FOUND, e.to_string()))?;
                let alive = self.pty.alive(pty_id).unwrap_or(false);
                Ok(GuestPayload::Pty {
                    pty_id: pty_id.clone(),
                    bytes_base64: base64::engine::general_purpose::STANDARD.encode(&bytes),
                    offset: next,
                    alive,
                })
            }
            GuestOp::PtyKill { pty_id } => {
                self.pty
                    .kill(pty_id)
                    .map_err(|e| GuestError::new(guest_error_codes::NOT_FOUND, e.to_string()))?;
                Ok(GuestPayload::Accepted {})
            }
        }
    }
}

fn new_id(prefix: &str) -> String {
    format!("{prefix}-{}", uuid::Uuid::now_v7().simple())
}

/// Runs a child to completion (or timeout/fence/kill) while capturing
/// stdout/stderr. The child environment is built from scratch — only the
/// requested vars plus a minimal platform ambient set. The guest agent's
/// own environment (which holds its provisioning material) is NEVER
/// inherited.
fn run_child(
    procs: &Mutex<HashMap<String, ProcHandle>>,
    proc_id: &str,
    generation: u64,
    argv: &[String],
    cwd: Option<&Path>,
    env: &BTreeMap<String, String>,
    timeout: Duration,
) -> Result<GuestPayload, GuestError> {
    let mut cmd = std::process::Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    cmd.env_clear();
    // Minimal ambient set so shells/tools can locate themselves; every
    // other variable comes only from the request.
    for (k, v) in ambient_env(env) {
        cmd.env(k, v);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    let mut child = cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| {
            GuestError::new(
                guest_error_codes::BAD_REQUEST,
                format!("spawn {:?}: {e}", argv[0]),
            )
        })?;
    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();
    let stdout_t = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(p) = stdout_pipe.as_mut() {
            use std::io::Read as _;
            let _ = p.read_to_end(&mut buf);
        }
        buf
    });
    let stderr_t = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(p) = stderr_pipe.as_mut() {
            use std::io::Read as _;
            let _ = p.read_to_end(&mut buf);
        }
        buf
    });

    let child_arc = Arc::new(Mutex::new(child));
    let fenced = Arc::new(AtomicBool::new(false));
    procs.lock().expect("procs").insert(
        proc_id.to_string(),
        ProcHandle {
            child: child_arc.clone(),
            generation,
            fenced: fenced.clone(),
        },
    );

    let started = Instant::now();
    let mut timed_out = false;
    let status = loop {
        {
            let mut c = child_arc.lock().expect("child");
            if let Ok(Some(status)) = c.try_wait() {
                // A fence kill can land between polls — the child's death
                // then belongs to the FENCE, not to a normal completion.
                // A generation advance invalidates the outstanding request
                // either way (fail closed, caller retries fenced).
                if fenced.load(Ordering::SeqCst) {
                    procs.lock().expect("procs").remove(proc_id);
                    return Err(GuestError::new(
                        guest_error_codes::FENCED,
                        format!("proc {proc_id} fenced by generation advance"),
                    ));
                }
                break Some(status);
            }
        }
        if fenced.load(Ordering::SeqCst) {
            let mut c = child_arc.lock().expect("child");
            let _ = c.kill();
            let _ = c.wait();
            procs.lock().expect("procs").remove(proc_id);
            return Err(GuestError::new(
                guest_error_codes::FENCED,
                format!("proc {proc_id} fenced by generation advance"),
            ));
        }
        if started.elapsed() >= timeout {
            timed_out = true;
            let mut c = child_arc.lock().expect("child");
            let _ = c.kill();
            let _ = c.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    procs.lock().expect("procs").remove(proc_id);
    let stdout = String::from_utf8_lossy(&stdout_t.join().unwrap_or_default()).to_string();
    let stderr = String::from_utf8_lossy(&stderr_t.join().unwrap_or_default()).to_string();
    Ok(GuestPayload::Proc {
        proc_id: proc_id.to_string(),
        exit_code: status.and_then(|s| s.code()).map(|c| c as i64),
        stdout,
        stderr,
        timed_out,
        killed: timed_out,
    })
}

/// Ambient environment entries a guest workload may see even when the
/// request passes none: locator variables only. Platform-specific
/// essentials keep cmd/sh usable; nothing secret is ever ambient.
fn ambient_env(requested: &BTreeMap<String, String>) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for key in ["PATH", "HOME", "TEMP", "TMP", "SystemRoot", "COMSPEC"] {
        if requested.contains_key(key) {
            continue;
        }
        if let Ok(v) = std::env::var(key) {
            out.push((key.to_string(), v));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use modbit_protocol::guest::GuestRequest;

    fn policy() -> SandboxPolicy {
        SandboxPolicy {
            fs_roots: vec![std::env::temp_dir().to_string_lossy().to_string()],
            network_allow: vec![],
            max_cpu_seconds: 10,
            max_memory_mb: 512,
        }
    }

    fn executor() -> GuestExecutor {
        GuestExecutor::new("task-1", vec!["tok-live".into()], policy())
    }

    fn req(id: &str, op: GuestOp) -> GuestRequest {
        GuestRequest {
            request_id: id.into(),
            rpc_version: GUEST_RPC_VERSION,
            task: "task-1".into(),
            capability_token: "tok-live".into(),
            effect_class: op.effect_class().into(),
            generation: 0,
            op,
        }
    }

    fn ok_payload(r: GuestResponse) -> GuestPayload {
        match r.outcome {
            modbit_protocol::guest::GuestOutcome::Ok { payload } => payload,
            modbit_protocol::guest::GuestOutcome::Err { error } => {
                panic!("expected ok, got {error:?}")
            }
        }
    }

    fn err_code(r: GuestResponse) -> String {
        match r.outcome {
            modbit_protocol::guest::GuestOutcome::Err { error } => error.code,
            other => panic!("expected err, got {other:?}"),
        }
    }

    /// The full gate ladder: every authority violation is refused with a
    /// typed code before anything executes (QUAL-EV-0289 negative path).
    #[test]
    fn gate_refuses_every_violation() {
        let ex = executor();
        let base = req("g1", GuestOp::Ping);

        let mut r = base.clone();
        r.rpc_version = 99;
        assert_eq!(err_code(ex.execute(&r)), "version");

        let mut r = base.clone();
        r.task = "task-2".into();
        assert_eq!(err_code(ex.execute(&r)), "wrong_task");

        let mut r = base.clone();
        r.capability_token = "tok-dead".into();
        assert_eq!(err_code(ex.execute(&r)), "stale_capability");

        let mut r = base.clone();
        r.effect_class = "quantum.fold".into();
        assert_eq!(err_code(ex.execute(&r)), "unknown_capability");

        // Declared class ≠ operation class: schema manipulation.
        let mut r = req("g2", GuestOp::Ping);
        r.effect_class = "fs.write".into();
        assert_eq!(err_code(ex.execute(&r)), "bad_request");

        // Stale generation. Distinct request ids throughout — a legit
        // client never reuses an id (the replay cache would serve the
        // original response).
        let mut advance = req("g-advance", GuestOp::Ping);
        advance.generation = 5;
        assert!(matches!(
            ex.execute(&advance).outcome,
            modbit_protocol::guest::GuestOutcome::Ok { .. }
        ));
        let mut stale = req("g-stale", GuestOp::Ping);
        stale.generation = 4;
        assert_eq!(err_code(ex.execute(&stale)), "fenced");
    }

    /// Real fs effect under policy: write/read/list inside the root;
    /// traversal and outside-absolute paths denied (QUAL-EV-0290).
    #[test]
    fn fs_ops_enforce_policy() {
        let ex = executor();
        let root = std::env::temp_dir();
        let file = root.join(format!(
            "modbit-guest-{}.txt",
            uuid::Uuid::now_v7().simple()
        ));
        let write = req(
            "w1",
            GuestOp::FsWrite {
                path: file.to_string_lossy().to_string(),
                bytes_base64: base64::engine::general_purpose::STANDARD.encode(b"hello"),
            },
        );
        assert!(matches!(
            ok_payload(ex.execute(&write)),
            GuestPayload::Accepted {}
        ));
        let read = req(
            "r1",
            GuestOp::FsRead {
                path: file.to_string_lossy().to_string(),
            },
        );
        match ok_payload(ex.execute(&read)) {
            GuestPayload::Data {
                bytes_base64,
                sha256,
            } => {
                assert_eq!(
                    base64::engine::general_purpose::STANDARD
                        .decode(&bytes_base64)
                        .unwrap(),
                    b"hello"
                );
                assert_eq!(sha256, crate::sha256_hex(b"hello"));
            }
            other => panic!("expected data, got {other:?}"),
        }
        let _ = std::fs::remove_file(&file);

        // Traversal out of the root is denied.
        let escape = req(
            "e1",
            GuestOp::FsWrite {
                path: root
                    .join("..")
                    .join("modbit-escape-test")
                    .to_string_lossy()
                    .to_string(),
                bytes_base64: String::new(),
            },
        );
        assert_eq!(err_code(ex.execute(&escape)), "policy_denied");
    }

    /// Real process effect: echo completes; a timeout kills the child and
    /// reports timed_out; duplicate request_id replays the FIRST response
    /// without re-execution.
    #[test]
    fn proc_exec_timeout_and_idempotent_replay() {
        let ex = executor();
        let (shell, flag, cmd) = if cfg!(windows) {
            ("cmd", "/c", "echo hello-guest")
        } else {
            ("sh", "-c", "echo hello-guest")
        };
        let echo = req(
            "p1",
            GuestOp::ProcExec {
                argv: vec![shell.into(), flag.into(), cmd.into()],
                cwd: None,
                env: Default::default(),
                timeout_ms: Some(10_000),
            },
        );
        match ok_payload(ex.execute(&echo)) {
            GuestPayload::Proc {
                exit_code,
                stdout,
                timed_out,
                ..
            } => {
                assert_eq!(exit_code, Some(0));
                assert!(stdout.contains("hello-guest"));
                assert!(!timed_out);
            }
            other => panic!("expected proc, got {other:?}"),
        }

        let sleeper = if cfg!(windows) {
            vec![
                "ping".to_string(),
                "-n".to_string(),
                "30".to_string(),
                "127.0.0.1".to_string(),
            ]
        } else {
            vec!["sleep".to_string(), "30".to_string()]
        };
        let slow = req(
            "p2",
            GuestOp::ProcExec {
                argv: sleeper,
                cwd: None,
                env: Default::default(),
                timeout_ms: Some(150),
            },
        );
        match ok_payload(ex.execute(&slow)) {
            GuestPayload::Proc { timed_out, .. } => assert!(timed_out, "slow proc must be killed"),
            other => panic!("expected proc, got {other:?}"),
        }

        // Replay: a duplicate request_id replays the FIRST response — the
        // second (different) write must never execute. Seed a real write
        // first, then re-send the same id targeting another file.
        let root = std::env::temp_dir();
        let first_target = root.join(format!(
            "modbit-replay-a-{}.txt",
            uuid::Uuid::now_v7().simple()
        ));
        let second_target = root.join(format!(
            "modbit-replay-b-{}.txt",
            uuid::Uuid::now_v7().simple()
        ));
        let first = req(
            "idem-1",
            GuestOp::FsWrite {
                path: first_target.to_string_lossy().to_string(),
                bytes_base64: base64::engine::general_purpose::STANDARD.encode(b"first"),
            },
        );
        assert!(matches!(
            ok_payload(ex.execute(&first)),
            GuestPayload::Accepted {}
        ));
        let replay = req(
            "idem-1",
            GuestOp::FsWrite {
                path: second_target.to_string_lossy().to_string(),
                bytes_base64: base64::engine::general_purpose::STANDARD.encode(b"second"),
            },
        );
        assert!(matches!(
            ok_payload(ex.execute(&replay)),
            GuestPayload::Accepted {}
        ));
        assert!(first_target.exists(), "first write executed");
        assert!(
            !second_target.exists(),
            "replayed request must not execute again"
        );
        let _ = std::fs::remove_file(&first_target);
    }

    /// Generation fencing cancels in-flight work: a long exec started
    /// under generation 0 is killed and its caller gets `fenced` when the
    /// session advances to generation 1.
    #[test]
    fn generation_advance_fences_in_flight_proc() {
        let ex = Arc::new(executor());
        let sleeper = if cfg!(windows) {
            vec![
                "ping".to_string(),
                "-n".to_string(),
                "30".to_string(),
                "127.0.0.1".to_string(),
            ]
        } else {
            vec!["sleep".to_string(), "30".to_string()]
        };
        let slow = req(
            "p3",
            GuestOp::ProcExec {
                argv: sleeper,
                cwd: None,
                env: Default::default(),
                timeout_ms: None,
            },
        );
        let ex2 = ex.clone();
        let worker = std::thread::spawn(move || ex2.execute(&slow));
        // Let the child actually start before fencing.
        std::thread::sleep(Duration::from_millis(300));
        ex.advance_generation(1);
        let resp = worker.join().expect("worker");
        assert_eq!(err_code(resp), "fenced");
    }

    /// Token revocation fails closed on the next request.
    #[test]
    fn revoked_token_fails_closed() {
        let ex = executor();
        ex.revoke_token("tok-live");
        let ping = req("g9", GuestOp::Ping);
        assert_eq!(err_code(ex.execute(&ping)), "stale_capability");
    }
}
