//! Local authenticated SurfaceProtocol transport (M1.3, docs/30 § Local
//! SurfaceProtocol).
//!
//! Transport: length-prefixed protobuf frames over a Unix domain socket
//! (macOS/Linux) or named pipe (Windows). At Core startup a boot-scoped
//! random secret is generated; the desktop main process receives it through
//! the inherited secure channel (OS-specific delivery lands with the Electron
//! shell, M1.4) and proves possession via HMAC-SHA256 over both nonces and
//! the protocol version — the secret itself never crosses the wire.
//!
//! Handshake: server sends [`Challenge`] (server nonce) → client answers
//! [`Hello`] (HMAC proof, client nonce, protocol version) → server replies
//! [`AuthResult`]. Major-version mismatch keeps the connection usable for
//! read-only export while flagging `read_only` (docs/30 § Version
//! compatibility: mutation is blocked above this layer).
//!
//! The only permitted desktop peer is Electron main (never the renderer).

use std::fmt;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Arc;

use hmac::Hmac;
use interprocess::local_socket::traits::{Listener as _, Stream as _};
use interprocess::local_socket::{ListenerOptions, Stream};
use prost::Message;

use crate::modbit::protocol::v1 as pb;

/// Canonical protocol version of this build (docs/30 § Version compatibility).
pub const PROTOCOL_MAJOR: u32 = 1;
pub const PROTOCOL_MINOR: u32 = 0;
pub const SERVER_VERSION: &str = concat!("modbit-core ", env!("CARGO_PKG_VERSION"));

/// Hard frame ceiling: bounded IPC dispatch (docs/33 § Backpressure bounds).
pub const MAX_FRAME_BYTES: u32 = 8 * 1024 * 1024;

const NONCE_LEN: usize = 16;

/// Boot-scoped random secret shared with the desktop main process only.
#[derive(Clone)]
pub struct BootSecret([u8; 32]);

impl BootSecret {
    pub fn generate() -> Result<Self, TransportError> {
        let mut bytes = [0u8; 32];
        getrandom::getrandom(&mut bytes)
            .map_err(|e| TransportError::Io(std::io::Error::other(e.to_string())))?;
        Ok(Self(bytes))
    }

    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    fn hmac_proof(
        &self,
        server_nonce: &[u8],
        client_nonce: &[u8],
        major: u32,
        minor: u32,
    ) -> Vec<u8> {
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.0).expect("hmac accepts any key length");
        mac.update(server_nonce);
        mac.update(client_nonce);
        mac.update(&major.to_be_bytes());
        mac.update(&minor.to_be_bytes());
        mac.finalize().into_bytes().to_vec()
    }
}

use hmac::Mac;
use sha2::Sha256;

#[derive(Debug)]
pub enum TransportError {
    Io(std::io::Error),
    /// The peer failed authentication (wrong or missing boot secret).
    AuthRejected {
        reason: String,
    },
    /// The peer sent a frame larger than [`MAX_FRAME_BYTES`].
    FrameTooLarge {
        size: u32,
    },
    /// Handshake messages were malformed or in the wrong order.
    Protocol {
        reason: String,
    },
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TransportError::Io(e) => write!(f, "transport io: {e}"),
            TransportError::AuthRejected { reason } => write!(f, "auth rejected: {reason}"),
            TransportError::FrameTooLarge { size } => {
                write!(f, "frame of {size} bytes exceeds {MAX_FRAME_BYTES}")
            }
            TransportError::Protocol { reason } => write!(f, "protocol error: {reason}"),
        }
    }
}

impl std::error::Error for TransportError {}

impl From<std::io::Error> for TransportError {
    fn from(e: std::io::Error) -> Self {
        TransportError::Io(e)
    }
}

/// The listening endpoint name. Unix: a filesystem path; Windows: a named
/// pipe in the local namespace (docs/30 § Local SurfaceProtocol).
#[derive(Clone, Debug)]
pub struct EndpointName {
    inner: interprocess::local_socket::Name<'static>,
}

impl EndpointName {
    #[cfg(unix)]
    pub fn fs_path(path: PathBuf) -> Result<Self, TransportError> {
        use interprocess::local_socket::{GenericFilePath, ToFsName};
        Ok(Self {
            inner: path.to_fs_name::<GenericFilePath>().map_err(io_other)?,
        })
    }

    #[cfg(windows)]
    pub fn fs_path(path: PathBuf) -> Result<Self, TransportError> {
        use interprocess::local_socket::{GenericFilePath, ToFsName};
        Ok(Self {
            inner: path.to_fs_name::<GenericFilePath>().map_err(io_other)?,
        })
    }

