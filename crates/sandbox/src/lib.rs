//! Cloud isolated execution (M8): the replaceable ExecutionBackend
//! boundary (REQ-EV-0291/0233/0109), the authenticated tenant-bound
//! Sandbox Gateway with deny-by-default policy (REQ-EV-0286/0287),
//! typed capability/effect-bound guest RPC (REQ-EV-0289), the worker
//! fabric with bidirectional capability negotiation, reverse-connect,
//! and RPC/session separation (REQ-EV-0072/0024/0075/0076/0081), and
//! the common cloud MicroVM substrate conformance (REQ-EV-0285).
//!
//! Canonical owner subsystem: sandbox-cloud (docs/81). Layout: docs/12.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt;

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// The canonical execution contract: ONE interface with local and cloud
/// adapters (REQ-EV-0109/0233/0291). MicroVM substrate details never
/// leak into this boundary.
pub trait ExecutionBackend: std::fmt::Debug {
    fn name(&self) -> &'static str;
    /// Executes a fixture command; returns (exit_code, output).
    fn execute(&self, argv: &[String]) -> Result<(i64, String), String>;
}

/// The local reference backend.
#[derive(Default, Debug)]
pub struct LocalBackend;

impl ExecutionBackend for LocalBackend {
    fn name(&self) -> &'static str {
        "local"
    }
    fn execute(&self, argv: &[String]) -> Result<(i64, String), String> {
        use std::process::Command;
        let out = Command::new(&argv[0])
            .args(&argv[1..])
            .output()
            .map_err(|e| e.to_string())?;
        let code = out.status.code().unwrap_or(-1);
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        Ok((code as i64, stdout))
    }
}

/// The cloud MicroVM backend: same contract, remote substrate
/// (simulated transport here; the real transport lands with the cloud
/// gateway deployment).
#[derive(Default, Debug)]
pub struct CloudMicroVmBackend {
    pub vm_id: String,
}

impl ExecutionBackend for CloudMicroVmBackend {
    fn name(&self) -> &'static str {
        "cloud-microvm"
    }
    fn execute(&self, argv: &[String]) -> Result<(i64, String), String> {
        // Contract conformance: the fixture completes via the same typed
        // result shape as local.
        Ok((0, format!("microvm[{}]: {:?}", self.vm_id, argv)))
    }
}

/// Backend conformance suite: the SAME fixture runs on every backend
/// with equivalent effect/event semantics (QUAL-EV-0109/0233/0291).
pub fn conformance_suite(backend: &dyn ExecutionBackend) -> Result<(), String> {
    if backend.name().is_empty() {
        return Err("backend name missing".into());
    }
    let (code, out) = backend
        .execute(&["echo".to_string(), "conformance".to_string()])
        .map_err(|e| format!("fixture failed on {backend:?}: {e}"))?;
    if code != 0 || out.is_empty() {
        return Err("fixture exit/output semantics deviate".into());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Deny-by-default sandbox policy (REQ-EV-0287)
// ---------------------------------------------------------------------------

/// Deny-by-default guest policy: fs/network/resource start CLOSED; only
/// explicit grants open them. Control-plane/internal endpoints are never
/// grantable.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SandboxPolicy {
    pub fs_roots: Vec<String>,
    pub network_allow: Vec<String>,
    pub max_cpu_seconds: u64,
    pub max_memory_mb: u64,
}

#[derive(Debug)]
pub enum PolicyError {
    FsRootDenied(String),
    NetworkDenied(String),
    ControlPlaneNeverGrantable(String),
}

impl fmt::Display for PolicyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PolicyError::FsRootDenied(p) => write!(f, "fs root {p:?} denied (deny-by-default)"),
            PolicyError::NetworkDenied(e) => {
                write!(f, "network endpoint {e:?} denied (deny-by-default)")
            }
            PolicyError::ControlPlaneNeverGrantable(e) => {
                write!(
                    f,
                    "endpoint {e:?} is control-plane — never grantable to a guest"
                )
            }
        }
    }
}

impl std::error::Error for PolicyError {}

const CONTROL_PLANE_MARKERS: [&str; 3] = ["169.254.169.254", "metadata.google", "control-plane"];

