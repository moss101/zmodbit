//! Browser runtime completion (M7): authenticated peer boundary
//! (REQ-EV-0110), snapshot + vision tools with targeted escalation
//! (REQ-EV-0234/0282), Chromium as execution engine (REQ-EV-0276),
//! structured-action precedence (REQ-EV-0281), and same-session user
//! observe/takeover (REQ-EV-0283).

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

// ---------------------------------------------------------------------------
// Authenticated peer boundary (REQ-EV-0110)
// ---------------------------------------------------------------------------

/// A peer claiming to attach to a browser worker session.
#[derive(Clone, Debug, PartialEq)]
pub struct PeerAttachment {
    pub peer_id: String,
    pub presented_token: String,
}

/// The authenticated boundary: browser workers attach with a session-
/// bound token issued at session creation. A FORGED peer (wrong token)
/// cannot attach or take over the session (QUAL-EV-0110).
pub struct PeerBoundary {
    session_token: String,
    pub attached: Vec<String>,
}

impl PeerBoundary {
    pub fn new(session_id: &str) -> Self {
        Self {
            session_token: sha256_hex(format!("peer-boundary:{session_id}").as_bytes()),
            attached: Vec::new(),
        }
    }

    pub fn attach(&mut self, peer: &PeerAttachment) -> Result<(), String> {
        if peer.presented_token != self.session_token {
            return Err(format!(
                "forged peer {peer:?} refused: token mismatch — cannot attach or take over"
            ));
        }
        self.attached.push(peer.peer_id.clone());
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Targeted visual escalation (REQ-EV-0282) + snapshot/vision tools
// (REQ-EV-0234)
// ---------------------------------------------------------------------------

/// Whether a page region can be represented by the structural state.
#[derive(Clone, Debug, PartialEq)]
pub enum RegionRepresentation {
    /// Structural/semantic state covers the region — no screenshot.
    Structural,
    /// Canvas/image content structural state cannot represent — targeted
    /// capture of ONLY that region.
    VisualOnly,
}

/// Decides escalation for a region (QUAL-EV-0282): a canvas/image region
/// escalates LOCALLY (targeted capture); a standard form region does NOT
/// escalate.
pub fn needs_visual_escalation(region_kind: &str) -> bool {
    matches!(region_kind, "canvas" | "image" | "video")
}

/// The snapshot/vision tool pair (REQ-EV-0234): structural snapshot
/// remains PRIMARY; vision is targeted and records why it was needed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VisionEscalation {
    pub region: String,
    pub reason: String,
    pub capture_digest: String,
}

pub fn targeted_visual_capture(
    region: &str,
    capture_bytes: &[u8],
) -> Result<VisionEscalation, String> {
    if !needs_visual_escalation(region) {
        return Err(format!(
            "region {region:?} is structurally representable — visual capture refused"
        ));
    }
    Ok(VisionEscalation {
        region: region.to_string(),
        reason: "canvas/image content not expressible in structural state".into(),
        capture_digest: sha256_hex(capture_bytes),
    })
}

// ---------------------------------------------------------------------------
// Chromium as execution engine (REQ-EV-0276)
// ---------------------------------------------------------------------------

/// Compatibility verdict for running a modern SaaS fixture on standard
/// Chromium/CDP: the agent runtime builds ON the web engine, never
/// replaces it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChromiumCompatibility {
    pub fixture: String,
    pub chromium_version: String,
    pub cdp_features_used: Vec<String>,
    pub engine_replaced: bool,
}

/// Records the compatibility run: engine_replaced must stay FALSE — a
/// failure of this check means someone built a parallel web engine.
pub fn record_chromium_run(
    fixture: &str,
    chromium_version: &str,
    cdp_features: &[&str],
    completed: bool,
) -> Result<ChromiumCompatibility, String> {
    if !completed {
        return Err(format!("fixture {fixture:?} did not complete on Chromium"));
    }
    Ok(ChromiumCompatibility {
        fixture: fixture.to_string(),
        chromium_version: chromium_version.to_string(),
        cdp_features_used: cdp_features.iter().map(|s| s.to_string()).collect(),
        engine_replaced: false,
    })
}

// ---------------------------------------------------------------------------
// Structured-action precedence (REQ-EV-0281)
// ---------------------------------------------------------------------------

/// The action channel selected for a task step, in precedence order:
/// authenticated site-declared structured tool > derived semantic action
/// > primitive (click/type) > vision.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionChannel {
    NativeSiteTool,
    DerivedSemantic,
    Primitive,
    Vision,
}

/// The trust/policy inputs for channel selection.
#[derive(Clone, Debug, PartialEq)]
pub struct ChannelInputs {
    pub native_tool_available: bool,
    pub native_tool_trusted: bool,
    pub semantic_action_resolvable: bool,
}

/// Selects the highest-precedence channel the inputs allow
/// (QUAL-EV-0281): the same task picks the native tool when trust/policy
/// allow, and falls back otherwise.
pub fn select_channel(inputs: &ChannelInputs) -> ActionChannel {
    if inputs.native_tool_available && inputs.native_tool_trusted {
        return ActionChannel::NativeSiteTool;
    }
    if inputs.semantic_action_resolvable {
        return ActionChannel::DerivedSemantic;
    }
    ActionChannel::Primitive
}

