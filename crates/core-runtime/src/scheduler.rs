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
use modbit_tools::schema::{ParamSpec, ParamType, ToolSchema};
use modbit_tools::ToolRegistry;
use modbit_workspace::WorkspaceFileService;

use crate::one_agent::{AgentTask, OneAgentRuntime, RunObserver};

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
    /// Repository whose worktrees isolate task runs (E2E-001).
    pub repo_root: Option<PathBuf>,
    /// Where task worktrees are allocated (defaults to
    /// `<repo_root>/../.modbit/worktrees`).
    pub worktree_root: Option<PathBuf>,
    pub request_timeout: Duration,
    pub max_turns: u32,
}

impl SchedulerConfig {
    pub fn from_env() -> Self {
        let provider = match std::env::var("MODBIT_PROVIDER").as_deref() {
            Ok("anthropic") => Provider::Anthropic,
            _ => Provider::OpenAi,
        };
        let repo_root = std::env::var("MODBIT_REPO_ROOT")
            .ok()
            .map(PathBuf::from)
            .or_else(|| {
                let cwd = std::env::current_dir().ok()?;
                (cwd.join(".git").exists()).then_some(cwd)
            });
        SchedulerConfig {
            provider,
            model: std::env::var("MODBIT_MODEL").unwrap_or_else(|_| "gpt-4o-mini".into()),
            base_url: std::env::var("MODBIT_BASE_URL").ok().filter(|s| !s.is_empty()),
            broker: Arc::new(modbit_providers::transport::EnvSecretBroker),
            repo_root,
            worktree_root: None,
            request_timeout: Duration::from_secs(180),
            max_turns: 8,
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
}

impl Scheduler {
    /// Starts the poller thread that tails the store for `task_started`.
    pub fn spawn(store: Arc<EventStore>, config: SchedulerConfig) -> Arc<Self> {
        let scheduler = Arc::new(Self {
            store,
            config,
            in_flight: std::sync::Mutex::new(std::collections::HashSet::new()),
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

        // 1. Worktree + revision allocation (E2E-001).
        let repo_root = self.config.repo_root.clone().ok_or_else(|| {
            "no repository configured for runs (set MODBIT_REPO_ROOT)".to_string()
        })?;
        let repo = GitRepo::open(&repo_root).map_err(|e| format!("open repo: {e}"))?;
        let base_revision = repo.head().map_err(|e| format!("read HEAD: {e}"))?;
        let worktree_root = self
            .config
            .worktree_root
            .clone()
            .unwrap_or_else(|| default_worktree_root(&repo_root));
        let worktree_path = worktree_root.join(task_id.to_string());
        // worktree_add creates the task branch (-b) at the current HEAD.
        let branch = format!("modbit/{}", &task_id.to_string()[..12.min(task_id.to_string().len())]);
        repo.worktree_add(&worktree_path, &branch)
            .map_err(|e| format!("allocate worktree: {e}"))?;

        // 2. Context pack through the canonical file service on the worktree.
        let ws = Arc::new(
            WorkspaceFileService::open(&worktree_path).map_err(|e| format!("open workspace: {e}"))?,
        );
        let context_pack = build_context_pack(&ws, &title, &prompt);

        // 3. Task-scoped tools bound to the worktree.
        let registry = build_worktree_registry(&ws, &worktree_path);
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
        let transport = LiveGatewayTransport::new(&self.config);
        let runtime = OneAgentRuntime {
            transport: &transport,
            registry: &registry,
            kernel: &kernel,
            grants: &grants,
            max_turns: self.config.max_turns,
            observer: Some(&observer),
        };
        let task = AgentTask {
            task_id: task_id.to_string(),
            objective: format!("{title}\n\n{prompt}"),
            model: self.config.model.clone(),
            provider: format!("{:?}", self.config.provider).to_lowercase(),
            system_policy: system_policy(&worktree_path, &base_revision),
            workspace_rules: String::new(),
            context_pack,
        };
        let processor = CommandProcessor::new(self.store.clone());
        let result = runtime.run(&task);

        // 6. Transition the task from REAL outcomes (REQ-EV-0119: the host
        // decides; a model claim is never sufficient).
        match result {
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
            Err(err) => execute(
                &processor,
                task_id,
                CommandPayload::TaskWaiting {
                    task_id,
                    reason: modbit_domain::events::WaitingReason::Provider,
                },
            )
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

/// Task-scoped tools bound to the worktree. Paths go through the canonical
/// safe-path file service; shell commands run with the worktree as cwd.
fn build_worktree_registry(ws: &Arc<WorkspaceFileService>, worktree: &std::path::Path) -> ToolRegistry {
    let registry = ToolRegistry::new();

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
            "Run a command in the task worktree (structured argv; output captured)",
            Some(ToolSchema { aliases: Default::default(), parameters: shell_params }),
            {
                let worktree = worktree.to_path_buf();
                Arc::new(move |args| {
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
                    // Routing through modbit-execd lands in Phase 1 item 4;
                    // the boundary contract (cwd pinned to the worktree,
                    // captured output) is identical.
                    let out = std::process::Command::new(&argv[0])
                        .args(&argv[1..])
                        .current_dir(&worktree)
                        .output()
                        .map_err(|e| e.to_string())?;
                    Ok(serde_json::json!({
                        "exit_code": out.status.code(),
                        "stdout": tail(&String::from_utf8_lossy(&out.stdout), 8_000),
                        "stderr": tail(&String::from_utf8_lossy(&out.stderr), 8_000),
                    }))
                })
            },
        )
        .expect("register shell.run");

    registry
}

/// Host-issued capability grants for the worktool toolset.
fn worktree_grants() -> Vec<CapabilityGrant> {
    vec![
        CapabilityGrant { grant_id: "g-fs-read".into(), tool: "fs.read".into(), effect_class: EffectClass::ReadOnly },
        CapabilityGrant { grant_id: "g-fs-list".into(), tool: "fs.list".into(), effect_class: EffectClass::ReadOnly },
        CapabilityGrant { grant_id: "g-shell-run".into(), tool: "shell.run".into(), effect_class: EffectClass::External },
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
struct LiveGatewayTransport<'a> {
    config: &'a SchedulerConfig,
    runtime: tokio::runtime::Runtime,
    transport: HttpStreamTransport,
}

impl<'a> LiveGatewayTransport<'a> {
    fn new(config: &'a SchedulerConfig) -> Self {
        let transport = HttpStreamTransport::new(config.broker.clone())
            .expect("build provider transport");
        LiveGatewayTransport {
            config,
            runtime: tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("scheduler tokio runtime"),
            transport,
        }
    }

    fn endpoint(&self) -> String {
        let base = self
            .config
            .base_url
            .clone()
            .unwrap_or_else(|| match self.config.provider {
                Provider::OpenAi => "https://api.openai.com/v1".into(),
                Provider::Anthropic => "https://api.anthropic.com".into(),
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
                match stream.recv().await {
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
