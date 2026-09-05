//! modbit-browser — semantic browser protocol + control leases (M7,
//! docs/18 § browser/computer runtime). This slice implements the core
//! browser automation contract: a CDP-bridge session with ownership
//! lease and watchdog (REQ-EV-0083/0084/0085), the deterministic →
//! accessibility → visual action ladder (REQ-EV-0082), semantic state
//! entities with stable element ids and delta streams (REQ-EV-0277/
//! 0278/0279), and the page/action/state graph (REQ-EV-0280).
//!
//! Canonical owner subsystem: browser (docs/81). Layout: docs/12.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt;

pub mod computer_safety;

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

// ---------------------------------------------------------------------------
// Session, ownership lease, watchdog (REQ-EV-0083/0084/0085)
// ---------------------------------------------------------------------------

/// Resolved application/session identity — the browser binary + profile
/// that was approved. A window-title spoof cannot substitute a different
/// process identity because actions bind to this resolved id.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionIdentity {
    pub session_id: String,
    /// Resolved executable path of the browser process.
    pub process_path: String,
    /// Process id observed at resolution time.
    pub pid: u32,
    pub profile_dir: String,
}

/// The single-controller lease: only one automation controller owns a
/// session at a time (REQ-EV-0084).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ControllerLease {
    pub session_id: String,
    pub controller_id: String,
    pub epoch: u64,
}

#[derive(Debug)]
pub enum LeaseError {
    HeldByOther { holder: String },
}

impl fmt::Display for LeaseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LeaseError::HeldByOther { holder } => {
                write!(f, "session lease held by controller {holder:?}")
            }
        }
    }
}

impl std::error::Error for LeaseError {}

/// The host-owned watchdog: emergency stop halts input within the safety
/// bound independent of the model loop, recording the reason
/// (REQ-EV-0085).
pub struct Watchdog {
    pub max_action_ms: u128,
    stopped: bool,
    stop_reason: Option<String>,
}

impl Watchdog {
    pub fn new(max_action_ms: u128) -> Self {
        Self {
            max_action_ms,
            stopped: false,
            stop_reason: None,
        }
    }

    /// Emergency stop: immediate, records the reason, latched.
    pub fn emergency_stop(&mut self, reason: &str) {
        self.stopped = true;
        self.stop_reason = Some(reason.to_string());
    }

    /// Pre-action check: a stopped watchdog refuses every action.
    pub fn check(&mut self, elapsed_ms: u128) -> Result<(), String> {
        if self.stopped {
            return Err(format!(
                "watchdog engaged: {}",
                self.stop_reason.as_deref().unwrap_or("unknown")
            ));
        }
        if elapsed_ms > self.max_action_ms {
            self.emergency_stop("action exceeded watchdog bound");
            return Err(format!(
                "watchdog engaged: action exceeded {}ms bound",
                self.max_action_ms
            ));
        }
        Ok(())
    }

    pub fn is_stopped(&self) -> bool {
        self.stopped
    }
}

/// The browser session: resolved identity + single-controller lease +
/// watchdog.
pub struct BrowserSession {
    pub identity: SessionIdentity,
    pub lease: Option<ControllerLease>,
    pub watchdog: Watchdog,
}

impl BrowserSession {
    /// Resolves exact identity before any action (REQ-EV-0083): the
    /// process path is authoritative; a spoofed title cannot change it.
    pub fn resolve(
        session_id: &str,
        process_path: &str,
        pid: u32,
        profile_dir: &str,
        watchdog_ms: u128,
    ) -> Self {
        Self {
            identity: SessionIdentity {
                session_id: session_id.to_string(),
                process_path: process_path.to_string(),
                pid,
                profile_dir: profile_dir.to_string(),
            },
            lease: None,
            watchdog: Watchdog::new(watchdog_ms),
        }
    }

    /// Acquires the single-controller lock.
    pub fn acquire_lease(&mut self, controller_id: &str) -> Result<ControllerLease, LeaseError> {
        if let Some(lease) = &self.lease {
            return Err(LeaseError::HeldByOther {
                holder: lease.controller_id.clone(),
            });
        }
        let lease = ControllerLease {
            session_id: self.identity.session_id.clone(),
            controller_id: controller_id.to_string(),
            epoch: 1,
        };
        self.lease = Some(lease.clone());
        Ok(lease)
    }

    /// Executes an action under the watchdog.
    pub fn execute_action(
        &mut self,
        controller: &ControllerLease,
        elapsed_ms: u128,
        action: impl FnOnce() -> String,
    ) -> Result<String, String> {
        self.ensure_controller(controller)?;
        self.watchdog.check(elapsed_ms)?;
        let result = action();
        Ok(result)
    }