    #[cfg(windows)]
    pub fn namespace(name: &str) -> Result<Self, TransportError> {
        use interprocess::local_socket::{GenericNamespace, ToNsName};
        Ok(Self {
            inner: name.to_ns_name::<GenericNamespace>().map_err(io_other)?,
        })
    }

    /// A unique endpoint under the platform's temp area — for tests and for
    /// the per-boot Core socket.
    pub fn ephemeral(tag: &str) -> Result<Self, TransportError> {
        let mut nonce = [0u8; 8];
        getrandom::getrandom(&mut nonce)
            .map_err(|e| TransportError::Io(std::io::Error::other(e.to_string())))?;
        let unique: String = nonce.iter().map(|b| format!("{b:02x}")).collect();
        #[cfg(unix)]
        {
            let mut path = std::env::temp_dir();
            path.push(format!("modbit-{tag}-{unique}.sock"));
            Self::fs_path(path)
        }
        #[cfg(windows)]
        {
            let _ = tag;
            Self::namespace(&format!("modbit-{tag}-{unique}"))
        }
    }

    pub fn interprocess_name(&self) -> &interprocess::local_socket::Name<'static> {
        &self.inner
    }
}

fn io_other(e: impl ToString) -> TransportError {
    TransportError::Io(std::io::Error::other(e.to_string()))
}

/// Binds the Core-side listener (Unix socket / named pipe).
pub fn bind(name: &EndpointName) -> Result<interprocess::local_socket::Listener, TransportError> {
    ListenerOptions::new()
        .name(name.interprocess_name().to_owned())
        .create_sync()
        .map_err(io_other)
}

fn write_frame(stream: &mut impl ReadWrite, payload: &[u8]) -> Result<(), TransportError> {
    let len = u32::try_from(payload.len()).map_err(|_| TransportError::FrameTooLarge {
        size: payload.len() as u32,
    })?;
    if len > MAX_FRAME_BYTES {
        return Err(TransportError::FrameTooLarge { size: len });
    }
    stream.write_all(&len.to_be_bytes())?;
    stream.write_all(payload)?;
    stream.flush()?;
    Ok(())
}

fn read_exact_n(stream: &mut impl ReadWrite, buf: &mut [u8]) -> Result<(), TransportError> {
    let mut filled = 0;
    while filled < buf.len() {
        let n = stream.read(&mut buf[filled..])?;
        if n == 0 {
            return Err(TransportError::Protocol {
                reason: "peer closed mid-frame".into(),
            });
        }
        filled += n;
    }
    Ok(())
}

fn read_frame(stream: &mut impl ReadWrite) -> Result<Vec<u8>, TransportError> {
    let mut len_bytes = [0u8; 4];
    read_exact_n(stream, &mut len_bytes)?;
    let len = u32::from_be_bytes(len_bytes);
    if len > MAX_FRAME_BYTES {
        return Err(TransportError::FrameTooLarge { size: len });
    }
    let mut payload = vec![0u8; len as usize];
    read_exact_n(stream, &mut payload)?;
    Ok(payload)
}

trait ReadWrite: Read + Write {}
impl<T: Read + Write> ReadWrite for T {}

fn random_nonce() -> Result<[u8; NONCE_LEN], TransportError> {
    let mut nonce = [0u8; NONCE_LEN];
    getrandom::getrandom(&mut nonce)
        .map_err(|e| TransportError::Io(std::io::Error::other(e.to_string())))?;
    Ok(nonce)
}

/// Server side of the handshake. Returns `(accepted, read_only, negotiated)`.
/// On rejection an `AuthResult{ok=false}` frame is still sent before the
/// stream is closed, so well-behaved clients can surface the reason.
fn server_handshake(
    stream: &mut Stream,
    secret: &BootSecret,
) -> Result<(bool, bool, (u32, u32)), TransportError> {
    let server_nonce = random_nonce()?;
    write_frame(
        stream,
        &pb::Challenge {
            server_nonce: server_nonce.to_vec(),
        }
        .encode_to_vec(),
    )?;

    let hello_bytes = read_frame(stream)?;
    let hello =
        pb::Hello::decode(hello_bytes.as_slice()).map_err(|e| TransportError::Protocol {
            reason: format!("bad Hello: {e}"),
        })?;

    let mut mac = Hmac::<Sha256>::new_from_slice(&secret.0).expect("hmac accepts any key length");
    mac.update(&server_nonce);
    mac.update(&hello.client_nonce);
    mac.update(&hello.major.to_be_bytes());
    mac.update(&hello.minor.to_be_bytes());
    let auth_ok = mac.verify_slice(&hello.proof).is_ok();

    if !auth_ok {
        let result = pb::AuthResult {
            ok: false,
            read_only: false,
            negotiated_major: 0,
            negotiated_minor: 0,
            server_version: SERVER_VERSION.into(),
            error: "boot secret proof mismatch".into(),
        };
        write_frame(stream, &result.encode_to_vec())?;
        return Err(TransportError::AuthRejected {
            reason: "boot secret proof mismatch".into(),
        });
    }

    let read_only = hello.major != PROTOCOL_MAJOR;
    // With PROTOCOL_MINOR == 0 the min is constant today, but the negotiation
    // rule (lower of the two minors) is the locked contract for future minors.
    #[allow(clippy::unnecessary_min_or_max)]
    let negotiated_minor = hello.minor.min(PROTOCOL_MINOR);
    let result = pb::AuthResult {
        ok: true,
        read_only,
        negotiated_major: PROTOCOL_MAJOR,
        negotiated_minor,
        server_version: SERVER_VERSION.into(),
        error: String::new(),
    };
    write_frame(stream, &result.encode_to_vec())?;
    Ok((true, read_only, (PROTOCOL_MAJOR, negotiated_minor)))
}

