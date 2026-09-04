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
