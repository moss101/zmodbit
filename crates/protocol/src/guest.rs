//! Guest identity + typed guest RPC wire contract (docs/24 § substrate;
//! layout docs/33 `services/modbit-guest`): the versioned, SIGNED
//! `modbit-guest` identity manifest (M8.4) and the versioned typed
//! process/fs/PTY request frames carrying task/effect/capability/generation
//! identity (M8.5, REQ-EV-0289). Canonical owner: the protocol crate —
//! the guest binary, the host-side client and the substrate tooling share
//! these types from here; no second definition is permitted.
//!
//! Signing: the manifest signature is HMAC-SHA256 over the canonical JSON
//! encoding of the manifest under a PROVISIONING key that reaches the
//! substrate out-of-band (never inside the guest image). Development/test
//! signing uses an ephemeral key generated per boot; production signing
//! uses the operator's key service through the SAME sign/verify calls.
//! A dev signature therefore proves the verification PATH, not production
//! signing identity — the production key ceremony stays an operator gate
//! (Future-tasks §4 Phase 4 residual).

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

/// Wire protocol major of the guest RPC contract. A mismatch in the major
/// is incompatible; the minor negotiates down to the lower of the two
/// (same rule as the transport handshake, transport.rs).
pub const GUEST_PROTOCOL_MAJOR: u32 = 1;
pub const GUEST_PROTOCOL_MINOR: u32 = 0;

/// Typed guest RPC version (REQ-EV-0289: versioned guest RPC). Unknown
/// versions are rejected before any other check.
pub const GUEST_RPC_VERSION: u32 = 1;

/// The signed identity of a running guest. `task` binds the guest to
/// exactly one task for its whole lifetime; `build_hash` is the SHA-256
/// of the running guest executable so the host pins WHAT code it is
/// talking to, not just which version string it claims.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuestManifest {
    pub guest_version: String,
    pub protocol_major: u32,
    pub protocol_minor: u32,
    pub build_hash: String,
    pub task: String,
}

/// First frame on an accepted guest connection: identity + signature.
/// The host MUST verify before serving any request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuestManifestFrame {
    pub manifest: GuestManifest,
    pub signature_hex: String,
}

/// Signature errors (fail closed: any error refuses the guest).
#[derive(Debug, PartialEq, Eq)]
pub enum GuestIdentityError {
    BadSignature,
    UnsupportedVersion { major: u32, minor: u32 },
}

impl std::fmt::Display for GuestIdentityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GuestIdentityError::BadSignature => {
                write!(f, "guest manifest signature mismatch (fail closed)")
            }
            GuestIdentityError::UnsupportedVersion { major, minor } => {
                write!(f, "guest protocol {major}.{minor} incompatible with host {GUEST_PROTOCOL_MAJOR}.{GUEST_PROTOCOL_MINOR}")
            }
        }
    }
}

impl std::error::Error for GuestIdentityError {}

/// Signs the canonical JSON encoding of the manifest under `key`.
pub fn sign_manifest(manifest: &GuestManifest, key: &[u8]) -> String {
    let bytes = serde_json::to_vec(manifest).expect("manifest serializes");
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("hmac accepts any key length");
    mac.update(&bytes);
    hex_encode(&mac.finalize().into_bytes())
}

/// Verifies a manifest signature and protocol compatibility. Constant-
/// time signature comparison via HMAC verify (never early-exit compare).
pub fn verify_manifest(
    manifest: &GuestManifest,
    signature_hex: &str,
    key: &[u8],
) -> Result<(), GuestIdentityError> {
    if manifest.protocol_major != GUEST_PROTOCOL_MAJOR {
        return Err(GuestIdentityError::UnsupportedVersion {
            major: manifest.protocol_major,
            minor: manifest.protocol_minor,
        });
    }
    let bytes = serde_json::to_vec(manifest).expect("manifest serializes");
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("hmac accepts any key length");
    mac.update(&bytes);
    let expected = match hex_decode(signature_hex) {
        Ok(raw) => raw,
        Err(_) => return Err(GuestIdentityError::BadSignature),
    };
    mac.verify_slice(&expected)
        .map_err(|_| GuestIdentityError::BadSignature)
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn hex_decode(s: &str) -> Result<Vec<u8>, ()> {
    if !s.len().is_multiple_of(2) {
        return Err(());
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|_| ()))
        .collect()
}