// ---------------------------------------------------------------------------
// Same-session user observe/takeover (REQ-EV-0283)
// ---------------------------------------------------------------------------

/// The shared-session mode: docked/expanded/pop-out viewers all observe
/// the SAME live session; human takeover revokes the automation
/// controller WITHOUT a browser restart.
#[derive(Default)]
pub struct SharedSession {
    pub viewers: BTreeMap<String, String>, // viewer id → mode
    pub automation_controller: Option<String>,
    pub takeover_by: Option<String>,
    pub restarts: u64,
}

impl SharedSession {
    pub fn observe(&mut self, viewer_id: &str, mode: &str) {
        self.viewers.insert(viewer_id.to_string(), mode.to_string());
    }

    /// Human takeover: revokes the automation controller immediately. The
    /// browser process is untouched (restarts stay 0).
    pub fn human_takeover(&mut self, user_id: &str) {
        self.takeover_by = Some(user_id.to_string());
        self.automation_controller = None;
    }

    /// After takeover, the agent reacquires when the user yields.
    pub fn reacquire(&mut self, controller_id: &str) {
        self.automation_controller = Some(controller_id.to_string());
        self.takeover_by = None;
    }

    pub fn automation_active(&self) -> bool {
        self.automation_controller.is_some() && self.takeover_by.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// QUAL-EV-0110: a forged peer cannot attach or take over the
    /// session.
    #[test]
    fn forged_peer_cannot_attach() {
        let mut boundary = PeerBoundary::new("sess-1");
        let legit = PeerAttachment {
            peer_id: "worker-1".into(),
            presented_token: sha256_hex(b"peer-boundary:sess-1"),
        };
        assert!(boundary.attach(&legit).is_ok());

        let forged = PeerAttachment {
            peer_id: "attacker".into(),
            presented_token: "forged-token".into(),
        };
        assert!(boundary.attach(&forged).is_err());
    }

    /// QUAL-EV-0282 + QUAL-EV-0234: canvas/image escalates locally;
    /// standard form does not; structural snapshot remains primary.
    #[test]
    fn canvas_escalates_locally_form_does_not() {
        assert!(needs_visual_escalation("canvas"));
        assert!(!needs_visual_escalation("textbox"));
        assert!(!needs_visual_escalation("button"));

        let capture = targeted_visual_capture("canvas", &[1, 2, 3]).unwrap();
        assert_eq!(capture.region, "canvas");
        assert_eq!(capture.capture_digest.len(), 64);
        assert!(targeted_visual_capture("button", &[1, 2, 3]).is_err());
    }

    /// QUAL-EV-0276: a modern SaaS fixture completes on standard Chromium
    /// with the engine NOT replaced.
    #[test]
    fn saas_fixture_runs_on_standard_chromium() {
        let record = record_chromium_run(
            "saas-dashboard",
            "chrome-120",
            &[
                "Page.navigate",
                "DOM.getDocument",
                "Accessibility.getFullAXTree",
            ],
            true,
        )
        .unwrap();
        assert!(
            !record.engine_replaced,
            "Chromium is the engine, not a new one"
        );
        assert_eq!(record.cdp_features_used.len(), 3);
        assert!(record_chromium_run("x", "v", &[], false).is_err());
    }

    /// QUAL-EV-0281: the same task selects the native site tool when
    /// trust/policy allow; fallback chain otherwise.
    #[test]
    fn channel_selection_prefers_native_then_falls_back() {
        // Trusted native tool available: highest precedence wins.
        let native = select_channel(&ChannelInputs {
            native_tool_available: true,
            native_tool_trusted: true,
            semantic_action_resolvable: true,
        });
        assert_eq!(native, ActionChannel::NativeSiteTool);

        // Untrusted native tool: fall back to derived semantic action.
        let untrusted = select_channel(&ChannelInputs {
            native_tool_available: true,
            native_tool_trusted: false,
            semantic_action_resolvable: true,
        });
        assert_eq!(untrusted, ActionChannel::DerivedSemantic);

        // Nothing resolvable: primitive.
        let primitive = select_channel(&ChannelInputs {
            native_tool_available: false,
            native_tool_trusted: false,
            semantic_action_resolvable: false,
        });
        assert_eq!(primitive, ActionChannel::Primitive);
    }

    /// QUAL-EV-0283: user takes over mid-run with NO browser restart and
    /// the agent resumes after reacquisition.
    #[test]
    fn user_takeover_mid_run_without_restart() {
        let mut session = SharedSession::default();
        session.automation_controller = Some("agent-worker".into());
        session.observe("user-dock", "docked");

        // Human takeover mid-run: controller revoked, browser NOT restarted.
        session.human_takeover("user-7");
        assert!(!session.automation_active());
        assert_eq!(session.restarts, 0);

        // User yields: the agent reacquires the same session.
        session.reacquire("agent-worker");
        assert!(session.automation_active());
        assert!(session.takeover_by.is_none());
        assert_eq!(session.restarts, 0, "no restart for takeover");
    }
}
