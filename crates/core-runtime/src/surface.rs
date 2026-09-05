//! SurfaceProtocol request dispatch (M1.4): ties the authenticated transport
//! to the idempotent command processor and the docs/31 projections.
//!
//! Fleet reads are projection reads (derived state); mutations go through
//! `CommandProcessor` so command idempotency and the state machine hold.

use std::sync::Arc;

use prost::Message;
use rusqlite::OptionalExtension;

use modbit_domain::commands::CommandPayload;
use modbit_domain::{Actor, ActorType, Command, SessionId, TaskId};
use modbit_event_store::{CommandProcessor, EventStore};
use modbit_protocol::modbit::protocol::v1 as pb;

/// Core services shared by the SurfaceProtocol dispatch loop.
#[derive(Clone)]
pub struct CoreServices {
    store: Arc<EventStore>,
    processor: CommandProcessor,
    workspace: Option<std::sync::Arc<modbit_workspace::WorkspaceFileService>>,
    /// Task-worktree layout for GetDiff (explicit; no process-env reads
    /// inside dispatch). Set by the host binary at construction.
    task_worktrees: Option<std::sync::Arc<dyn crate::scheduler::WorktreeSource>>,
    /// Live run-control signals (Phase 2.3): Stop/Pause/Steer reach the
    /// in-flight run through the scheduler's registry.
    run_controls: Option<std::sync::Arc<crate::scheduler::RunControls>>,
}

#[derive(Default, Clone)]
struct RunSummary {
    state: String,
    failure_code: String,
}

fn actor() -> Actor {
    Actor {
        actor_type: ActorType::User,
        actor_id: "desktop-main".into(),
    }
}

impl CoreServices {
    pub fn new(store: Arc<EventStore>) -> Self {
        Self {
            processor: CommandProcessor::new(store.clone()),
            store,
            workspace: None,
            task_worktrees: None,
            run_controls: None,
        }
    }

    /// Attaches the scheduler's live run-control registry (Phase 2.3):
    /// StopTask/PauseTask/SteerTask signal in-flight runs in addition to
    /// writing the durable lifecycle events.
    pub fn with_run_controls(
        mut self,
        controls: std::sync::Arc<crate::scheduler::RunControls>,
    ) -> Self {
        self.run_controls = Some(controls);
        self
    }

    /// Attaches the task-worktree layout source (GetDiff): the shared
    /// repo/worktree roots the scheduler allocates from.
    pub fn with_task_worktrees(
        mut self,
        source: std::sync::Arc<dyn crate::scheduler::WorktreeSource>,
    ) -> Self {
        self.task_worktrees = Some(source);
        self
    }

    /// Attaches the canonical workspace for the Trusted Code Surface.
    pub fn with_workspace(
        mut self,
        workspace: std::sync::Arc<modbit_workspace::WorkspaceFileService>,
    ) -> Self {
        self.workspace = Some(workspace);
        self
    }

    pub fn store(&self) -> &EventStore {
        &self.store
    }

    /// Decodes a `SurfaceRequest` frame and produces the encoded
    /// `SurfaceResponse`. Never fails: protocol/mapping errors become
    /// `ok=false` responses, keeping the dispatch loop alive.
    pub fn handle(&self, request_bytes: &[u8]) -> Vec<u8> {
        let response = match pb::SurfaceRequest::decode(request_bytes) {
            Ok(request) => self.dispatch(request),
            Err(e) => pb::SurfaceResponse {
                ok: false,
                error: format!("bad SurfaceRequest: {e}"),
                ..Default::default()
            },
        };
        response.encode_to_vec()
    }

