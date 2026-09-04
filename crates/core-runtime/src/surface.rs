//! SurfaceProtocol request dispatch (M1.4): ties the authenticated transport
//! to the idempotent command processor and the docs/31 projections.
//!
//! Fleet reads are projection reads (derived state); mutations go through
//! `CommandProcessor` so command idempotency and the state machine hold.

use std::sync::Arc;

use prost::Message;
use rusqlite::OptionalExtension;

use modbit_domain::commands::CommandPayload;
use modbit_domain::{Actor, ActorType, Command, SessionId};
use modbit_event_store::{CommandProcessor, EventStore};
use modbit_protocol::modbit::protocol::v1 as pb;

/// Core services shared by the SurfaceProtocol dispatch loop.
pub struct CoreServices {
    store: Arc<EventStore>,
    processor: CommandProcessor,
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
        }
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
            None => pb::SurfaceResponse {
                ok: false,
                error: "empty SurfaceRequest".into(),
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
