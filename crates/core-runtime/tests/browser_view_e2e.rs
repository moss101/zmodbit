//! Browser live-view + takeover E2E (Phase 7 residual): the SAME live
//! Chromium session surfaced to the pane — GetBrowserView observes,
//! SetBrowserLease flips the controller lease (takeover = user, return =
//! agent). On runners without a usable browser the launch failure is the
//! recorded gap (typed error, no hang); the full flow runs where a
//! browser exists.

use std::path::PathBuf;
use std::sync::Arc;

use prost::Message as _;

use modbit_core_runtime::CoreServices;
use modbit_event_store::EventStore;
use modbit_protocol::modbit::protocol::v1 as pb;

fn tempdir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "modbit-bv-{tag}-{}",
        uuid::Uuid::now_v7().simple()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn roundtrip(services: &CoreServices, request: pb::surface_request::Request) -> pb::SurfaceResponse {
    let bytes = pb::SurfaceRequest {
        request: Some(request),
    }
    .encode_to_vec();
    let response = services.handle(&bytes);
    pb::SurfaceResponse::decode(response.as_slice()).unwrap()
}

#[test]
fn browser_view_launches_observes_and_takes_over() {
    let db = tempdir("db");
    let store = Arc::new(EventStore::open(&db.join("core.db")).unwrap());
    let services = CoreServices::new(store);

    let resp = roundtrip(
        &services,
        pb::surface_request::Request::GetBrowserView(pb::GetBrowserViewRequest {
            task_id: "t-live-1".into(),
        }),
    );
    if !resp.ok {
        // Recorded gap: no usable browser on this machine. The typed
        // error is the contract; nothing hangs, nothing fabricates.
        assert!(
            resp.error.contains("browser") || resp.error.contains("Chromium"),
            "typed launch error: {}",
            resp.error
        );
        println!("browser_view e2e skipped (recorded gap): {}", resp.error);
        return;
    }
    let view = resp.browser_view.expect("view");
    assert_eq!(view.task_id, "t-live-1");
    assert!(!view.png_base64.is_empty(), "a real frame is captured");
    assert_eq!(view.lease, "agent", "agent holds the lease by default");

    // Takeover: the user flips the lease.
    let resp = roundtrip(
        &services,
        pb::surface_request::Request::SetBrowserLease(pb::SetBrowserLeaseCommand {
            task_id: "t-live-1".into(),
            owner: "user".into(),
        }),
    );
    assert!(resp.ok, "{:?}", resp.error);
    let view = resp.browser_view.expect("view after takeover");
    assert_eq!(view.lease, "user");

    // Return control.
    let resp = roundtrip(
        &services,
        pb::surface_request::Request::SetBrowserLease(pb::SetBrowserLeaseCommand {
            task_id: "t-live-1".into(),
            owner: "agent".into(),
        }),
    );
    assert_eq!(resp.browser_view.expect("view").lease, "agent");

    // Invalid owner: typed refusal.
    let resp = roundtrip(
        &services,
        pb::surface_request::Request::SetBrowserLease(pb::SetBrowserLeaseCommand {
            task_id: "t-live-1".into(),
            owner: "nobody".into(),
        }),
    );
    assert!(!resp.ok, "invalid owner must refuse");
    assert!(resp.error.contains("lease owner"), "{}", resp.error);
}
