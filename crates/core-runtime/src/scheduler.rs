//! The single scheduler (docs/14 § Main runtime loop; Future-tasks.md
//! Phase 1 item 3). NOTHING ELSE may start a run: the scheduler tails the
//! durable event store for `task_started` events — whichever surface
//! (SurfaceProtocol socket, HTTP daemon `/commands`) executed the StartTask
//! command, the run begins here and only here.
//!
//! On `task_started` the scheduler, on one worker:
//! 1. allocates a dedicated git worktree + branch for the task (docs/14 §
//!    worktree isolation; E2E-001 "task creates dedicated worktree");
//! 2. builds the context pack through the canonical WorkspaceFileService;
//! 3. registers the task-scoped tools bound to that worktree;
//! 4. runs `OneAgentRuntime` over the production provider transport
//!    (`HttpStreamTransport`, ADR-0002);
//! 5. writes RunStarted / TurnPrepared / RunStep(ModelInvoke|ToolCall) /
//!    RunCompleted / RunFailed events into the store via a `RunObserver`;
//! 6. transitions the task from REAL outcomes only: completion →
//!    TaskReadyForReview, exhaustion → TaskFailed, provider outage →
//!    TaskWaiting(Provider) (docs/13; the model never self-certifies,
//!    REQ-EV-0119).
//!
//! Canonical owner subsystem: core-runtime (docs/81). Layout: docs/12.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use modbit_domain::events::{Actor, ActorType, AggregateType, DomainEvent, EventEnvelope, StepType};
use modbit_domain::ids::{RunId, RunStepId, SessionId, TaskId, TurnId};
use modbit_domain::{Command, CommandPayload};
use modbit_event_store::{CommandProcessor, EventStore, Outcome};
use modbit_git::GitRepo;
use modbit_policy::{CapabilityGrant, EffectClass, PolicyKernel};
use modbit_providers::gateway::{
    anthropic_request_body, openai_request_body, parse_anthropic_sse_payload,
    parse_openai_sse_payload, ModelRequest, Provider, StreamEvent,
};
use modbit_providers::transport::{
    HttpStreamTransport, ModelTransport as ProvidersTransport, OutgoingRequest, SecretBroker,
    TransportEvent,
};
use modbit_terminal::client::ExecdClient;
use modbit_tools::schema::{ParamSpec, ParamType, ToolSchema};
use modbit_tools::ToolRegistry;
use modbit_workspace::WorkspaceFileService;

use crate::one_agent::{AgentTask, OneAgentRuntime, RunControl, RunObserver};

/// Poll cadence for the store tail.
const POLL_INTERVAL: Duration = Duration::from_millis(150);

/// Configuration for scheduler runs; env-driven in production
/// (`SchedulerConfig::from_env`), injected in tests.
pub struct SchedulerConfig {
    pub provider: Provider,
    pub model: String,
    /// Pins the provider base URL (tests point this at a local fixture).
    pub base_url: Option<String>,
    pub broker: Arc<dyn SecretBroker>,
    pub request_timeout: Duration,
    pub max_turns: u32,
    /// modbit-execd broker address (docs/21). shell.run routes through it;
    /// unset means shell execution is unavailable and fails closed.
    pub execd_addr: Option<String>,
    /// Task-worktree layout source (repo + worktree roots). Defaults to
    /// the env-backed source when unset.
    pub worktrees: Option<Arc<dyn WorktreeSource>>,
    /// Per-model request settings (Phase 2.2): resolved from the model
    /// profile, env overrides applied (MODBIT_MAX_OUTPUT_TOKENS,
    /// MODBIT_TEMPERATURE, MODBIT_REASONING_EFFORT).
    pub model_settings: modbit_providers::profiles::ModelSettings,
    /// Input-token budget before the loop compacts the conversation
    /// (Phase 2.2; MODBIT_MAX_INPUT_TOKENS).
    pub max_input_tokens: u64,
}