    fn dispatch(&self, request: pb::SurfaceRequest) -> pb::SurfaceResponse {
        match request.request {
            Some(pb::surface_request::Request::GetTaskEvents(get)) => {
                match self.task_events(&get.task_id) {
                    Ok(events) => pb::SurfaceResponse {
                        ok: true,
                        error: String::new(),
                        fleet: Default::default(),
                        task: Default::default(),
                        session_id: String::new(),
                        task_events: Some(pb::TaskEvents {
                            task_id: get.task_id,
                            events,
                        }),
                        code_view: Default::default(),
                        ..Default::default()
                    },
                    Err(e) => pb::SurfaceResponse {
                        ok: false,
                        error: e,
                        ..Default::default()
                    },
                }
            }
            Some(pb::surface_request::Request::GetCodeView(get)) => {
                match self.code_view(&get.path) {
                    Ok(view) => pb::SurfaceResponse {
                        ok: true,
                        error: String::new(),
                        fleet: Default::default(),
                        task: Default::default(),
                        session_id: String::new(),
                        task_events: Default::default(),
                        code_view: Some(view),
                        ..Default::default()
                    },
                    Err(e) => pb::SurfaceResponse {
                        ok: false,
                        error: e,
                        ..Default::default()
                    },
                }
            }
            Some(pb::surface_request::Request::GetFleet(_)) => match self.fleet() {
                Ok(fleet) => pb::SurfaceResponse {
                    ok: true,
                    fleet: Some(fleet),
                    ..Default::default()
                },
                Err(e) => pb::SurfaceResponse {
                    ok: false,
                    error: e,
                    ..Default::default()
                },
            },
            Some(pb::surface_request::Request::QueueTask(queue)) => self.lifecycle_response(
                &queue.task_id,
                CommandPayload::QueueTask {
                    task_id: parse_task_id(&queue.task_id),
                },
            ),
            Some(pb::surface_request::Request::StartTask(start)) => self.lifecycle_response(
                &start.task_id,
                CommandPayload::StartTask {
                    task_id: parse_task_id(&start.task_id),
                },
            ),
            Some(pb::surface_request::Request::TaskReadyForReview(review)) => self
                .lifecycle_response(
                    &review.task_id,
                    CommandPayload::TaskReadyForReview {
                        task_id: parse_task_id(&review.task_id),
                    },
                ),
            Some(pb::surface_request::Request::CompleteTask(complete)) => self.lifecycle_response(
                &complete.task_id,
                CommandPayload::CompleteTask {
                    task_id: parse_task_id(&complete.task_id),
                    summary: complete.summary,
                    // Surface completions are client claims, never
                    // host-verified (REQ-EV-0119).
                    host_verified: false,
                },
            ),
            Some(pb::surface_request::Request::CreateSession(create)) => {
                let outcome = self.execute(Command {
                    command_id: new_command_id(),
                    actor: actor(),
                    payload: CommandPayload::CreateSession {
                        display_name: create.display_name,
                    },
                });
                match outcome {
                    Ok(Some(aggregate_id)) => pb::SurfaceResponse {
                        ok: true,
                        session_id: aggregate_id,
                        ..Default::default()
                    },
                    Ok(None) => pb::SurfaceResponse {
                        ok: false,
                        error: "session creation produced no event".into(),
                        ..Default::default()
                    },
                    Err(e) => pb::SurfaceResponse {
                        ok: false,
                        error: e,
                        ..Default::default()
                    },
                }
            }
            Some(pb::surface_request::Request::CreateTask(create)) => {
                // docs/32 § Task composer: CreateSession if needed, then
                // CreateTask. An empty session id resolves the default session.
                let session_id = if create.session_id.is_empty() {
                    match self.ensure_default_session() {
                        Ok(id) => id,
                        Err(e) => {
                            return pb::SurfaceResponse {
                                ok: false,
                                error: e,
                                ..Default::default()
                            }
                        }
                    }
                } else {
                    create.session_id
                };
                let outcome = self.execute(Command {
                    command_id: new_command_id(),
                    actor: actor(),
                    payload: CommandPayload::CreateTask {
                        session_id: match SessionId::parse(&session_id) {
                            Ok(id) => id,
                            Err(e) => {
                                return pb::SurfaceResponse {
                                    ok: false,
                                    error: format!("bad session id: {e}"),
                                    ..Default::default()
                                }
                            }
                        },
                        title: create.title,
                        prompt: create.prompt,
                    },
                });
                match outcome {
                    // The processor minted the task id; the created event's
                    // aggregate id IS the authoritative task id.
                    Ok(Some(aggregate_id)) => {
                        let task = self.task_view(&aggregate_id);
                        pb::SurfaceResponse {
                            ok: true,
                            task,
                            ..Default::default()
                        }
                    }
                    Ok(None) => pb::SurfaceResponse {
                        ok: false,
                        error: "task creation produced no event".into(),
                        ..Default::default()
                    },
                    Err(e) => pb::SurfaceResponse {
                        ok: false,
                        error: e,
                        ..Default::default()
                    },
                }
            }
            Some(pb::surface_request::Request::SteerTask(steer)) => {
                // Phase 2.3: queue the note for the in-flight run (injected
                // as a user message on the next turn), then record the
                // durable steer event.
                if let Some(controls) = &self.run_controls {
                    controls.steer(&steer.task_id, steer.note.clone());
                }
                self.lifecycle_response(
                    &steer.task_id,
                    CommandPayload::SteerTask {
                        task_id: parse_task_id(&steer.task_id),
                        steer_note: steer.note,
                    },
                )
            }
            Some(pb::surface_request::Request::PauseTask(pause)) => {
                // Phase 2.3: signal the in-flight run to park at the next
                // turn boundary BEFORE parking the durable state.
                if let Some(controls) = &self.run_controls {
                    controls.pause(&pause.task_id);
                }
                self.lifecycle_response(
                    &pause.task_id,
                    CommandPayload::TaskWaiting {
                        task_id: parse_task_id(&pause.task_id),
                        reason: modbit_domain::events::WaitingReason::UserInput,
                    },
                )
            }
            Some(pb::surface_request::Request::StopTask(stop)) => {
                // Phase 2.3: signal the in-flight run FIRST (abort the
                // model stream, kill the broker tool), then record the
                // durable cancellation.
                if let Some(controls) = &self.run_controls {
                    controls.cancel(&stop.task_id);
                }
                self.lifecycle_response(
                    &stop.task_id,
                    CommandPayload::CancelTask {
                        task_id: parse_task_id(&stop.task_id),
                        reason: if stop.reason.is_empty() {
                            "stopped by user".into()
                        } else {
                            stop.reason
                        },
                    },
                )
            },
            Some(pb::surface_request::Request::GetRunDetail(get)) => match self.run_detail(&get.task_id) {
                Ok(run_detail) => pb::SurfaceResponse {
                    ok: true,
                    run_detail: Some(run_detail),
                    ..Default::default()
                },
                Err(e) => pb::SurfaceResponse {
                    ok: false,
                    error: e,
                    ..Default::default()
                },
            },
            Some(pb::surface_request::Request::GetDiff(get)) => match self.diff(&get.task_id) {
                Ok(diff) => pb::SurfaceResponse {
                    ok: true,
                    diff: Some(diff),
                    ..Default::default()
                },
                Err(e) => pb::SurfaceResponse {
                    ok: false,
                    error: e,
                    ..Default::default()
                },
            },
            // Phase 2.6: paginated read of a stored tool-output reference.
            Some(pb::surface_request::Request::ReadOutputRef(read)) => {
                match self.read_output_ref(&read.output_ref_id, read.offset, read.max_bytes) {
                    Ok(view) => pb::SurfaceResponse {
                        ok: true,
                        output_chunk: Some(view),
                        ..Default::default()
                    },
                    Err(e) => pb::SurfaceResponse {
                        ok: false,
                        error: e,
                        ..Default::default()
                    },
                }
            }
            None => pb::SurfaceResponse {
                ok: false,
                error: "empty SurfaceRequest".into(),
                ..Default::default()
            },
        }
    }