/// An authenticated Core↔desktop connection.
pub struct Connection {
    stream: Stream,
    pub read_only: bool,
    pub negotiated: (u32, u32),
}

impl fmt::Debug for Connection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Connection")
            .field("read_only", &self.read_only)
            .field("negotiated", &self.negotiated)
            .finish_non_exhaustive()
    }
}

impl Connection {
    pub fn send(&mut self, payload: &[u8]) -> Result<(), TransportError> {
        write_frame(&mut self.stream, payload)
    }

    pub fn receive(&mut self) -> Result<Vec<u8>, TransportError> {
        read_frame(&mut self.stream)
    }
}

/// Connects to Core and performs the client side of the boot handshake.
pub fn connect(name: &EndpointName, secret: &BootSecret) -> Result<Connection, TransportError> {
    connect_with_version(name, secret, PROTOCOL_MAJOR, PROTOCOL_MINOR)
}

/// Handshake with an explicit client protocol version — used by the version
/// negotiation path (docs/30 § Version compatibility).
pub fn connect_with_version(
    name: &EndpointName,
    secret: &BootSecret,
    major: u32,
    minor: u32,
) -> Result<Connection, TransportError> {
    let mut stream = Stream::connect(name.interprocess_name().to_owned())
        .map_err(|e| TransportError::Io(std::io::Error::other(e.to_string())))?;

    let challenge_bytes = read_frame(&mut stream)?;
    let challenge = pb::Challenge::decode(challenge_bytes.as_slice()).map_err(|e| {
        TransportError::Protocol {
            reason: format!("bad Challenge: {e}"),
        }
    })?;
    let client_nonce = random_nonce()?;

    let proof = secret.hmac_proof(&challenge.server_nonce, &client_nonce, major, minor);
    write_frame(
        &mut stream,
        &pb::Hello {
            proof,
            client_nonce: client_nonce.to_vec(),
            major,
            minor,
        }
        .encode_to_vec(),
    )?;

    let result_bytes = read_frame(&mut stream)?;
    let result =
        pb::AuthResult::decode(result_bytes.as_slice()).map_err(|e| TransportError::Protocol {
            reason: format!("bad AuthResult: {e}"),
        })?;
    if !result.ok {
        return Err(TransportError::AuthRejected {
            reason: result.error,
        });
    }
    Ok(Connection {
        stream,
        read_only: result.read_only,
        negotiated: (result.negotiated_major, result.negotiated_minor),
    })
}

/// Post-auth frame processor: request payload in, response frame out.
pub type FrameHandler = Arc<dyn Fn(&[u8]) -> Vec<u8> + Send + Sync>;

/// Serves authenticated connections until the listener fails. `handler`
/// receives each post-auth frame payload and produces the response frame.
pub fn serve(
    listener: interprocess::local_socket::Listener,
    secret: BootSecret,
    handler: FrameHandler,
) {
    loop {
        let Ok(mut stream) = listener.accept() else {
            return;
        };
        let secret = secret.clone();
        let handler = handler.clone();
        std::thread::spawn(move || {
            // A rejected or misbehaving peer must not take the server down:
            // the connection is closed and the accept loop continues.
            let Ok((accepted, _read_only, _version)) = server_handshake(&mut stream, &secret)
            else {
                return;
            };
            if !accepted {
                return;
            }
            loop {
                match read_frame(&mut stream) {
                    Ok(request) => {
                        let response = handler(&request);
                        if write_frame(&mut stream, &response).is_err() {
                            return;
                        }
                    }
                    // Clean EOF from the client ends the connection.
                    Err(TransportError::Protocol { .. }) | Err(TransportError::Io(_)) => return,
                    Err(_) => return,
                }
            }
        });
    }
}