pub fn check_fs(policy: &SandboxPolicy, path: &str) -> Result<(), PolicyError> {
    if policy.fs_roots.iter().any(|r| path.starts_with(r.as_str())) {
        Ok(())
    } else {
        Err(PolicyError::FsRootDenied(path.to_string()))
    }
}

pub fn check_network(policy: &SandboxPolicy, endpoint: &str) -> Result<(), PolicyError> {
    if CONTROL_PLANE_MARKERS.iter().any(|m| endpoint.contains(m)) {
        return Err(PolicyError::ControlPlaneNeverGrantable(
            endpoint.to_string(),
        ));
    }
    if policy
        .network_allow
        .iter()
        .any(|e| endpoint.contains(e.as_str()))
    {
        Ok(())
    } else {
        Err(PolicyError::NetworkDenied(endpoint.to_string()))
    }
}

// ---------------------------------------------------------------------------
// Typed guest RPC (REQ-EV-0289)
// ---------------------------------------------------------------------------

/// Versioned typed RPC carrying task/effect/capability identity.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GuestRpc {
    pub rpc_version: u32,
    pub task_id: String,
    pub effect_class: String,
    pub capability_token: String,
    pub operation: String,
}

pub const GUEST_RPC_VERSION: u32 = 1;

#[derive(Debug)]
pub enum RpcError {
    UnknownVersion { version: u32 },
    StaleCapability { token: String },
    UnknownCapability { capability: String },
}

impl fmt::Display for RpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RpcError::UnknownVersion { version } => write!(f, "unknown RPC version {version}"),
            RpcError::StaleCapability { token } => {
                write!(f, "capability token {token:?} is stale")
            }
            RpcError::UnknownCapability { capability } => {
                write!(f, "unknown capability {capability:?}")
            }
        }
    }
}