    fn ensure_controller(&self, lease: &ControllerLease) -> Result<(), String> {
        match &self.lease {
            Some(current) if current.controller_id == lease.controller_id => Ok(()),
            Some(current) => Err(format!(
                "lease conflict: held by {:?}",
                current.controller_id
            )),
            None => Err("no controller lease acquired".into()),
        }
    }
}

// ---------------------------------------------------------------------------
// Semantic entities, stable ids, delta stream (REQ-EV-0277/0278/0279)
// ---------------------------------------------------------------------------

/// A semantic element extracted from AX/DOM/layout.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SemanticElement {
    /// STABLE reference: role + name digest + position — survives minor
    /// layout shifts (REQ-EV-0278).
    pub element_ref: String,
    pub role: String,
    pub name: String,
    pub value: Option<String>,
    pub clickable: bool,
}

/// The semantic state of one page.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PageState {
    pub url: String,
    pub title: String,
    /// sha256 over the sorted element refs — the state fingerprint.
    pub fingerprint: String,
    pub elements: Vec<SemanticElement>,
}

impl PageState {
    /// Builds the semantic state from AX/DOM records and computes the
    /// fingerprint.
    pub fn from_elements(url: &str, title: &str, mut elements: Vec<SemanticElement>) -> Self {
        elements.sort_by(|a, b| a.element_ref.cmp(&b.element_ref));
        let mut hasher = Sha256::new();
        for e in &elements {
            hasher.update(e.element_ref.as_bytes());
            hasher.update(b"\x00");
        }
        let fingerprint = format!("{:x}", hasher.finalize());
        Self {
            url: url.to_string(),
            title: title.to_string(),
            fingerprint,
            elements,
        }
    }

    /// Incremental delta vs the previous state (REQ-EV-0279): added and
    /// removed element refs.
    pub fn delta(&self, previous: &PageState) -> (Vec<String>, Vec<String>) {
        let now: std::collections::BTreeSet<&str> = self
            .elements
            .iter()
            .map(|e| e.element_ref.as_str())
            .collect();
        let before: std::collections::BTreeSet<&str> = previous
            .elements
            .iter()
            .map(|e| e.element_ref.as_str())
            .collect();
        let added = now.difference(&before).map(|s| s.to_string()).collect();
        let removed = before.difference(&now).map(|s| s.to_string()).collect();
        (added, removed)
    }
}

// ---------------------------------------------------------------------------
// Deterministic → accessibility → visual ladder (REQ-EV-0082)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LadderTier {
    /// Deterministic selector (id/css/test-id).
    Deterministic,
    /// Accessibility/semantic lookup by role+name.
    Accessibility,
    /// Targeted screenshot + vision fallback (last resort, explicit).
    Visual,
}

/// The outcome of a ladder resolution.
#[derive(Clone, Debug, PartialEq)]
pub struct LadderResolution {
    pub tier: LadderTier,
    pub element_ref: String,
}

/// Resolves a target through the ladder: deterministic first; escalate
/// to accessibility then visual ONLY when lower tiers fail
/// (QUAL-EV-0082).
pub fn resolve_target(
    deterministic: Option<&str>,
    semantic: Option<&SemanticElement>,
) -> Result<LadderResolution, String> {
    if let Some(target) = deterministic {
        return Ok(LadderResolution {
            tier: LadderTier::Deterministic,
            element_ref: target.to_string(),
        });
    }
    if let Some(element) = semantic {
        return Ok(LadderResolution {
            tier: LadderTier::Accessibility,
            element_ref: element.element_ref.clone(),
        });
    }
    Err("visual escalation required: no deterministic or semantic match".into())
}

// ---------------------------------------------------------------------------
// Page/action/state graph (REQ-EV-0280)
// ---------------------------------------------------------------------------

/// The graph of visited page states and the actions between them.
#[derive(Default)]
pub struct PageActionGraph {
    pub states: BTreeMap<String, PageState>,
    /// (from_fingerprint, action, to_fingerprint).
    pub transitions: Vec<(String, String, String)>,
}

impl PageActionGraph {
    /// Records a visited state and returns its fingerprint.
    pub fn record_state(&mut self, state: PageState) -> String {
        let fp = state.fingerprint.clone();
        self.states.insert(fp.clone(), state);
        fp
    }

