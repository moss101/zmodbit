//! Browser host (Phase 7 residual): ONE live Chromium session per task,
//! held by the Core and shared by the agent and the user-visible pane
//! (docs/60 step 12: "the same live Chromium session"). The session is
//! lazy — the first view request launches headless Chromium via the CDP
//! bridge. The CONTROLLER LEASE decides who drives: the agent by
//! default; a user takeover flips the lease, and agent-side browser
//! actions must check it before acting (the enforcing consumer is the
//! browser tool path; the lease is the boundary it consults —
//! crates/browser ControllerLease semantics).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use modbit_browser::cdp::CdpBrowser;

#[derive(Debug)]
pub struct BrowserHostError {
    pub message: String,
}

impl std::fmt::Display for BrowserHostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "browser host: {}", self.message)
    }
}

fn err(message: impl Into<String>) -> BrowserHostError {
    BrowserHostError {
        message: message.into(),
    }
}

/// Who holds the controller lease for a task's browser.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeaseOwner {
    Agent,
    User,
}

impl LeaseOwner {
    pub fn as_str(self) -> &'static str {
        match self {
            LeaseOwner::Agent => "agent",
            LeaseOwner::User => "user",
        }
    }
}

struct Session {
    browser: CdpBrowser,
    lease: LeaseOwner,
}

/// One captured frame for the live view.
pub struct BrowserFrame {
    pub url: String,
    pub title: String,
    pub png: Vec<u8>,
    pub lease: LeaseOwner,
}

#[derive(Default)]
pub struct BrowserHost {
    sessions: Mutex<HashMap<String, Session>>,
    browser_bin: Option<PathBuf>,
}

impl BrowserHost {
    pub fn new() -> Self {
        Self::default()
    }

    /// Launch behavior uses the same discovery as the CDP bridge
    /// (MODBIT_BROWSER_BIN or per-OS paths).
    pub fn with_browser_bin(mut self, bin: PathBuf) -> Self {
        self.browser_bin = Some(bin);
        self
    }

    fn launch(&self) -> Result<CdpBrowser, BrowserHostError> {
        let bin = self
            .browser_bin
            .clone()
            .or_else(CdpBrowser::find_browser)
            .ok_or_else(|| err("no Chromium-family browser available on this machine"))?;
        CdpBrowser::launch(&bin).map_err(|e| err(e.to_string()))
    }

    /// Captures the live frame for a task: launch-on-demand without poisoning the
    /// session map on failure. If the session's browser process has died
    /// (browser-host crash/restart, W6), the dead session is dropped and
    /// the browser relaunches ONCE — the frame is served from the fresh
    /// session (lease resets to agent: the user can take over again).
    pub fn view(&self, task_id: &str) -> Result<BrowserFrame, BrowserHostError> {
        let mut sessions = self.sessions.lock().map_err(|_| err("poisoned"))?;
        if !sessions.contains_key(task_id) {
            let browser = self.launch()?;
            sessions.insert(
                task_id.to_string(),
                Session {
                    browser,
                    lease: LeaseOwner::Agent,
                },
            );
        }
        let snapshot_result = sessions
            .get_mut(task_id)
            .expect("just inserted")
            .browser
            .snapshot();
        let state = match snapshot_result {
            Ok(state) => state,
            // Dead session: drop it and relaunch once in place.
            Err(dead) => {
                let _ = sessions.remove(task_id);
                let browser = self
                    .launch()
                    .map_err(|e| err(format!("relaunch after browser loss failed: {dead}; {e}")))?;
                sessions.insert(
                    task_id.to_string(),
                    Session {
                        browser,
                        lease: LeaseOwner::Agent,
                    },
                );
                sessions
                    .get_mut(task_id)
                    .expect("just inserted")
                    .browser
                    .snapshot()
                    .map_err(|e| err(format!("relaunch after browser loss failed: {dead}; {e}")))?
            }
        };
        let session = sessions.get_mut(task_id).expect("present");
        let png = session.browser.capture().map_err(|e| err(e.to_string()))?;
        Ok(BrowserFrame {
            url: state.url,
            title: state.title,
            png,
            lease: session.lease,
        })
    }

    /// Recovery-testing hook (M8/W6 browser-host restart): hard-kills
    /// the task session's browser process, simulating a browser crash.
    /// Returns false when no session exists. The NEXT view() must drop
    /// the dead session, relaunch, and serve the fresh frame.
    pub fn kill_session_browser(&self, task_id: &str) -> bool {
        let mut sessions = match self.sessions.lock() {
            Ok(s) => s,
            Err(_) => return false,
        };
        match sessions.get_mut(task_id) {
            Some(session) => {
                session.browser.kill_child();
                true
            }
            None => false,
        }
    }

    /// Takeover (owner = User) or return control (owner = Agent). The
    /// flip is idempotent and always succeeds while the session lives.
    pub fn set_lease(&self, task_id: &str, owner: LeaseOwner) -> Result<(), BrowserHostError> {
        let mut sessions = self.sessions.lock().map_err(|_| err("poisoned"))?;
        if !sessions.contains_key(task_id) {
            let browser = self.launch()?;
            sessions.insert(
                task_id.to_string(),
                Session {
                    browser,
                    lease: owner,
                },
            );
            return Ok(());
        }
        sessions.get_mut(task_id).expect("checked").lease = owner;
        Ok(())
    }

    pub fn lease_of(&self, task_id: &str) -> Option<LeaseOwner> {
        self.sessions
            .lock()
            .ok()
            .and_then(|s| s.get(task_id).map(|sess| sess.lease))
    }

    /// Shuts a task's session down (task completed/cancelled cleanup).
    pub fn release(&self, task_id: &str) {
        if let Ok(mut sessions) = self.sessions.lock() {
            if let Some(sess) = sessions.remove(task_id) {
                sess.browser.shutdown();
            }
        }
    }
}
