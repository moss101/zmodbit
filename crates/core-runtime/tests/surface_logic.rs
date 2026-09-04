//! Direct CoreServices logic test (no transport): create_task with an empty
//! session id must ensure a session and return the task view.

use std::sync::Arc;

use modbit_core_runtime::CoreServices;
use modbit_event_store::EventStore;
use modbit_protocol::modbit::protocol::v1 as pb;
use prost::Message;

#[test]
fn create_task_with_empty_session_creates_default_session_and_task() {
    let mut db = std::env::temp_dir();
    db.push(format!("m1.4-logic-{}.db", uuid::Uuid::now_v7()));
    let services = CoreServices::new(Arc::new(EventStore::open(&db).unwrap()));

    let request = pb::SurfaceRequest {
        request: Some(pb::surface_request::Request::CreateTask(
            pb::CreateTaskCommand {
                session_id: String::new(),
                title: "direct".into(),
                prompt: "p".into(),
            },
        )),
    };
    let response =
        pb::SurfaceResponse::decode(services.handle(&request.encode_to_vec()).as_slice()).unwrap();
    assert!(response.ok, "error: {}", response.error);
    services.store().with_conn(|conn| {
        let mut stmt = conn
            .prepare("SELECT event_id, aggregate_type, aggregate_id, event_type FROM events")
            .unwrap();
        let rows = stmt
            .query_map([], |r| {
                Ok(format!(
                    "{:?}",
                    (
                        r.get::<_, String>(0),
                        r.get::<_, String>(1),
                        r.get::<_, String>(2),
                        r.get::<_, String>(3)
                    )
                ))
            })
            .unwrap();
        for row in rows {
            eprintln!("EVENT ROW: {}", row.unwrap());
        }
    });
    let task = response.task.as_ref().expect("task view");
    assert_eq!(task.title, "direct");

    let fleet_request = pb::SurfaceRequest {
        request: Some(pb::surface_request::Request::GetFleet(
            pb::GetFleetRequest {},
        )),
    };
    let response =
        pb::SurfaceResponse::decode(services.handle(&fleet_request.encode_to_vec()).as_slice())
            .unwrap();
    assert!(response.ok, "error: {}", response.error);
    assert_eq!(response.fleet.as_ref().unwrap().tasks.len(), 1);
}