impl SchedulerConfig {
    pub fn from_env() -> Self {
        let provider = match std::env::var("MODBIT_PROVIDER").as_deref() {
            Ok("anthropic") => Provider::Anthropic,
            _ => Provider::OpenAi,
        };
        let model = std::env::var("MODBIT_MODEL")
            .or_else(|_| std::env::var("MODBIT_LIVE_MODEL"))
            .unwrap_or_else(|_| "gpt-4o-mini".into());
        // Per-model profile, env overrides win (Phase 2.2).
        let mut model_settings = modbit_providers::profiles::resolve_model_settings(&model);
        if let Ok(v) = std::env::var("MODBIT_MAX_OUTPUT_TOKENS") {
            if let Ok(tokens) = v.parse::<u32>() {
                model_settings.max_output_tokens = tokens;
            }
        }
        if let Ok(v) = std::env::var("MODBIT_TEMPERATURE") {
            if let Ok(t) = v.parse::<f32>() {
                model_settings.temperature = t;
            }
        }
        if let Ok(v) = std::env::var("MODBIT_REASONING_EFFORT") {
            if let Some(effort) = modbit_providers::profiles::parse_reasoning_effort(&v) {
                model_settings.reasoning_effort = Some(effort);
            }
        }
        SchedulerConfig {
            provider,
            // MODBIT_LIVE_MODEL is the documented live-proof override (the
            // qualification script exports it); MODBIT_MODEL takes precedence.
            model,
            base_url: std::env::var("MODBIT_BASE_URL").ok().filter(|s| !s.is_empty()),
            broker: Arc::new(modbit_providers::transport::EnvSecretBroker),
            worktrees: EnvWorktreeSource::from_env()
                .map(|s| Arc::new(s) as Arc<dyn WorktreeSource>),
            // Total-request budget: reasoning-tier models legitimately
            // stream one response for several minutes; 180s killed healthy
            // streams (observed live: first invoke never completed).
            request_timeout: Duration::from_secs(600),
            max_turns: std::env::var("MODBIT_MAX_TURNS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(8),
            execd_addr: std::env::var("MODBIT_EXECD_ADDR").ok().filter(|s| !s.is_empty()),
            model_settings,
            max_input_tokens: std::env::var("MODBIT_MAX_INPUT_TOKENS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(crate::one_agent::DEFAULT_MAX_INPUT_TOKENS),
        }
    }
}

/// Handle over the running scheduler poller. Dropping it does not stop the
/// thread (the scheduler owns the process lifecycle in `modbit-core`).
pub struct Scheduler {
    store: Arc<EventStore>,
    config: SchedulerConfig,
    /// Tasks with an in-flight run; guards the poller against a concurrent
    /// direct `run_task` claim racing the worktree allocation.
    in_flight: std::sync::Mutex<std::collections::HashSet<String>>,
    /// Live stop/pause/steer signals for in-flight runs (Phase 2.3),
    /// shared with the surface handlers through `controls()`.
    controls: Arc<RunControls>,
}

/// Per-run control signal (Phase 2.3). Cheap atomics; steer notes drain
/// from the shared per-task outbox (race-free: a note queued before the
/// run registered still rides the run's next turn).
pub struct RunSignal {
    task_id: String,
    cancelled: Arc<std::sync::atomic::AtomicBool>,
    paused: std::sync::atomic::AtomicBool,
    notes: Arc<NoteOutbox>,
}

impl RunSignal {
    fn new(task_id: String, notes: Arc<NoteOutbox>) -> Self {
        RunSignal {
            task_id,
            cancelled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            paused: std::sync::atomic::AtomicBool::new(false),
            notes,
        }
    }

    /// The cancellation flag shared with the transport and execd tools.
    pub fn cancel_token(&self) -> Arc<std::sync::atomic::AtomicBool> {
        self.cancelled.clone()
    }
}

impl crate::one_agent::RunControl for RunSignal {
    fn is_cancelled(&self) -> bool {
        self.cancelled.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn is_paused(&self) -> bool {
        self.paused.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn take_steer_notes(&self) -> Vec<String> {
        self.notes
            .0
            .lock()
            .expect("steer outbox")
            .get_mut(&self.task_id)
            .map(|queue| queue.drain(..).collect())
            .unwrap_or_default()
    }
}

/// Per-task steer-note outbox shared by every RunSignal of a task.
#[derive(Default)]
struct NoteOutbox(
    std::sync::Mutex<std::collections::HashMap<String, std::collections::VecDeque<String>>>,
);

/// Shared registry of live run signals: the surface (Stop/Pause/Steer
/// RPCs) signals in-flight runs; the scheduler registers and finishes
/// them around each run.
#[derive(Default)]
pub struct RunControls {
    runs: std::sync::Mutex<std::collections::HashMap<String, Arc<RunSignal>>>,
    notes: Arc<NoteOutbox>,
}

impl RunControls {
    pub fn new() -> Self {
        Self::default()
    }

    fn register(&self, task_id: &str) -> Arc<RunSignal> {
        let signal = Arc::new(RunSignal::new(task_id.to_string(), self.notes.clone()));
        self.runs
            .lock()
            .expect("run controls")
            .insert(task_id.to_string(), signal.clone());
        signal
    }

    fn finish(&self, task_id: &str) {
        self.runs.lock().expect("run controls").remove(task_id);
        // Notes stay in the outbox: a steer that raced ahead of the next
        // run still rides that run's first turn boundary.
    }

    /// StopTask: abort the in-flight run (stream + broker tool) at or
    /// before the next turn boundary. Returns false when no run is live
    /// (the durable CancelTask command still applies).
    pub fn cancel(&self, task_id: &str) -> bool {
        match self.runs.lock().expect("run controls").get(task_id) {
            Some(signal) => {
                signal
                    .cancelled
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                true
            }
            None => false,
        }
    }

    /// PauseTask: park the in-flight run at the next turn boundary.
    pub fn pause(&self, task_id: &str) -> bool {
        match self.runs.lock().expect("run controls").get(task_id) {
            Some(signal) => {
                signal.paused.store(true, std::sync::atomic::Ordering::SeqCst);
                true
            }
            None => false,
        }
    }

    /// SteerTask: queue a note the run injects as a user message on the
    /// next turn. The outbox is race-free — the note rides the next turn
    /// boundary even if it arrives before the run registers.
    pub fn steer(&self, task_id: &str, note: String) {
        self.notes
            .0
            .lock()
            .expect("steer outbox")
            .entry(task_id.to_string())
            .or_default()
            .push_back(note);
    }
}

impl Scheduler {
    /// Live run-control surface (Phase 2.3): the wire handlers signal
    /// in-flight runs through this shared handle.
    pub fn controls(&self) -> Arc<RunControls> {
        self.controls.clone()
    }

    /// Starts the poller thread that tails the store for `task_started`.
    pub fn spawn(store: Arc<EventStore>, config: SchedulerConfig) -> Arc<Self> {
        let scheduler = Arc::new(Self {
            store,
            config,
            in_flight: std::sync::Mutex::new(std::collections::HashSet::new()),
            controls: Arc::new(RunControls::new()),
        });
        let weak = Arc::downgrade(&scheduler);
        std::thread::Builder::new()
            .name("modbit-scheduler".into())
            .spawn(move || {
                let mut offset: u64 = 0;
                loop {
                    let Some(s) = weak.upgrade() else { return };
                    match s.store.events_since_global(offset, 100) {
                        Ok((events, new_offset)) => {
                            for e in &events {
                                if e.event_type == "task_started" {
                                    // Sequential on purpose: the M2 loop is
                                    // single-agent; concurrent children are
                                    // M6 admission work.
                                    if let Err(err) = s.run_task(&e.aggregate_id) {
                                        eprintln!(
                                            "modbit scheduler: task {} run failed: {err}",
                                            e.aggregate_id
                                        );
                                    }
                                }
                            }
                            offset = new_offset;
                        }
                        Err(err) => {
                            eprintln!("modbit scheduler: store tail error: {err}");
                        }
                    }
                    std::thread::sleep(POLL_INTERVAL);
                }
            })
            .expect("spawn scheduler thread");
        scheduler
    }

    /// One full task run (the docs/14 loop's single-agent slice). The
    /// poller calls this for each `task_started`; tests drive it directly.
    /// This is the ONLY entry that starts a run.
    pub fn run_task(&self, task_id_str: &str) -> Result<(), String> {
        let Ok(task_id) = TaskId::parse(task_id_str) else {
            return Err(format!("malformed task id {task_id_str:?}"));
        };
        let (session_id, title, prompt) = read_task_brief(&self.store, &task_id)?;

        // Idempotency: a task that already has a run is never re-run
        // (resume is the M4 recovery spine's job, not a fresh run).
        if task_has_run(&self.store, &task_id)? {
            return Ok(());
        }
        // Claim: exactly one in-flight run per task (poller vs direct call).
        {
            let mut in_flight = self.in_flight.lock().expect("scheduler mutex");
            if !in_flight.insert(task_id.to_string()) {
                return Ok(()); // a run is already executing for this task
            }
        }
        let _release = ClaimGuard {
            scheduler_in_flight: &self.in_flight,
            task: task_id.to_string(),
        };

        // 1. Worktree + revision allocation (E2E-001), through the shared
        // layout source (the GetDiff surface reads the same truth).
        // No configured repository is a typed failure (task parks), never
        // a scheduler panic — the poller must survive misconfiguration.
        let source: Arc<dyn WorktreeSource> = self
            .config
            .worktrees
            .clone()
            .or_else(|| EnvWorktreeSource::from_env().map(|s| Arc::new(s) as Arc<dyn WorktreeSource>))
            .ok_or_else(|| "no repository configured for runs (set MODBIT_REPO_ROOT)".to_string())?;
        let layout = source
            .layout(&task_id.to_string())
            .ok_or_else(|| "no repository configured for runs (set MODBIT_REPO_ROOT)".to_string())?;
        let worktree_path = layout.worktree.clone();
        let base_revision = layout.base_revision.clone();
        let repo = GitRepo::open(
            &source.repo_root().ok_or("worktree source has no repository root")?,
        )
        .map_err(|e| format!("open repo: {e}"))?;
        repo.worktree_add(&worktree_path, &layout.branch)
            .map_err(|e| format!("allocate worktree: {e}"))?;

        // 2. Context pack through the canonical file service on the worktree.
        let ws = Arc::new(
            WorkspaceFileService::open(&worktree_path).map_err(|e| format!("open workspace: {e}"))?,
        );
        let context_pack = build_context_pack(&ws, &title, &prompt);

        // 3. Task-scoped tools bound to the worktree. shell.run routes
        // through modbit-execd (durable broker); everything stays inside the
        // worktree boundary.
        let execd = self
            .config
            .execd_addr
            .as_deref()
            .and_then(|addr| ExecdClient::connect(addr).ok());
        // Phase 2.3: this run's cancellation signal — StopTask flips
        // `cancelled` (transport aborts the stream, execd kills the broker
        // run), PauseTask flips `paused` (park at the next turn boundary),
        // SteerTask queues notes for the next turn.
        let signal = self.controls.register(&task_id.to_string());
        let registry = build_worktree_registry(&ws, &worktree_path, execd.as_ref(), signal.cancel_token());
        let kernel = PolicyKernel::new(vec![]);
        for grant in worktree_grants() {
            kernel.grant(grant);
        }
        let grants = worktree_grants();

        // 4-5. Run the one-agent runtime over the production transport,
        // writing every Run/Turn/RunStep transition into the store.
        let observer = EventStoreObserver {
            store: self.store.clone(),
            session_id,
            task_id,
            sequences: std::sync::Mutex::new(std::collections::HashMap::new()),
            current_run: std::sync::Mutex::new(None),
        };
        let transport = LiveGatewayTransport::new(&self.config, signal.cancel_token());
        let runtime = OneAgentRuntime {
            transport: &transport,
            registry: &registry,
            kernel: &kernel,
            grants: &grants,
            max_turns: self.config.max_turns,
            observer: Some(&observer),
            control: Some(&*signal),
        };
        let task = AgentTask {
            task_id: task_id.to_string(),
            objective: format!("{title}\n\n{prompt}"),
            model: self.config.model.clone(),
            provider: format!("{:?}", self.config.provider).to_lowercase(),
            system_policy: system_policy(&worktree_path, &base_revision),
            workspace_rules: read_workspace_rules(&worktree_path),
            context_pack,
            model_settings: self.config.model_settings,
            max_input_tokens: self.config.max_input_tokens,
        };
        let processor = CommandProcessor::new(self.store.clone());
        let result = runtime.run(&task);
        // Phase 2.3: the run signal leaves the control registry when the
        // run ends — a later Stop/Steer for this task must not hit a dead
        // run (it lands on the durable state instead).
        self.controls.finish(&task_id.to_string());

        // 6. Transition the task from REAL outcomes (REQ-EV-0119: the host
        // decides; a model claim is never sufficient).
        match result {
            // Phase 2.3: Stop/Pause already transitioned the durable task
            // state from the surface; the run aborted at the boundary (or
            // mid-stream). Never overwrite Cancelled/Waiting with a run
            // outcome.
            Ok(run) if run.cancelled || run.paused => {
                eprintln!(
                    "modbit scheduler: task {task_id} run {} (stop_reason {:?})",
                    if run.cancelled { "cancelled" } else { "paused" },
                    run.stop_reason
                );
                Ok(())
            }
            Ok(run) => match run.final_state {
                modbit_domain::turn::TurnState::Completed => {
                    execute(
                        &processor,
                        task_id,
                        CommandPayload::TaskReadyForReview { task_id },
                    )
                }
                _ => execute(
                    &processor,
                    task_id,
                    CommandPayload::FailTask {
                        task_id,
                        failure_code: "run_exhausted".into(),
                        message: tail(&run.assembled_text, 500),
                    },
                ),
            },
            // A transport/provider failure is an outage, not a task defect:
            // the task parks in Waiting(Provider) for retry, never silently
            // retried here (docs/15 failover runs before effects only).
            Err(err) => {
                // Phase 2.3: a stream aborted by StopTask returns here with
                // the signal already flipped — the task is Cancelled on the
                // store side; parking it in Waiting would resurrect it.
                if signal.is_cancelled() {
                    eprintln!(
                        "modbit scheduler: task {task_id} stream aborted by stop ({err})"
                    );
                    return Ok(());
                }
                // Surface the transport failure: a parked task with no
                // diagnostics is undebuggable from the outside.
                eprintln!("modbit scheduler: task {task_id} run errored: {err}");
                execute(
                    &processor,
                    task_id,
                    CommandPayload::TaskWaiting {
                        task_id,
                        reason: modbit_domain::events::WaitingReason::Provider,
                    },
                )
            }
            .map_err(|e| format!("park task after transport error ({err}): {e}")),
        }
    }
}

fn execute(
    processor: &CommandProcessor,
    _task_id: TaskId,
    payload: CommandPayload,
) -> Result<(), String> {
    match processor.execute(Command {
        command_id: uuid::Uuid::now_v7().to_string(),
        actor: Actor {
            actor_type: ActorType::System,
            actor_id: "scheduler".into(),
        },
        payload,
    }) {
        Ok(Outcome::Applied { .. } | Outcome::Replayed { .. }) => Ok(()),
        Ok(Outcome::Rejected { reason }) => Err(format!("task transition rejected: {reason}")),
        Err(e) => Err(e.to_string()),
    }
}

/// Tail of a string for failure messages, bounded.
fn tail(s: &str, max: usize) -> String {
    s.chars().rev().take(max).collect::<Vec<_>>().into_iter().rev().collect()
}

/// The deterministic task-worktree layout shared by the scheduler and the
/// GetDiff surface: path, branch and base revision for a task id.
pub struct WorktreeLayout {
    pub worktree: PathBuf,
    pub branch: String,
    pub base_revision: String,
}

/// Source of task-worktree layouts, shared by the scheduler and the GetDiff
/// surface. Explicit configuration beats ambient env inside dispatch.
pub trait WorktreeSource: Send + Sync + 'static {
    /// The deterministic layout (path, branch, base revision) for a task.
    fn layout(&self, task_id: &str) -> Option<WorktreeLayout>;
    /// The backing repository root, when the source knows it.
    fn repo_root(&self) -> Option<std::path::PathBuf> {
        None
    }
}

/// Env-backed source for host wiring (MODBIT_REPO_ROOT / cwd git repo,
/// MODBIT_WORKTREE_ROOT override). The bin constructs it once at boot.
pub struct EnvWorktreeSource {
    repo_root: PathBuf,
    worktree_root: PathBuf,
    base_revision: String,
}

impl EnvWorktreeSource {
    pub fn from_env() -> Option<Self> {
        let repo_root = std::env::var("MODBIT_REPO_ROOT")
            .ok()
            .map(PathBuf::from)
            .or_else(|| {
                let cwd = std::env::current_dir().ok()?;
                (cwd.join(".git").exists()).then_some(cwd)
            })?;
        let repo = GitRepo::open(&repo_root).ok()?;
        let base_revision = repo.head().ok()?;
        let worktree_root = std::env::var("MODBIT_WORKTREE_ROOT")
            .ok()
            .map(PathBuf::from)
            .unwrap_or_else(|| default_worktree_root(&repo_root));
        Some(EnvWorktreeSource {
            repo_root,
            worktree_root,
            base_revision,
        })
    }

    pub fn repo_root(&self) -> &std::path::Path {
        &self.repo_root
    }

    pub fn worktree_root(&self) -> &std::path::Path {
        &self.worktree_root
    }
}

impl WorktreeSource for EnvWorktreeSource {
    fn layout(&self, task_id: &str) -> Option<WorktreeLayout> {
        let branch = format!("modbit/{}", &task_id[..12.min(task_id.len())]);
        Some(WorktreeLayout {
            worktree: self.worktree_root.join(task_id),
            branch,
            base_revision: self.base_revision.clone(),
        })
    }

    fn repo_root(&self) -> Option<std::path::PathBuf> {
        Some(self.repo_root.clone())
    }
}

fn default_worktree_root(repo_root: &std::path::Path) -> PathBuf {
    repo_root
        .parent()
        .map(|p| p.join(".modbit").join("worktrees"))
        .unwrap_or_else(|| repo_root.join("../.modbit/worktrees"))
}

/// Reads the task brief (session, title, prompt) from its created event.
fn read_task_brief(
    store: &EventStore,
    task_id: &TaskId,
) -> Result<(SessionId, String, String), String> {
    let events = store.load(&task_id.to_string()).map_err(|e| e.to_string())?;
    for e in &events {
        if let DomainEvent::TaskCreated {
            session_id,
            title,
            prompt,
        } = &e.payload
        {
            return Ok((*session_id, title.clone(), prompt.clone()));
        }
    }
    Err(format!("task {task_id} has no TaskCreated event"))
}

/// True when a run aggregate already references this task (idempotent skip).
fn task_has_run(store: &EventStore, task_id: &TaskId) -> Result<bool, String> {
    store
        .with_conn(|conn| {
            let mut stmt = conn
                .prepare("SELECT aggregate_id FROM events WHERE aggregate_type = 'run' AND sequence = 1")
                .map_err(|e| e.to_string())?;
            let ids = stmt
                .query_map([], |r| r.get::<_, String>(0))
                .map_err(|e| e.to_string())?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| e.to_string())?;
            Ok::<_, String>(ids)
        })
        .map(|ids| {
            ids.iter().any(|id| {
                store
                    .load(id)
                    .ok()
                    .and_then(|es| es.first().map(|e| e.payload.clone()))
                    .map(|p| {
                        matches!(&p, DomainEvent::RunStarted { task_id: t, .. } if t == task_id)
                    })
                    .unwrap_or(false)
            })
        })
}

/// Bounded context pack from the real worktree via the canonical service
/// (the M3 context engine replaces the internals, not this wiring).
fn build_context_pack(ws: &WorkspaceFileService, title: &str, prompt: &str) -> String {
    let entries = ws.list("").unwrap_or_default();
    let shown: Vec<String> = entries.iter().take(50).cloned().collect();
    format!(
        "# Task\n{title}\n\n# Objective\n{prompt}\n\n# Workspace files (top level, first 50)\n{}\n\n# Total top-level entries\n{}",
        shown.join("\n"),
        entries.len()
    )
}

/// Workspace rules files (Future-tasks Phase 2 item 4, docs/14 step 3):
/// read the repo's instruction files into the `workspace_rules` prompt
/// segment with per-file sha256 provenance. Sources, in order: root
/// AGENTS.md, root CLAUDE.md, `.modbit/rules.md`, then every
/// `.cursor/rules/*.mdc` (sorted), then AGENTS.md/CLAUDE.md found in
/// subdirectories (bounded walk, root-first so deeper files appear
/// later). Content is repo data: it rides as context, never as system
/// authority (docs/52 — external content is not instruction).
fn read_workspace_rules(worktree: &std::path::Path) -> String {
    let mut sections: Vec<String> = Vec::new();

    fn add_file(sections: &mut Vec<String>, path: &std::path::Path, display: &str) {
        const MAX: usize = 64 * 1024;
        let Ok(bytes) = std::fs::read(path) else { return };
        let digest = {
            use sha2::{Digest, Sha256};
            let mut hasher = Sha256::new();
            hasher.update(&bytes);
            format!("{:x}", hasher.finalize())
        };
        let truncated = bytes.len() > MAX;
        let mut text =
            String::from_utf8_lossy(&bytes[..bytes.len().min(MAX)]).to_string();
        if truncated {
            text.push_str("\n…[rules file truncated at 64 KiB]\n");
        }
        sections.push(format!(
            "## {display} (sha256:{digest}{extra})\n{text}",
            extra = if truncated { ", truncated" } else { "" }
        ));
    }

    // Root-level canonical sources.
    for name in ["AGENTS.md", "CLAUDE.md", ".modbit/rules.md"] {
        let path = worktree.join(name);
        if path.is_file() {
            add_file(&mut sections, &path, name);
        }
    }
    // Cursor-style rule packs.
    if let Ok(entries) = std::fs::read_dir(worktree.join(".cursor/rules")) {
        let mut mdc: Vec<_> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "mdc"))
            .collect();
        mdc.sort();
        for path in mdc {
            let display = path
                .strip_prefix(worktree)
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| path.display().to_string());
            add_file(&mut sections, &path, &display);
        }
    }
    // Directory-scoped AGENTS.md/CLAUDE.md down the tree (root-first
    // order; deeper entries appear later and override by proximity).
    fn walk(dir: &std::path::Path, worktree: &std::path::Path, depth: usize, sections: &mut Vec<String>) {
        const MAX_WALK_DEPTH: usize = 4;
        if depth > MAX_WALK_DEPTH {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        let mut children: Vec<_> = entries.flatten().map(|e| e.path()).collect();
        children.sort();
        for child in children {
            if child.is_dir() {
                let name = child.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if name == ".git" || name.starts_with('.') || name == "node_modules" || name == "target" {
                    continue;
                }
                for rule in ["AGENTS.md", "CLAUDE.md"] {
                    let path = child.join(rule);
                    if path.is_file() {
                        let display = path
                            .strip_prefix(worktree)
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|_| path.display().to_string());
                        add_file(sections, &path, &display);
                    }
                }
                walk(&child, worktree, depth + 1, sections);
            }
        }
    }
    walk(worktree, worktree, 1, &mut sections);

    if sections.is_empty() {
        String::new()
    } else {
        format!(
            "# Workspace rules (repo-provided, provenance-hashed)\n\n{}\n",
            sections.join("\n\n")
        )
    }
}