    fn lifecycle_response(&self, task_id: &str, payload: CommandPayload) -> pb::SurfaceResponse {
        match self.execute(Command {
            command_id: new_command_id(),
            actor: actor(),
            payload,
        }) {
            Ok(_) => {
                let task = self.task_view(task_id);
                pb::SurfaceResponse {
                    ok: true,
                    task,
                    ..Default::default()
                }
            }
            Err(e) => pb::SurfaceResponse {
                ok: false,
                error: e,
                ..Default::default()
            },
        }
    }

    fn execute(&self, command: Command) -> Result<Option<String>, String> {
        match self.processor.execute(command) {
            Ok(modbit_event_store::Outcome::Applied { event_ids }) => {
                let first = event_ids.first().cloned();
                Ok(match first {
                    Some(event_id) => {
                        // The created aggregate id equals the creation event's
                        // aggregate; resolve it from the event stream.
                        self.aggregate_of_event(&event_id)?
                    }
                    None => None,
                })
            }
            Ok(modbit_event_store::Outcome::Replayed { event_ids }) => {
                let first = event_ids.first().cloned();
                Ok(match first {
                    Some(event_id) => self.aggregate_of_event(&event_id)?,
                    None => None,
                })
            }
            Ok(modbit_event_store::Outcome::Rejected { reason }) => Err(reason),
            Err(e) => Err(e.to_string()),
        }
    }