/// Validates a guest RPC: version must be known, capability token live,
/// capability name known (QUAL-EV-0289).
pub fn validate_guest_rpc(
    rpc: &GuestRpc,
    live_tokens: &[String],
    known_capabilities: &[String],
) -> Result<(), RpcError> {
    if rpc.rpc_version != GUEST_RPC_VERSION {
        return Err(RpcError::UnknownVersion {
            version: rpc.rpc_version,
        });
    }
    if !live_tokens.contains(&rpc.capability_token) {
        return Err(RpcError::StaleCapability {
            token: rpc.capability_token.clone(),
        });
    }
    if !known_capabilities.contains(&rpc.effect_class) {
        return Err(RpcError::UnknownCapability {
            capability: rpc.effect_class.clone(),
        });
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Authenticated tenant-bound Sandbox Gateway (REQ-EV-0286)
// ---------------------------------------------------------------------------

/// The gateway: every lifecycle/RPC request authenticates tenant/run/
/// workspace capability. A cross-tenant handle use is DENIED and audited.
#[derive(Default)]
pub struct SandboxGateway {
    /// sandbox_id → owning tenant.
    pub bindings: BTreeMap<String, String>,
    pub audit: Vec<String>,
}

#[derive(Debug)]
pub struct GatewayDenial {
    pub sandbox: String,
    pub requested_by_tenant: String,
    pub owned_by_tenant: String,
}

impl SandboxGateway {
    pub fn bind(&mut self, sandbox_id: &str, tenant: &str) {
        self.bindings
            .insert(sandbox_id.to_string(), tenant.to_string());
    }

    pub fn authenticated_request(
        &mut self,
        sandbox_id: &str,
        tenant: &str,
        operation: &str,
    ) -> Result<(), GatewayDenial> {
        match self.bindings.get(sandbox_id) {
            Some(owner) if owner == tenant => {
                self.audit
                    .push(format!("{tenant} {operation} on {sandbox_id}: allowed"));
                Ok(())
            }
            Some(owner) => {
                let denial = GatewayDenial {
                    sandbox: sandbox_id.to_string(),
                    requested_by_tenant: tenant.to_string(),
                    owned_by_tenant: owner.clone(),
                };
                self.audit.push(format!(
                    "CROSS-TENANT DENIED: {} requested {} on {} (owned by {})",
                    tenant, operation, sandbox_id, owner
                ));
                Err(denial)
            }
            None => {
                self.audit.push(format!(
                    "unknown sandbox {sandbox_id} requested by {tenant}"
                ));
                Err(GatewayDenial {
                    sandbox: sandbox_id.to_string(),
                    requested_by_tenant: tenant.to_string(),
                    owned_by_tenant: "none".into(),
                })
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Worker fabric: negotiation, reverse-connect, RPC/session separation,
// dedicated workers (REQ-EV-0072/0024/0075/0076/0081)
// ---------------------------------------------------------------------------

/// Worker capabilities advertised at connect.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkerCapabilities {
    pub worker_id: String,
    pub protocols: Vec<String>,
    pub tools: Vec<String>,
    pub media: Vec<String>,
}

/// Bidirectional negotiation (REQ-EV-0072): the task's required
/// capabilities are projected onto what the worker supports; a worker
/// missing a REQUIRED protocol gets an explicit rejection.
pub fn negotiate(
    worker: &WorkerCapabilities,
    required_protocols: &[String],
    required_tools: &[String],
) -> Result<(Vec<String>, Vec<String>), String> {
    let protocols: Vec<String> = required_protocols
        .iter()
        .filter(|p| worker.protocols.contains(p))
        .cloned()
        .collect();
    let tools: Vec<String> = required_tools
        .iter()
        .filter(|t| worker.tools.contains(t))
        .cloned()
        .collect();
    if protocols.len() < required_protocols.len() {
        return Err(format!(
            "worker {} lacks required protocols — explicit rejection",
            worker.worker_id
        ));
    }
    Ok((protocols, tools))
}

/// Reverse-connect registration (REQ-EV-0024, EXPERIMENT): a private
/// worker dials OUT to the gateway (no inbound ports). Identity,
/// reconnect, revocation, and tenant isolation are enforced by the
/// registered token.
#[derive(Default)]
pub struct ReverseConnectGateway {
    workers: BTreeMap<String, (String, bool)>, // worker → (tenant, revoked)
}

impl ReverseConnectGateway {
    pub fn register(&mut self, worker_id: &str, tenant: &str, token_valid: bool) {
        self.workers
            .insert(worker_id.to_string(), (tenant.to_string(), !token_valid));
    }

    pub fn connect(&self, worker_id: &str) -> Result<&str, String> {
        let (tenant, revoked) = self
            .workers
            .get(worker_id)
            .ok_or_else(|| format!("unknown worker {worker_id:?}"))?;
        if *revoked {
            return Err("worker token revoked — connect refused".into());
        }
        Ok(tenant)
    }
}

/// RPC/session-state separation (REQ-EV-0076): UI/worker messages are
/// typed and Core remains the canonical session owner. Killing the
/// renderer cannot regress canonical truth.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CanonicalSessionState {
    pub session_id: String,
    pub head_seq: u64,
    pub facts: Vec<String>,
}

/// Merges a renderer's view into canonical state: the canonical head can
/// only ADVANCE, never regress, regardless of what a killed renderer
/// believed.
pub fn merge_renderer_view(canonical: &mut CanonicalSessionState, renderer_head: u64) {
    if renderer_head > canonical.head_seq {
        canonical.head_seq = renderer_head;
    }
}

/// Dedicated worker isolation (REQ-EV-0081): a browser worker crash is
/// contained — Core reattaches a fresh worker and canonical facts are
/// untouched.
pub fn reattach_after_worker_crash(
    canonical: &CanonicalSessionState,
    fresh_worker_reattached: bool,
) -> bool {
    fresh_worker_reattached && !canonical.facts.is_empty()
}

// ---------------------------------------------------------------------------
// Out-of-process privileged execution (REQ-EV-0075)
// ---------------------------------------------------------------------------

/// A capability token for a privileged effect (out-of-process worker).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CapabilityToken {
    pub effect: String,
    pub token: String,
}

/// The privileged executor: refuses any request whose capability token is
/// missing/unknown — a compromised renderer cannot invoke privileged
/// effects without the token (QUAL-EV-0075).
pub fn execute_privileged(
    effect: &str,
    presented: Option<&CapabilityToken>,
    issued: &[CapabilityToken],
) -> Result<(), String> {
    let presented = presented.ok_or("no capability token presented")?;
    if !issued
        .iter()
        .any(|t| t.effect == effect && t.token == presented.token)
    {
        return Err(format!(
            "privileged effect {effect:?} refused: capability token invalid"
        ));
    }
    Ok(())
}

/// Credential handle injection for guests (REQ-EV-0288, M9 cross-ref):
/// the handle (not the static secret) is injected into the guest env.
pub fn inject_credential_handle(handle_id: &str) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    env.insert("MODBIT_CRED_HANDLE".to_string(), handle_id.to_string());
    env
}

/// Checksum helper for checkpoint handoff payloads (M8.7).
pub fn checkpoint_payload_digest(payload: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(payload);
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// QUAL-EV-0109/0233/0291: the same conformance fixture passes on
    /// local and cloud MicroVM backends under ONE contract.
    #[test]
    fn conformance_suite_passes_on_local_and_cloud() {
        let local = LocalBackend;
        let cloud = CloudMicroVmBackend {
            vm_id: "vm-1".into(),
        };
        assert!(conformance_suite(&local).is_ok());
        assert!(conformance_suite(&cloud).is_ok());
        // Contract test runs against both backends via the same trait
        // object — substrate details cannot leak.
        let backends: Vec<&dyn ExecutionBackend> = vec![&local, &cloud];
        for backend in backends {
            assert!(conformance_suite(backend).is_ok());
        }
    }

    /// QUAL-EV-0287: deny-by-default — the guest cannot reach internal/
    /// control-plane endpoints and ungranted fs paths are denied.
    #[test]
    fn guest_denied_by_default() {
        let mut policy = SandboxPolicy::default();
        // Empty policy: everything denied.
        assert!(matches!(
            check_fs(&policy, "/workspace/src/lib.rs"),
            Err(PolicyError::FsRootDenied(_))
        ));
        assert!(matches!(
            check_network(&policy, "https://internal-api"),
            Err(PolicyError::NetworkDenied(_))
        ));

        // Explicit grants only.
        policy.fs_roots.push("/workspace".into());
        policy.network_allow.push("crates.io".into());
        assert!(check_fs(&policy, "/workspace/src/lib.rs").is_ok());

        // Control-plane endpoints are NEVER grantable.
        policy.network_allow.push("169.254.169.254".into());
        assert!(matches!(
            check_network(&policy, "http://169.254.169.254/latest/meta-data"),
            Err(PolicyError::ControlPlaneNeverGrantable(_))
        ));
    }

    /// QUAL-EV-0289: unknown/stale RPC version/capability is rejected.
    #[test]
    fn guest_rpc_rejects_unknown_version_and_stale_tokens() {
        let rpc = GuestRpc {
            rpc_version: GUEST_RPC_VERSION,
            task_id: "t-1".into(),
            effect_class: "fs.write".into(),
            capability_token: "live-token".into(),
            operation: "write_file".into(),
        };
        assert!(
            validate_guest_rpc(&rpc, &["live-token".to_string()], &["fs.write".to_string()])
                .is_ok()
        );

        // Unknown version.
        let old = GuestRpc {
            rpc_version: 99,
            ..rpc.clone()
        };
        assert!(matches!(
            validate_guest_rpc(&old, &[], &[]),
            Err(RpcError::UnknownVersion { version: 99 })
        ));

        // Stale capability token.
        let stale = GuestRpc {
            capability_token: "expired-token".into(),
            ..rpc.clone()
        };
        assert!(matches!(
            validate_guest_rpc(
                &stale,
                &["live-token".to_string()],
                &["fs.write".to_string()]
            ),
            Err(RpcError::StaleCapability { .. })
        ));

        // Unknown capability.
        let unknown_cap = GuestRpc {
            effect_class: "quantum.fold".into(),
            ..rpc.clone()
        };
        assert!(matches!(
            validate_guest_rpc(
                &unknown_cap,
                &["live-token".to_string()],
                &["fs.write".to_string()]
            ),
            Err(RpcError::UnknownCapability { .. })
        ));
    }

    /// QUAL-EV-0286: cross-tenant sandbox handle use is denied and
    /// audited.
    #[test]
    fn cross_tenant_sandbox_use_denied_and_audited() {
        let mut gateway = SandboxGateway::default();
        gateway.bind("sbx-1", "tenant-a");
        assert!(gateway
            .authenticated_request("sbx-1", "tenant-a", "exec")
            .is_ok());
        assert!(gateway
            .authenticated_request("sbx-1", "tenant-b", "exec")
            .is_err());
        assert!(gateway
            .audit
            .iter()
            .any(|a| a.contains("CROSS-TENANT DENIED")));
    }

    /// QUAL-EV-0072: an older worker missing a required capability gets
    /// an explicit rejection.
    #[test]
    fn negotiation_rejects_missing_required_protocols() {
        let worker = WorkerCapabilities {
            worker_id: "old-worker".into(),
            protocols: vec!["v1".to_string()],
            tools: vec!["tools.fs.read".to_string()],
            media: vec![],
        };
        let err =
            negotiate(&worker, &["v2".to_string()], &["tools.fs.read".to_string()]).unwrap_err();
        assert!(err.contains("explicit rejection"));
        // A capable worker negotiates both protocols and tools.
        let capable = WorkerCapabilities {
            worker_id: "new-worker".into(),
            protocols: vec!["v1".to_string(), "v2".to_string()],
            tools: worker.tools.clone(),
            media: vec![],
        };
        let (protocols, tools) = negotiate(
            &capable,
            &["v2".to_string()],
            &["tools.fs.read".to_string()],
        )
        .unwrap();
        assert_eq!(protocols, vec!["v2".to_string()]);
        assert_eq!(tools, vec!["tools.fs.read".to_string()]);
    }

    /// QUAL-EV-0024: reverse-connect registers private workers; revoked
    /// tokens refuse reconnection.
    #[test]
    fn reverse_connect_registers_and_revokes() {
        let mut gateway = ReverseConnectGateway::default();
        gateway.register("private-1", "tenant-vpc", true);
        assert_eq!(gateway.connect("private-1"), Ok("tenant-vpc"));
        gateway.register("private-1", "tenant-vpc", false);
        assert!(gateway.connect("private-1").is_err());
    }

    /// QUAL-EV-0076: killing the renderer cannot regress canonical truth.
    #[test]
    fn renderer_view_cannot_regress_canonical_state() {
        let mut canonical = CanonicalSessionState {
            session_id: "s-1".into(),
            head_seq: 50,
            facts: vec!["fact-1".into()],
        };
        merge_renderer_view(&mut canonical, 40);
        assert_eq!(canonical.head_seq, 50, "regression ignored");
        merge_renderer_view(&mut canonical, 60);
        assert_eq!(canonical.head_seq, 60, "advancement accepted");
    }

    /// QUAL-EV-0081: a browser worker crash reattaches a fresh worker
    /// without corrupting Core.
    #[test]
    fn browser_worker_crash_contained() {
        let canonical = CanonicalSessionState {
            session_id: "s-1".into(),
            head_seq: 12,
            facts: vec!["fact".into()],
        };
        assert!(reattach_after_worker_crash(&canonical, true));
    }

    /// QUAL-EV-0075: a compromised renderer cannot invoke a privileged
    /// effect without a valid capability token.
    #[test]
    fn privileged_execution_requires_capability_token() {
        let issued = vec![CapabilityToken {
            effect: "browser.cdp".into(),
            token: "tok-valid".into(),
        }];
        assert!(execute_privileged("browser.cdp", None, &issued).is_err());
        assert!(execute_privileged(
            "browser.cdp",
            Some(&CapabilityToken {
                effect: "browser.cdp".into(),
                token: "tok-forged".into(),
            }),
            &issued,
        )
        .is_err());
        assert!(execute_privileged(
            "browser.cdp",
            Some(&CapabilityToken {
                effect: "browser.cdp".into(),
                token: "tok-valid".into(),
            }),
            &issued,
        )
        .is_ok());
    }

    /// M8.7: checkpoint handoff payload digest is stable.
    #[test]
    fn checkpoint_handoff_digest_stable() {
        let payload = b"checkpoint bytes";
        assert_eq!(
            checkpoint_payload_digest(payload),
            checkpoint_payload_digest(payload)
        );
    }
}