fn system_policy(worktree: &std::path::Path, base_revision: &str) -> String {
    format!(
        "You are the Modbit engineering agent working in an isolated git worktree.\n\
         - Work only inside the worktree: {}\n\
         - Base revision: {base_revision}\n\
         - Prefer the provided tools for every action; report results factually.\n\
         - Do not claim success without running the relevant checks.",
        worktree.display()
    )
}

fn param(spec: ParamType, required: bool, description: &str) -> ParamSpec {
    ParamSpec {
        param_type: spec,
        required,
        default: None,
        description: description.into(),
    }
}

/// Task-scoped tools bound to the worktree (docs/17 tool families; Phase 1
/// item 4): safe-path fs access, execd-routed shell, change engine edits
/// behind an edit gate, literal search, git status/diff, and verification
/// runners. Every effector is the canonical owner crate — no local
/// reimplementation.
#[allow(clippy::too_many_lines)]
pub fn build_worktree_registry(
    ws: &Arc<WorkspaceFileService>,
    worktree: &std::path::Path,
    execd: Option<&ExecdClient>,
    cancel: Arc<std::sync::atomic::AtomicBool>,
) -> ToolRegistry {
    let registry = ToolRegistry::new();

    // ---- fs.read / fs.list: canonical safe-path file service ----------
    let mut read_params = std::collections::BTreeMap::new();
    read_params.insert("path".into(), param(ParamType::Str, true, "File path inside the worktree"));
    registry
        .register_with_schema(
            "fs.read",
            "1.0.0",
            EffectClass::ReadOnly,
            "Read a UTF-8 file from the worktree",
            Some(ToolSchema { aliases: Default::default(), parameters: read_params }),
            {
                let ws = ws.clone();
                Arc::new(move |args| {
                    let path = args.get("path").and_then(|v| v.as_str()).ok_or("missing path")?;
                    // Files checked out by git are adopted on first touch so
                    // reads carry revisions (canonical change-engine guard).
                    let _ = ws.adopt(path);
                    let (bytes, rev) = ws.read(path).map_err(|e| e.to_string())?;
                    Ok(serde_json::json!({
                        "content": String::from_utf8_lossy(&bytes),
                        "file_revision": rev,
                    }))
                })
            },
        )
        .expect("register fs.read");

    let mut list_params = std::collections::BTreeMap::new();
    list_params.insert("dir".into(), param(ParamType::Str, false, "Directory inside the worktree (default: root)"));
    registry
        .register_with_schema(
            "fs.list",
            "1.0.0",
            EffectClass::ReadOnly,
            "List entries of a directory in the worktree",
            Some(ToolSchema { aliases: Default::default(), parameters: list_params }),
            {
                let ws = ws.clone();
                Arc::new(move |args| {
                    let dir = args.get("dir").and_then(|v| v.as_str()).unwrap_or("");
                    let entries = ws.list(dir).map_err(|e| e.to_string())?;
                    Ok(serde_json::json!({ "entries": entries }))
                })
            },
        )
        .expect("register fs.list");

    // ---- shell.run: routed through modbit-execd (docs/21) -------------
    // Output is durable and offset-addressable in the broker's run dir —
    // E2E-003 output survival. Fails closed when no broker is configured.
    let mut shell_params = std::collections::BTreeMap::new();
    shell_params.insert(
        "argv".into(),
        ParamSpec {
            param_type: ParamType::Str,
            required: true,
            default: None,
            description: "Whitespace-separated command argv (structured argv only, no shell)".into(),
        },
    );
    registry
        .register_with_schema(
            "shell.run",
            "1.0.0",
            EffectClass::External,
            "Run a command in the task worktree through the durable process broker (output captured)",
            Some(ToolSchema { aliases: Default::default(), parameters: shell_params }),
            {
                let execd = execd.cloned();
                let worktree = worktree.to_path_buf();
                let cancel = cancel.clone();
                Arc::new(move |args| {
                    let execd = execd.as_ref().ok_or(
                        "shell execution unavailable: no modbit-execd broker configured (set MODBIT_EXECD_ADDR)",
                    )?;
                    let argv = args
                        .get("argv")
                        .and_then(|v| v.as_str())
                        .ok_or("missing argv")?
                        .split_whitespace()
                        .map(String::from)
                        .collect::<Vec<_>>();
                    if argv.is_empty() {
                        return Err("empty argv".into());
                    }
                    let run_id = format!("task-{}", uuid::Uuid::now_v7().simple());
                    // Phase 2.3: the run's cancellation flag races the
                    // wait — StopTask kills the broker run (no orphan
                    // process) and surfaces a typed cancellation.
                    let (status, output) = execd
                        .run_capture_cancellable(
                            &run_id,
                            &argv,
                            Some(&worktree),
                            Duration::from_secs(600),
                            256 * 1024,
                            &cancel,
                        )
                        .map_err(|e| format!("execd: {e}"))?;
                    let exit_code = match status.state {
                        modbit_terminal::RunState::Exited(code) => code,
                        _ => -1,
                    };
                    Ok(serde_json::json!({
                        "exit_code": exit_code,
                        "state": format!("{:?}", status.state),
                        "output": tail(&String::from_utf8_lossy(&output), 8_000),
                        "broker_run_id": run_id,
                    }))
                })
            },
        )
        .expect("register shell.run");

    // ---- change.propose / change.apply: edit gate + change engine -----
    let mut propose_params = std::collections::BTreeMap::new();
    propose_params.insert("path".into(), param(ParamType::Str, true, "File to edit"));
    propose_params.insert("old_text".into(), param(ParamType::Str, true, "Exact existing text to replace (must occur exactly once)"));
    propose_params.insert("new_text".into(), param(ParamType::Str, true, "Replacement text"));
    registry
        .register_with_schema(
            "change.propose",
            "1.0.0",
            EffectClass::ReadOnly,
            "Preview an edit WITHOUT writing: verifies the old text occurs exactly once and returns the resulting content head",
            Some(ToolSchema { aliases: Default::default(), parameters: propose_params }),
            {
                let ws = ws.clone();
                Arc::new(move |args| {
                    let path = args.get("path").and_then(|v| v.as_str()).ok_or("missing path")?;
                    let old = args.get("old_text").and_then(|v| v.as_str()).ok_or("missing old_text")?;
                    let new = args.get("new_text").and_then(|v| v.as_str()).ok_or("missing new_text")?;
                    let _ = ws.adopt(path);
                    let (bytes, rev) = ws.read(path).map_err(|e| e.to_string())?;
                    let content = String::from_utf8_lossy(&bytes).to_string();
                    let count = content.matches(old).count();
                    if count != 1 {
                        return Ok(serde_json::json!({
                            "ok": false,
                            "occurrences": count,
                            "reason": "old_text must occur exactly once (edit gate; blind writes are refused)",
                        }));
                    }
                    let proposed = content.replacen(old, new, 1);
                    Ok(serde_json::json!({
                        "ok": true,
                        "occurrences": 1,
                        "file_revision": rev,
                        "preview_head": tail(&proposed, 600),
                    }))
                })
            },
        )
        .expect("register change.propose");

    let mut apply_params = std::collections::BTreeMap::new();
    apply_params.insert("path".into(), param(ParamType::Str, true, "File to edit"));
    apply_params.insert("old_text".into(), param(ParamType::Str, true, "Exact existing text to replace (must occur exactly once)"));
    apply_params.insert("new_text".into(), param(ParamType::Str, true, "Replacement text"));
    apply_params.insert("expected_revision".into(), param(ParamType::Int, false, "File revision from the read that produced old_text (optimistic concurrency guard)"));
    registry
        .register_with_schema(
            "change.apply",
            "1.0.0",
            EffectClass::Write,
            "Apply an edit through the change engine: edit gate (unique match) + revision-guarded atomic replace",
            Some(ToolSchema { aliases: Default::default(), parameters: apply_params }),
            {
                let ws = ws.clone();
                Arc::new(move |args| {
                    let path = args.get("path").and_then(|v| v.as_str()).ok_or("missing path")?;
                    let old = args.get("old_text").and_then(|v| v.as_str()).ok_or("missing old_text")?;
                    let new = args.get("new_text").and_then(|v| v.as_str()).ok_or("missing new_text")?;
                    let _ = ws.adopt(path);
                    let (bytes, rev) = ws.read(path).map_err(|e| e.to_string())?;
                    if let Some(expected) = args.get("expected_revision").and_then(|v| v.as_i64()) {
                        if expected >= 0 && expected as u64 != rev {
                            return Ok(serde_json::json!({
                                "ok": false,
                                "reason": format!("stale revision: expected {expected}, file is at {rev}; re-read and re-propose"),
                            }));
                        }
                    }
                    let content = String::from_utf8_lossy(&bytes).to_string();
                    let count = content.matches(old).count();
                    if count != 1 {
                        return Ok(serde_json::json!({
                            "ok": false,
                            "occurrences": count,
                            "reason": "old_text must occur exactly once (edit gate)",
                        }));
                    }
                    let updated = content.replacen(old, new, 1);
                    let new_rev = ws
                        .replace(path, updated.as_bytes(), rev)
                        .map_err(|e| e.to_string())?;
                    Ok(serde_json::json!({ "ok": true, "file_revision": new_rev }))
                })
            },
        )
        .expect("register change.apply");

    // ---- search.grep: literal search, bounded (index arrives with M3) --
    let mut grep_params = std::collections::BTreeMap::new();
    grep_params.insert("pattern".into(), param(ParamType::Str, true, "Literal text to find"));
    grep_params.insert("path".into(), param(ParamType::Str, false, "Limit search to this directory (default: worktree root)"));
    registry
        .register_with_schema(
            "search.grep",
            "1.0.0",
            EffectClass::ReadOnly,
            "Search file contents in the worktree for a literal string; returns path:line matches (bounded)",
            Some(ToolSchema { aliases: Default::default(), parameters: grep_params }),
            {
                let worktree = worktree.to_path_buf();
                Arc::new(move |args| {
                    let pattern = args.get("pattern").and_then(|v| v.as_str()).ok_or("missing pattern")?;
                    let base = args
                        .get("path")
                        .and_then(|v| v.as_str())
                        .map(|p| worktree.join(p))
                        .unwrap_or_else(|| worktree.clone());
                    let mut matches = Vec::new();
                    let mut visited = 0usize;
                    walk_files(&base, &mut |path| {
                        visited += 1;
                        if visited > 2_000 || matches.len() >= 200 {
                            return;
                        }
                        let Ok(meta) = std::fs::metadata(path) else { return };
                        if !meta.is_file() || meta.len() > 256 * 1024 {
                            return;
                        }
                        let Ok(bytes) = std::fs::read(path) else { return };
                        let text = String::from_utf8_lossy(&bytes);
                        for (idx, line) in text.lines().enumerate() {
                            if line.contains(pattern) {
                                let rel = path.strip_prefix(&worktree).unwrap_or(path);
                                matches.push(format!("{}:{}", rel.display(), idx + 1));
                                if matches.len() >= 200 {
                                    break;
                                }
                            }
                        }
                    });
                    Ok(serde_json::json!({ "matches": matches, "files_searched": visited }))
                })
            },
        )
        .expect("register search.grep");

    // ---- git.status / git.diff: canonical git crate --------------------
    let status_params = std::collections::BTreeMap::new();
    registry
        .register_with_schema(
            "git.status",
            "1.0.0",
            EffectClass::ReadOnly,
            "Working-tree status of the task worktree (porcelain codes)",
            Some(ToolSchema { aliases: Default::default(), parameters: status_params }),
            {
                let worktree = worktree.to_path_buf();
                Arc::new(move |_args| {
                    let repo = GitRepo::open(&worktree).map_err(|e| e.to_string())?;
                    let entries = repo.status_porcelain().map_err(|e| e.to_string())?;
                    Ok(serde_json::json!({
                        "entries": entries.iter()
                            .map(|(xy, path)| serde_json::json!({ "code": xy, "path": path }))
                            .collect::<Vec<_>>(),
                    }))
                })
            },
        )
        .expect("register git.status");

    let diff_params = std::collections::BTreeMap::new();
    registry
        .register_with_schema(
            "git.diff",
            "1.0.0",
            EffectClass::ReadOnly,
            "Numstat diff of the worktree's uncommitted changes against HEAD",
            Some(ToolSchema { aliases: Default::default(), parameters: diff_params }),
            {
                let worktree = worktree.to_path_buf();
                Arc::new(move |_args| {
                    let repo = GitRepo::open(&worktree).map_err(|e| e.to_string())?;
                    let diffs = repo.diff_workdir_numstat("HEAD").map_err(|e| e.to_string())?;
                    Ok(serde_json::json!({
                        "files": diffs.iter()
                            .map(|d| serde_json::json!({ "path": d.path, "additions": d.additions, "deletions": d.deletions }))
                            .collect::<Vec<_>>(),
                    }))
                })
            },
        )
        .expect("register git.diff");

    // ---- test.run: verification engine with runner adapters -----------
    let mut test_params = std::collections::BTreeMap::new();
    test_params.insert("runner".into(), param(ParamType::Str, true, "Test runner: cargo | vitest | pytest"));
    test_params.insert("args".into(), param(ParamType::Str, false, "Extra args appended to the runner invocation"));
    registry
        .register_with_schema(
            "test.run",
            "1.0.0",
            EffectClass::External,
            "Run the project's test suite in the worktree through a runner adapter (cargo/vitest/pytest)",
            Some(ToolSchema { aliases: Default::default(), parameters: test_params }),
            {
                let worktree = worktree.to_path_buf();
                Arc::new(move |args| {
                    let runner = args.get("runner").and_then(|v| v.as_str()).ok_or("missing runner")?;
                    let extra = args.get("args").and_then(|v| v.as_str()).unwrap_or("");
                    let mut argv: Vec<String> = match runner {
                        "cargo" => vec!["cargo", "test"],
                        "vitest" => vec!["pnpm", "exec", "vitest", "run"],
                        "pytest" => vec!["python3", "-m", "pytest"],
                        other => return Err(format!("unknown runner {other:?} (cargo|vitest|pytest)")),
                    }
                    .into_iter()
                    .map(String::from)
                    .collect();
                    argv.extend(extra.split_whitespace().map(String::from));
                    let gate = modbit_verification::Gate::new(runner, &[], 900)
                        .with_cwd(worktree.clone());
                    let gate = modbit_verification::Gate { argv, ..gate };
                    let report = modbit_verification::run_plan(&[gate]).map_err(|e| e.to_string())?;
                    Ok(serde_json::to_value(&report).unwrap_or_default())
                })
            },
        )
        .expect("register test.run");

    registry
}