/// The canonical effect classes a guest request can carry. The declared
/// `effect_class` MUST match the operation actually present — a mismatch
/// is a schema-manipulation attempt and is rejected before execution.
pub const GUEST_EFFECT_CLASSES: [&str; 10] = [
    "proc.exec",
    "proc.kill",
    "fs.read",
    "fs.write",
    "fs.list",
    "pty.open",
    "pty.write",
    "pty.read",
    "pty.kill",
    "session.ping",
];

/// The operation of a typed guest request. Every variant carries exactly
/// what the executor needs — nothing else is interpreted.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum GuestOp {
    ProcExec {
        argv: Vec<String>,
        #[serde(default)]
        cwd: Option<String>,
        #[serde(default)]
        env: std::collections::BTreeMap<String, String>,
        #[serde(default)]
        timeout_ms: Option<u64>,
    },
    ProcKill {
        proc_id: String,
    },
    FsRead {
        path: String,
    },
    FsWrite {
        path: String,
        /// base64 of the exact bytes to write.
        bytes_base64: String,
    },
    FsList {
        path: String,
    },
    PtyOpen {
        argv: Vec<String>,
        #[serde(default)]
        cwd: Option<String>,
        #[serde(default)]
        cols: u16,
        #[serde(default)]
        rows: u16,
    },
    PtyWrite {
        pty_id: String,
        bytes_base64: String,
    },
    PtyRead {
        pty_id: String,
        offset: usize,
        max: usize,
    },
    PtyKill {
        pty_id: String,
    },
    Ping,
}

impl GuestOp {
    /// The effect class this operation actually requires.
    pub fn effect_class(&self) -> &'static str {
        match self {
            GuestOp::ProcExec { .. } => "proc.exec",
            GuestOp::ProcKill { .. } => "proc.kill",
            GuestOp::FsRead { .. } => "fs.read",
            GuestOp::FsWrite { .. } => "fs.write",
            GuestOp::FsList { .. } => "fs.list",
            GuestOp::PtyOpen { .. } => "pty.open",
            GuestOp::PtyWrite { .. } => "pty.write",
            GuestOp::PtyRead { .. } => "pty.read",
            GuestOp::PtyKill { .. } => "pty.kill",
            GuestOp::Ping => "session.ping",
        }
    }
}

/// A typed guest RPC (REQ-EV-0289): versioned, and carrying task,
/// capability and generation identity on EVERY request.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GuestRequest {
    pub request_id: String,
    pub rpc_version: u32,
    pub task: String,
    pub capability_token: String,
    pub effect_class: String,
    /// Session generation the request belongs to; the guest refuses
    /// (fences) requests stamped with any older generation.
    pub generation: u64,
    #[serde(flatten)]
    pub op: GuestOp,
}

/// Typed payload of a successful guest response.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GuestPayload {
    Proc {
        proc_id: String,
        exit_code: Option<i64>,
        stdout: String,
        stderr: String,
        timed_out: bool,
        killed: bool,
    },
    Data {
        bytes_base64: String,
        sha256: String,
    },
    Listing {
        entries: Vec<String>,
    },
    Pty {
        pty_id: String,
        bytes_base64: String,
        offset: usize,
        alive: bool,
    },
    Accepted {},
    Pong {
        guest_version: String,
        protocol_major: u32,
        protocol_minor: u32,
    },
}

/// Typed error codes — never a bare string; the host branches on `code`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuestError {
    pub code: String,
    pub message: String,
}

impl GuestError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        GuestError {
            code: code.to_string(),
            message: message.into(),
        }
    }
}