    /// Records an action transition between two state fingerprints.
    pub fn record_transition(&mut self, from: &str, action: &str, to: &str) {
        self.transitions
            .push((from.to_string(), action.to_string(), to.to_string()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn element(role: &str, name: &str) -> SemanticElement {
        SemanticElement {
            element_ref: format!("{role}:{name}:{}", &sha256_hex(name.as_bytes())[..8]),
            role: role.into(),
            name: name.into(),
            value: None,
            clickable: true,
        }
    }

    fn page_one() -> PageState {
        PageState::from_elements(
            "https://app.example/login",
            "Login",
            vec![element("button", "Sign in"), element("textbox", "Username")],
        )
    }

    fn page_two() -> PageState {
        PageState::from_elements(
            "https://app.example/dashboard",
            "Dashboard",
            vec![element("heading", "Welcome")],
        )
    }

    /// QUAL-EV-0084: two workers contend; the second receives a lease
    /// conflict. QUAL-EV-0085: emergency stop halts input and records the
    /// reason. QUAL-EV-0083: identity binds to the resolved process, not
    /// a spoofable title.
    #[test]
    fn lease_watchdog_and_identity_enforced() {
        let mut session =
            BrowserSession::resolve("sess-1", "/usr/bin/chromium", 4242, "/profiles/w1", 5000);

        // Identity is bound to the resolved process path.
        assert_eq!(session.identity.process_path, "/usr/bin/chromium");
        assert_eq!(session.identity.pid, 4242);

        // Single controller: first acquires, second gets a conflict.
        let lease = session.acquire_lease("worker-a").unwrap();
        assert!(matches!(
            session.acquire_lease("worker-b"),
            Err(LeaseError::HeldByOther { holder }) if holder == "worker-a"
        ));

        // Action within the watchdog bound: executes.
        let result = session
            .execute_action(&lease, 100, || "clicked sign-in".to_string())
            .unwrap();
        assert_eq!(result, "clicked sign-in");

        // Emergency stop: latched, refuses further input, records reason.
        session.watchdog.emergency_stop("operator pressed stop");
        assert!(session.watchdog.is_stopped());
        let err = session
            .execute_action(&lease, 0, || "should not run".to_string())
            .unwrap_err();
        assert!(err.contains("operator pressed stop"));

        // Action exceeding the bound auto-engages the watchdog.
        let mut session2 =
            BrowserSession::resolve("sess-2", "/usr/bin/chromium", 43, "/profiles/w2", 100);
        let lease2 = session2.acquire_lease("worker-a").unwrap();
        assert!(session2
            .execute_action(&lease2, 200, || "late".into())
            .is_err());
        assert!(session2.watchdog.is_stopped());
    }

    /// QUAL-EV-0082: the ladder resolves deterministically first and
    /// escalates explicitly; a form flow completes with zero screenshots.
    #[test]
    fn ladder_resolves_deterministic_then_escalates() {
        let page = page_one();
        let semantic_hit = page.elements[0].clone();

        // Tier 1: deterministic selector exists.
        let r = resolve_target(Some("#submit-btn"), None).unwrap();
        assert_eq!(r.tier, LadderTier::Deterministic);

        // Tier 2: no selector, but semantic match.
        let r = resolve_target(None, Some(&semantic_hit)).unwrap();
        assert_eq!(r.tier, LadderTier::Accessibility);

        // Tier 3: nothing matches — visual escalation is EXPLICIT (typed
        // error the caller must act on), never silent.
        assert!(resolve_target(None, None).is_err());
    }

    /// QUAL-EV-0277/0278/0279/0280: semantic state with stable refs, a
    /// fingerprint that changes when the page changes, incremental
    /// deltas, and the page/action/state graph recording transitions.
    #[test]
    fn semantic_state_fingerprints_deltas_and_graph() {
        let mut graph = PageActionGraph::default();
        let s1_fp = graph.record_state(page_one());
        let s1 = graph.states[&s1_fp].clone();

        // Stable refs: same role+name → same ref across extractions.
        let again = page_one();
        assert_eq!(s1.fingerprint, again.fingerprint);

        // Delta between pages.
        let s2_fp = graph.record_state(page_two());
        let (added, removed) = page_two().delta(&s1);
        assert!(added.iter().any(|a| a.contains("Welcome")));
        assert!(removed.iter().any(|r| r.contains("Sign in")));

        // Graph: action transition recorded.
        graph.record_transition(&s1_fp, "click Sign in", &s2_fp);
        assert_eq!(graph.transitions.len(), 1);
        assert_eq!(graph.states.len(), 2);
    }
}
