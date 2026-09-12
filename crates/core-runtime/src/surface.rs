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
    /// Phase 7 item 1: the agent fleet journal (AgentGraph ownership);
    /// admitted children are persisted here and survive restarts.
    agent_fleet: Option<std::sync::Arc<std::sync::Mutex<crate::agent_fleet::AgentFleet>>>,
    /// Live run-control signals (Phase 2.3): Stop/Pause/Steer reach the
    /// in-flight run through the scheduler's registry.
    run_controls: Option<std::sync::Arc<crate::scheduler::RunControls>>,
}

/// RFC3339-ish timestamp matching the store's format (no chrono dep).
fn now_string() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let (y, m, d, hh, mm, ss) = days_to_civil(secs);
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

/// Days-since-epoch → (y, m, d, h, m, s). Howard Hinnant's civil algorithm
/// (the same one the scheduler uses for its event timestamps).
fn days_to_civil(secs: u64) -> (i64, u64, u64, u64, u64, u64) {
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
    (y, m as u64, d as u64, rem / 3600, (rem % 3600) / 60, rem % 60)
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
            agent_fleet: None,
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

    /// Attaches the agent-fleet journal (Phase 7 item 1): the daemon passes
    /// a path next to the durable store so admitted children survive
    /// restarts. Tests can pass a journal under their temp dir.
    pub fn with_agent_fleet(
        mut self,
        journal: std::path::PathBuf,
    ) -> std::io::Result<Self> {
        self.agent_fleet = Some(std::sync::Arc::new(std::sync::Mutex::new(
            crate::agent_fleet::AgentFleet::load(&journal)?,
        )));
        Ok(self)
    }

    /// The broker name a provider's key is stored under (the same names
    /// the env broker reads — OPENAI_API_KEY / ANTHROPIC_API_KEY).
    fn credential_env_for(provider: &str) -> &'static str {
        if provider == "anthropic" {
            "ANTHROPIC_API_KEY"
        } else {
            "OPENAI_API_KEY"
        }
    }

    /// Phase 4.2: the persisted settings as a wire view (empty strings =
    /// unset → the boot/env configuration applies).
    fn settings_view(&self) -> Result<pb::SettingsView, String> {
        let doc = self
            .store
            .with_conn(|conn| modbit_event_store::settings::get(conn).map_err(|e| e.to_string()))?;
        let empty = || pb::SettingsView::default();
        let Some(doc) = doc else { return Ok(empty()) };
        let get_str = |k: &str| {
            doc.get(k)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string()
        };
        let provider = if get_str("provider").is_empty() {
            "openai".to_string()
        } else {
            get_str("provider")
        };
        Ok(pb::SettingsView {
            provider: get_str("provider"),
            model: get_str("model"),
            base_url: get_str("base_url"),
            max_turns: doc.get("max_turns").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
            execution_mode: get_str("execution_mode"),
            has_api_key: modbit_providers::keychain::has_secret(Self::credential_env_for(&provider)),
        })
    }

    /// Phase 4.1: registers a repository (by local path, or by cloning
    /// `clone_url` into the daemon's worktree root) and records its
    /// default branch. The registry is runtime configuration state
    /// (recent_repos table, docs/31) — not event history.
    fn register_repo(
        &self,
        register: &pb::RegisterRepoCommand,
    ) -> Result<modbit_event_store::repos::RecentRepo, String> {
        use modbit_git::GitRepo;

        let source = self.task_worktrees.as_ref().ok_or_else(|| {
            "no worktree source configured for repo registration".to_string()
        })?;
        let worktree_root = source
            .worktree_root()
            .ok_or_else(|| "worktree source has no worktree root".to_string())?;

        let (repo, clone_url) = if !register.clone_url.is_empty() {
            // Windows URLs arrive with backslash separators; normalize so
            // the last segment (and only it) names the clone directory.
            let normalized = register.clone_url.replace('\\', "/");
            let dir_name = normalized
                .rsplit('/')
                .find(|s| !s.is_empty())
                .unwrap_or("repo")
                .trim_end_matches(".git")
                .to_string();
            let into = worktree_root.join("registered").join(dir_name);
            (
                GitRepo::clone(&register.clone_url, &into).map_err(|e| e.to_string())?,
                register.clone_url.clone(),
            )
        } else if !register.path.is_empty() {
            (
                GitRepo::open(std::path::Path::new(&register.path))
                .map_err(|e| e.to_string())?,
                String::new(),
            )
        } else {
            return Err("register_repo needs a path or a clone_url".to_string());
        };

        let default_branch = repo.default_branch().map_err(|e| e.to_string())?;
        let repo_id = format!("repo-{}", uuid::Uuid::now_v7().simple());
        let now = modbit_event_store::repos::RecentRepo {
            repo_id: repo_id.clone(),
            path: repo.path().display().to_string(),
            clone_url,
            default_branch,
            registered_at: now_string(),
            last_used_at: now_string(),
        };
        self.store
            .with_conn(|conn| modbit_event_store::repos::register(conn, &now))
            .map_err(|e| e.to_string())?;
        Ok(now)
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
                        repo_id: (!create.repo_id.is_empty())
                            .then(|| create.repo_id.clone()),
                        base_branch: (!create.base_branch.is_empty())
                            .then_some(create.base_branch),
                        parent_task_id: (!create.parent_task_id.is_empty())
                            .then_some(create.parent_task_id),
                    },
                });
                match outcome {
                    // The processor minted the task id; the created event's
                    // aggregate id IS the authoritative task id.
                    Ok(Some(aggregate_id)) => {
                        // Phase 7 item 2 (REQ ledger EV-0150): acquire the declared
                        // write scope; denial cancels the minted task —
                        // admission stays all-or-nothing.
                        let repo_key = if create.repo_id.is_empty() {
                            "default".to_string()
                        } else {
                            create.repo_id.clone()
                        };
                        let acquired = self.store.with_conn(|conn| {
                            modbit_event_store::write_scopes::acquire(
                                conn,
                                &aggregate_id,
                                &repo_key,
                                &create.write_scope,
                            )
                        });
                        if let Err(e) = acquired {
                            let _ = self.execute(Command {
                                command_id: new_command_id(),
                                actor: actor(),
                                payload: CommandPayload::CancelTask {
                                    task_id: modbit_domain::TaskId::parse(&aggregate_id)
                                        .expect("minted task id"),
                                    reason: "write-scope acquisition failed".into(),
                                },
                            });
                            return pb::SurfaceResponse {
                                ok: false,
                                error: e,
                                ..Default::default()
                            };
                        }
                        if let modbit_event_store::write_scopes::AcquireOutcome::Denied {
                            holder_task_id,
                            path,
                        } = acquired.unwrap()
                        {
                            let _ = self.execute(Command {
                                command_id: new_command_id(),
                                actor: actor(),
                                payload: CommandPayload::CancelTask {
                                    task_id: modbit_domain::TaskId::parse(&aggregate_id)
                                        .expect("minted task id"),
                                    reason: "write-scope denied".into(),
                                },
                            });
                            return pb::SurfaceResponse {
                                ok: false,
                                error: format!(
                                    "write-set conflict with {holder_task_id}: {path}"
                                ),
                                ..Default::default()
                            };
                        }
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
            Some(pb::surface_request::Request::RegisterRepo(register)) => {
                // Phase 4.1: register an existing local repo, or clone by
                // URL into the daemon's worktree root. The repo's default
                // branch is captured at registration.
                let outcome = self.register_repo(&register);
                match outcome {
                    Ok(repo) => pb::SurfaceResponse {
                        ok: true,
                        repo: Some(pb::RecentRepoView {
                            repo_id: repo.repo_id,
                            path: repo.path,
                            clone_url: repo.clone_url,
                            default_branch: repo.default_branch,
                            registered_at: repo.registered_at,
                            last_used_at: repo.last_used_at,
                        }),
                        ..Default::default()
                    },
                    Err(e) => pb::SurfaceResponse {
                        ok: false,
                        error: e,
                        ..Default::default()
                    },
                }
            }
            Some(pb::surface_request::Request::ListRecentRepos(_)) => {
                let listed = self
                    .store
                    .with_conn(|conn| modbit_event_store::repos::list(conn).map_err(|e| e.to_string()));
                match listed {
                    Ok(repos) => pb::SurfaceResponse {
                        ok: true,
                        recent_repos: Some(pb::RecentRepoList {
                            repos: repos
                                .into_iter()
                                .map(|r| pb::RecentRepoView {
                                    repo_id: r.repo_id,
                                    path: r.path,
                                    clone_url: r.clone_url,
                                    default_branch: r.default_branch,
                                    registered_at: r.registered_at,
                                    last_used_at: r.last_used_at,
                                })
                                .collect(),
                        }),
                        ..Default::default()
                    },
                    Err(e) => pb::SurfaceResponse {
                        ok: false,
                        error: e,
                        ..Default::default()
                    },
                }
            }
            Some(pb::surface_request::Request::GetSettings(_)) => {
                let view = self.settings_view();
                match view {
                    Ok(settings) => pb::SurfaceResponse {
                        ok: true,
                        settings: Some(settings),
                        ..Default::default()
                    },
                    Err(e) => pb::SurfaceResponse {
                        ok: false,
                        error: e,
                        ..Default::default()
                    },
                }
            }
            Some(pb::surface_request::Request::UpdateSettings(update)) => {
                // Phase 4.2: partial update — empty fields keep the stored
                // value. Phase 4.3: an api_key goes to the OS KEYCHAIN and
                // is NEVER merged into the settings document, the
                // environment, or the event store (docs/31 § Secrets).
                let outcome = self
                    .store
                    .with_conn(|conn| {
                        let patch = serde_json::json!({
                            "provider": if update.provider.is_empty() { serde_json::Value::Null } else { serde_json::json!(update.provider) },
                            "model": if update.model.is_empty() { serde_json::Value::Null } else { serde_json::json!(update.model) },
                            "base_url": if update.base_url.is_empty() { serde_json::Value::Null } else { serde_json::json!(update.base_url) },
                            "max_turns": if update.max_turns == 0 { serde_json::Value::Null } else { serde_json::json!(update.max_turns) },
                            "execution_mode": if update.execution_mode.is_empty() { serde_json::Value::Null } else { serde_json::json!(update.execution_mode) },
                        });
                        // Drop nulls: merge only provided fields.
                        let patch_obj = patch
                            .as_object()
                            .map(|o| {
                                o.iter()
                                    .filter(|(_, v)| !v.is_null())
                                    .map(|(k, v)| (k.clone(), v.clone()))
                                    .collect::<serde_json::Map<String, serde_json::Value>>()
                            })
                            .unwrap_or_default();
                        let merged = modbit_event_store::settings::merge(
                            conn,
                            &serde_json::Value::Object(patch_obj),
                        )
                        .map_err(|e| e.to_string())?;
                        Ok((merged, ()))
                    })
                    .map(|(merged, _)| merged);
                if !update.api_key.is_empty() {
                    // Bind the key to the CURRENT provider (the update's
                    // provider if given, else the stored one, else openai).
                    let stored = self
                        .store
                        .with_conn(|conn| modbit_event_store::settings::get(conn).map_err(|e| e.to_string()))
                        .ok()
                        .flatten()
                        .unwrap_or_else(|| serde_json::Value::Object(Default::default()));
                    let provider = if !update.provider.is_empty() {
                        update.provider.clone()
                    } else {
                        stored
                            .get("provider")
                            .and_then(|v| v.as_str())
                            .unwrap_or("openai")
                            .to_string()
                    };
                    let credential_name = Self::credential_env_for(&provider);
                    if let Err(e) =
                        modbit_providers::keychain::store_secret(credential_name, &update.api_key)
                    {
                        return pb::SurfaceResponse {
                            ok: false,
                            error: e.to_string(),
                            ..Default::default()
                        };
                    }
                }
                match outcome {
                    Ok(merged) => {
                        let mut view = self.settings_view().unwrap_or_default();
                        // Reflect the merged document immediately.
                        if let Some(v) = merged.get("provider").and_then(|v| v.as_str()) {
                            view.provider = v.to_string();
                        }
                        if let Some(v) = merged.get("model").and_then(|v| v.as_str()) {
                            view.model = v.to_string();
                        }
                        if let Some(v) = merged.get("base_url").and_then(|v| v.as_str()) {
                            view.base_url = v.to_string();
                        }
                        if let Some(t) = merged.get("max_turns").and_then(|v| v.as_u64()) {
                            view.max_turns = t as u32;
                        }
                        if let Some(m) = merged.get("execution_mode").and_then(|v| v.as_str()) {
                            view.execution_mode = m.to_string();
                        }
                        pb::SurfaceResponse {
                            ok: true,
                            settings: Some(view),
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
            Some(pb::surface_request::Request::ApproveEffect(approve)) => {
                // Phase 5: resolve the pending approval. First decision
                // wins (replays after a Core restart cannot re-resolve);
                // on approval the blocked run's gate appends the live
                // grant and the effect proceeds.
                let outcome = self.store.with_conn(|conn| {
                    modbit_event_store::approvals::resolve(
                        conn,
                        &approve.approval_id,
                        "approved",
                        if approve.resolved_by.is_empty() {
                            "operator"
                        } else {
                            &approve.resolved_by
                        },
                        &crate::scheduler::rfc3339_now(),
                    )
                    .map_err(|e| e.to_string())
                });
                match outcome {
                    Ok(true) => pb::SurfaceResponse {
                        ok: true,
                        ..Default::default()
                    },
                    Ok(false) => pb::SurfaceResponse {
                        ok: false,
                        error: format!("approval {} is not pending", approve.approval_id),
                        ..Default::default()
                    },
                    Err(e) => pb::SurfaceResponse {
                        ok: false,
                        error: e,
                        ..Default::default()
                    },
                }
            }
            Some(pb::surface_request::Request::DenyEffect(deny)) => {
                let outcome = self.store.with_conn(|conn| {
                    modbit_event_store::approvals::resolve(
                        conn,
                        &deny.approval_id,
                        "denied",
                        if deny.resolved_by.is_empty() {
                            "operator"
                        } else {
                            &deny.resolved_by
                        },
                        &crate::scheduler::rfc3339_now(),
                    )
                    .map_err(|e| e.to_string())
                });
                match outcome {
                    Ok(true) => pb::SurfaceResponse {
                        ok: true,
                        ..Default::default()
                    },
                    Ok(false) => pb::SurfaceResponse {
                        ok: false,
                        error: format!("approval {} is not pending", deny.approval_id),
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
            // Phase 5 item 4: hunk-level review surface.
            Some(pb::surface_request::Request::GetDiffHunks(get)) => {
                match self.diff_hunks(&get.task_id) {
                    Ok(view) => pb::SurfaceResponse {
                        ok: true,
                        diff_hunks: Some(view),
                        ..Default::default()
                    },
                    Err(e) => pb::SurfaceResponse {
                        ok: false,
                        error: e,
                        ..Default::default()
                    },
                }
            }
            Some(pb::surface_request::Request::ResolveReviewHunk(resolve)) => {
                match self.resolve_review_hunk(&resolve) {
                    Ok(()) => pb::SurfaceResponse {
                        ok: true,
                        ..Default::default()
                    },
                    Err(e) => pb::SurfaceResponse {
                        ok: false,
                        error: e,
                        ..Default::default()
                    },
                }
            }
            // Phase 7 item 1: children through the scheduler.
            Some(pb::surface_request::Request::SpawnAgent(spawn)) => {
                match self.spawn_agent(&spawn) {
                    Ok(task) => pb::SurfaceResponse {
                        ok: true,
                        task,
                        ..Default::default()
                    },
                    Err(e) => pb::SurfaceResponse {
                        ok: false,
                        error: e,
                        ..Default::default()
                    },
                }
            }
            Some(pb::surface_request::Request::ParkAgent(park)) => {
                match self.park_agent(&park) {
                    Ok(task) => pb::SurfaceResponse {
                        ok: true,
                        task,
                        ..Default::default()
                    },
                    Err(e) => pb::SurfaceResponse {
                        ok: false,
                        error: e,
                        ..Default::default()
                    },
                }
            }
            Some(pb::surface_request::Request::ResumeAgent(resume)) => {
                match self.resume_agent(&resume) {
                    Ok(task) => pb::SurfaceResponse {
                        ok: true,
                        task,
                        ..Default::default()
                    },
                    Err(e) => pb::SurfaceResponse {
                        ok: false,
                        error: e,
                        ..Default::default()
                    },
                }
            }
            Some(pb::surface_request::Request::AgentResult(req)) => {
                match self.agent_result(&req) {
                    Ok(view) => pb::SurfaceResponse {
                        ok: true,
                        agent_result: Some(view),
                        ..Default::default()
                    },
                    Err(e) => pb::SurfaceResponse {
                        ok: false,
                        error: e,
                        ..Default::default()
                    },
                }
            }
            Some(pb::surface_request::Request::RunVariants(variants)) => {
                match self.run_variants(&variants) {
                    Ok(task) => pb::SurfaceResponse {
                        ok: true,
                        task,
                        ..Default::default()
                    },
                    Err(e) => pb::SurfaceResponse {
                        ok: false,
                        error: e,
                        ..Default::default()
                    },
                }
            }
            Some(pb::surface_request::Request::CreateAutomation(create)) => {
                match self.create_automation(&create) {
                    Ok(()) => pb::SurfaceResponse {
                        ok: true,
                        ..Default::default()
                    },
                    Err(e) => pb::SurfaceResponse {
                        ok: false,
                        error: e,
                        ..Default::default()
                    },
                }
            }
            Some(pb::surface_request::Request::AddReviewComment(add)) => {
                match self.add_review_comment(&add) {
                    Ok(()) => pb::SurfaceResponse {
                        ok: true,
                        ..Default::default()
                    },
                    Err(e) => pb::SurfaceResponse {
                        ok: false,
                        error: e,
                        ..Default::default()
                    },
                }
            }
            Some(pb::surface_request::Request::GetReviewChecklist(get)) => {
                match self.review_checklist(&get.task_id) {
                    Ok(view) => pb::SurfaceResponse {
                        ok: true,
                        review_checklist: Some(view),
                        ..Default::default()
                    },
                    Err(e) => pb::SurfaceResponse {
                        ok: false,
                        error: e,
                        ..Default::default()
                    },
                }
            }
            Some(pb::surface_request::Request::ListAutomations(_)) => {
                match self
                    .store
                    .with_conn(modbit_event_store::automations::list)
                {
                    Ok(rows) => pb::SurfaceResponse {
                        ok: true,
                        automations: Some(pb::AutomationList {
                            automations: rows
                                .into_iter()
                                .map(|a| pb::AutomationView {
                                    automation_id: a.automation_id,
                                    name: a.name,
                                    cron: a.cron,
                                    event_pattern: a.event_pattern,
                                    objective: a.objective,
                                    enabled: a.enabled,
                                    last_fire_key: a.last_fire_key,
                                })
                                .collect(),
                        }),
                        ..Default::default()
                    },
                    Err(e) => pb::SurfaceResponse {
                        ok: false,
                        error: e,
                        ..Default::default()
                    },
                }
            }
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
                    "SELECT task_id, session_id, goal_text, state, generation, created_at, parent_task_id
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
                        r.get::<_, Option<String>>(6)?,
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
                |(task_id, session_id, goal_text, state, generation, created_at, parent_task_id): (
                    String,
                    String,
                    String,
                    String,
                    i64,
                    String,
                    Option<String>,
                )| pb::TaskView {
                    task_id,
                    session_id,
                    title: goal_text.lines().next().unwrap_or_default().to_string(),
                    state: map_state(&state),
                    created_at,
                    generation: generation as u64,
                    parent_task_id: parent_task_id.unwrap_or_default(),
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

    /// Shared worktree resolution for the review surface (Phase 5 item 4):
    /// the task's layout plus its worktree opened as a git repository.
    fn review_repo(
        &self,
        task_id: &str,
    ) -> Result<(crate::scheduler::WorktreeLayout, modbit_git::GitRepo), String> {
        let source = self.task_worktrees.as_ref().ok_or_else(|| {
            "task worktrees not configured on this core (host must attach the layout)".to_string()
        })?;
        let config = source
            .layout(task_id)
            .ok_or_else(|| "no repository configured for task worktrees".to_string())?;
        if !config.worktree.exists() {
            return Err(format!("task {task_id} has no allocated worktree"));
        }
        let repo = modbit_git::GitRepo::open(&config.worktree).map_err(|e| e.to_string())?;
        Ok((config, repo))
    }

    /// Hunk-level diff content (Phase 5 item 4): the changed ranges of the
    /// task worktree against its base revision, with the new-file start
    /// line each review decision is keyed on.
    fn diff_hunks(&self, task_id: &str) -> Result<pb::DiffHunksView, String> {
        let (config, repo) = self.review_repo(task_id)?;
        let hunks = repo
            .diff_hunks_from(&config.base_revision)
            .map_err(|e| e.to_string())?;
        // Comments are REVISION-BOUND: only those stamped with the
        // current base revision surface (stale comments stay durable but
        // invisible).
        let comments = self.review_comments(task_id, &config.base_revision)?;
        Ok(pb::DiffHunksView {
            task_id: task_id.to_string(),
            branch: config.branch,
            base_revision: config.base_revision.clone(),
            hunks: hunks
                .into_iter()
                .map(|h| pb::DiffHunkView {
                    path: h.path,
                    new_start: h.new_start as u64,
                    lines: h.lines,
                })
                .collect(),
            comments,
        })
    }

    /// Durable inline comments for a task at one revision.
    fn review_comments(
        &self,
        task_id: &str,
        revision: &str,
    ) -> Result<Vec<pb::ReviewCommentView>, String> {
        let rows: Vec<(String, String)> = self.store.with_conn(|conn| {
            let Ok(mut stmt) = conn.prepare(
                "SELECT payload_inline, occurred_at FROM events
                 WHERE aggregate_id = ?1 AND event_type = 'review_comment_added'
                 ORDER BY rowid",
            ) else {
                return Vec::new();
            };
            let Ok(rows) = stmt
                .query_map([task_id], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })
                .map(|rows| rows.collect::<Result<Vec<_>, _>>())
            else {
                return Vec::new();
            };
            rows.unwrap_or_default()
        });
        let mut out = Vec::new();
        for (payload, occurred_at) in rows {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&payload) else {
                continue;
            };
            if v["revision"].as_str() != Some(revision) {
                continue;
            }
            out.push(pb::ReviewCommentView {
                path: v["path"].as_str().unwrap_or_default().to_string(),
                new_start: v["new_start"].as_u64().unwrap_or(0),
                body: v["body"].as_str().unwrap_or_default().to_string(),
                revision: v["revision"].as_str().unwrap_or_default().to_string(),
                created_at: occurred_at.to_string(),
            });
        }
        Ok(out)
    }

    /// Records one hunk review decision (Phase 5 item 4). Reject first
    /// inverse-applies ONLY that hunk in the task worktree (the durable
    /// event must record an effect that already happened); accept leaves
    /// the worktree untouched. Both append `ReviewHunkResolved`.
    fn resolve_review_hunk(&self, resolve: &pb::ResolveReviewHunkCommand) -> Result<(), String> {
        let (config, repo) = self.review_repo(&resolve.task_id)?;
        if !resolve.accepted {
            repo.reject_hunk(
                &resolve.path,
                resolve.new_start as usize,
                &config.base_revision,
            )
            .map_err(|e| e.to_string())?;
        }
        let task_id =
            modbit_domain::TaskId::parse(&resolve.task_id).map_err(|e| e.to_string())?;
        self.execute(Command {
            command_id: new_command_id(),
            actor: actor(),
            payload: CommandPayload::ResolveReviewHunk {
                task_id,
                path: resolve.path.clone(),
                new_start: resolve.new_start as usize,
                accepted: resolve.accepted,
                revision: config.base_revision.clone(),
            },
        })
        .map(|_| ())
    }

    /// Phase 7 item 1 — transactional subagent admission (docs/14): the
    /// checks run in order and only when ALL pass does a real child task
    /// enter the scheduler (CreateTask → QueueTask → StartTask with its
    /// own isolated worktree). Any failure refuses admission with no
    /// partial reservation; a failure after task creation compensates by
    /// cancelling the minted child.
    fn spawn_agent(
        &self,
        spawn: &pb::SpawnAgentCommand,
    ) -> Result<Option<pb::TaskView>, String> {
        if spawn.parent_task_id.is_empty() {
            return Err("spawn_agent requires parent_task_id".into());
        }

        // Idempotent re-attach BEFORE any admission step (REQ-EV-0007):
        // a replayed spawn returns the already-admitted child.
        if !spawn.idempotency_key.is_empty() {
            if let Some(fleet) = &self.agent_fleet {
                let fleet = fleet.lock().map_err(|_| "agent fleet poisoned".to_string())?;
                if let Some(node) = fleet.find_by_idempotency_key(&spawn.idempotency_key) {
                    let task_id = node.task_id.clone();
                    drop(fleet);
                    return Ok(self.task_view(&task_id));
                }
            }
        }

        // (a) parent active at the expected generation (WorkGraph truth).
        let (parent_state, parent_gen, parent_session) = self.store.with_conn(|conn| {
            conn.query_row(
                "SELECT state, generation, session_id FROM tasks WHERE task_id = ?1",
                [&spawn.parent_task_id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                },
            )
            .map_err(|_| format!("parent task {} does not exist", spawn.parent_task_id))
        })?;
        if matches!(
            parent_state.as_str(),
            "completed" | "failed" | "cancelled"
        ) {
            return Err(format!(
                "parent task {} is not active (state {parent_state})",
                spawn.parent_task_id
            ));
        }
        if spawn.parent_generation != 0 && spawn.parent_generation != parent_gen as u64 {
            return Err(format!(
                "parent generation fenced: parent is at {parent_gen}, request expected {}",
                spawn.parent_generation
            ));
        }

        // (b) capacity ticket: bounded concurrent children per lineage
        // (first element of the typed capacity vector, docs/14 § tickets;
        // lease expiry is the terminal state, generation fencing above).
        let max_children: usize = std::env::var("MODBIT_MAX_CHILD_AGENTS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(4);
        let active_children: i64 = self.store.with_conn(|conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM tasks WHERE parent_task_id = ?1
                 AND (state IN ('queued','running','ready_for_review') OR state LIKE 'waiting_%')",
                [&spawn.parent_task_id],
                |r| r.get(0),
            )
            .map_err(|e| e.to_string())
        })?;
        if active_children as usize >= max_children {
            return Err(format!(
                "capacity ticket exhausted: {active_children} active children (max {max_children})"
            ));
        }

        // (c) declared write-scope conflict is checked by the durable
        // write coordinator (REQ ledger EV-0150) AFTER the child task is minted,
        // with compensation on denial — see acquire below. Undeclared
        // children acquire nothing (merge verification still applies).

        // (7) AgentGraph node + WorkGraph ownership persisted; the child
        // runs through THE scheduler like any other task.
        let session_id =
            SessionId::parse(&parent_session).map_err(|e| format!("bad parent session: {e}"))?;
        let child_id = self
            .execute(Command {
                command_id: new_command_id(),
                actor: actor(),
                payload: CommandPayload::CreateTask {
                    session_id,
                    title: spawn
                        .objective
                        .lines()
                        .next()
                        .unwrap_or("child agent")
                        .to_string(),
                    prompt: spawn.objective.clone(),
                    repo_id: None,
                    base_branch: None,
                    parent_task_id: Some(spawn.parent_task_id.clone()),
                },
            })?
            .ok_or_else(|| "child task creation produced no event".to_string())?;

        // Compensation: a failure after task creation cancels the minted
        // child so no partial reservation leaks.
        fn compensate(svc: &CoreServices, child_id: &str, err: String) -> String {
            let _ = svc.store.with_conn(|conn| {
                modbit_event_store::write_scopes::release(conn, child_id)
            });
            if let Ok(task_id) = modbit_domain::TaskId::parse(child_id) {
                let _ = svc.execute(Command {
                    command_id: new_command_id(),
                    actor: actor(),
                    payload: CommandPayload::CancelTask {
                        task_id,
                        reason: "admission failed after task creation".into(),
                    },
                });
            }
            err
        }

        if let Some(fleet) = &self.agent_fleet {
            let mut fleet = fleet
                .lock()
                .map_err(|e| compensate(self, &child_id, format!("agent fleet poisoned: {e}")))?;
            let parent_agent = fleet.node(&spawn.parent_task_id).map(|n| n.agent_id.clone());
            fleet
                .spawn_child(
                    parent_agent.as_deref(),
                    &child_id,
                    &child_id,
                    &spawn.objective,
                    spawn
                        .write_scope
                        .split(',')
                        .map(|p| p.trim().to_string())
                        .filter(|p| !p.is_empty())
                        .collect(),
                    (!spawn.idempotency_key.is_empty())
                        .then_some(spawn.idempotency_key.as_str()),
                )
                .map_err(|e| {
                    compensate(
                        self,
                        &child_id,
                        format!("agent node persist failed: {e}"),
                    )
                })?;
        }

        // (c, continued) durable write-scope acquisition (REQ ledger EV-0150):
        // overlapping declarations are denied BEFORE the child starts.
        let repo_key = self.parent_repo_key(&spawn.parent_task_id);
        match self.store.with_conn(|conn| {
            modbit_event_store::write_scopes::acquire(
                conn,
                &child_id,
                &repo_key,
                &spawn.write_scope,
            )
        })? {
            modbit_event_store::write_scopes::AcquireOutcome::Acquired => {}
            modbit_event_store::write_scopes::AcquireOutcome::Denied {
                holder_task_id,
                path,
            } => {
                return Err(compensate(
                    self,
                    &child_id,
                    format!("write-set conflict with {holder_task_id}: {path}"),
                ));
            }
        }

        self.execute(Command {
            command_id: new_command_id(),
            actor: actor(),
            payload: CommandPayload::QueueTask {
                task_id: parse_task_id(&child_id),
            },
        })
        .map_err(|e| compensate(self, &child_id, e))?;
        self.execute(Command {
            command_id: new_command_id(),
            actor: actor(),
            payload: CommandPayload::StartTask {
                task_id: parse_task_id(&child_id),
            },
        })
        .map_err(|e| compensate(self, &child_id, e))?;

        Ok(self.task_view(&child_id))
    }

    /// Repo key for write-scope coordination: the registered repo the
    /// lineage runs against, or the daemon default bucket.
    fn parent_repo_key(&self, task_id: &str) -> String {
        self.store
            .with_conn(|conn| {
                conn.query_row(
                    "SELECT payload_inline FROM events
                     WHERE aggregate_id = ?1 AND event_type = 'task_created'
                     ORDER BY rowid LIMIT 1",
                    [task_id],
                    |r| r.get::<_, String>(0),
                )
                .ok()
                .and_then(|payload| serde_json::from_str::<serde_json::Value>(&payload).ok())
                .and_then(|v| {
                    v["repo_id"]
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                })
            })
            .unwrap_or_else(|| "default".to_string())
    }

    /// Parks a child agent (docs/14 § steering): the in-flight run parks
    /// at the next turn boundary, the durable state moves to
    /// Waiting(UserInput), and the fleet node records Parked.
    fn park_agent(&self, park: &pb::ParkAgentCommand) -> Result<Option<pb::TaskView>, String> {
        if let Some(controls) = &self.run_controls {
            controls.pause(&park.task_id);
        }
        if let Some(fleet) = &self.agent_fleet {
            fleet
                .lock()
                .map_err(|_| "agent fleet poisoned".to_string())?
                .park(&park.task_id)
                .map_err(|e| e.to_string())?;
        }
        self.execute(Command {
            command_id: new_command_id(),
            actor: actor(),
            payload: CommandPayload::TaskWaiting {
                task_id: parse_task_id(&park.task_id),
                reason: modbit_domain::events::WaitingReason::UserInput,
            },
        })?;
        Ok(self.task_view(&park.task_id))
    }

    /// Resumes a parked child agent: fleet status returns to Foreground
    /// and the task re-enters the scheduler via StartTask.
    fn resume_agent(
        &self,
        resume: &pb::ResumeAgentCommand,
    ) -> Result<Option<pb::TaskView>, String> {
        if let Some(fleet) = &self.agent_fleet {
            fleet
                .lock()
                .map_err(|_| "agent fleet poisoned".to_string())?
                .resume(&resume.task_id)
                .map_err(|e| e.to_string())?;
        }
        self.execute(Command {
            command_id: new_command_id(),
            actor: actor(),
            payload: CommandPayload::StartTask {
                task_id: parse_task_id(&resume.task_id),
            },
        })?;
        Ok(self.task_view(&resume.task_id))
    }

    /// Bounded wait for a child agent's terminal state, then its
    /// SubagentResult view (docs/14 § agent communication). A timeout
    /// returns the CURRENT state — the caller decides whether to keep
    /// waiting; nothing is fabricated.
    fn agent_result(&self, req: &pb::AgentResultRequest) -> Result<pb::AgentResultView, String> {
        let deadline = std::time::Instant::now()
            + std::time::Duration::from_millis(req.timeout_ms.min(120_000));
        let (state, parent) = loop {
            let row = self.store.with_conn(|conn| {
                conn.query_row(
                    "SELECT state, COALESCE(parent_task_id,'') FROM tasks WHERE task_id = ?1",
                    [&req.task_id],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
                )
                .map_err(|_| format!("task {} does not exist", req.task_id))
            })?;
            if matches!(row.0.as_str(), "completed" | "failed" | "cancelled")
                || std::time::Instant::now() >= deadline
            {
                break row;
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        };
        let (summary, failure_code) = self.store.with_conn(|conn| {
            let summary = conn
                .query_row(
                    "SELECT payload_inline FROM events
                     WHERE aggregate_id = ?1 AND event_type = 'task_completed'
                     ORDER BY rowid DESC LIMIT 1",
                    [&req.task_id],
                    |r| r.get::<_, String>(0),
                )
                .ok()
                .and_then(|payload| {
                    serde_json::from_str::<serde_json::Value>(&payload)
                        .ok()
                        .and_then(|v| v["summary"].as_str().map(str::to_string))
                })
                .unwrap_or_default();
            let failure_code = conn
                .query_row(
                    "SELECT payload_inline FROM events
                     WHERE aggregate_id = ?1 AND event_type = 'task_failed'
                     ORDER BY rowid DESC LIMIT 1",
                    [&req.task_id],
                    |r| r.get::<_, String>(0),
                )
                .ok()
                .and_then(|payload| {
                    serde_json::from_str::<serde_json::Value>(&payload)
                        .ok()
                        .and_then(|v| v["failure_code"].as_str().map(str::to_string))
                })
                .unwrap_or_default();
            Ok::<_, String>((summary, failure_code))
        })?;
        Ok(pb::AgentResultView {
            task_id: req.task_id.clone(),
            parent_task_id: parent,
            state,
            summary,
            failure_code,
        })
    }

    /// Phase 7 item 2 — "run N variants" from New Task: one umbrella task
    /// plus N admitted children running the same objective in parallel,
    /// each in its own isolated worktree. Variants are alternatives, not
    /// parallel builders: they declare no write scope (merge verification
    /// decides what survives — see the Phase 7 item 1 conflict proof).
    fn run_variants(&self, cmd: &pb::RunVariantsCommand) -> Result<Option<pb::TaskView>, String> {
        if cmd.objective.trim().is_empty() {
            return Err("run_variants requires an objective".into());
        }
        if cmd.count < 2 || cmd.count > 4 {
            return Err("run_variants count must be between 2 and 4".into());
        }
        let umbrella_id = self
            .execute(Command {
                command_id: new_command_id(),
                actor: actor(),
                payload: CommandPayload::CreateTask {
                    session_id: SessionId::parse(&self.ensure_default_session()?)
                        .map_err(|e| format!("bad default session: {e}"))?,
                    title: format!(
                        "variants ×{}: {}",
                        cmd.count,
                        cmd.objective.lines().next().unwrap_or("objective")
                    ),
                    prompt: cmd.objective.clone(),
                    repo_id: (!cmd.repo_id.is_empty()).then_some(cmd.repo_id.clone()),
                    base_branch: (!cmd.base_branch.is_empty())
                        .then_some(cmd.base_branch.clone()),
                    parent_task_id: None,
                },
            })?
            .ok_or_else(|| "umbrella task creation produced no event".to_string())?;

        // Idempotency keys derive from the umbrella: a replayed run_variants
        // re-attaches the already-admitted variants instead of doubling.
        for i in 0..cmd.count {
            let spawn = pb::SpawnAgentCommand {
                parent_task_id: umbrella_id.clone(),
                objective: format!(
                    "variant {}/{} of: {}",
                    i + 1,
                    cmd.count,
                    cmd.objective
                ),
                write_scope: String::new(),
                idempotency_key: format!("variant-{umbrella_id}-{i}"),
                parent_generation: 0,
            };
            self.spawn_agent(&spawn)
                .map_err(|e| format!("variant {} refused: {e}", i + 1))?;
        }
        Ok(self.task_view(&umbrella_id))
    }

    /// Phase 7 item 4: registers a durable automation. Exactly one
    /// trigger (cron spec or event pattern) must be present; cron specs
    /// are validated AT CREATION — an unparsable spec never registers.
    fn create_automation(&self, create: &pb::CreateAutomationCommand) -> Result<(), String> {
        if create.cron.is_empty() == create.event_pattern.is_empty() {
            return Err("automation needs exactly one trigger: cron or event_pattern".into());
        }
        if !create.cron.is_empty() && crate::automation::CronSpec::parse(&create.cron).is_none() {
            return Err(format!("unparsable cron spec: {:?}", create.cron));
        }
        self.store.with_conn(|conn| {
            modbit_event_store::automations::create(
                conn,
                modbit_event_store::automations::Automation {
                    automation_id: String::new(),
                    name: create.name.clone(),
                    cron: create.cron.clone(),
                    event_pattern: create.event_pattern.clone(),
                    objective: create.objective.clone(),
                    enabled: true,
                    last_fire_key: String::new(),
                },
            )
        })?;
        Ok(())
    }

    /// Phase 5 residual: records an inline comment bound to one hunk at
    /// the task's CURRENT base revision (the surface stamps the revision
    /// — clients cannot bind to an arbitrary revision).
    fn add_review_comment(&self, add: &pb::AddReviewCommentCommand) -> Result<(), String> {
        if add.body.trim().is_empty() {
            return Err("review comment body must not be empty".into());
        }
        let (config, _repo) = self.review_repo(&add.task_id)?;
        let task_id = modbit_domain::TaskId::parse(&add.task_id).map_err(|e| e.to_string())?;
        self.execute(Command {
            command_id: new_command_id(),
            actor: actor(),
            payload: CommandPayload::AddReviewComment {
                task_id,
                path: add.path.clone(),
                new_start: add.new_start as usize,
                body: add.body.clone(),
                revision: config.base_revision.clone(),
            },
        })
        .map(|_| ())
    }

    /// Phase 5 residual: the generated review checklist — a DETERMINED
    /// function of the diff and its review state (crate::review), bound
    /// to the current base revision.
    fn review_checklist(&self, task_id: &str) -> Result<pb::ReviewChecklistView, String> {
        let (config, repo) = self.review_repo(task_id)?;
        let hunks = repo
            .diff_hunks_from(&config.base_revision)
            .map_err(|e| e.to_string())?;
        // Which hunks carry comments / decisions at THIS revision?
        let comments = self.review_comments(task_id, &config.base_revision)?;
        let decided_rows: Vec<String> = self.store.with_conn(|conn| {
            let Ok(mut stmt) = conn.prepare(
                "SELECT payload_inline FROM events
                 WHERE aggregate_id = ?1 AND event_type = 'review_hunk_resolved'",
            ) else {
                return Vec::new();
            };
            let Ok(rows) = stmt
                .query_map([task_id], |r| r.get::<_, String>(0))
                .map(|rows| rows.collect::<Result<Vec<_>, _>>())
            else {
                return Vec::new();
            };
            rows.unwrap_or_default()
        });
        let decided: Vec<(String, usize)> = decided_rows
            .iter()
            .filter_map(|p| serde_json::from_str::<serde_json::Value>(p).ok())
            .filter(|v| v["revision"].as_str() == Some(config.base_revision.as_str()))
            .map(|v| {
                (
                    v["path"].as_str().unwrap_or_default().to_string(),
                    v["new_start"].as_u64().unwrap_or(0) as usize,
                )
            })
            .collect();

        // The durable decisions carry the revision they were made at —
        // the events store them only if the surface stamped them. Hunk
        // decisions made before the revision field existed still count
        // when the payload has no revision field.
        let inputs: Vec<crate::review::HunkInput> = hunks
            .iter()
            .map(|h| crate::review::HunkInput {
                commented: comments.iter().any(|c| {
                    c.path == h.path && c.new_start as usize == h.new_start
                }),
                decided: decided.iter().any(|(p, s)| p == &h.path && *s == h.new_start),
                path: h.path.clone(),
                new_start: h.new_start,
                lines: h.lines.clone(),
            })
            .collect();
        let items: Vec<pb::ChecklistItemView> = crate::review::generate_checklist(&inputs)
            .into_iter()
            .map(|i| pb::ChecklistItemView {
                id: i.id,
                text: i.text,
                done: i.done,
            })
            .collect();
        Ok(pb::ReviewChecklistView {
            task_id: task_id.to_string(),
            revision: config.base_revision,
            items,
        })
    }

    fn task_view(&self, task_id: &str) -> Option<pb::TaskView> {
        self.store.with_conn(|conn| {
            conn.query_row(
                "SELECT task_id, session_id, goal_text, state, generation, created_at, parent_task_id
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
                        r.get::<_, Option<String>>(6)?,
                    ))
                },
            )
            .ok()
            .map(
                |(task_id, session_id, goal_text, state, generation, created_at, parent_task_id): (
                    String,
                    String,
                    String,
                    String,
                    i64,
                    String,
                    Option<String>,
                )| pb::TaskView {
                    task_id,
                    session_id,
                    title: goal_text.lines().next().unwrap_or_default().to_string(),
                    state: map_state(&state),
                    created_at,
                    generation: generation as u64,
                    parent_task_id: parent_task_id.unwrap_or_default(),
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
