//! M2.9 integration test: the Trusted Code Review Surface over the real
//! CoreServices — get_code_view returns the immutable file payload bound to
//! workspace + file revisions, reads through the canonical
//! WorkspaceFileService, and refuses paths outside the workspace.

use std::sync::Arc;

use prost::Message;

use modbit_core_runtime::CoreServices;
use modbit_event_store::EventStore;
use modbit_protocol::modbit::protocol::v1 as pb;
use modbit_workspace::WorkspaceFileService;

#[test]
fn code_view_returns_revision_bound_payload_and_rejects_escape() {
    let mut db = std::env::temp_dir();
    db.push(format!("modbit-m29-{}.db", uuid::Uuid::now_v7().simple()));
    let store = Arc::new(EventStore::open(&db).unwrap());
    let mut ws_root = std::env::temp_dir();
    ws_root.push(format!("modbit-m29-ws-{}", uuid::Uuid::now_v7().simple()));
    let workspace = Arc::new(WorkspaceFileService::open(&ws_root).unwrap());

    let services = CoreServices::new(store.clone()).with_workspace(workspace.clone());
    workspace.create("src/lib.txt", b"trusted content").unwrap();

    // Fresh view: content bound to the current revisions.
    let request = pb::SurfaceRequest {
        request: Some(pb::surface_request::Request::GetCodeView(
            pb::GetCodeViewRequest {
                path: "src/lib.txt".into(),
            },
        )),
    };
    let response =
        pb::SurfaceResponse::decode(services.handle(&request.encode_to_vec()).as_slice()).unwrap();
    assert!(response.ok, "{}", response.error);
    let view = response.code_view.as_ref().expect("code view present");
    assert_eq!(view.path, "src/lib.txt");
    assert_eq!(view.content_text, "trusted content");
    assert!(!view.content_sha256.is_empty());
    assert!(view.file_revision >= 1);

    // Mutate through the service: the SAME path now returns the new content
    // with a bumped revision — stale cached views are detectable.
    workspace
        .replace("src/lib.txt", b"updated content", view.file_revision)
        .unwrap();
    let response2 =
        pb::SurfaceResponse::decode(services.handle(&request.encode_to_vec()).as_slice()).unwrap();
    let view2 = response2.code_view.as_ref().unwrap();
    assert_ne!(view2.file_revision, view.file_revision, "revision bumped");
    assert_eq!(view2.content_text, "updated content");

    // Path safety: traversal through the code-view request is refused.
    let escape = pb::SurfaceRequest {
        request: Some(pb::surface_request::Request::GetCodeView(
            pb::GetCodeViewRequest {
                path: "../../etc/passwd".into(),
            },
        )),
    };
    let response3 =
        pb::SurfaceResponse::decode(services.handle(&escape.encode_to_vec()).as_slice()).unwrap();
    assert!(
        !response3.ok,
        "traversal must be refused: {}",
        response3.error
    );
}

#[test]
fn code_view_for_missing_file_is_an_error() {
    let mut ws_root = std::env::temp_dir();
    ws_root.push(format!("modbit-m29-ws2-{}", uuid::Uuid::now_v7().simple()));
    let workspace = Arc::new(WorkspaceFileService::open(&ws_root).unwrap());
    let mut db = std::env::temp_dir();
    db.push(format!(
        "modbit-m29-db2-{}.db",
        uuid::Uuid::now_v7().simple()
    ));
    let store = Arc::new(EventStore::open(&db).unwrap());
    let services = CoreServices::new(store).with_workspace(workspace);

    let request = pb::SurfaceRequest {
        request: Some(pb::surface_request::Request::GetCodeView(
            pb::GetCodeViewRequest {
                path: "missing.txt".into(),
            },
        )),
    };
    let response =
        pb::SurfaceResponse::decode(services.handle(&request.encode_to_vec()).as_slice()).unwrap();
    assert!(!response.ok);
}