/// Bounded depth-first walk over regular files (search.grep substrate).
fn walk_files(dir: &std::path::Path, f: &mut dyn FnMut(&std::path::Path)) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = std::fs::metadata(&path) else { continue };
        if meta.is_dir() {
            let name = entry.file_name();
            // Skip VCS and dependency dirs: never search those.
            if name == ".git" || name == "node_modules" || name == "target" {
                continue;
            }
            walk_files(&path, f);
        } else if meta.is_file() {
            f(&path);
        }
    }
}

/// Host-issued capability grants for the worktree toolset. Grants are
/// least-privilege per effect class (docs/16); approvals can revoke any of
/// these without touching the others.
fn worktree_grants() -> Vec<CapabilityGrant> {
    vec![
        CapabilityGrant { grant_id: "g-fs-read".into(), tool: "fs.read".into(), effect_class: EffectClass::ReadOnly },
        CapabilityGrant { grant_id: "g-fs-list".into(), tool: "fs.list".into(), effect_class: EffectClass::ReadOnly },
        CapabilityGrant { grant_id: "g-grep".into(), tool: "search.grep".into(), effect_class: EffectClass::ReadOnly },
        CapabilityGrant { grant_id: "g-git-status".into(), tool: "git.status".into(), effect_class: EffectClass::ReadOnly },
        CapabilityGrant { grant_id: "g-git-diff".into(), tool: "git.diff".into(), effect_class: EffectClass::ReadOnly },
        CapabilityGrant { grant_id: "g-change-propose".into(), tool: "change.propose".into(), effect_class: EffectClass::ReadOnly },
        CapabilityGrant { grant_id: "g-change-apply".into(), tool: "change.apply".into(), effect_class: EffectClass::Write },
        CapabilityGrant { grant_id: "g-shell-run".into(), tool: "shell.run".into(), effect_class: EffectClass::External },
        CapabilityGrant { grant_id: "g-test-run".into(), tool: "test.run".into(), effect_class: EffectClass::External },
    ]
}

