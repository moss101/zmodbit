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
use std::time::{Duration, Instant};

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
#[derive(Clone)]
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
            // Phase 4.3: keychain-first secret broker — the OS keychain
            // (macOS Keychain / Windows Credential Manager / Secret
            // Service) wins; the env broker stays the fallback so CI and
            // headless boots keep working.
            broker: Arc::new(modbit_providers::keychain::KeychainSecretBroker),
            // Phase 4.1: without MODBIT_REPO_ROOT the repo picker (task
            // repo_id) replaces the ambient repo — but task worktrees still
            // need a ROOT, so a bare root source is configured from
            // MODBIT_WORKTREE_ROOT (or the default location).
            worktrees: EnvWorktreeSource::from_env()
                .map(|s| Arc::new(s) as Arc<dyn WorktreeSource>)
                .or_else(|| {
                    Some(Arc::new(DefaultWorktreeRoot(
                        std::env::var("MODBIT_WORKTREE_ROOT")
                            .map(PathBuf::from)
                            .unwrap_or_else(|_| {
                                std::env::temp_dir().join("modbit-worktrees")
                            }),
                    )) as Arc<dyn WorktreeSource>)
                }),
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
                // Phase 2.5 boot scan: tasks still `running` when the
                // daemon died are interrupted runs — resume each from its
                // last committed conversation checkpoint (docs/19 §
                // Checkpoint epochs). Sequential, before tailing.
                if let Some(s) = weak.upgrade() {
                    for task_id in interrupted_tasks(&s.store) {
                        eprintln!(
                            "modbit scheduler: resuming interrupted task {task_id}"
                        );
                        if let Err(err) = s.resume_task(&task_id) {
                            eprintln!(
                                "modbit scheduler: task {task_id} resume failed: {err}"
                            );
                        }
                    }
                }
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
                                        // Phase 4.1: a run that errors BEFORE
                                        // the run starts (e.g. an unknown repo
                                        // id) must not leave the task Running
                                        // forever — transition it durably.
                                        if let Ok(task_id) = TaskId::parse(&e.aggregate_id) {
                                            let processor = modbit_event_store::CommandProcessor::new(s.store.clone());
                                            let _ = processor.execute(Command {
                                                command_id: uuid::Uuid::now_v7().simple().to_string(),
                                                actor: Actor {
                                                    actor_type: ActorType::System,
                                                    actor_id: "scheduler".into(),
                                                },
                                                payload: CommandPayload::FailTask {
                                                    task_id,
                                                    failure_code: "run_allocation_failed".into(),
                                                    message: err.clone(),
                                                },
                                            });
                                        }
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
        self.run_task_inner(task_id_str, None)
    }

    /// Phase 2.5 (M4 recovery): resume an interrupted run from the last
    /// committed conversation checkpoint. Attempt 2 continues the same
    /// model-visible conversation with an interruption note; effects
    /// between the checkpoint and the kill surface to the model for
    /// verification.
    pub fn resume_task(&self, task_id_str: &str) -> Result<(), String> {
        let Ok(task_id) = TaskId::parse(task_id_str) else {
            return Err(format!("malformed task id {task_id_str:?}"));
        };
        // A missing checkpoint still resumes — as a fresh attempt that
        // carries the interruption note (unknown-outcome guidance).
        let checkpoint = load_latest_checkpoint(&self.store, &task_id).unwrap_or_default();
        self.run_task_inner(task_id_str, Some(checkpoint))
    }

    fn run_task_inner(
        &self,
        task_id_str: &str,
        resume: Option<Vec<modbit_providers::gateway::ChatMessage>>,
    ) -> Result<(), String> {
        let Ok(task_id) = TaskId::parse(task_id_str) else {
            return Err(format!("malformed task id {task_id_str:?}"));
        };
        let brief = read_task_brief(&self.store, &task_id)?;
        let (session_id, title, prompt) = (brief.session_id, brief.title, brief.prompt);
        let (repo_id, base_branch) = (brief.repo_id, brief.base_branch);

        // Idempotency: a task that already has a run is never re-run
        // (resume is the M4 recovery spine's job, not a fresh run).
        if resume.is_none() && task_has_run(&self.store, &task_id)? {
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
        // Phase 4.2: persisted settings overlay the boot configuration
        // for THIS run (provider/model/base_url/max_turns/execution_mode).
        // The settings screen writes them; env stays the fallback.
        let stored_settings =
            self.store
                .with_conn(|conn| {
                    modbit_event_store::settings::get(conn).map_err(|e| e.to_string())
                });
        let (s_provider, s_model, s_base, s_turns, s_mode) = match &stored_settings {
            Ok(Some(doc)) => (
                doc.get("provider").and_then(|v| v.as_str()).map(str::to_string),
                doc.get("model").and_then(|v| v.as_str()).map(str::to_string),
                doc.get("base_url").and_then(|v| v.as_str()).map(str::to_string),
                doc.get("max_turns").and_then(|v| v.as_u64()),
                doc.get("execution_mode")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
            ),
            _ => (None, None, None, None, None),
        };
        let mut run_config = self.config.clone();
        if let Some(v) = &s_provider {
            if v == "anthropic" {
                run_config.provider = Provider::Anthropic;
            } else if v == "openai" {
                run_config.provider = Provider::OpenAi;
            }
        }
        if let Some(m) = &s_model {
            if !m.is_empty() {
                run_config.model = m.clone();
            }
        }
        if let Some(b) = &s_base {
            if !b.is_empty() {
                run_config.base_url = Some(b.clone());
            }
        }
        if let Some(t) = s_turns {
            if t > 0 {
                run_config.max_turns = t as u32;
            }
        }
        let execution_mode = s_mode.unwrap_or_else(|| "default".to_string());

        // Phase 4.1: a task created against a REGISTERED repository runs
        // on that repo (per-task base branch selection). Tasks without a
        // repo selection fall back to the configured/env default source.
        let registered: Option<Arc<dyn WorktreeSource>> = repo_id.as_ref().and_then(|rid| {
            let lookup = self.store.with_conn(|conn| {
                modbit_event_store::repos::get(conn, rid).map_err(|e| e.to_string())
            });
            match lookup {
                Ok(Some(repo)) => {
                    let worktree_root = self
                        .config
                        .worktrees
                        .as_ref()
                        .and_then(|s| s.worktree_root())
                        .or_else(|| {
                            EnvWorktreeSource::from_env()
                                .and_then(|e| WorktreeSource::worktree_root(&e))
                        })
                        .unwrap_or_else(|| std::env::temp_dir().join("modbit-worktrees"));
                    Some(Arc::new(RegisteredRepoSource {
                        repo_root: PathBuf::from(&repo.path),
                        worktree_root,
                        default_branch: repo.default_branch,
                        requested_branch: base_branch
                            .clone()
                            .filter(|b| !b.trim().is_empty()),
                    }) as Arc<dyn WorktreeSource>)
                }
                Ok(None) => None,
                Err(e) => {
                    eprintln!("modbit scheduler: repo registry lookup failed for {rid}: {e}");
                    None
                }
            }
        });
        // No configured repository is a typed failure (task parks), never
        // a scheduler panic — the poller must survive misconfiguration.
        let source: Arc<dyn WorktreeSource> = match registered {
            Some(source) => source,
            None => self
                .config
                .worktrees
                .clone()
                .or_else(|| {
                    EnvWorktreeSource::from_env().map(|s| Arc::new(s) as Arc<dyn WorktreeSource>)
                })
                .ok_or_else(|| "no repository configured for runs (set MODBIT_REPO_ROOT)".to_string())?,
        };
        let layout = source
            .layout(&task_id.to_string())
            .ok_or_else(|| "no repository configured for runs (set MODBIT_REPO_ROOT)".to_string())?;
        let worktree_path = layout.worktree.clone();
        let base_revision = layout.base_revision.clone();
        let repo = GitRepo::open(
            &source.repo_root().ok_or("worktree source has no repository root")?,
        )
        .map_err(|e| format!("open repo: {e}"))?;
        if worktree_path.exists() {
            // Phase 2.5 resume (or a crashed prior attempt): the worktree
            // already exists — reuse it; its state IS the possibly-partial
            // effect surface the resumed run must verify.
            GitRepo::open(&worktree_path).map_err(|e| format!("open existing worktree: {e}"))?;
        } else if let Some(start_point) = &layout.start_point {
            // Phase 4.1: per-task base branch selection.
            repo.worktree_add_from(&worktree_path, &layout.branch, start_point)
                .map_err(|e| format!("allocate worktree: {e}"))?;
        } else {
            repo.worktree_add(&worktree_path, &layout.branch)
                .map_err(|e| format!("allocate worktree: {e}"))?;
        }

        // 2. Context pack through the canonical file service on the worktree.
        let ws = Arc::new(
            WorkspaceFileService::open(&worktree_path).map_err(|e| format!("open workspace: {e}"))?,
        );
        // IMP-EV-0004: the task's repository index, built by walking the
        // real worktree at task start (gitignore/hidden/policy/binary/size
        // filters). The writer is attached to the run plane below; refreshes
        // ride the change journal from change.apply and the turn boundary.
        let built_index =
            modbit_retrieval::task_index::TaskIndex::build_at(&worktree_path, ws.workspace_revision());

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
        // Phase 2.6: the output sink shares run-plane state with the
        // observer (sequences + current run) so streamed chunk events and
        // lifecycle events interleave on one consistent aggregate.
        let shared = Arc::new(RunPlaneShared::default());
        // M4.4: acquire this run's session lease — a boot of another core
        // that starts a run for the same session bumps the generation and
        // fences THIS writer out (typed StaleLease on its next append).
        {
            let lease_id = format!("lease-{}", uuid::Uuid::now_v7().simple());
            let leased = self.store.with_conn(|conn| {
                modbit_event_store::leases::acquire(
                    conn,
                    &session_id.to_string(),
                    &lease_id,
                    "modbit-scheduler",
                )
            });
            match leased {
                Ok(_lease) => {
                    *shared.lease.lock().expect("lease cell") =
                        Some((session_id, lease_id));
                }
                Err(e) => {
                    eprintln!(
                        "modbit scheduler: session lease acquire failed for {task_id}: {e}"
                    );
                }
            }
        }
        let output_sink: Arc<dyn ToolOutputSink> = Arc::new(RunPlaneOutputSink {
            store: self.store.clone(),
            session_id,
            shared: shared.clone(),
        });
        // M4.3: the run's worktree journal (epoch-fenced checkpoints on
        // every successful change.apply).
        let worktree_journal: Arc<WorktreeJournalWriter> = Arc::new(WorktreeJournalWriter::new(
            self.store.clone(),
            session_id,
            shared.clone(),
        ));
        // IMP-EV-0004: attach the task-start index build to the run plane
        // and queue its build evidence (drained onto the run aggregate
        // right after RunStarted).
        let task_index: Arc<TaskIndexWriter> = Arc::new(TaskIndexWriter::new(
            self.store.clone(),
            session_id,
            shared.clone(),
            built_index,
        ));
        task_index.queue_built_evidence();
        // M3.8: the task context pack compiles through modbit-context
        // (token-derived budget, full provenance, recently-changed section,
        // index-seeded file heads). Packed file paths are retrieval
        // evidence for the retrieve-before-edit gate (MOD-CTX-001).
        let (context_pack, packed_files) = build_context_pack(
            &ws,
            Some(&task_index),
            &title,
            &prompt,
            self.config.max_input_tokens,
        );
        task_index.record_evidence(packed_files);
        let registry = build_worktree_registry(
            &ws,
            &worktree_path,
            execd.as_ref(),
            signal.cancel_token(),
            Some(output_sink),
            Some(worktree_journal),
            Some(task_index.clone()),
        );
        // Phase 4.2: execution_mode "readonly" drops effect-class grants
        // (write/external) — a REAL consumer of the persisted setting: a
        // readonly task's kernel refuses change.apply/shell.run/test.run.
        let readonly = execution_mode == "readonly";
        let kernel = PolicyKernel::new(vec![]);
        // Phase 5: approvals mode keeps the base grants read-only — a
        // Write/External effect first becomes a durable approval request;
        // the gate appends a live provisional grant on approval.
        let approvals_mode = execution_mode == "approvals";
        let live_grants = Arc::new(LiveGrants(std::sync::Mutex::new(
            worktree_grants()
                .into_iter()
                .filter(|g| {
                    if approvals_mode {
                        g.effect_class == EffectClass::ReadOnly
                    } else {
                        !readonly || g.effect_class == EffectClass::ReadOnly
                    }
                })
                .collect(),
        )));

        // 4-5. Run the one-agent runtime over the production transport,
        // writing every Run/Turn/RunStep transition into the store.
        // The fence flips the run's cancellation token (transport abort,
        // broker kill, boundary abort) through one shared hook.
        *shared.cancel_hook.lock().expect("cancel hook") = Some(signal.cancel_token());
        let observer = EventStoreObserver {
            store: self.store.clone(),
            session_id,
            task_id,
            ws: ws.clone(),
            task_index: Some(task_index),
            shared,
        };
        let approval_gate: Option<DurableApprovalGate> = if approvals_mode {
            Some(DurableApprovalGate {
                store: self.store.clone(),
                task_id,
                grants: live_grants.clone(),
                wait_timeout: Duration::from_secs(300),
            })
        } else {
            None
        };
        let transport = LiveGatewayTransport::new(&run_config, signal.cancel_token());
        let runtime = OneAgentRuntime {
            transport: &transport,
            registry: &registry,
            kernel: &kernel,
            grants: live_grants.as_ref(),
            approval_gate: approval_gate.as_ref().map(|g| g as &dyn crate::one_agent::ApprovalGate),
            max_turns: run_config.max_turns,
            observer: Some(&observer),
            control: Some(&*signal),
            resume_conversation: resume,
            async_compaction: true,
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
    /// Phase 4.1: branch the new worktree branches FROM (per-task base
    /// branch selection); None = HEAD.
    pub start_point: Option<String>,
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
    /// Where task worktrees are allocated (clones land beside them).
    fn worktree_root(&self) -> Option<std::path::PathBuf> {
        None
    }
}

/// RFC3339-ish timestamp (shared shape with the event store formatter).
pub(crate) fn rfc3339_now() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Phase 5 (docs/13 Waiting(Approval), docs/23): the durable approval
/// gate. A kernel-denied protected effect PERSISTS a pending approval
/// and BLOCKS the tool call until the operator resolves it through the
/// surface (ApproveEffect/DenyEffect). On approval, a LIVE provisional
/// grant scoped to the exact intent hash is appended to the run's grant
/// set — the decision is granted, never the blanket capability. The
/// pending approval is durable (SQLite): a Core kill while waiting does
/// not lose it.
/// The live grant set for ONE run: read fresh by the runtime on every
/// tool call, mutated by the approval gate when an operator approves a
/// protected effect (Phase 5).
pub struct LiveGrants(pub std::sync::Mutex<Vec<modbit_policy::CapabilityGrant>>);

impl LiveGrants {
    pub fn snapshot(&self) -> Vec<modbit_policy::CapabilityGrant> {
        self.0.lock().expect("live grants").clone()
    }

    pub fn push(&self, grant: modbit_policy::CapabilityGrant) {
        self.0.lock().expect("live grants").push(grant);
    }
}

pub struct DurableApprovalGate {
    pub store: Arc<EventStore>,
    pub task_id: TaskId,
    pub grants: Arc<LiveGrants>,
    pub wait_timeout: Duration,
}

impl DurableApprovalGate {

    fn request_persisted(
        &self,
        intent_hash: &str,
        tool: &str,
        arguments: &serde_json::Value,
    ) -> String {
        let approval_id = format!(
            "appr-{}",
            &uuid::Uuid::now_v7().simple().to_string()[..16]
        );
        let scope = serde_json::json!({
            "intent_hash": intent_hash,
            "arguments": arguments,
        })
        .to_string();
        let _ = self.store.with_conn(|conn| {
            modbit_event_store::approvals::insert(
                conn,
                &modbit_event_store::approvals::PendingApproval {
                    approval_id: approval_id.clone(),
                    task_id: self.task_id.to_string(),
                    intent_hash: intent_hash.to_string(),
                    tool: tool.to_string(),
                    scope,
                    state: "pending".into(),
                    created_at: crate::scheduler::rfc3339_now(),
                },
            )
        });
        approval_id
    }
}

impl crate::one_agent::ApprovalGate for DurableApprovalGate {
    fn await_decision(
        &self,
        intent_hash: &str,
        tool: &str,
        arguments: &serde_json::Value,
    ) -> Option<String> {
        let approval_id = self.request_persisted(intent_hash, tool, arguments);
        eprintln!(
            "modbit scheduler: task {} effect {tool} awaiting approval {} (approve via the surface)",
            self.task_id, approval_id
        );
        // Block on the durable decision. The wait is bounded by the gate
        // timeout; an unresolved approval stays PENDING in the durable
        // store and a later identical call waits again (or the operator
        // approves and the model retries).
        let deadline = Instant::now() + self.wait_timeout;
        loop {
            if Instant::now() > deadline {
                eprintln!(
                    "modbit scheduler: approval {} still pending at gate timeout",
                    approval_id
                );
                return None;
            }
            let decision = self
                .store
                .with_conn(|conn| {
                    modbit_event_store::approvals::decision_for(
                        conn,
                        &self.task_id.to_string(),
                        intent_hash,
                    )
                    .map_err(|e| e.to_string())
                })
                .ok()
                .flatten();
            match decision.as_deref() {
                Some("approved") => {
                    // Append a LIVE provisional grant for THIS tool: the
                    // kernel re-check passes for the approved effect only.
                    // The gate grants the DECISION, never the blanket
                    // capability set.
                    self.grants.push(modbit_policy::CapabilityGrant {
                        grant_id: format!("g-approval-{approval_id}"),
                        tool: tool.to_string(),
                        effect_class: modbit_policy::EffectClass::Write,
                    });
                    self.grants.push(modbit_policy::CapabilityGrant {
                        grant_id: format!("g-approval-{approval_id}-ext"),
                        tool: tool.to_string(),
                        effect_class: modbit_policy::EffectClass::External,
                    });
                    return Some("approved".to_string());
                }
                Some("denied") => return Some("denied".to_string()),
                _ => {}
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }
}

/// Phase 4.1: a bare worktree root without a repository — the source
/// used when no MODBIT_REPO_ROOT is configured. Tasks created WITHOUT a
/// registered repo cannot allocate from it (layout returns None → the
/// task fails with the typed no-repository message); registration and
/// clones use its worktree root.
pub struct DefaultWorktreeRoot(pub std::path::PathBuf);

impl WorktreeSource for DefaultWorktreeRoot {
    fn layout(&self, _task_id: &str) -> Option<WorktreeLayout> {
        None
    }

    fn repo_root(&self) -> Option<std::path::PathBuf> {
        None
    }

    fn worktree_root(&self) -> Option<std::path::PathBuf> {
        Some(self.0.clone())
    }
}

/// Phase 4.1: a task-scoped source for a REGISTERED repository — the
/// worktree is allocated from that repo, branching from the requested
/// base branch (or the repo's default) instead of the daemon-wide HEAD.
pub struct RegisteredRepoSource {
    pub repo_root: std::path::PathBuf,
    pub worktree_root: std::path::PathBuf,
    pub default_branch: String,
    pub requested_branch: Option<String>,
}

impl WorktreeSource for RegisteredRepoSource {
    fn layout(&self, task_id: &str) -> Option<WorktreeLayout> {
        let branch = format!("modbit/{}", &task_id[..12.min(task_id.len())]);
        Some(WorktreeLayout {
            worktree: self.worktree_root.join(task_id),
            branch,
            base_revision: self
                .requested_branch
                .clone()
                .unwrap_or_else(|| self.default_branch.clone()),
            start_point: self.requested_branch.clone(),
        })
    }

    fn repo_root(&self) -> Option<std::path::PathBuf> {
        Some(self.repo_root.clone())
    }

    fn worktree_root(&self) -> Option<std::path::PathBuf> {
        Some(self.worktree_root.clone())
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
            start_point: None,
        })
    }

    fn repo_root(&self) -> Option<std::path::PathBuf> {
        Some(self.repo_root.clone())
    }

    fn worktree_root(&self) -> Option<std::path::PathBuf> {
        Some(self.worktree_root.clone())
    }
}

fn default_worktree_root(repo_root: &std::path::Path) -> PathBuf {
    repo_root
        .parent()
        .map(|p| p.join(".modbit").join("worktrees"))
        .unwrap_or_else(|| repo_root.join("../.modbit/worktrees"))
}

/// The task's brief + Phase 4.1 repo selection (docs/14 step 2).
struct TaskBrief {
    session_id: SessionId,
    title: String,
    prompt: String,
    repo_id: Option<String>,
    base_branch: Option<String>,
}

/// Reads the task brief (session, title, prompt) and the Phase 4.1 repo
/// selection (registered repo id + base branch) from its created event.
fn read_task_brief(
    store: &EventStore,
    task_id: &TaskId,
) -> Result<TaskBrief, String> {
    let events = store.load(&task_id.to_string()).map_err(|e| e.to_string())?;
    for e in &events {
        if let DomainEvent::TaskCreated {
            session_id,
            title,
            prompt,
            repo_id,
            base_branch,
        } = &e.payload
        {
            return Ok(TaskBrief {
                session_id: *session_id,
                title: title.clone(),
                prompt: prompt.clone(),
                repo_id: repo_id.clone(),
                base_branch: base_branch.clone(),
            });
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

/// Compiles the task context pack through the canonical context engine
/// (docs/18 § Context Pack; M3.8): fragments packed value-ordered under a
/// token-derived character budget by `modbit-context`'s pack compiler,
/// EVERY packed fragment carrying full provenance (source, sha256,
/// revision, retrieval reason — validated by the provenance module), with
/// a "recently changed files" section from the workspace journal and
/// index-seeded file heads. Content is read FRESH through the workspace
/// service (hydration discipline — the index never supplies bytes).
/// Returns the pack text and the packed file paths (retrieval evidence).
fn build_context_pack(
    ws: &WorkspaceFileService,
    task_index: Option<&TaskIndexWriter>,
    title: &str,
    prompt: &str,
    max_input_tokens: u64,
) -> (String, Vec<String>) {
    use modbit_context::pack_compiler::{pack_with_budget, Fragment};
    use modbit_context::provenance::{validate_envelope, EnvelopeFragment, Provenance};

    // ~4 chars per token; bounded to keep the prompt segment sane.
    const CHARS_PER_TOKEN: usize = 4;
    let budget = (max_input_tokens as usize)
        .saturating_mul(CHARS_PER_TOKEN)
        .clamp(2_000, 64_000);
    let revision = ws.workspace_revision();
    let repo = "task-worktree".to_string();

    let mut fragments: Vec<Fragment> = Vec::new();
    let mut reasons: Vec<(String, String)> = Vec::new(); // path -> retrieval reason

    // 1. Task brief — critical, always retained.
    let brief = format!("# Task\n{title}\n\n# Objective\n{prompt}");
    reasons.insert(0, ("task:brief".into(), "task brief".into()));
    fragments.push(Fragment {
        path: "task:brief".into(),
        text: brief,
        value: 1.0,
        critical: true,
    });

    // 2. Recently changed files (journal tail, latest state per path).
    if let Ok(changes) = ws.changes() {
        let mut latest: std::collections::BTreeMap<&str, &modbit_workspace::FileChangeEvent> =
            std::collections::BTreeMap::new();
        for event in &changes {
            latest.insert(event.path.as_str(), event);
        }
        let mut recent: Vec<_> = latest.values().collect();
        recent.sort_by_key(|e| std::cmp::Reverse(e.workspace_revision));
        let lines: Vec<String> = recent
            .iter()
            .take(10)
            .map(|e| {
                format!(
                    "- {} ({:?} at workspace revision {}, content sha256 {})",
                    e.path,
                    e.change_kind,
                    e.workspace_revision,
                    &e.sha256[..e.sha256.len().min(12)]
                )
            })
            .collect();
        if !lines.is_empty() {
            reasons.push(("workspace:recently-changed".into(), "workspace change journal".into()));
            fragments.push(Fragment {
                path: "workspace:recently-changed".into(),
                text: format!("# Recently changed files\n{}", lines.join("\n")),
                value: 0.9,
                critical: false,
            });
        }
    }

    // 3. Top-level workspace map (bounded).
    let entries = ws.list("").unwrap_or_default();
    let shown: Vec<String> = entries.iter().take(50).cloned().collect();
    reasons.push(("workspace:top-level".into(), "workspace map".into()));
    fragments.push(Fragment {
        path: "workspace:top-level".into(),
        text: format!(
            "# Workspace files (top level, first 50)\n{}\n\n# Total top-level entries\n{}",
            shown.join("\n"),
            entries.len()
        ),
        value: 0.5,
        critical: false,
    });

    // 4. Index-seeded file heads: the task objective queries the live
    // index; each hit contributes a bounded fresh-content head.
    if let (Some(index), false) = (task_index, prompt.trim().is_empty()) {
        for hit in index.retrieval_hits(ws, prompt, 8) {
            if hit.path.ends_with(":brief")
                || hit.path.starts_with("workspace:")
                || hit.path.starts_with("task:")
            {
                continue;
            }
            let Ok((bytes, _rev)) = ws.read(&hit.path) else { continue };
            let head: String = String::from_utf8_lossy(&bytes)
                .lines()
                .take(40)
                .collect::<Vec<_>>()
                .join("\n");
            if head.trim().is_empty() {
                continue;
            }
            let score = (hit.score.clamp(0.0, 4.0) / 4.0) + 0.6;
            reasons.push((format!("file:{}", hit.path), format!("index query hit (score {:.3})", hit.score)));
            fragments.push(Fragment {
                path: format!("file:{}", hit.path),
                text: head,
                value: score.min(0.95),
                critical: false,
            });
        }
    }

    // Pack under budget through the canonical compiler, then render with
    // provenance lines validated by the provenance module.
    let mut store = modbit_context::pack_compiler::CompressionStore::new();
    match pack_with_budget(&fragments, budget, &mut store) {
        Ok(pack) => {
            let envelope: Vec<EnvelopeFragment> = pack
                .packed
                .iter()
                .map(|f| EnvelopeFragment {
                    text: f.text.clone(),
                    ephemeral: false,
                    provenance: Some(Provenance {
                        source: f.path.clone(),
                        repo: repo.clone(),
                        revision,
                        sha256: modbit_context::pack_compiler::sha256_hex(f.text.as_bytes()),
                        retrieval_reason: reasons
                            .iter()
                            .find(|(p, _)| *p == f.path)
                            .map(|(_, r)| r.clone())
                            .unwrap_or_else(|| "packed".into()),
                    }),
                })
                .collect();
            if validate_envelope(&envelope).is_err() {
                // A provenance bug must never silently ship an
                // unattributable pack: fall back to the brief only.
                eprintln!("modbit scheduler: context pack provenance validation failed; shipping brief only");
                let brief_text = fragments[0].text.clone();
                return (
                    format!(
                        "{brief_text}\n\n# Context pack\nunavailable (provenance validation failed)"
                    ),
                    Vec::new(),
                );
            }
            let mut sections: Vec<String> = Vec::new();
            let mut packed_files: Vec<String> = Vec::new();
            for (fragment, env) in pack.packed.iter().zip(&envelope) {
                let provenance = env.provenance.as_ref().expect("validated above");
                sections.push(format!(
                    "=== {} ===\n{}\n[provenance] source={} sha256={} revision={} reason={}",
                    fragment.path,
                    fragment.text,
                    provenance.source,
                    &provenance.sha256[..16],
                    provenance.revision,
                    provenance.retrieval_reason,
                ));
                if let Some(file) = fragment.path.strip_prefix("file:") {
                    packed_files.push(file.to_string());
                }
            }
            let handles: Vec<String> = pack
                .handles
                .iter()
                .map(|h| format!("- {} (hydratable by digest {})", h.path, &h.sha256[..12]))
                .collect();
            let text = format!(
                "{}\n\n=== context-pack ===\npacked {} fragment(s), {} compressed handle(s), {} / {} budgeted characters (context engine: modbit-context)\n{}",
                sections.join("\n\n"),
                pack.packed.len(),
                pack.handles.len(),
                pack.used_bytes,
                budget,
                if handles.is_empty() {
                    String::new()
                } else {
                    format!("compressed (hydratable on demand):\n{}", handles.join("\n"))
                }
            );
            (text, packed_files)
        }
        Err(e) => {
            // Critical overflow (budget smaller than the brief): ship the
            // brief unbound rather than fail the task.
            eprintln!("modbit scheduler: context pack failed ({e}); shipping brief only");
            (fragments[0].text.clone(), Vec::new())
        }
    }
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
            // Display paths are canonicalized to forward slashes so the
            // provenance (and the model-visible segment) is OS-uniform.
            let display = path
                .strip_prefix(worktree)
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| path.display().to_string())
                .replace('\\', "/");
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
                            .unwrap_or_else(|_| path.display().to_string())
                            .replace('\\', "/");
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
    output_sink: Option<Arc<dyn ToolOutputSink>>,
    worktree_journal: Option<Arc<WorktreeJournalWriter>>,
    task_index: Option<Arc<TaskIndexWriter>>,
) -> ToolRegistry {
    let registry = ToolRegistry::new();

    // M2.10: the media pipeline for binary reads — content-addressed
    // objects live under the worktree's .modbit dir (untracked; the
    // revision-bound diff never sees them), budgets enforced on ingest.
    let media: Option<Arc<modbit_tools::media::MediaPipeline>> =
        modbit_tools::media::MediaPipeline::new(&worktree.join(".modbit/objects"), 8 * 1024 * 1024)
            .ok()
            .map(Arc::new);

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
                let media = media.clone();
                let task_index = task_index.clone();
                Arc::new(move |args| {
                    let path = args.get("path").and_then(|v| v.as_str()).ok_or("missing path")?;
                    // Files checked out by git are adopted on first touch so
                    // reads carry revisions (canonical change-engine guard).
                    let _ = ws.adopt(path);
                    let (bytes, rev) = ws.read(path).map_err(|e| e.to_string())?;
                    // MOD-CTX-001: a read IS retrieval evidence for the
                    // retrieve-before-edit gate.
                    if let Some(index) = task_index.as_ref() {
                        index.record_evidence([path.to_string()]);
                    }
                    // M2.10: binary media (PNG/JPEG/PDF) reads through the
                    // media pipeline — typed envelope with provenance
                    // digest and content-addressed artifact, bounded
                    // preview instead of lossy binary in the conversation.
                    if let Some(pipeline) = media.as_ref() {
                        let detected = modbit_tools::media::detect_type(&bytes)
                            .map(|(t, _)| t)
                            .unwrap_or(modbit_tools::media::MediaType::Text);
                        if !matches!(
                            detected,
                            modbit_tools::media::MediaType::Text
                                | modbit_tools::media::MediaType::Binary
                        ) {
                            let envelope = pipeline
                                .ingest(path, path, &bytes)
                                .map_err(|e| format!("media ingest: {e}"))?;
                            return Ok(serde_json::json!({
                                "content": format!(
                                    "[media: {} {} bytes, sha256 {}]",
                                    envelope.mime, envelope.byte_length, envelope.sha256
                                ),
                                "file_revision": rev,
                                "media": {
                                    "kind": format!("{:?}", envelope.media_type).to_lowercase(),
                                    "mime": envelope.mime,
                                    "byte_length": envelope.byte_length,
                                    "sha256": envelope.sha256,
                                    "object_ref": envelope.object_rel_path,
                                },
                            }));
                        }
                    }
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
            description: "Command argv: a JSON array of strings (preferred, exact argv semantics) or a single string parsed with shell word splitting (quotes honored, no shell expansion)".into(),
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
                let sink = output_sink.clone();
                let journal = worktree_journal.clone();
                Arc::new(move |args| {
                    let execd = execd.as_ref().ok_or(
                        "shell execution unavailable: no modbit-execd broker configured (set MODBIT_EXECD_ADDR)",
                    )?;
                    // Phase 2.6: argv accepts a JSON array (exact argv) or a
                    // string split on shell words (quotes honored).
                    let argv = match args.get("argv") {
                        Some(serde_json::Value::Array(items)) => {
                            let mut argv = Vec::with_capacity(items.len());
                            for item in items {
                                let part = item
                                    .as_str()
                                    .ok_or("argv array entries must be strings")?;
                                if part.is_empty() {
                                    return Err("argv array entries must be non-empty".into());
                                }
                                argv.push(part.to_string());
                            }
                            argv
                        }
                        Some(serde_json::Value::String(command)) => split_shell_words(command)?,
                        _ => return Err("missing argv (JSON array of strings, or a command string)".into()),
                    };
                    if argv.is_empty() {
                        return Err("empty argv".into());
                    }
                    let run_id = format!("task-{}", uuid::Uuid::now_v7().simple());
                    // Phase 2.6: stream output DURING execution — every
                    // drain emits a bounded chunk event through the sink
                    // (progress in the durable run plane) — and keep the
                    // full bytes for a paginated OutputRef at completion.
                    // Phase 2.3 semantics unchanged: the cancellation flag
                    // races the wait; StopTask kills the broker run (no
                    // orphan process).
                    let (status, output) = run_streaming_capture(
                        execd,
                        &run_id,
                        &argv,
                        &worktree,
                        &cancel,
                        sink.as_deref(),
                    )
                    .map_err(|e| format!("execd: {e}"))?;
                    let exit_code = match status.state {
                        modbit_terminal::RunState::Exited(code) => code,
                        _ => -1,
                    };
                    // Full output -> paginated OutputRef (runtime table);
                    // the inline tail stays short.
                    let output_ref = sink.as_deref().and_then(|sink| {
                        sink.store_full(&run_id, &output).map(|stored| {
                            serde_json::json!({
                                "output_ref_id": stored.output_ref_id,
                                "byte_length": stored.byte_length,
                                "preview": stored.preview,
                            })
                        })
                    });
                    // M4.5: the terminal surface's reattachment cursor —
                    // the unified CursorMeta contract, captured durably
                    // (read_output resumes from `position`; live=false
                    // after exit but the broker replay window holds).
                    if let Some(journal) = journal.as_ref() {
                        journal.record_surface(
                            modbit_checkpoint::cursor_meta::SurfaceKind::Terminal,
                            &run_id,
                            output.len() as u64,
                            0,
                            false,
                        );
                    }
                    let mut result = serde_json::json!({
                        "exit_code": exit_code,
                        "state": format!("{:?}", status.state),
                        "output": tail(&String::from_utf8_lossy(&output), 2_000),
                        "broker_run_id": run_id,
                    });
                    if let Some(output_ref) = output_ref {
                        result["output_ref"] = output_ref;
                    }
                    Ok(result)
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
                let task_index = task_index.clone();
                Arc::new(move |args| {
                    let path = args.get("path").and_then(|v| v.as_str()).ok_or("missing path")?;
                    let old = args.get("old_text").and_then(|v| v.as_str()).ok_or("missing old_text")?;
                    let new = args.get("new_text").and_then(|v| v.as_str()).ok_or("missing new_text")?;
                    // MOD-CTX-001 retrieve-before-edit gate: an edit
                    // proposal must be grounded in retrieved context. When
                    // the task index tracks evidence and THIS path has
                    // none — no fs.read, no query/search hit, not packed —
                    // the proposal is refused with the remedy named. With
                    // no index (tracking unavailable) the gate stays open
                    // and says so.
                    if let Some(index) = task_index.as_ref() {
                        if !index.has_evidence(path) {
                            return Ok(serde_json::json!({
                                "ok": false,
                                "gate": "retrieve_before_edit",
                                "reason": format!(
                                    "no retrieval evidence for '{path}' in this task; read it with fs.read or surface it via context.query / search.grep / search.symbol before proposing an edit (docs/02 MOD-CTX-001)"
                                ),
                            }));
                        }
                    }
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
                let journal = worktree_journal.clone();
                let task_index = task_index.clone();
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
                    // M4.3: record the edit into the run's journal and
                    // write an epoch-fenced worktree checkpoint (baseline
                    // = pre-edit bytes, delta = post-edit content).
                    if let Some(journal) = journal.as_ref() {
                        journal.record(
                            path,
                            bytes.to_vec(),
                            Some(updated.as_bytes().to_vec()),
                        );
                    }
                    // IMP-EV-0004: the edit advanced the workspace
                    // revision — refresh the repository index from the
                    // change journal and emit the recomputed-segment
                    // evidence (only affected segments move).
                    if let Some(index) = task_index.as_ref() {
                        index.refresh_from_journal(&ws, "change_apply");
                    }
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
                let task_index = task_index.clone();
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
                    // MOD-CTX-001: matched files are retrieval evidence.
                    if let Some(index) = task_index.as_ref() {
                        let paths: std::collections::BTreeSet<String> = matches
                            .iter()
                            .filter_map(|m| m.rsplit_once(':').map(|(p, _)| p.to_string()))
                            .collect();
                        index.record_evidence(paths);
                    }
                    Ok(serde_json::json!({ "matches": matches, "files_searched": visited }))
                })
            },
        )
        .expect("register search.grep");

    // ---- context.query: fused BM25 + path + symbol index query --------
    let mut cq_params = std::collections::BTreeMap::new();
    cq_params.insert("query".into(), param(ParamType::Str, true, "What to find: terms, an identifier, or a path fragment"));
    cq_params.insert("mode".into(), param(ParamType::Str, false, "auto (default: the retrieval planner routes L0-L3) | fused | exact | regex | path | impact (import dependents of a path)"));
    cq_params.insert("limit".into(), param(ParamType::Int, false, "Max hits (default 20, max 50; exact/regex/path max 200)"));
    registry
        .register_with_schema(
            "context.query",
            "1.0.0",
            EffectClass::ReadOnly,
            "Query the task's repository index for context: fused mode ranks Tantivy BM25 + exact path + tree-sitter symbols with provenance; exact/regex/path modes query the M3.1 index directly. Freshened from the change journal before answering.",
            Some(ToolSchema { aliases: Default::default(), parameters: cq_params }),
            {
                let ws = ws.clone();
                let task_index = task_index.clone();
                Arc::new(move |args| {
                    let Some(index) = task_index.as_ref() else {
                        return Ok(serde_json::json!({ "hits": [], "note": "index unavailable for this task" }));
                    };
                    let query = args.get("query").and_then(|v| v.as_str()).ok_or("missing query")?;
                    let limit = args
                        .get("limit")
                        .and_then(|v| v.as_i64())
                        .map(|l| l.clamp(1, 50) as usize)
                        .unwrap_or(20);
                    let mode = args
                        .get("mode")
                        .and_then(|v| v.as_str())
                        .unwrap_or("auto")
                        .to_lowercase();
                    let result = match mode.as_str() {
                        "exact" => index.query_exact(&ws, query, limit),
                        "regex" => index.query_regex(&ws, query, limit)?,
                        "path" => index.query_paths(&ws, query, limit),
                        "impact" => index.query_impact(&ws, query, limit),
                        "fused" => index.query_context(&ws, query, limit),
                        // auto (default): the M3.7 planner routes.
                        _ => index.query_auto(&ws, query, limit),
                    };
                    // MOD-CTX-001: hit paths are retrieval evidence.
                    if let Some(hits) = result.get("hits").and_then(|h| h.as_array()) {
                        let paths = hits.iter().filter_map(|h| {
                            h.get("path")
                                .and_then(|p| p.as_str())
                                .map(String::from)
                                .or_else(|| h.as_str().map(String::from))
                        });
                        index.record_evidence(paths);
                    }
                    Ok(result)
                })
            },
        )
        .expect("register context.query");

    // ---- search.symbol: tree-sitter definitions/references ------------
    let mut sym_params = std::collections::BTreeMap::new();
    sym_params.insert("name".into(), param(ParamType::Str, true, "Exact symbol name to resolve"));
    registry
        .register_with_schema(
            "search.symbol",
            "1.0.0",
            EffectClass::ReadOnly,
            "Find symbol definitions and references by exact name via tree-sitter (Rust, TypeScript/TSX, JavaScript, Python). Freshened from the change journal before answering.",
            Some(ToolSchema { aliases: Default::default(), parameters: sym_params }),
            {
                let ws = ws.clone();
                let task_index = task_index.clone();
                Arc::new(move |args| {
                    let Some(index) = task_index.as_ref() else {
                        return Ok(serde_json::json!({ "definitions": [], "references": [], "note": "index unavailable for this task" }));
                    };
                    let name = args.get("name").and_then(|v| v.as_str()).ok_or("missing name")?;
                    let result = index.query_symbol(&ws, name);
                    // MOD-CTX-001: def/ref paths are retrieval evidence.
                    let paths = result
                        .get("definitions")
                        .and_then(|d| d.as_array())
                        .map(|a| a.iter())
                        .into_iter()
                        .flatten()
                        .chain(
                            result
                                .get("references")
                                .and_then(|r| r.as_array())
                                .map(|a| a.iter())
                                .into_iter()
                                .flatten(),
                        )
                        .filter_map(|e| e.get("path").and_then(|p| p.as_str()).map(String::from));
                    index.record_evidence(paths);
                    Ok(result)
                })
            },
        )
        .expect("register search.symbol");

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
pub fn worktree_grants() -> Vec<CapabilityGrant> {
    vec![
        CapabilityGrant { grant_id: "g-fs-read".into(), tool: "fs.read".into(), effect_class: EffectClass::ReadOnly },
        CapabilityGrant { grant_id: "g-fs-list".into(), tool: "fs.list".into(), effect_class: EffectClass::ReadOnly },
        CapabilityGrant { grant_id: "g-grep".into(), tool: "search.grep".into(), effect_class: EffectClass::ReadOnly },
        CapabilityGrant { grant_id: "g-context-query".into(), tool: "context.query".into(), effect_class: EffectClass::ReadOnly },
        CapabilityGrant { grant_id: "g-search-symbol".into(), tool: "search.symbol".into(), effect_class: EffectClass::ReadOnly },
        CapabilityGrant { grant_id: "g-git-status".into(), tool: "git.status".into(), effect_class: EffectClass::ReadOnly },
        CapabilityGrant { grant_id: "g-git-diff".into(), tool: "git.diff".into(), effect_class: EffectClass::ReadOnly },
        CapabilityGrant { grant_id: "g-change-propose".into(), tool: "change.propose".into(), effect_class: EffectClass::ReadOnly },
        CapabilityGrant { grant_id: "g-change-apply".into(), tool: "change.apply".into(), effect_class: EffectClass::Write },
        CapabilityGrant { grant_id: "g-shell-run".into(), tool: "shell.run".into(), effect_class: EffectClass::External },
        CapabilityGrant { grant_id: "g-test-run".into(), tool: "test.run".into(), effect_class: EffectClass::External },
    ]
}

/// Loads the latest conversation checkpoint for a task's runs (Phase
/// 2.5): the newest `conversation_checkpointed` event on any run of the
/// task, deserialized back into the gateway message projection.
fn load_latest_checkpoint(
    store: &Arc<EventStore>,
    task_id: &TaskId,
) -> Option<Vec<modbit_providers::gateway::ChatMessage>> {
    // The task's runs come from run_started payloads (the `runs`
    // projection table is not populated in this store generation).
    let json: Option<String> = store.with_conn(|conn| {
        conn.query_row(
            "SELECT json_extract(payload_inline, '$.conversation_json')
             FROM events
             WHERE event_type = 'conversation_checkpointed'
               AND aggregate_id IN (
                 SELECT aggregate_id FROM events
                 WHERE event_type = 'run_started'
                   AND json_extract(payload_inline, '$.task_id') = ?1
               )
             ORDER BY sequence DESC LIMIT 1",
            [task_id.to_string()],
            |row| row.get(0),
        )
        .ok()
    });
    json.and_then(|json| serde_json::from_str(&json).ok())
}

/// Tasks stuck in `running` at daemon boot (Phase 2.5): their run died
/// with the process. The scheduler resumes each from its last checkpoint.
fn interrupted_tasks(store: &Arc<EventStore>) -> Vec<String> {
    store.with_conn(|conn| {
        let Ok(mut stmt) = conn.prepare("SELECT task_id FROM tasks WHERE state = 'running'") else {
            return Vec::new();
        };
        stmt.query_map([], |row| row.get::<_, String>(0))
            .map(|rows| rows.flatten().collect())
            .unwrap_or_default()
    })
}

/// Stored full output handle (Phase 2.6): the paginated OutputRef.
pub struct StoredOutput {
    pub output_ref_id: String,
    pub byte_length: u64,
    pub preview: String,
}

/// Streaming sink for tool output (Future-tasks Phase 2 item 6): tools
/// that produce incremental output emit bounded chunk events DURING
/// execution and store the full bytes behind a paginated OutputRef in
/// the runtime store (docs/31 § Core tables).
pub trait ToolOutputSink: Send + Sync {
    /// One streamed chunk: bounded preview rides a durable run event.
    fn chunk(&self, broker_run_id: &str, offset: u64, bytes: &[u8]);
    /// Stores the full output; returns the paginated reference.
    fn store_full(&self, broker_run_id: &str, bytes: &[u8]) -> Option<StoredOutput>;
}

/// State shared by the observer and the output sink for one run: the
/// current run id and per-aggregate sequence accounting (both append to
/// the same run aggregate, so sequences MUST be reserved under one lock),
/// plus the session lease this run writes under (M4.4 fencing).
#[derive(Default)]
struct RunPlaneShared {
    run: std::sync::Mutex<Option<RunId>>,
    sequences: std::sync::Mutex<std::collections::HashMap<String, u64>>,
    /// Session lease for lease-fenced appends; None = unfenced (tests).
    lease: std::sync::Mutex<Option<(SessionId, String)>>,
    /// Set when a fenced append is rejected (another core owns the
    /// session): the run must abort — never write, never continue.
    fenced: std::sync::atomic::AtomicBool,
    /// The run's cancellation token; flipped on fencing so the transport,
    /// tools and turn boundary all abort through one path.
    cancel_hook: std::sync::Mutex<Option<Arc<std::sync::atomic::AtomicBool>>>,
    /// Run-aggregate events produced BEFORE the run id exists (e.g. the
    /// task-start repository-index build, IMP-EV-0004); drained onto the
    /// run aggregate right after RunStarted.
    pending_run_events: std::sync::Mutex<Vec<DomainEvent>>,
}

impl RunPlaneShared {
    /// Queues a run-aggregate event emitted before the run id is known.
    fn queue_run_event(&self, payload: DomainEvent) {
        self.pending_run_events
            .lock()
            .expect("pending run events")
            .push(payload);
    }

    fn take_pending_run_events(&self) -> Vec<DomainEvent> {
        std::mem::take(
            &mut self
                .pending_run_events
                .lock()
                .expect("pending run events"),
        )
    }
}

impl RunPlaneShared {
    /// Appends under the session lease when one is held. A stale lease is
    /// FATAL for this run: the fence flag flips, the cancel hook fires,
    /// and the typed error returns (M4.4: a fenced-out writer must not
    /// write or continue).
    fn append_fenced(&self, store: &EventStore, envelope: &mut EventEnvelope) -> Result<(), String> {
        let lease = self.lease.lock().expect("lease cell").clone();
        let outcome = match lease {
            Some((session_id, lease_id)) => store
                .append_with_lease(&session_id.to_string(), &lease_id, &mut [envelope.clone()])
                .map_err(|e| e.to_string()),
            None => store.append(&mut [envelope.clone()]).map_err(|e| e.to_string()),
        };
        if let Err(err) = &outcome {
            if err.contains("stale lease") {
                self.fenced.store(true, std::sync::atomic::Ordering::SeqCst);
                if let Some(hook) = self
                    .cancel_hook
                    .lock()
                    .expect("cancel hook")
                    .as_ref()
                {
                    hook.store(true, std::sync::atomic::Ordering::SeqCst);
                }
                eprintln!(
                    "modbit scheduler: run fenced out (session lease lost): {err}"
                );
            }
        }
        outcome
    }
}

/// Production sink: chunk events on the Run aggregate (run id arrives
/// with run_started through the shared cell); full output into the
/// runtime store's output_refs table.
struct RunPlaneOutputSink {
    store: Arc<EventStore>,
    session_id: SessionId,
    shared: Arc<RunPlaneShared>,
}

impl RunPlaneShared {
    /// Builds and appends one Run-aggregate event under the session
    /// lease, mirroring the observer's envelope + sequence rules. Shared
    /// by the output sink and the worktree journal writer.
    fn append_run_event(
        &self,
        store: &EventStore,
        session_id: SessionId,
        run_id: RunId,
        payload: DomainEvent,
    ) {
        let aggregate_id = run_id.to_string();
        let mut envelope = EventEnvelope {
            event_id: uuid::Uuid::now_v7().to_string(),
            session_id,
            task_id: None,
            run_id: Some(run_id),
            turn_id: None,
            step_id: None,
            aggregate_type: AggregateType::Run,
            aggregate_id: aggregate_id.clone(),
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
        let next = sequences.get(&aggregate_id).copied().unwrap_or(1);
        envelope.sequence = next;
        envelope.seal();
        sequences.insert(aggregate_id, next + 1);
        drop(sequences);
        if let Err(e) = self.append_fenced(store, &mut envelope) {
            eprintln!("modbit scheduler: append run event failed: {e}");
        }
    }
}

/// M4.3: writes the run's worktree checkpoint journal (docs/22). Every
/// successful change.apply records baseline + delta into a
/// modbit-checkpoint DeltaJournal and persists an epoch-fenced
/// WorktreeCheckpointed run event (strictly increasing epochs via the
/// crate's CheckpointStore; replay restores the edited state exactly).
pub struct WorktreeJournalWriter {
    store: Arc<EventStore>,
    session_id: SessionId,
    shared: Arc<RunPlaneShared>,
    inner: std::sync::Mutex<modbit_checkpoint::delta::DeltaJournal>,
    checkpoints: std::sync::Mutex<modbit_checkpoint::CheckpointStore>,
    next_epoch: std::sync::atomic::AtomicU64,
}

impl WorktreeJournalWriter {
    fn new(
        store: Arc<EventStore>,
        session_id: SessionId,
        shared: Arc<RunPlaneShared>,
    ) -> Self {
        WorktreeJournalWriter {
            store,
            session_id,
            shared,
            inner: std::sync::Mutex::new(modbit_checkpoint::delta::DeltaJournal::default()),
            checkpoints: std::sync::Mutex::new(modbit_checkpoint::CheckpointStore::new()),
            next_epoch: std::sync::atomic::AtomicU64::new(1),
        }
    }

    /// Records one SURFACE cursor (M4.5: terminal/browser/sandbox
    /// reattachment metadata through the unified CursorMeta contract)
    /// and writes a durable epoch-fenced checkpoint carrying it.
    pub fn record_surface(
        &self,
        surface: modbit_checkpoint::cursor_meta::SurfaceKind,
        handle: &str,
        position: u64,
        revision: u64,
        live: bool,
    ) {
        const MAX_JOURNAL_BYTES: usize = 256 * 1024;
        let run_id = match *self.shared.run.lock().expect("run cell") {
            Some(id) => id,
            None => return,
        };
        let epoch = self
            .next_epoch
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let Ok(mut journal) = self.inner.lock() else { return };
        let meta = modbit_checkpoint::cursor_meta::CursorMeta {
            surface,
            handle: handle.to_string(),
            position,
            revision,
            live,
        };
        // Latest cursor per (surface, handle) wins in the persisted view.
        journal
            .surfaces
            .retain(|m| !(m.surface == surface && m.handle == meta.handle));
        journal.surfaces.push(meta);
        let Ok(journal_json) = serde_json::to_string(&*journal) else {
            return;
        };
        if journal_json.len() > MAX_JOURNAL_BYTES {
            return;
        }
        if let Err(e) = self
            .checkpoints
            .lock()
            .expect("checkpoint store")
            .write(epoch, &journal_json)
        {
            eprintln!("modbit scheduler: worktree checkpoint fenced: {e}");
            return;
        }
        self.shared.append_run_event(
            &self.store,
            self.session_id,
            run_id,
            DomainEvent::WorktreeCheckpointed { epoch, journal_json },
        );
    }

    /// Records one edit and writes the durable epoch-fenced checkpoint.
    pub fn record(&self, path: &str, baseline: Vec<u8>, delta: Option<Vec<u8>>) {
        const MAX_JOURNAL_BYTES: usize = 256 * 1024;
        let run_id = match *self.shared.run.lock().expect("run cell") {
            Some(id) => id,
            None => return,
        };
        let epoch = self
            .next_epoch
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let Ok(mut journal) = self.inner.lock() else { return };
        journal
            .baseline
            .entry(path.to_string())
            .or_insert_with(|| baseline.clone());
        if let Some(content) = delta {
            journal.deltas.push(modbit_checkpoint::delta::WorktreeDelta {
                path: path.to_string(),
                content: Some(content),
            });
        }
        let Ok(journal_json) = serde_json::to_string(&*journal) else {
            return;
        };
        if journal_json.len() > MAX_JOURNAL_BYTES {
            // Bounded journals: a run editing beyond the bound keeps its
            // in-memory journal but stops persisting snapshots (git
            // remains the full-fidelity record for the worktree).
            return;
        }
        // Epoch fencing (crate-enforced): strictly increasing writes only.
        if let Err(e) = self
            .checkpoints
            .lock()
            .expect("checkpoint store")
            .write(epoch, &journal_json)
        {
            eprintln!("modbit scheduler: worktree checkpoint fenced: {e}");
            return;
        }
        self.shared.append_run_event(
            &self.store,
            self.session_id,
            run_id,
            DomainEvent::WorktreeCheckpointed { epoch, journal_json },
        );
    }
}

/// Phase 3 / IMP-EV-0004: the task-scoped repository index. Built ONCE at
/// task start by walking the real worktree (modbit-retrieval walker:
/// gitignore/hidden/policy/binary/size filters), then refreshed
/// incrementally from the workspace change journal — the canonical FS
/// delta source — on every successful change.apply and at the turn
/// boundary. Evidence rides `index_updated` run events: root digest, file
/// count, and the only-affected-segments recomputation list (QUAL-EV-0004).
pub struct TaskIndexWriter {
    store: Arc<EventStore>,
    session_id: SessionId,
    shared: Arc<RunPlaneShared>,
    index: std::sync::Mutex<modbit_retrieval::task_index::TaskIndex>,
    /// Paths with retrieval evidence in this task (MOD-CTX-001
    /// retrieve-before-edit): read via fs.read, hit by context.query /
    /// search.grep / search.symbol, or packed into the task context.
    evidence: std::sync::Mutex<std::collections::BTreeSet<String>>,
    /// Headless LSP sessions per language (M3.4): lazily spawned from the
    /// MODBIT_LSP_<LANG> env commands, killed when the writer drops.
    /// None = a previous spawn failed (do not retry every query).
    lsp_sessions:
        std::sync::Mutex<std::collections::BTreeMap<String, Option<modbit_diagnostics::lsp::LspSession>>>,
}

/// Evidence bound for the recomputed-segment list in one event.
const MAX_RECOMPUTED_EVIDENCE: usize = 128;

impl TaskIndexWriter {
    fn new(
        store: Arc<EventStore>,
        session_id: SessionId,
        shared: Arc<RunPlaneShared>,
        index: modbit_retrieval::task_index::TaskIndex,
    ) -> Self {
        TaskIndexWriter {
            store,
            session_id,
            shared,
            index: std::sync::Mutex::new(index),
            evidence: std::sync::Mutex::new(std::collections::BTreeSet::new()),
            lsp_sessions: std::sync::Mutex::new(std::collections::BTreeMap::new()),
        }
    }

    /// Records retrieval evidence for paths (MOD-CTX-001): they were
    /// surfaced to the agent (read, queried, matched, or packed).
    pub(crate) fn record_evidence<I: IntoIterator<Item = String>>(&self, paths: I) {
        let mut evidence = self.evidence.lock().expect("evidence set");
        evidence.extend(paths);
    }

    /// Whether a path has retrieval evidence yet.
    pub(crate) fn has_evidence(&self, path: &str) -> bool {
        self.evidence
            .lock()
            .expect("evidence set")
            .contains(path)
    }

    /// Ranked index hits for pack seeding (task-context compilation):
    /// freshens the index first, then queries. Provenance rides each hit.
    pub(crate) fn retrieval_hits(
        &self,
        ws: &WorkspaceFileService,
        query: &str,
        limit: usize,
    ) -> Vec<modbit_retrieval::task_index::ContextHit> {
        self.refresh_from_journal(ws, "context_pack");
        let index = self.index.lock().expect("task index mutex");
        index.context_query(query, limit)
    }

    /// Emits the task-start build summary. Called before the run id exists
    /// (the index is built when the task starts, before the run does), so
    /// the event is queued and drained right after RunStarted.
    fn queue_built_evidence(&self) {
        let index = self.index.lock().expect("task index mutex");
        self.shared.queue_run_event(DomainEvent::IndexUpdated {
            reason: "task_start".into(),
            workspace_revision: index.indexed_workspace_revision,
            file_count: index.repo.files.len() as u32,
            root_digest: index.root_digest(),
            recomputed: Vec::new(),
            recomputed_truncated: false,
        });
    }

    /// Drains the workspace change journal for everything after the last
    /// indexed revision, folds it into both index halves incrementally,
    /// and emits the evidence event with the recomputed segments. A journal
    /// with nothing new is a no-op (no event). Returns true when the index
    /// moved.
    fn refresh_from_journal(&self, ws: &WorkspaceFileService, reason: &str) -> bool {
        let Ok(events) = ws.changes() else { return false };
        let mut index = self.index.lock().expect("task index mutex");
        let fresh: Vec<_> = events
            .into_iter()
            .filter(|e| e.workspace_revision > index.indexed_workspace_revision)
            .collect();
        if fresh.is_empty() {
            return false;
        }
        let new_revision = fresh
            .iter()
            .map(|e| e.workspace_revision)
            .max()
            .unwrap_or(index.indexed_workspace_revision);
        let changes: Vec<modbit_retrieval::task_index::IndexChange> = fresh
            .iter()
            .map(|e| modbit_retrieval::task_index::IndexChange {
                path: e.path.clone(),
                deleted: matches!(e.change_kind, modbit_workspace::FileChangeKind::Deleted),
            })
            .collect();
        let recomputed = index.apply_delta(ws.root(), &changes, new_revision);
        let recomputed_truncated = recomputed.len() > MAX_RECOMPUTED_EVIDENCE;
        let evidence: Vec<String> = recomputed
            .iter()
            .take(MAX_RECOMPUTED_EVIDENCE)
            .map(|r| match r {
                modbit_retrieval::merkle::Recomputed::FileLeaf { path } => {
                    format!("leaf:{path}")
                }
                modbit_retrieval::merkle::Recomputed::DirNode { dir } => format!("dir:{dir}"),
            })
            .collect();
        self.shared.append_run_event(
            &self.store,
            self.session_id,
            match *self.shared.run.lock().expect("run cell") {
                Some(id) => id,
                // Refresh can only be triggered from inside the run; a
                // missing run id means the run already ended — keep the
                // index fresh but emit nothing (no aggregate to ride).
                None => return true,
            },
            DomainEvent::IndexUpdated {
                reason: reason.to_string(),
                workspace_revision: index.indexed_workspace_revision,
                file_count: index.repo.files.len() as u32,
                root_digest: index.root_digest(),
                recomputed: evidence,
                recomputed_truncated,
            },
        );
        true
    }

    /// context.query (docs/17: Context Engine): refreshes the index from
    /// the change journal FIRST (freshness before answering), then runs
    /// the fused BM25 + path + symbol query. Bounded, deterministic.
    fn query_context(&self, ws: &WorkspaceFileService, query: &str, limit: usize) -> serde_json::Value {
        self.refresh_from_journal(ws, "context_query");
        let index = self.index.lock().expect("task index mutex");
        let hits = index.context_query(query, limit);
        serde_json::json!({
            "hits": hits
                .iter()
                .map(|h| {
                    serde_json::json!({
                        "path": h.path,
                        "line": h.line,
                        "score": (h.score * 1000.0).round() / 1000.0,
                        "sources": h.sources,
                    })
                })
                .collect::<Vec<_>>(),
            "note": "paths + provenance only; read files with fs.read for current content",
        })
    }

    /// search.symbol (docs/17: Search family): refreshes the index, then
    /// resolves definitions and references for an exact symbol name via
    /// the tree-sitter surface (Rust, TypeScript/TSX, JavaScript, Python),
    /// ENRICHED with the headless LSP answer when a server is configured
    /// for the language (M3.4 — docs/18: LSP where available; the
    /// deterministic tree-sitter surface always stays the backbone).
    fn query_symbol(&self, ws: &WorkspaceFileService, name: &str) -> serde_json::Value {
        self.refresh_from_journal(ws, "symbol_query");
        let defs = {
            let index = self.index.lock().expect("task index mutex");
            index.symbol_definitions(name)
        };
        let refs = {
            let index = self.index.lock().expect("task index mutex");
            index.symbol_references(name)
        };
        let mut result = serde_json::json!({
            "definitions": defs,
            "references": refs,
        });
        if let Some(lsp) = self.lsp_enrich(ws, name) {
            result["lsp"] = lsp;
        }
        result
    }

    /// The M3.4 LSP enrichment for one symbol: spawns/reuses the
    /// language's headless server, resolves definition + references at
    /// the first tree-sitter definition site, and returns the normalized
    /// section. Any failure degrades to `{"unavailable": reason}` — the
    /// tool never fails because LSP did.
    fn lsp_enrich(&self, ws: &WorkspaceFileService, name: &str) -> Option<serde_json::Value> {
        let first_def = {
            let index = self.index.lock().expect("task index mutex");
            index.symbol_definitions(name).into_iter().next()?
        };
        let lang = modbit_retrieval::symbols::language_of(&first_def.path)?;
        let env_key = format!(
            "MODBIT_LSP_{}",
            match lang {
                "rust" => "RUST",
                "typescript" | "tsx" => "TYPESCRIPT",
                "javascript" => "JAVASCRIPT",
                "python" => "PYTHON",
                _ => return None,
            }
        );
        let command = std::env::var(&env_key).ok().filter(|c| !c.trim().is_empty())?;

        // Read the defining file's fresh bytes for didOpen (no locks held).
        // Files checked out by git are adopted on first touch (the same
        // convention as the fs.read tool).
        let _ = ws.adopt(&first_def.path);
        let bytes = match ws.read(&first_def.path).map(|(b, _)| b) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("lsp_enrich: read failed: {e}");
                return None;
            }
        };

        let mut sessions = self.lsp_sessions.lock().expect("lsp sessions");
        if !sessions.contains_key(lang) {
            let spawned = modbit_diagnostics::lsp::LspSession::spawn(&command, ws.root())
                .map(|mut s| {
                    s.request_timeout = Duration::from_secs(10);
                    s
                })
                .map_err(|e| e.to_string());
            sessions.insert(lang.to_string(), spawned.ok());
        }
        // A fresh error is cached as None (spawn retried never).
        let slot = match sessions.get_mut(lang) {
            Some(Some(session)) => session,
            _ => {
                eprintln!("lsp_enrich: session unavailable for {lang}");
                return Some(serde_json::json!({
                    "unavailable": format!(
                        "no working {env_key} server (command: {command:?}); the tree-sitter surface above is authoritative"
                    ),
                }));
            }
        };
        let session = slot;
        if let Err(e) = session.ensure_open(&first_def.path, &bytes) {
            return Some(serde_json::json!({ "unavailable": e.to_string() }));
        }
        let text = String::from_utf8_lossy(&bytes);
        let col = text
            .lines()
            .nth(first_def.line.saturating_sub(1))
            .and_then(|l| l.find(name).map(|b| l[..b].chars().count()))
            .unwrap_or(0);
        let definitions = session
            .definition(&first_def.path, first_def.line.saturating_sub(1), col)
            .map_err(|e| {
                eprintln!("lsp_enrich: definition failed: {e}");
                e.to_string()
            });
        let references = session
            .references(&first_def.path, first_def.line.saturating_sub(1), col, false)
            .map_err(|e| {
                eprintln!("lsp_enrich: references failed: {e}");
                e.to_string()
            });
        Some(match (definitions, references) {
            (Ok(d), Ok(r)) => serde_json::json!({
                "server": command,
                "definitions": d,
                "references": r,
            }),
            (Err(e), _) | (_, Err(e)) => serde_json::json!({ "unavailable": e }),
        })
    }

    /// M3.1 direct index modes for context.query: exact term, regex, and
    /// path queries over the indexed corpus (fused stays the default).
    fn query_exact(&self, ws: &WorkspaceFileService, term: &str, limit: usize) -> serde_json::Value {
        self.refresh_from_journal(ws, "exact_query");
        let index = self.index.lock().expect("task index mutex");
        let hits = index.query_exact(term, limit);
        serde_json::json!({
            "mode": "exact",
            "hits": hits.iter().map(|h| serde_json::json!({
                "path": h.path, "line": h.line_no, "snippet": h.snippet,
            })).collect::<Vec<_>>(),
        })
    }

    fn query_regex(
        &self,
        ws: &WorkspaceFileService,
        pattern: &str,
        limit: usize,
    ) -> Result<serde_json::Value, String> {
        self.refresh_from_journal(ws, "regex_query");
        let index = self.index.lock().expect("task index mutex");
        let hits = index.query_regex(pattern, limit).map_err(|e| e.to_string())?;
        Ok(serde_json::json!({
            "mode": "regex",
            "hits": hits.iter().map(|h| serde_json::json!({
                "path": h.path, "line": h.line_no, "snippet": h.snippet,
            })).collect::<Vec<_>>(),
        }))
    }

    /// M3.6 impact mode: the import-impact set of a corpus path
    /// (transitive dependents, bounded BFS, test files flagged).
    fn query_impact(&self, ws: &WorkspaceFileService, path: &str, limit: usize) -> serde_json::Value {
        self.refresh_from_journal(ws, "impact_query");
        let index = self.index.lock().expect("task index mutex");
        let limit = limit.clamp(1, 200);
        let dependents: Vec<serde_json::Value> = index
            .impact(path, 3)
            .into_iter()
            .take(limit)
            .map(|(p, hops)| {
                serde_json::json!({
                    "path": p,
                    "hops": hops,
                    "test": p.contains("/tests/")
                        || p.ends_with("_test.rs")
                        || p.ends_with(".test.ts")
                        || p.ends_with(".test.tsx")
                        || p.ends_with(".test.js")
                        || p.starts_with("test_"),
                })
            })
            .collect();
        serde_json::json!({
            "mode": "impact",
            "path": path,
            "dependents": dependents,
            "note": "import graph over the indexed corpus (use/import edges); uncommitted-task edits ride the change journal",
        })
    }

    fn query_paths(&self, ws: &WorkspaceFileService, needle: &str, limit: usize) -> serde_json::Value {
        self.refresh_from_journal(ws, "path_query");
        let index = self.index.lock().expect("task index mutex");
        let mut paths = index.repo.path(needle);
        paths.truncate(limit.clamp(1, 200));
        serde_json::json!({ "mode": "path", "hits": paths })
    }

    /// M3.7: AUTO mode — the retrieval planner (modbit-context) classifies
    /// the query from REAL index signals and routes to the MINIMUM
    /// sufficient level; escalation happens only when lower levels cannot
    /// serve the query (REQ ledger EV-0001 (owner context-engine), docs/18 § Retrieval planner). The
    /// plan rides the response as provenance of the routing decision.
    /// L3 (engineering) currently escalates to the fused query plus a
    /// note — its full evidence graph (Git/diagnostics/runtime) is M3.6.
    fn query_auto(&self, ws: &WorkspaceFileService, query: &str, limit: usize) -> serde_json::Value {
        use modbit_context::planner::{plan, QuerySignals, RetrievalLevel};

        self.refresh_from_journal(ws, "auto_query");
        let trimmed = query.trim();
        let terms: Vec<&str> = trimmed
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .filter(|t| !t.is_empty())
            .collect();
        let lower = trimmed.to_lowercase();
        let structural = terms.len() == 1
            && matches! {
                lower.rsplit('_').next(),
                Some("fn") | Some("struct") | Some("trait") | Some("impl") | Some("enum")
            } || ["fn ", "struct ", "trait ", "impl ", "definition", "definition of", "references of"]
                .iter()
                .any(|k| lower.starts_with(k));
        let engineering = ["how ", "why ", "flow", "architecture", "impact", "owns "]
            .iter()
            .any(|k| lower.starts_with(k) || lower.contains(k));

        // Real index signals (bounded probes on the live corpus).
        let (exact_hit, exact_recall_insufficient, symbol_defined) = {
            let index = self.index.lock().expect("task index mutex");
            let exact_hits = index.query_exact(trimmed, 4).len();
            let defined = !index.symbol_definitions(trimmed).is_empty();
            (
                exact_hits > 0,
                exact_hits > 0 && exact_hits < 2 && terms.len() == 1 && !defined,
                defined,
            )
        };

        let signals = QuerySignals {
            exact_hit: exact_hit || symbol_defined,
            multi_term: terms.len() > 1,
            structural,
            engineering,
            exact_recall_insufficient,
        };
        let plan = plan(&signals);
        let level = format!("{:?}", plan.level).to_lowercase();

        let mut result = match plan.level {
            RetrievalLevel::Exact => {
                let mut r = self.query_exact(ws, trimmed, limit);
                r["mode"] = serde_json::json!("auto:exact");
                r
            }
            RetrievalLevel::Structural if symbol_defined && terms.len() == 1 => {
                let mut r = self.query_symbol(ws, trimmed);
                r["mode"] = serde_json::json!("auto:structural");
                r
            }
            RetrievalLevel::Structural => {
                let mut r = self.query_context(ws, trimmed, limit);
                r["mode"] = serde_json::json!("auto:structural:fused");
                r
            }
            RetrievalLevel::Engineering => {
                let mut r = self.query_context(ws, trimmed, limit * 2);
                // M3.6: the L3 answer carries the import-impact set of the
                // top hit — what a change to it would touch.
                if let Some(top) = r
                    .get("hits")
                    .and_then(|h| h.as_array())
                    .and_then(|h| h.first())
                    .and_then(|h| h.get("path"))
                    .and_then(|p| p.as_str())
                    .map(str::to_string)
                {
                    let index = self.index.lock().expect("task index mutex");
                    let impact: Vec<serde_json::Value> = index
                        .impact(&top, 2)
                        .into_iter()
                        .take(10)
                        .map(|(p, hops)| {
                            serde_json::json!({ "path": p, "hops": hops })
                        })
                        .collect();
                    r["impact_of_top_hit"] = serde_json::json!({ "path": top, "dependents": impact });
                }
                r["mode"] = serde_json::json!("auto:engineering:fused");
                r["note"] = serde_json::json!(
                    "engineering level: fused context + import-impact of the top hit; pull-based diagnostics-in-context and runtime evidence remain future work"
                );
                r
            }
            RetrievalLevel::Hybrid => {
                let mut r = self.query_context(ws, trimmed, limit);
                r["mode"] = serde_json::json!("auto:hybrid");
                r
            }
        };
        result["plan"] = serde_json::json!({
            "level": level,
            "rationale": plan.rationale,
            "signals": {
                "exact_hit": signals.exact_hit,
                "multi_term": signals.multi_term,
                "structural": signals.structural,
                "engineering": signals.engineering,
                "exact_recall_insufficient": signals.exact_recall_insufficient,
            },
        });
        result
    }
}

impl RunPlaneOutputSink {
    /// Mirrors EventStoreObserver::append's envelope + sequence rules.
    fn append_chunk(&self, run_id: &RunId, payload: DomainEvent) {
        let aggregate_id = run_id.to_string();
        let mut envelope = EventEnvelope {
            event_id: uuid::Uuid::now_v7().to_string(),
            session_id: self.session_id,
            task_id: None,
            run_id: Some(*run_id),
            turn_id: None,
            step_id: None,
            aggregate_type: AggregateType::Run,
            aggregate_id: aggregate_id.clone(),
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
        let mut sequences = self.shared.sequences.lock().expect("observer mutex");
        let next = sequences.get(&aggregate_id).copied().unwrap_or(1);
        envelope.sequence = next;
        envelope.seal();
        sequences.insert(aggregate_id, next + 1);
        drop(sequences);
        // M4.4: streamed chunks ride the same lease-fenced run plane.
        if let Err(e) = self.shared.append_fenced(&self.store, &mut envelope) {
            eprintln!("modbit scheduler: append output chunk failed: {e}");
        }
    }
}

/// Bounded preview length for streamed chunk events (matches the runtime
/// store's bounded-preview philosophy, docs/31).
const CHUNK_PREVIEW_CHARS: usize = 256;

impl ToolOutputSink for RunPlaneOutputSink {
    fn chunk(&self, broker_run_id: &str, offset: u64, bytes: &[u8]) {
        let run_id = match *self.shared.run.lock().expect("run cell") {
            Some(id) => id,
            None => return,
        };
        let preview: String = String::from_utf8_lossy(bytes)
            .chars()
            .take(CHUNK_PREVIEW_CHARS)
            .collect();
        self.append_chunk(
            &run_id,
            DomainEvent::ToolOutputChunk {
                broker_run_id: broker_run_id.to_string(),
                offset,
                byte_length: bytes.len() as u64,
                preview,
            },
        );
    }

    fn store_full(&self, broker_run_id: &str, bytes: &[u8]) -> Option<StoredOutput> {
        let output_ref_id = format!("outref-{broker_run_id}");
        let stored = self
            .store
            .runtime()
            .write_output_ref(&output_ref_id, "text/plain", bytes)
            .ok()?;
        Some(StoredOutput {
            output_ref_id: stored.output_ref_id,
            byte_length: stored.byte_length,
            preview: stored.preview_text,
        })
    }
}

/// Streams a broker run to completion (Phase 2.6): drains output while
/// the process runs — each drain emits a chunk event through the sink —
/// then a final drain collects the tail. Cancellation and timeout
/// semantics match run_capture_cancellable (kill, no orphan process).
fn run_streaming_capture(
    execd: &ExecdClient,
    run_id: &str,
    argv: &[String],
    cwd: &std::path::Path,
    cancel: &std::sync::atomic::AtomicBool,
    sink: Option<&dyn ToolOutputSink>,
) -> Result<(modbit_terminal::client::SpawnStatus, Vec<u8>), modbit_terminal::TerminalError> {
    use std::sync::atomic::Ordering;
    const TIMEOUT: Duration = Duration::from_secs(600);
    const DRAIN_MAX: usize = 256 * 1024;
    execd.spawn(run_id, argv, Some(cwd))?;
    let mut offset: u64 = 0;
    let mut collected: Vec<u8> = Vec::new();
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if cancel.load(Ordering::SeqCst) {
            execd.stop(run_id)?;
            return Err(modbit_terminal::TerminalError::Cancelled(run_id.to_string()));
        }
        let (bytes, new_offset) = execd.read_output(run_id, offset, DRAIN_MAX)?;
        if !bytes.is_empty() {
            if let Some(sink) = sink {
                sink.chunk(run_id, offset, &bytes);
            }
            collected.extend_from_slice(&bytes);
            offset = new_offset;
        }
        let status = execd.status(run_id)?;
        if status.state != modbit_terminal::RunState::Running {
            // Final drain after exit: consume to EOF (one bounded read is
            // not enough — bytes written between the last poll and
            // termination can exceed one chunk; losing them would
            // truncate the artifact).
            loop {
                let (bytes, new_offset) = execd.read_output(run_id, offset, DRAIN_MAX)?;
                if bytes.is_empty() {
                    break;
                }
                if let Some(sink) = sink {
                    sink.chunk(run_id, offset, &bytes);
                }
                collected.extend_from_slice(&bytes);
                offset = new_offset;
            }
            return Ok((status, collected));
        }
        if Instant::now() >= deadline {
            execd.stop(run_id)?;
            return Err(modbit_terminal::TerminalError::Timeout(run_id.to_string()));
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Shell word splitting for the string form of `shell.run` argv (Phase
/// 2.6): whitespace-separated words with single/double-quoted segments
/// honored (a quote makes whitespace literal until the matching close).
/// No expansions, no escapes beyond the quotes themselves — the argv is
/// still passed to exec WITHOUT a shell.
fn split_shell_words(command: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut started = false;
    for ch in command.chars() {
        match ch {
            '\'' if !in_double => {
                in_single = !in_single;
                started = true;
            }
            '"' if !in_single => {
                in_double = !in_double;
                started = true;
            }
            c if c.is_whitespace() && !in_single && !in_double => {
                if started {
                    words.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            c => {
                current.push(c);
                started = true;
            }
        }
    }
    if in_single || in_double {
        return Err("unterminated quote in argv string".into());
    }
    if started {
        words.push(current);
    }
    Ok(words)
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
    /// The run's workspace — the turn-boundary index refresh drains its
    /// change journal (IMP-EV-0004).
    ws: Arc<WorkspaceFileService>,
    /// The task's repository index writer; None when the initial walk
    /// failed (the run proceeds unindexed rather than not at all).
    task_index: Option<Arc<TaskIndexWriter>>,
    /// Sequence accounting + current run, SHARED with the output sink
    /// (both append to the same run aggregate).
    shared: Arc<RunPlaneShared>,
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
        let mut sequences = self.shared.sequences.lock().expect("observer mutex");
        let next = sequences.get(aggregate_id).copied().unwrap_or(1);
        envelope.sequence = next;
        envelope.seal();
        sequences.insert(aggregate_id.to_string(), next + 1);
        drop(sequences);
        // M4.4: the run plane writes under the session lease — a fenced-out
        // core gets a typed StaleLease, flips the fence and cancels its run.
        if let Err(e) = self.shared.append_fenced(&self.store, &mut envelope) {
            eprintln!("modbit scheduler: append run event failed: {e}");
        }
    }
}

impl EventStoreObserver {
    fn run_of(&self) -> Option<RunId> {
        *self.shared.run.lock().expect("observer mutex")
    }
}

impl RunObserver for EventStoreObserver {
    fn run_started(&self, run_id: &str, attempt: u32) {
        let Ok(parsed) = RunId::parse(run_id) else { return };
        *self.shared.run.lock().expect("observer mutex") = Some(parsed);
        self.append(
            AggregateType::Run,
            run_id,
            DomainEvent::RunStarted {
                task_id: self.task_id,
                attempt,
            },
        );
        // Events produced at task start (before the run id existed — e.g.
        // the repository-index build, IMP-EV-0004) land on the run
        // aggregate right after RunStarted, preserving order.
        for payload in self.shared.take_pending_run_events() {
            self.shared
                .append_run_event(&self.store, self.session_id, parsed, payload);
        }
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
        // IMP-EV-0004: reconcile the repository index at the turn boundary.
        // The journal drain covers every workspace-mediated write of the
        // turn (change engine edits); shell-spawned writes carry no journal
        // events and are reconciled by the query path before any indexed
        // answer (M3.2).
        if let Some(index) = self.task_index.as_ref() {
            index.refresh_from_journal(&self.ws, "turn_boundary");
        }
        self.append(AggregateType::Turn, turn_id, DomainEvent::TurnCompleted);
    }

    fn conversation_checkpointed(
        &self,
        run_id: &str,
        turn_ordinal: u32,
        conversation_json: &str,
    ) {
        self.append(
            AggregateType::Run,
            run_id,
            DomainEvent::ConversationCheckpointed {
                turn_ordinal,
                conversation_json: conversation_json.to_string(),
            },
        );
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
mod shell_words_tests {
    use super::split_shell_words;

    /// Phase 2.6: the string form honors quotes; the array form is exact.
    #[test]
    fn shell_words_splitting_honors_quotes() {
        assert_eq!(
            split_shell_words("sh run_tests.sh").unwrap(),
            vec!["sh".to_string(), "run_tests.sh".to_string()]
        );
        assert_eq!(
            split_shell_words("echo \"hello world\" tail").unwrap(),
            vec!["echo".to_string(), "hello world".to_string(), "tail".to_string()]
        );
        assert_eq!(
            split_shell_words("echo 'a  b' \"c d\"").unwrap(),
            vec!["echo".to_string(), "a  b".to_string(), "c d".to_string()]
        );
        assert_eq!(split_shell_words("  ").unwrap(), Vec::<String>::new());
        assert!(split_shell_words("echo \"unterminated").is_err());
        assert!(split_shell_words("echo 'unterminated").is_err());
    }
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