    fn aggregate_of_event(&self, event_id: &str) -> Result<Option<String>, String> {
        self.store
            .with_conn(|conn| {
                conn.query_row(
                    "SELECT aggregate_id FROM events WHERE event_id = ?1",
                    [event_id],
                    |r| r.get::<_, Option<String>>(0),
                )
            })
            .map_err(|e| e.to_string())
    }

    /// First committed session, if any. `get_fleet` never creates state.
    fn first_session(&self) -> Result<Option<String>, String> {
        self.store
            .with_conn(|conn| {
                conn.query_row(
                    "SELECT aggregate_id FROM events WHERE aggregate_type = 'session'
                     ORDER BY sequence LIMIT 1",
                    [],
                    |r| r.get::<_, Option<String>>(0),
                )
                .optional()
            })
            .map_err(|e| e.to_string())
            .map(|row| row.flatten())
    }

    fn ensure_default_session(&self) -> Result<String, String> {
        if let Some(id) = self.first_session()? {
            return Ok(id);
        }
        match self.execute(Command {
            command_id: new_command_id(),
            actor: actor(),
            payload: CommandPayload::CreateSession {
                display_name: "Default".into(),
            },
        })? {
            Some(id) => Ok(id),
            None => Err("default session creation produced no event".into()),
        }
    }

    /// Context Inspector data: the task's durable event stream (docs/32
    /// timeline; events are committed facts, never fabricated).
    fn task_events(&self, task_id: &str) -> Result<Vec<pb::EventEnvelope>, String> {
        let exists: bool = self
            .store
            .with_conn(|conn| {
                conn.query_row(
                    "SELECT COUNT(*) FROM events WHERE aggregate_id = ?1",
                    [task_id],
                    |r| r.get::<_, i64>(0),
                )
                .map(|n| n > 0)
            })
            .map_err(|e| e.to_string())?;
        if !exists {
            return Err(format!("task {task_id} does not exist"));
        }
        let envelopes = self.store.load(task_id).map_err(|e| e.to_string())?;
        Ok(envelopes
            .iter()
            .map(|e| pb::EventEnvelope {
                event_id: e.event_id.clone(),
                tenant_id: String::new(),
                aggregate_id: e.aggregate_id.clone(),
                generation: e.sequence,
                event_type: e.event_type.clone(),
                schema_version: Some(pb::SchemaVersion {
                    major: e.schema_version.0,
                    minor: e.schema_version.1,
                }),
                occurred_at: Some(rfc3339_to_timestamp(&e.occurred_at)),
                payload: serde_json::to_vec(&e.payload)
                    .map_err(|e| e.to_string())
                    .unwrap_or_default(),
            })
            .collect())
    }