/// Releases the in-flight claim when the run ends (success or failure).
struct ClaimGuard<'a> {
    scheduler_in_flight: &'a std::sync::Mutex<std::collections::HashSet<String>>,
    task: String,
}

impl Drop for ClaimGuard<'_> {
    fn drop(&mut self) {
        self.scheduler_in_flight
            .lock()
            .expect("scheduler mutex")
            .remove(&self.task);
    }
}

/// Bridges the async providers transport to the runtime's sync trait:
/// builds the provider body, streams over `HttpStreamTransport` on a
/// dedicated tokio runtime, parses per provider and returns the normalized
/// event vector (fragment merging happens inside the runtime loop).
/// Phase 2.3: the run's cancellation flag races the stream — StopTask
/// aborts an in-flight model stream instead of waiting it out.
struct LiveGatewayTransport<'a> {
    config: &'a SchedulerConfig,
    runtime: tokio::runtime::Runtime,
    transport: HttpStreamTransport,
    cancel: Arc<std::sync::atomic::AtomicBool>,
}

impl<'a> LiveGatewayTransport<'a> {
    fn new(
        config: &'a SchedulerConfig,
        cancel: Arc<std::sync::atomic::AtomicBool>,
    ) -> Self {
        let transport = HttpStreamTransport::new(config.broker.clone())
            .expect("build provider transport");
        LiveGatewayTransport {
            config,
            runtime: tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("scheduler tokio runtime"),
            transport,
            cancel,
        }
    }