/// Error code constants (stable wire values).
pub mod guest_error_codes {
    pub const VERSION: &str = "version";
    pub const STALE_CAPABILITY: &str = "stale_capability";
    pub const UNKNOWN_CAPABILITY: &str = "unknown_capability";
    pub const FENCED: &str = "fenced";
    pub const WRONG_TASK: &str = "wrong_task";
    pub const POLICY_DENIED: &str = "policy_denied";
    pub const TIMEOUT: &str = "timeout";
    pub const NOT_FOUND: &str = "not_found";
    pub const BAD_REQUEST: &str = "bad_request";
    pub const INTERNAL: &str = "internal";
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum GuestOutcome {
    Ok { payload: GuestPayload },
    Err { error: GuestError },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GuestResponse {
    pub request_id: String,
    #[serde(flatten)]
    pub outcome: GuestOutcome,
}

impl GuestResponse {
    pub fn ok(request_id: &str, payload: GuestPayload) -> Self {
        GuestResponse {
            request_id: request_id.to_string(),
            outcome: GuestOutcome::Ok { payload },
        }
    }
    pub fn err(request_id: &str, error: GuestError) -> Self {
        GuestResponse {
            request_id: request_id.to_string(),
            outcome: GuestOutcome::Err { error },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> GuestManifest {
        GuestManifest {
            guest_version: "0.1.0".into(),
            protocol_major: GUEST_PROTOCOL_MAJOR,
            protocol_minor: GUEST_PROTOCOL_MINOR,
            build_hash: "abc123".into(),
            task: "task-1".into(),
        }
    }

    /// M8.4: the manifest verifies under the right key and FAILS CLOSED
    /// under a wrong key or any tampering.
    #[test]
    fn manifest_signature_verifies_and_fails_closed() {
        let key = b"provisioning-key-dev";
        let m = manifest();
        let sig = sign_manifest(&m, key);
        assert!(verify_manifest(&m, &sig, key).is_ok());

        // Wrong key.
        assert_eq!(
            verify_manifest(&m, &sig, b"other-key"),
            Err(GuestIdentityError::BadSignature)
        );
        // Tampered build hash.
        let mut tampered = m.clone();
        tampered.build_hash = "ffff".into();
        assert_eq!(
            verify_manifest(&tampered, &sig, key),
            Err(GuestIdentityError::BadSignature)
        );
        // Tampered task binding.
        let mut rebind = m.clone();
        rebind.task = "task-2".into();
        assert_eq!(
            verify_manifest(&rebind, &sig, key),
            Err(GuestIdentityError::BadSignature)
        );
        // Malformed signature hex.
        assert_eq!(
            verify_manifest(&m, "zz", key),
            Err(GuestIdentityError::BadSignature)
        );
    }

    /// M8.4: an incompatible protocol major refuses before signature
    /// details even matter (fail closed on version).
    #[test]
    fn manifest_rejects_incompatible_major() {
        let key = b"k";
        let mut m = manifest();
        m.protocol_major = 99;
        let sig = sign_manifest(&m, key);
        assert_eq!(
            verify_manifest(&m, &sig, key),
            Err(GuestIdentityError::UnsupportedVersion {
                major: 99,
                minor: GUEST_PROTOCOL_MINOR
            })
        );
    }

    /// M8.5: typed requests round-trip through the wire encoding with
    /// their operation, identity and generation intact.
    #[test]
    fn guest_request_response_round_trip() {
        let req = GuestRequest {
            request_id: "r-1".into(),
            rpc_version: GUEST_RPC_VERSION,
            task: "task-1".into(),
            capability_token: "tok".into(),
            effect_class: "proc.exec".into(),
            generation: 3,
            op: GuestOp::ProcExec {
                argv: vec!["echo".into(), "hi".into()],
                cwd: None,
                env: Default::default(),
                timeout_ms: Some(1000),
            },
        };
        let bytes = serde_json::to_vec(&req).expect("json");
        let back: GuestRequest = serde_json::from_slice(&bytes).expect("json");
        assert_eq!(back, req);
        assert_eq!(back.op.effect_class(), "proc.exec");

        let resp = GuestResponse::ok(
            "r-1",
            GuestPayload::Proc {
                proc_id: "p-1".into(),
                exit_code: Some(0),
                stdout: "hi\n".into(),
                stderr: String::new(),
                timed_out: false,
                killed: false,
            },
        );
        let rbytes = serde_json::to_vec(&resp).expect("json");
        let rback: GuestResponse = serde_json::from_slice(&rbytes).expect("json");
        assert_eq!(rback, resp);
    }
}