    /// Trusted Code Surface payload (docs/20): immutable file content bound
    /// to workspace + file revisions, read through the canonical
    /// WorkspaceFileService — the renderer never owns buffers.
    fn code_view(&self, path: &str) -> Result<pb::CodeViewModel, String> {
        let ws = self
            .workspace
            .as_ref()
            .ok_or_else(|| "no workspace open".to_string())?;
        let (bytes, file_revision) = ws.read(path).map_err(|e| e.to_string())?;
        let sha256 = ws
            .stat(path)
            .map_err(|e| e.to_string())?
            .map(|(_, sha, _)| sha)
            .unwrap_or_default();
        Ok(pb::CodeViewModel {
            workspace_revision: ws.workspace_revision(),
            file_revision,
            path: path.to_string(),
            content_sha256: sha256,
            content_text: String::from_utf8(bytes)
                .map_err(|_| "file is not valid UTF-8".to_string())?,
        })
    }

    /// Fleet snapshot from the tasks projection (docs/31 § `tasks`).
    fn fleet(&self) -> Result<pb::Fleet, String> {
        let default_session = self.first_session()?.unwrap_or_default();
        let tasks = self.store.with_conn(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT task_id, session_id, goal_text, state, generation, created_at
                     FROM tasks ORDER BY created_at DESC, task_id",
                )
                .map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, i64>(4)?,
                        r.get::<_, String>(5)?,
                    ))
                })
                .map_err(|e| e.to_string())?
                .collect::<Result<Vec<_>, rusqlite::Error>>()
                .map_err(|e| e.to_string())?;
            Ok::<_, String>(rows)
        })?;
        let tasks = tasks
            .into_iter()
            .map(
                |(task_id, session_id, goal_text, state, generation, created_at): (
                    String,
                    String,
                    String,
                    String,
                    i64,
                    String,
                )| pb::TaskView {
                    task_id,
                    session_id,
                    title: goal_text.lines().next().unwrap_or_default().to_string(),
                    state: map_state(&state),
                    created_at,
                    generation: generation as u64,
                },
            )
            .collect();
        Ok(pb::Fleet {
            tasks,
            default_session_id: default_session,
        })
    }

    /// Run detail (docs/13 Run/Turn/RunStep) assembled from the durable
    /// run-plane aggregates of the task's runs; committed facts only.
    fn run_detail(&self, task_id: &str) -> Result<pb::RunDetailView, String> {
        let run_ids: Vec<String> = self
            .store
            .with_conn(|conn| -> Result<Vec<String>, String> {
            let mut stmt = conn
                .prepare(
                    "SELECT aggregate_id FROM events WHERE aggregate_type='run' \
                     AND event_type='run_started' ORDER BY rowid",
                )
                .map_err(|e| e.to_string())?;
            let rows: Vec<String> = stmt
                .query_map([], |r| r.get::<_, String>(0))
                .map_err(|e| e.to_string())?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| e.to_string())?;
            Ok(rows)
        })
        .map_err(|e: String| e)?;
        let mut runs: Vec<(String, RunSummary)> = Vec::new();
        for id in run_ids {
            let events = self.store.load(&id).map_err(|e| e.to_string())?;
            let Some(first) = events.first() else { continue };
            let modbit_domain::DomainEvent::RunStarted { task_id: rid, .. } = &first.payload
            else {
                continue;
            };
            if rid.to_string() != task_id {
                continue;
            }
            let mut summary = RunSummary::default();
            for e in &events {
                match &e.payload {
                    modbit_domain::DomainEvent::RunCompleted => summary.state = "completed".into(),
                    modbit_domain::DomainEvent::RunFailed { failure_code } => {
                        summary.state = "failed".into();
                        summary.failure_code = failure_code.clone();
                    }
                    _ => {}
                }
            }
            if summary.state.is_empty() {
                summary.state = "running".into();
            }
            runs.push((id, summary));
        }
        if runs.is_empty() {
            return Err(format!("task {task_id} has no runs"));
        }
        // Latest run only for the detail view.
        let (run_id, summary) = runs.last().cloned().unwrap();
        let turns = self.turns_and_steps(&run_id)?;
        Ok(pb::RunDetailView {
            task_id: task_id.to_string(),
            turns,
            run_state: summary.state,
            failure_code: summary.failure_code,
        })
    }

    /// Loads the turns of this run (TurnPrepared references it) with their
    /// steps; each aggregate contributes its derived terminal state.
    fn turns_and_steps(&self, run_id: &str) -> Result<Vec<pb::TurnView>, String> {
        let aggregate_ids = |aggregate_type: &str| -> Result<Vec<(i64, String)>, String> {
            self.store.with_conn(|conn| {
                let mut stmt = conn
                    .prepare(
                        "SELECT MIN(rowid), aggregate_id FROM events WHERE aggregate_type = ?1 \
                         GROUP BY aggregate_id ORDER BY MIN(rowid)",
                    )
                    .map_err(|e| e.to_string())?;
                let rows = stmt
                    .query_map([aggregate_type], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))
                    .map_err(|e| e.to_string())?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|e| e.to_string())?;
                Ok(rows)
            })
        };

        // Turns of this run, in preparation order.
        let mut turns: Vec<(i64, String, u32)> = Vec::new();
        for (rowid, tid) in aggregate_ids("turn")? {
            let events = self.store.load(&tid).map_err(|e| e.to_string())?;
            if let Some(modbit_domain::DomainEvent::TurnPrepared { run_id: r, ordinal }) =
                events.first().map(|e| &e.payload)
            {
                if r.to_string() == run_id {
                    turns.push((rowid, tid, *ordinal));
                }
            }
        }

        // Steps grouped under their turn, in aggregate creation order.
        let mut steps_by_turn: std::collections::HashMap<String, Vec<pb::RunStepView>> =
            std::collections::HashMap::new();
        for (_rowid, sid) in aggregate_ids("run_step")? {
            let events = self.store.load(&sid).map_err(|e| e.to_string())?;
            let Some(modbit_domain::DomainEvent::RunStepPrepared { turn_id, step_type, .. }) =
                events.first().map(|e| &e.payload)
            else {
                continue;
            };
            let mut state = "prepared".to_string();
            let mut failure_code = String::new();
            for e in &events {
                match &e.payload {
                    modbit_domain::DomainEvent::RunStepCompleted => state = "completed".into(),
                    modbit_domain::DomainEvent::RunStepFailed { failure_code: f } => {
                        state = "failed".into();
                        failure_code = f.clone();
                    }
                    _ => {}
                }
            }
            steps_by_turn.entry(turn_id.to_string()).or_default().push(pb::RunStepView {
                step_id: sid,
                turn_id: turn_id.to_string(),
                step_type: step_type.as_str().to_string(),
                state,
                failure_code,
            });
        }

        let mut views = Vec::new();
        for (_rowid, tid, _ordinal) in turns {
            // Turn terminal state from its own aggregate events.
            let events = self.store.load(&tid).map_err(|e| e.to_string())?;
            let mut state = "streaming".to_string();
            for e in &events {
                match &e.payload {
                    modbit_domain::DomainEvent::TurnCompleted => state = "completed".into(),
                    modbit_domain::DomainEvent::TurnFailed { .. } => state = "failed".into(),
                    _ => {}
                }
            }
            views.push(pb::TurnView {
                turn_id: tid.clone(),
                state,
                steps: steps_by_turn.remove(&tid).unwrap_or_default(),
            });
        }
        Ok(views)
    }

    /// Revision-bound diff of the task's worktree against its base revision
    /// (E2E-001 review substrate). The worktree location follows the
    /// scheduler's deterministic allocation.
    /// Paginated OutputRef read (Phase 2.6): bounded ranges over the
    /// runtime store's content-addressed output payloads. Ids are opaque
    /// primary keys — no path surface, no traversal.
    fn read_output_ref(
        &self,
        output_ref_id: &str,
        offset: u64,
        max_bytes: u64,
    ) -> Result<pb::OutputRefChunkView, String> {
        const MAX_PAGE: u64 = 512 * 1024;
        let (data, total_length) = self
            .store
            .runtime()
            .read_output_range(output_ref_id, offset, max_bytes.min(MAX_PAGE))
            .map_err(|e| e.to_string())?;
        Ok(pb::OutputRefChunkView {
            output_ref_id: output_ref_id.to_string(),
            offset,
            data,
            total_length,
        })
    }

    fn diff(&self, task_id: &str) -> Result<pb::DiffView, String> {
        let source = self.task_worktrees.as_ref().ok_or_else(|| {
            "task worktrees not configured on this core (host must attach the layout)".to_string()
        })?;
        let config = source.layout(task_id).ok_or_else(|| {
            "no repository configured for task worktrees".to_string()
        })?;
        if !config.worktree.exists() {
            return Err(format!("task {task_id} has no allocated worktree"));
        }
        let repo = modbit_git::GitRepo::open(&config.worktree).map_err(|e| e.to_string())?;
        let files = repo
            .diff_workdir_numstat(&config.base_revision)
            .map_err(|e| e.to_string())?;
        Ok(pb::DiffView {
            task_id: task_id.to_string(),
            branch: config.branch,
            base_revision: config.base_revision,
            files: files
                .into_iter()
                .map(|f| pb::DiffFileView {
                    path: f.path,
                    additions: f.additions,
                    deletions: f.deletions,
                })
                .collect(),
        })
    }

    fn task_view(&self, task_id: &str) -> Option<pb::TaskView> {
        self.store.with_conn(|conn| {
            conn.query_row(
                "SELECT task_id, session_id, goal_text, state, generation, created_at
                 FROM tasks WHERE task_id = ?1",
                [task_id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, i64>(4)?,
                        r.get::<_, String>(5)?,
                    ))
                },
            )
            .ok()
            .map(
                |(task_id, session_id, goal_text, state, generation, created_at): (
                    String,
                    String,
                    String,
                    String,
                    i64,
                    String,
                )| pb::TaskView {
                    task_id,
                    session_id,
                    title: goal_text.lines().next().unwrap_or_default().to_string(),
                    state: map_state(&state),
                    created_at,
                    generation: generation as u64,
                },
            )
        })
    }
}