    fn endpoint(&self) -> String {
        // Pin via config, else the provider's own env-resolved endpoint
        // (OPENAI_BASE_URL / ANTHROPIC_BASE_URL). Falling back to a
        // hard-coded vendor URL sent a z.ai key to api.openai.com → 401.
        let base = self
            .config
            .base_url
            .clone()
            .unwrap_or_else(|| match self.config.provider {
                Provider::OpenAi => std::env::var("OPENAI_BASE_URL")
                    .unwrap_or_else(|_| "https://api.openai.com/v1".into()),
                Provider::Anthropic => std::env::var("ANTHROPIC_BASE_URL")
                    .unwrap_or_else(|_| "https://api.anthropic.com".into()),
            });
        match self.config.provider {
            Provider::OpenAi => format!("{base}/chat/completions"),
            Provider::Anthropic => format!("{base}/v1/messages"),
        }
    }
}

impl<'a> crate::one_agent::ModelTransport for LiveGatewayTransport<'a> {
    fn stream(&self, request: &ModelRequest) -> Result<Vec<StreamEvent>, String> {
        let body = match self.config.provider {
            Provider::OpenAi => openai_request_body(request),
            Provider::Anthropic => anthropic_request_body(request),
        };
        let outgoing = OutgoingRequest {
            provider: self.config.provider,
            url: self.endpoint(),
            body: serde_json::to_vec(&body).map_err(|e| e.to_string())?,
            timeout: self.config.request_timeout,
        };
        self.runtime.block_on(async move {
            let mut stream = self
                .transport
                .stream(outgoing)
                .map_err(|e| e.to_string())?;
            let mut events = Vec::new();
            loop {
                // Phase 2.3: race the stream against the cancellation flag
                // (100ms cadence) so StopTask aborts a stalled provider
                // stream instead of blocking the run for the full timeout.
                let event = tokio::select! {
                    event = stream.recv() => event,
                    _ = tokio::time::sleep(Duration::from_millis(100)) => {
                        if self.cancel.load(std::sync::atomic::Ordering::SeqCst) {
                            return Err("run cancelled during model stream".into());
                        }
                        continue;
                    }
                };
                match event {
                    Some(Ok(TransportEvent::SseData(payload))) => {
                        let parsed = match self.config.provider {
                            Provider::OpenAi => parse_openai_sse_payload(&payload),
                            Provider::Anthropic => parse_anthropic_sse_payload(&payload),
                        };
                        if let Some(event) = parsed {
                            events.push(event);
                        }
                    }
                    Some(Ok(TransportEvent::Usage(_))) => {
                        // Usage arrives on StreamEvent::Usage via the
                        // parsers' usage frames; the transport snapshot is
                        // redundant here.
                    }
                    Some(Ok(TransportEvent::Eof)) => return Ok(events),
                    Some(Err(e)) => return Err(e.to_string()),
                    None => return Ok(events),
                }
            }
        })
    }
}

