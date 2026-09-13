//! Host-side guest client (M8.4/M8.5): connects to a running
//! `modbit-guest` over the authenticated transport, verifies the SIGNED
//! guest manifest BEFORE any request, and exchanges typed requests/
//! responses with request ids, generation fencing and timeouts. The same
//! client serves the development TCP transport today and the production
//! vsock transport later — the guest contract is transport-independent
//! (REQ-EV-0291: substrate details stay behind the boundary).

use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use modbit_protocol::guest::{
    verify_manifest, GuestError, GuestIdentityError, GuestManifest, GuestManifestFrame, GuestOp,
    GuestOutcome, GuestPayload, GuestRequest, GuestResponse, GUEST_RPC_VERSION,
};
use modbit_protocol::transport::{BootSecret, Connection};

/// A verified connection to one guest. `manifest` is the identity the
/// guest proved; every request is stamped with it (task binding).
pub struct GuestClient {
    conn: Connection<TcpStream>,
    pub manifest: GuestManifest,
    next_request: u64,
    conn_prefix: String,
}

impl std::fmt::Debug for GuestClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GuestClient")
            .field("manifest", &self.manifest)
            .field("conn_prefix", &self.conn_prefix)
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub enum GuestClientError {
    Transport(String),
    Identity(GuestIdentityError),
    Guest(GuestError),
}

impl std::fmt::Display for GuestClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GuestClientError::Transport(e) => write!(f, "guest transport: {e}"),
            GuestClientError::Identity(e) => write!(f, "guest identity: {e}"),
            GuestClientError::Guest(e) => write!(f, "guest {}: {}", e.code, e.message),
        }
    }
}

impl GuestClient {
    /// Connects, authenticates (boot-secret challenge), reads and VERIFIES
    /// the signed manifest, and checks protocol compatibility. Any failure
    /// closes the relationship before a single request is served.
    pub fn connect(
        stream: TcpStream,
        secret: &BootSecret,
        provisioning_key: &[u8],
    ) -> Result<Self, GuestClientError> {
        // Bounded waits: a wedged guest must surface as an error, not a
        // hung caller (failure semantics are part of the feature).
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .map_err(|e| GuestClientError::Transport(e.to_string()))?;
        let mut conn = Connection::over_stream(stream, secret)
            .map_err(|e| GuestClientError::Transport(e.to_string()))?;
        let frame_bytes = conn
            .receive()
            .map_err(|e| GuestClientError::Transport(e.to_string()))?;
        let frame: GuestManifestFrame = serde_json::from_slice(&frame_bytes)
            .map_err(|e| GuestClientError::Transport(format!("bad manifest frame: {e}")))?;
        verify_manifest(&frame.manifest, &frame.signature_hex, provisioning_key)
            .map_err(GuestClientError::Identity)?;
        // Connection-unique request-id prefix: the guest keeps an
        // idempotency cache across reconnections, so ids must never
        // collide between DIFFERENT client instances (a deliberate replay
        // re-sends the original id; an accidental reuse must not).
        let conn_prefix = uuid::Uuid::now_v7().simple().to_string();
        Ok(GuestClient {
            conn,
            manifest: frame.manifest,
            next_request: 0,
            conn_prefix,
        })
    }

    /// Issues one typed request. `generation` is the caller's fencing
    /// token: advancing it fences in-flight work of older generations.
    pub fn call(
        &mut self,
        capability_token: &str,
        generation: u64,
        op: GuestOp,
    ) -> Result<GuestPayload, GuestClientError> {
        self.next_request += 1;
        let req = GuestRequest {
            request_id: format!(
                "h-{}-{}-{}",
                self.conn_prefix, self.manifest.task, self.next_request
            ),
            rpc_version: GUEST_RPC_VERSION,
            task: self.manifest.task.clone(),
            capability_token: capability_token.to_string(),
            effect_class: op.effect_class().to_string(),
            generation,
            op,
        };
        let bytes =
            serde_json::to_vec(&req).map_err(|e| GuestClientError::Transport(e.to_string()))?;
        self.conn
            .send(&bytes)
            .map_err(|e| GuestClientError::Transport(e.to_string()))?;
        let resp_bytes = self
            .conn
            .receive()
            .map_err(|e| GuestClientError::Transport(e.to_string()))?;
        let resp: GuestResponse = serde_json::from_slice(&resp_bytes)
            .map_err(|e| GuestClientError::Transport(format!("bad response frame: {e}")))?;
        if resp.request_id != req.request_id {
            return Err(GuestClientError::Transport(format!(
                "response id {} does not match request {}",
                resp.request_id, req.request_id
            )));
        }
        match resp.outcome {
            GuestOutcome::Ok { payload } => Ok(payload),
            GuestOutcome::Err { error } => Err(GuestClientError::Guest(error)),
        }
    }

    /// Convenience for the conformance contract: execute argv, return
    /// (exit_code, stdout).
    pub fn exec(
        &mut self,
        token: &str,
        generation: u64,
        argv: &[String],
        timeout_ms: u64,
    ) -> Result<(i64, String), String> {
        match self.call(
            token,
            generation,
            GuestOp::ProcExec {
                argv: argv.to_vec(),
                cwd: None,
                env: Default::default(),
                timeout_ms: Some(timeout_ms),
            },
        ) {
            Ok(GuestPayload::Proc {
                exit_code,
                stdout,
                timed_out,
                ..
            }) => {
                if timed_out {
                    Err("guest proc timed out".into())
                } else {
                    Ok((exit_code.unwrap_or(-1), stdout))
                }
            }
            Ok(other) => Err(format!("unexpected guest payload {other:?}")),
            Err(GuestClientError::Guest(e)) => Err(format!("{}: {}", e.code, e.message)),
            Err(e) => Err(e.to_string()),
        }
    }
}

/// The ExecutionBackend adapter over a REAL guest (REQ-EV-0291): the same
/// conformance suite that passes on the local reference backend runs here
/// only when a live, verified guest actually serves the requests. There is
/// no canned success path — construction already proved the guest's
/// signed identity.
#[derive(Clone, Debug)]
pub struct GuestBackend {
    client: Arc<Mutex<GuestClient>>,
    pub token: String,
    pub generation: u64,
    pub timeout_ms: u64,
}

impl GuestBackend {
    /// Wraps an already-verified client (see [`GuestClient::connect`]).
    pub fn new(client: GuestClient, token: &str, generation: u64) -> Self {
        GuestBackend {
            client: Arc::new(Mutex::new(client)),
            token: token.to_string(),
            generation,
            timeout_ms: 30_000,
        }
    }

    pub fn manifest(&self) -> GuestManifest {
        self.client.lock().expect("guest client").manifest.clone()
    }
}

impl crate::ExecutionBackend for GuestBackend {
    fn name(&self) -> &'static str {
        "guest-tcp"
    }
    fn execute(&self, argv: &[String]) -> Result<(i64, String), String> {
        let mut client = self.client.lock().expect("guest client");
        client.exec(&self.token, self.generation, argv, self.timeout_ms)
    }
}