/// Projection state strings (docs/31 `tasks.state`) → canonical TaskStatus.
fn map_state(state: &str) -> i32 {
    match state {
        "created" => pb::TaskStatus::Created as i32,
        "queued" => pb::TaskStatus::Queued as i32,
        "running" => pb::TaskStatus::Started as i32,
        "ready_for_review" => pb::TaskStatus::ReadyForReview as i32,
        "completed" => pb::TaskStatus::Completed as i32,
        "failed" => pb::TaskStatus::Failed as i32,
        "cancelled" => pb::TaskStatus::Cancelled as i32,
        s if s.starts_with("waiting_") => pb::TaskStatus::Waiting as i32,
        _ => pb::TaskStatus::Unspecified as i32,
    }
}

fn new_command_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

/// Converts the RFC3339 timestamps emitted by the store into protobuf
/// Timestamps. Inverse of the event store's own formatting.
fn rfc3339_to_timestamp(s: &str) -> prost_types::Timestamp {
    // Format: YYYY-MM-DDTHH:MM:SS.mmmZ (produced by the event store).
    let parse_err = || prost_types::Timestamp {
        seconds: 0,
        nanos: 0,
    };
    let bytes = s.as_bytes();
    if bytes.len() != 24 || !s.ends_with('Z') {
        return parse_err();
    }
    let num = |a: usize, b: usize| -> Option<i64> { s.get(a..b)?.parse().ok() };
    let (year, month, day) = match (num(0, 4), num(5, 7), num(8, 10)) {
        (Some(y), Some(m), Some(d)) => (y, m, d),
        _ => return parse_err(),
    };
    let (hh, mm, ss, ms) = match (num(11, 13), num(14, 16), num(17, 19), num(20, 23)) {
        (Some(a), Some(b), Some(c), Some(d)) => (a, b, c, d),
        _ => return parse_err(),
    };
    // Days from civil (Howard Hinnant), inverse of the store's formatter.
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + hh * 3_600 + mm * 60 + ss;
    prost_types::Timestamp {
        seconds: secs,
        nanos: (ms * 1_000_000) as i32,
    }
}

fn parse_task_id(task_id: &str) -> TaskId {
    TaskId::parse(task_id).unwrap_or_else(|_| TaskId::generate())
}