/// Writes every runtime transition as durable Run/Turn/RunStep events
/// (docs/13). Sequences are per aggregate, starting at 1.
struct EventStoreObserver {
    store: Arc<EventStore>,
    session_id: SessionId,
    task_id: TaskId,
    sequences: std::sync::Mutex<std::collections::HashMap<String, u64>>,
    current_run: std::sync::Mutex<Option<RunId>>,
}

impl EventStoreObserver {
    /// Appends one run-plane event. Id fields carry ONLY the aggregate's
    /// own id (the store reconstructs them from the aggregate on load, so
    /// cross-references travel in the payload, not the envelope).
    fn append(&self, aggregate: AggregateType, aggregate_id: &str, payload: DomainEvent) {
        let (task_id, run_id, turn_id, step_id) = match aggregate {
            AggregateType::Run => (
                None,
                Some(RunId::parse(aggregate_id).unwrap_or_else(|_| RunId::generate())),
                None,
                None,
            ),
            AggregateType::Turn => (
                None,
                None,
                Some(TurnId::parse(aggregate_id).unwrap_or_else(|_| TurnId::generate())),
                None,
            ),
            _ => (
                None,
                None,
                None,
                Some(RunStepId::parse(aggregate_id).unwrap_or_else(|_| RunStepId::generate())),
            ),
        };
        let mut envelope = EventEnvelope {
            event_id: uuid::Uuid::now_v7().to_string(),
            session_id: self.session_id,
            task_id,
            run_id,
            turn_id,
            step_id,
            aggregate_type: aggregate,
            aggregate_id: aggregate_id.to_string(),
            sequence: 0,
            event_type: EventEnvelope::event_type_of(&payload).to_string(),
            schema_version: modbit_domain::SCHEMA_VERSION,
            occurred_at: now_rfc3339(),
            actor: Actor {
                actor_type: ActorType::System,
                actor_id: "scheduler".into(),
            },
            causation_id: None,
            correlation_id: None,
            payload,
            payload_object_hash: None,
            integrity_hash: String::new(),
        };
        let mut sequences = self.sequences.lock().expect("observer mutex");
        let next = sequences.get(aggregate_id).copied().unwrap_or(1);
        envelope.sequence = next;
        envelope.seal();
        sequences.insert(aggregate_id.to_string(), next + 1);
        drop(sequences);
        if let Err(e) = self.store.append(&mut [envelope]) {
            eprintln!("modbit scheduler: append run event failed: {e}");
        }
    }
}

