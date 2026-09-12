//! Sandbox gateway shared types (docs/24): the gateway owns the mapping
//! from authenticated tenant/session/task to substrate lease and relays
//! opaque framed payloads. The envelope is the ONLY thing the gateway
//! parses — the wrapped bytes are the SurfaceProtocol, which the gateway
//! never interprets (boundary, not brain).

use base64::Engine as _;
use serde::{Deserialize, Serialize};

/// First frame after authentication: who is the peer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Registration {
    /// "worker" hosts Cores; "guest" is a tenant-side client (desktop/CLI
    /// or the cloud API acting for a user).
    pub role: String,
    pub tenant: String,
    /// Workers may scope to one task; empty = all tasks of the tenant.
    #[serde(default)]
    pub task: String,
}

/// A relayed request/response. `payload` is base64 of the opaque framed
/// protocol bytes (SurfaceRequest from guests, SurfaceResponse from
/// workers). The gateway NEVER parses `payload`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope {
    #[serde(default)]
    pub tenant: String,
    #[serde(default)]
    pub task: String,
    pub payload: String,
}

impl Envelope {
    pub fn encode_payload(raw: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(raw)
    }

    pub fn decode_payload(&self) -> Result<Vec<u8>, String> {
        base64::engine::general_purpose::STANDARD
            .decode(&self.payload)
            .map_err(|e| format!("bad payload encoding: {e}"))
    }
}

/// Gateway → peer error frame (typed, never silent).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewayError {
    pub error: String,
}
