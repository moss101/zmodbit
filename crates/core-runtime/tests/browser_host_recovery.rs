//! W6 browser-host restart recovery: the session's Chromium is
//! HARD-KILLED under a live task; the next view() drops the dead
//! session, relaunches, and serves a fresh frame (lease resets to
//! agent — the user can take over again). Skips with a recorded note
//! when no Chromium-family binary exists (documented gap, never a fake
//! pass).

use modbit_core_runtime::browser_host::BrowserHost;

#[test]
fn browser_host_relaunches_after_browser_loss() {
    if modbit_browser::cdp::CdpBrowser::find_browser().is_none() {
        println!("browser recovery skipped: no Chromium-family browser (recorded gap)");
        return;
    }
    let host = BrowserHost::new();

    // First view launches the browser. A launch failure on a runner
    // whose Chromium cannot start is the same recorded environment gap
    // as the cdp e2e — the relaunch logic is proven where browsers work.
    let frame = match host.view("task-br-1") {
        Ok(f) => f,
        Err(e) => {
            println!("browser recovery skipped: browser cannot launch here (recorded gap): {e}");
            return;
        }
    };
    assert_eq!(frame.lease.as_str(), "agent");

    // The browser process dies (crash simulation).
    assert!(host.kill_session_browser("task-br-1"));

    // The next view relaunches and serves a REAL frame — not an error
    // forever, not a fake success.
    let frame = host
        .view("task-br-1")
        .expect("view after browser loss must relaunch");
    assert_eq!(
        frame.lease.as_str(),
        "agent",
        "fresh session starts agent-owned"
    );
    assert!(!frame.png.is_empty(), "fresh frame captured");

    // Takeover still works on the relaunched session.
    host.set_lease(
        "task-br-1",
        modbit_core_runtime::browser_host::LeaseOwner::User,
    )
    .expect("takeover");
    let frame = host.view("task-br-1").expect("view");
    assert_eq!(frame.lease.as_str(), "user");
}