impl EventStoreObserver {
    fn run_of(&self) -> Option<RunId> {
        *self.current_run.lock().expect("observer mutex")
    }
}

impl RunObserver for EventStoreObserver {
    fn run_started(&self, run_id: &str, attempt: u32) {
        let Ok(parsed) = RunId::parse(run_id) else { return };
        *self.current_run.lock().expect("observer mutex") = Some(parsed);
        self.append(
            AggregateType::Run,
            run_id,
            DomainEvent::RunStarted {
                task_id: self.task_id,
                attempt,
            },
        );
    }



    fn turn_prepared(&self, turn_id: &str, ordinal: u32) {
        let run_id = self.run_of();
        self.append(
            AggregateType::Turn,
            turn_id,
            DomainEvent::TurnPrepared {
                run_id: run_id.unwrap_or_else(RunId::generate),
                ordinal,
            },
        );
    }

    fn model_invoke_started(&self, turn_id: &str, step_id: &str) {
        let Ok(parsed_turn) = TurnId::parse(turn_id) else { return };
        self.append(
            AggregateType::RunStep,
            step_id,
            DomainEvent::RunStepPrepared {
                turn_id: parsed_turn,
                step_type: StepType::ModelInvoke,
                ordinal: 0,
            },
        );
    }

    fn model_invoke_finished(&self, _turn_id: &str, step_id: &str, _usage: Option<modbit_providers::TokenUsage>) {
        self.append(AggregateType::RunStep, step_id, DomainEvent::RunStepCompleted);
    }

    fn tool_step_started(&self, turn_id: &str, step_id: &str, _call_id: &str, _name: &str) {
        let Ok(parsed_turn) = TurnId::parse(turn_id) else { return };
        self.append(
            AggregateType::RunStep,
            step_id,
            DomainEvent::RunStepPrepared {
                turn_id: parsed_turn,
                step_type: StepType::ToolCall,
                ordinal: 0,
            },
        );
    }

    fn tool_step_finished(&self, turn_id: &str, step_id: &str, _call_id: &str, _name: &str, ok: bool) {
        let _ = (turn_id, _call_id, _name);
        let payload = if ok {
            DomainEvent::RunStepCompleted
        } else {
            DomainEvent::RunStepFailed {
                failure_code: "tool_refused_or_failed".into(),
            }
        };
        self.append(AggregateType::RunStep, step_id, payload);
    }

    fn turn_completed(&self, turn_id: &str) {
        self.append(AggregateType::Turn, turn_id, DomainEvent::TurnCompleted);
    }

    fn compaction_applied(
        &self,
        turn_id: &str,
        epoch_id: &str,
        affected_messages: u32,
        reclaimed_tokens: u64,
        manifest_digest: &str,
    ) {
        let Ok(parsed_turn) = TurnId::parse(turn_id) else { return };
        let run_id = self.run_of().unwrap_or_else(RunId::generate);
        self.append(
            AggregateType::Run,
            &run_id.to_string(),
            DomainEvent::CompactionApplied {
                turn_id: parsed_turn,
                epoch_id: epoch_id.to_string(),
                affected_messages,
                reclaimed_tokens,
                manifest_digest: manifest_digest.to_string(),
            },
        );
    }

    fn turn_failed(&self, turn_id: &str, failure_code: &str) {
        self.append(
            AggregateType::Turn,
            turn_id,
            DomainEvent::TurnFailed {
                failure_code: failure_code.into(),
            },
        );
    }

    fn run_completed(&self, run_id: &str) {
        self.append(AggregateType::Run, run_id, DomainEvent::RunCompleted);
    }

    fn run_failed(&self, run_id: &str, failure_code: &str) {
        self.append(
            AggregateType::Run,
            run_id,
            DomainEvent::RunFailed {
                failure_code: failure_code.into(),
            },
        );
    }
}

/// RFC3339 timestamp matching the store's own format.
fn now_rfc3339() -> String {
    // The event store's formatter (crate-private); reproduce the same shape:
    // YYYY-MM-DDTHH:MM:SS.mmmZ from the system clock.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let millis = now.subsec_millis();
    let days = secs / 86_400;
    let (year, month, day) = civil_from_days(days as i64);
    let rem = secs % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Howard Hinnant's civil-from-days.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod rules_tests {
    use super::read_workspace_rules;

    fn tempdir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("modbit-rules-{tag}-{}", uuid::Uuid::now_v7().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Phase 2.4: the four canonical sources are read with per-file
    /// sha256 provenance; subdirectory AGENTS.md appears after the root;
    /// empty repos yield an empty segment.
    #[test]
    fn rules_sources_read_with_provenance() {
        let root = tempdir("full");
        std::fs::write(root.join("AGENTS.md"), "root: always run clippy").unwrap();
        std::fs::write(root.join("CLAUDE.md"), "claude: be terse").unwrap();
        std::fs::create_dir_all(root.join(".modbit")).unwrap();
        std::fs::write(root.join(".modbit/rules.md"), "modbit: prefer tools").unwrap();
        std::fs::create_dir_all(root.join(".cursor/rules")).unwrap();
        std::fs::write(root.join(".cursor/rules/testing.mdc"), "cursor: test first").unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/AGENTS.md"), "src: no unsafe").unwrap();

        let rules = read_workspace_rules(&root);
        assert!(rules.contains("# Workspace rules (repo-provided, provenance-hashed)"));
        assert!(rules.contains("## AGENTS.md (sha256:"));
        assert!(rules.contains("root: always run clippy"));
        assert!(rules.contains("## .cursor/rules/testing.mdc (sha256:"));
        assert!(rules.contains("test first"));
        // Provenance hashes are the real sha256 of the file bytes.
        let digest = {
            use sha2::{Digest, Sha256};
            let mut hasher = Sha256::new();
            hasher.update(b"root: always run clippy");
            format!("{:x}", hasher.finalize())
        };
        assert!(rules.contains(&digest), "provenance hash must match file bytes");
        // Ordering: root AGENTS.md before the subdirectory's.
        let root_pos = rules.find("root: always run clippy").unwrap();
        let src_pos = rules.find("src: no unsafe").unwrap();
        assert!(root_pos < src_pos, "root rules precede subdirectory rules");
        // .git-like directories are skipped.
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join(".git/AGENTS.md"), "poison").unwrap();
        let rules = read_workspace_rules(&root);
        assert!(!rules.contains("poison"), "hidden directories are skipped");
    }

    #[test]
    fn no_rules_files_yield_empty_segment() {
        let root = tempdir("empty");
        assert_eq!(read_workspace_rules(&root), "");
    }
}
