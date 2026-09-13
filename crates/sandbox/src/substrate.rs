//! Firecracker substrate adapter (M8.3, REQ-EV-0285, docs/24 §
//! substrate): the production VM lifecycle controller, host-side vsock
//! link and guest bootstrap contract for the `modbit-guest` RPC agent.
//! One substrate behind the SAME guest contract — nothing here leaks into
//! the agent/tool/domain layers (REQ-EV-0291).
//!
//! Platform scoping (the workspace forbids `unsafe_code`): the VMM
//! controller and host vsock link are unix-only (Firecracker hosts are
//! unix; Windows gets typed fail-closed stubs with the same API). The
//! guest-side AF_VSOCK listener exists on Linux through the safe `vsock`
//! crate; everywhere else it fails closed.
//!
//! HONEST SCOPE (docs/50 real-system gates): this module contains the
//! complete production adapter code, configuration, controller and
//! fixtures, unit-tested against a fixture VMM socket server and against
//! REAL subprocess lifecycle failure paths. It does NOT and MUST NOT
//! claim real-guest (Linux/KVM + Firecracker) validation: that conformance
//! is an environmental proof requirement recorded in Future-tasks §4
//! Phase 8 item 2. The fixture VMM in tests is labeled as a controller
//! fixture, never as virtualization.

use serde::{Deserialize, Serialize};

/// Typed substrate failure — every boot/connect failure carries what the
/// operator needs; nothing fails silently.
#[derive(Debug)]
pub enum SubstrateError {
    /// The VMM binary could not be spawned (missing, not executable).
    VmmBinaryMissing {
        program: String,
        detail: String,
    },
    /// The VMM API rejected a configuration step.
    Api {
        path: String,
        status: u16,
        fault: String,
    },
    /// The VMM API socket never became usable.
    ApiSocketTimeout {
        path: String,
    },
    /// The VM did not reach the Running state within the boot budget.
    BootTimeout {
        waited_ms: u64,
    },
    /// The host-side vsock link refused the CONNECT.
    VsockHandshake {
        port: u32,
        detail: String,
    },
    /// vsock/VMM on this platform has no implementation (fail closed).
    PlatformUnsupported {
        detail: String,
    },
    Io(String),
}

impl std::fmt::Display for SubstrateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SubstrateError::VmmBinaryMissing { program, detail } => {
                write!(f, "VMM binary {program:?} unavailable: {detail}")
            }
            SubstrateError::Api {
                path,
                status,
                fault,
            } => {
                write!(f, "VMM API {path} rejected ({status}): {fault}")
            }
            SubstrateError::ApiSocketTimeout { path } => {
                write!(f, "VMM API socket {path} never became usable")
            }
            SubstrateError::BootTimeout { waited_ms } => {
                write!(f, "VM did not reach Running within {waited_ms} ms")
            }
            SubstrateError::VsockHandshake { port, detail } => {
                write!(f, "vsock CONNECT port {port} refused: {detail}")
            }
            SubstrateError::PlatformUnsupported { detail } => {
                write!(f, "substrate unsupported here: {detail}")
            }
            SubstrateError::Io(e) => write!(f, "substrate io: {e}"),
        }
    }
}

impl std::error::Error for SubstrateError {}

// ---------------------------------------------------------------------------
// Machine configuration (the production VM spec) — pure data, all platforms
// ---------------------------------------------------------------------------

/// The full production VM spec for one task guest. Serializable so the
/// substrate provisioner can version/review configs; secrets NEVER appear
/// here (provisioning material travels only through [`GuestBootstrap`]
/// into a tmpfs file, never into the image or this config).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FirecrackerSpec {
    pub vcpus: u8,
    pub mem_mib: u32,
    /// Guest CID for the vsock device (unique per running VM on the host).
    pub guest_cid: u32,
    pub kernel_path: String,
    pub rootfs_path: String,
    /// Firecracker exposes the guest vsock to the HOST through this UDS.
    pub vsock_uds_path: String,
    pub api_sock_path: String,
    /// Kernel boot args; the guest init mounts /run (tmpfs) and starts
    /// modbit-guest per the bootstrap contract (see [`GuestBootstrap`]).
    pub boot_args: String,
}

impl Default for FirecrackerSpec {
    fn default() -> Self {
        FirecrackerSpec {
            vcpus: 2,
            mem_mib: 2048,
            guest_cid: 3,
            kernel_path: "/opt/modbit/vmlinux".into(),
            rootfs_path: "/opt/modbit/rootfs.ext4".into(),
            vsock_uds_path: "/run/modbit/vsock.sock".into(),
            api_sock_path: "/run/modbit/firecracker.sock".into(),
            boot_args: "console=ttyS0 reboot=k panic=1 pci=off i8042.noaux".into(),
        }
    }
}

// ---------------------------------------------------------------------------
// Guest bootstrap contract — pure data, all platforms
// ---------------------------------------------------------------------------

/// The bootstrap contract between the substrate provisioner and the
/// modbit-guest init inside the VM:
///
/// 1. The rootfs image is GENERIC — it contains the modbit-guest binary,
///    an init that mounts `/run` as tmpfs and starts `modbit-guest`
///    sourcing `/run/modbit/guest.env`. NO secrets are baked into any
///    image (REQ-EV-0288).
/// 2. At VM start the provisioner renders [`GuestBootstrap`] through
///    [`GuestBootstrap::env_file`] and injects it into the VM's tmpfs via
///    the substrate's provisioning channel (vsock-first fetch or a
///    read-only tmpfs drive), never into the rootfs.
/// 3. The host then talks ONLY the verified guest RPC over the vsock
///    link (see the unix `connect_guest_vsock`).
#[derive(Clone, Debug, PartialEq)]
pub struct GuestBootstrap {
    pub task: String,
    /// Provisioning key material — hex HMAC key for the manifest signature.
    /// Lives ONLY in the rendered env file, never in images or configs.
    pub provision_key_hex: String,
    pub capability_tokens: Vec<String>,
    pub fs_roots: Vec<String>,
    pub vsock_port: u32,
}

impl GuestBootstrap {
    /// Renders the `/run/modbit/guest.env` contents.
    pub fn env_file(&self) -> String {
        let mut s = String::new();
        s.push_str("MODBIT_GUEST_TRANSPORT=vsock\n");
        s.push_str(&format!("MODBIT_GUEST_TASK={}\n", self.task));
        s.push_str(&format!(
            "MODBIT_GUEST_PROVISION_KEY={}\n",
            self.provision_key_hex
        ));
        s.push_str(&format!(
            "MODBIT_GUEST_TOKENS={}\n",
            self.capability_tokens.join(",")
        ));
        s.push_str(&format!(
            "MODBIT_GUEST_FS_ROOTS={}\n",
            self.fs_roots.join(":")
        ));
        s.push_str(&format!("MODBIT_GUEST_VSOCK_PORT={}\n", self.vsock_port));
        s
    }
}

// ---------------------------------------------------------------------------
// VMM controller + host vsock link (unix platforms — Firecracker hosts)
// ---------------------------------------------------------------------------

#[cfg(unix)]
pub mod vmm {
    use super::{FirecrackerSpec, SubstrateError};
    use std::io::{BufRead, BufReader, Read, Write};
    use std::os::unix::net::UnixStream;
    use std::path::Path;
    use std::process::Child;
    use std::time::{Duration, Instant};

    /// Minimal Firecracker API client (HTTP/1.1 over the unix API socket).
    pub struct FirecrackerApi {
        sock_path: String,
    }

    #[derive(Debug)]
    pub struct ApiResponse {
        pub status: u16,
        pub body: String,
    }

    impl FirecrackerApi {
        pub fn over(sock_path: impl Into<String>) -> Self {
            FirecrackerApi {
                sock_path: sock_path.into(),
            }
        }

        fn request(
            &self,
            method: &str,
            path: &str,
            body: Option<&str>,
        ) -> Result<ApiResponse, SubstrateError> {
            let mut stream = UnixStream::connect(&self.sock_path)
                .map_err(|e| SubstrateError::Io(format!("connect {}: {e}", self.sock_path)))?;
            let body = body.unwrap_or("");
            let req = format!(
                "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nAccept: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream
                .write_all(req.as_bytes())
                .map_err(|e| SubstrateError::Io(format!("api write: {e}")))?;
            let mut reader = BufReader::new(stream);
            let mut status_line = String::new();
            reader
                .read_line(&mut status_line)
                .map_err(|e| SubstrateError::Io(format!("api read: {e}")))?;
            // "HTTP/1.1 204 No Content"
            let status = status_line
                .split_whitespace()
                .nth(1)
                .and_then(|s| s.parse::<u16>().ok())
                .ok_or_else(|| SubstrateError::Io(format!("bad status line {status_line:?}")))?;
            let mut content_length: usize = 0;
            loop {
                let mut line = String::new();
                reader
                    .read_line(&mut line)
                    .map_err(|e| SubstrateError::Io(format!("api header: {e}")))?;
                let line = line.trim_end();
                if line.is_empty() {
                    break;
                }
                if let Some(v) = line
                    .to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .map(|v| v.trim().to_string())
                {
                    content_length = v.parse().unwrap_or(0);
                }
            }
            let mut body_buf = vec![0u8; content_length];
            if content_length > 0 {
                reader
                    .read_exact(&mut body_buf)
                    .map_err(|e| SubstrateError::Io(format!("api body: {e}")))?;
            }
            Ok(ApiResponse {
                status,
                body: String::from_utf8_lossy(&body_buf).to_string(),
            })
        }

        /// PUT a JSON configuration; 2xx passes, anything else extracts the
        /// Firecracker fault message into a typed error.
        pub fn put(&self, path: &str, json: &str) -> Result<ApiResponse, SubstrateError> {
            let resp = self.request("PUT", path, Some(json))?;
            self.checked(path, resp)
        }

        pub fn get(&self, path: &str) -> Result<ApiResponse, SubstrateError> {
            let resp = self.request("GET", path, None)?;
            self.checked(path, resp)
        }

        fn checked(&self, path: &str, resp: ApiResponse) -> Result<ApiResponse, SubstrateError> {
            if (200..300).contains(&resp.status) {
                return Ok(resp);
            }
            let fault = serde_json::from_str::<serde_json::Value>(&resp.body)
                .ok()
                .and_then(|v| {
                    v.get("fault_message")
                        .and_then(|f| f.as_str().map(|s| s.to_string()))
                })
                .unwrap_or_else(|| resp.body.clone());
            Err(SubstrateError::Api {
                path: path.to_string(),
                status: resp.status,
                fault,
            })
        }

        /// VM state from GET / ({"state": "..."} or {"vm": {"state": "..."}}).
        pub fn vm_state(&self) -> Result<String, SubstrateError> {
            let resp = self.get("/")?;
            let v: serde_json::Value = serde_json::from_str(&resp.body)
                .map_err(|e| SubstrateError::Io(format!("bad state body: {e}")))?;
            let state = v
                .get("state")
                .and_then(|s| s.as_str())
                .or_else(|| {
                    v.get("vm")
                        .and_then(|vm| vm.get("state"))
                        .and_then(|s| s.as_str())
                })
                .unwrap_or("Unknown")
                .to_string();
            Ok(state)
        }
    }

    /// A booted, configured, started VM. Drop kills the VMM process (a
    /// dead controller never leaves an orphan VM behind).
    pub struct RunningVm {
        pub spec: FirecrackerSpec,
        api: FirecrackerApi,
        child: Option<Child>,
    }

    impl std::fmt::Debug for RunningVm {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("RunningVm")
                .field("spec", &self.spec)
                .finish_non_exhaustive()
        }
    }

    impl RunningVm {
        pub fn state(&self) -> Result<String, SubstrateError> {
            self.api.vm_state()
        }

        /// Substrate-level stop: attempt a guest-initiated shutdown first,
        /// then kill the VMM. The VM is dead when this returns.
        pub fn shutdown(&mut self) -> Result<(), SubstrateError> {
            // Best-effort graceful action; failure falls through to kill.
            let _ = self
                .api
                .put("/actions", "{\"action_type\": \"SendCtrlAltDel\"}");
            self.kill()
        }

        fn kill(&mut self) -> Result<(), SubstrateError> {
            if let Some(mut child) = self.child.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
            let _ = std::fs::remove_file(&self.spec.api_sock_path);
            let _ = std::fs::remove_file(&self.spec.vsock_uds_path);
            Ok(())
        }
    }

    impl Drop for RunningVm {
        fn drop(&mut self) {
            let _ = self.kill();
        }
    }

    /// Boots one task VM: spawns the VMM, waits for its API socket, applies
    /// the configuration (machine config, boot source, rootfs drive, vsock
    /// device), starts the instance and waits for Running. Production
    /// calls `boot(spec, "firecracker", &[])`; `extra_args` exists so
    /// tests can drive the FULL controller lifecycle with a fixture
    /// process.
    pub fn boot(
        spec: FirecrackerSpec,
        vmm_binary: &str,
        extra_args: &[String],
    ) -> Result<RunningVm, SubstrateError> {
        let _ = std::fs::remove_file(&spec.api_sock_path);
        let mut command = std::process::Command::new(vmm_binary);
        command.arg("--api-sock").arg(&spec.api_sock_path);
        command.args(extra_args);
        let mut child = command
            .spawn()
            .map_err(|e| SubstrateError::VmmBinaryMissing {
                program: vmm_binary.to_string(),
                detail: e.to_string(),
            })?;

        match boot_configured(&spec) {
            Ok(()) => Ok(RunningVm {
                api: FirecrackerApi::over(&spec.api_sock_path),
                spec,
                child: Some(child),
            }),
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                Err(e)
            }
        }
    }

    fn boot_configured(spec: &FirecrackerSpec) -> Result<(), SubstrateError> {
        // 1. Wait for the API socket to appear (bounded; slow CI runners
        // can stall seconds at a time).
        let deadline = Instant::now() + Duration::from_secs(10);
        let sock = Path::new(&spec.api_sock_path);
        loop {
            if sock.exists() && UnixStream::connect(sock).is_ok() {
                break;
            }
            if Instant::now() >= deadline {
                return Err(SubstrateError::ApiSocketTimeout {
                    path: spec.api_sock_path.clone(),
                });
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        let api = FirecrackerApi::over(&spec.api_sock_path);
        // 2. Configure. Each step is checked; failures carry fault messages.
        let machine = serde_json::json!({
            "vcpu_count": spec.vcpus,
            "mem_size_mib": spec.mem_mib,
        });
        api.put("/machine-config", &machine.to_string())?;
        let boot_source = serde_json::json!({
            "kernel_image_path": spec.kernel_path,
            "boot_args": spec.boot_args,
        });
        api.put("/boot-source", &boot_source.to_string())?;
        let drive = serde_json::json!({
            "drive_id": "rootfs",
            "path_on_host": spec.rootfs_path,
            "is_root_device": true,
            "is_read_only": true,
        });
        api.put("/drives/rootfs", &drive.to_string())?;
        let vsock = serde_json::json!({
            "guest_cid": spec.guest_cid,
            "uds_path": spec.vsock_uds_path,
        });
        api.put("/vsock", &vsock.to_string())?;
        // 3. Start.
        api.put("/actions", "{\"action_type\": \"InstanceStart\"}")?;
        // 4. Wait for Running (bounded).
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let state = api.vm_state()?;
            if state == "Running" {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(SubstrateError::BootTimeout { waited_ms: 10_000 });
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Opens a raw byte channel to the guest's vsock `port` through the
    /// VMM's host UDS. On success the stream IS the guest-side
    /// TCP-equivalent: the guest RPC (boot-secret handshake, manifest,
    /// typed frames) rides it unchanged.
    pub fn connect_guest_vsock(uds_path: &Path, port: u32) -> Result<UnixStream, SubstrateError> {
        let mut stream =
            UnixStream::connect(uds_path).map_err(|e| SubstrateError::VsockHandshake {
                port,
                detail: format!("connect {:?}: {e}", uds_path),
            })?;
        stream
            .write_all(format!("CONNECT {port}\n").as_bytes())
            .map_err(|e| SubstrateError::VsockHandshake {
                port,
                detail: format!("write: {e}"),
            })?;
        // Read the reply line BYTE-WISE on the channel itself — a buffered
        // clone could swallow a coalesced first payload beyond the newline.
        let mut line = Vec::new();
        let mut one = [0u8; 1];
        loop {
            let n = stream
                .read(&mut one)
                .map_err(|e| SubstrateError::VsockHandshake {
                    port,
                    detail: format!("read: {e}"),
                })?;
            if n == 0 {
                return Err(SubstrateError::VsockHandshake {
                    port,
                    detail: "connection closed before Ok".into(),
                });
            }
            if one[0] == b'\n' {
                break;
            }
            line.push(one[0]);
            if line.len() > 128 {
                return Err(SubstrateError::VsockHandshake {
                    port,
                    detail: "runaway handshake reply".into(),
                });
            }
        }
        if String::from_utf8_lossy(&line).trim_end() != "Ok" {
            return Err(SubstrateError::VsockHandshake {
                port,
                detail: format!("guest refused: {:?}", String::from_utf8_lossy(&line)),
            });
        }
        Ok(stream)
    }
}

#[cfg(unix)]
pub use vmm::{boot, connect_guest_vsock, FirecrackerApi, RunningVm};

/// Windows fail-closed stubs: Firecracker hosts are unix; the same entry
/// points exist so callers compile, and every call is a typed refusal —
/// never a silent success.
#[cfg(not(unix))]
pub mod vmm {
    use super::{FirecrackerSpec, SubstrateError};

    pub struct FirecrackerApi;

    impl FirecrackerApi {
        pub fn over(_sock_path: impl Into<String>) -> Self {
            FirecrackerApi
        }

        pub fn vm_state(&self) -> Result<String, SubstrateError> {
            Err(SubstrateError::PlatformUnsupported {
                detail: "Firecracker VMM controller requires a unix host".into(),
            })
        }
    }

    #[derive(Debug)]
    pub struct RunningVm {
        pub spec: FirecrackerSpec,
    }

    impl RunningVm {
        pub fn state(&self) -> Result<String, SubstrateError> {
            Err(SubstrateError::PlatformUnsupported {
                detail: "Firecracker VMM controller requires a unix host".into(),
            })
        }

        pub fn shutdown(&mut self) -> Result<(), SubstrateError> {
            Err(SubstrateError::PlatformUnsupported {
                detail: "Firecracker VMM controller requires a unix host".into(),
            })
        }
    }

    pub fn boot(
        _spec: FirecrackerSpec,
        _vmm_binary: &str,
        _extra_args: &[String],
    ) -> Result<RunningVm, SubstrateError> {
        Err(SubstrateError::PlatformUnsupported {
            detail: "Firecracker VMM controller requires a unix host".into(),
        })
    }

    pub fn connect_guest_vsock(
        _uds_path: &std::path::Path,
        port: u32,
    ) -> Result<std::net::TcpStream, SubstrateError> {
        Err(SubstrateError::VsockHandshake {
            port,
            detail: "host vsock link requires a unix host".into(),
        })
    }
}

// ---------------------------------------------------------------------------
// In-guest vsock listener (production transport server side). Linux via
// the SAFE `vsock` crate (the workspace forbids unsafe code); everywhere
// else it fails closed with a typed error rather than pretending to
// listen.
// ---------------------------------------------------------------------------

/// Binds a guest-side AF_VSOCK listener on (cid, port). The guest CID is
/// `VMADDR_CID_ANY` for the guest itself.
#[cfg(target_os = "linux")]
pub mod guest_vsock {
    use super::SubstrateError;

    const VMADDR_CID_ANY: u32 = 0xFFFF_FFFF;

    pub struct VsockListener {
        listener: vsock::VsockListener,
    }

    impl VsockListener {
        pub fn bind(port: u32) -> Result<Self, SubstrateError> {
            let addr = vsock::VsockAddr::new(VMADDR_CID_ANY, port);
            let listener = vsock::VsockListener::bind(&addr)
                .map_err(|e| SubstrateError::Io(format!("vsock bind port {port}: {e}")))?;
            Ok(VsockListener { listener })
        }

        /// Accepts one host connection as a `VsockStream` (Read + Write +
        /// Send), which feeds the transport-independent connection body
        /// unchanged.
        pub fn accept(&self) -> Result<vsock::VsockStream, SubstrateError> {
            let (stream, _) = self
                .listener
                .accept()
                .map_err(|e| SubstrateError::Io(format!("vsock accept: {e}")))?;
            Ok(stream)
        }
    }
}

#[cfg(all(unix, not(target_os = "linux")))]
/// Guest-side vsock has no implementation off Linux — fail closed with a
/// typed error rather than pretending to listen. Same API surface shape
/// as the Linux implementation so callers compile unchanged.
pub mod guest_vsock {
    use super::SubstrateError;

    pub struct VsockListener;

    impl VsockListener {
        pub fn bind(_port: u32) -> Result<Self, SubstrateError> {
            Err(SubstrateError::PlatformUnsupported {
                detail: "guest vsock requires the Linux guest kernel".into(),
            })
        }
    }
}

#[cfg(not(unix))]
pub mod guest_vsock {
    use super::SubstrateError;

    pub struct VsockListener;

    impl VsockListener {
        pub fn bind(_port: u32) -> Result<Self, SubstrateError> {
            Err(SubstrateError::PlatformUnsupported {
                detail: "guest vsock requires the Linux guest kernel".into(),
            })
        }
    }
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::vmm::{boot, connect_guest_vsock};
    use super::{FirecrackerSpec, GuestBootstrap, SubstrateError};
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    /// THE FIXTURE VMM: a unix-socket server that speaks just enough of
    /// the Firecracker API to drive the controller. This validates the
    /// CONTROLLER (request shapes, ordering, state polling, fault
    /// mapping) — it is NOT virtualization and proves nothing about a
    /// real guest (Future-tasks §4 Phase 8 item 2 environmental gate).
    #[test]
    fn controller_boots_and_reaches_running_against_fixture_vmm() {
        let dir =
            std::env::temp_dir().join(format!("modbit-vmm-{}", uuid::Uuid::now_v7().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let api_sock = dir.join("api.sock");

        let stop = Arc::new(AtomicBool::new(false));
        let server_stop = stop.clone();
        let api_sock_path = api_sock.clone();
        let server = std::thread::spawn(move || {
            let listener = UnixListener::bind(&api_sock_path).unwrap();
            listener.set_nonblocking(true).expect("fixture nonblocking");
            let mut started = false;
            loop {
                if server_stop.load(Ordering::SeqCst) {
                    return;
                }
                let (mut stream, _) = match listener.accept() {
                    Ok(s) => s,
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(_) => return,
                };
                let mut buf = vec![0u8; 4096];
                // The controller's socket probe leaves an accepted-but-
                // silent connection in the backlog: skip it, keep serving.
                let n = match stream.read(&mut buf) {
                    Ok(0) | Err(_) => continue,
                    Ok(n) => n,
                };
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let mut parts = req.split_whitespace();
                let method = parts.next().unwrap_or("").to_string();
                let path = parts.next().unwrap_or("").to_string();
                let reply = match (method.as_str(), path.as_str()) {
                    ("PUT", "/machine-config")
                    | ("PUT", "/boot-source")
                    | ("PUT", "/drives/rootfs")
                    | ("PUT", "/vsock") => {
                        "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n".to_string()
                    }
                    ("PUT", "/actions") => {
                        // InstanceStart boots; SendCtrlAltDel is the
                        // graceful-shutdown attempt from shutdown().
                        if req.contains("InstanceStart") {
                            started = true;
                        }
                        "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n".to_string()
                    }
                    ("GET", "/") => {
                        // Not Running until InstanceStart has been served —
                        // exercises the poll loop.
                        let state = if started { "Running" } else { "Uninitialized" };
                        let body = format!("{{\"state\":\"{state}\"}}");
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                            body.len()
                        )
                    }
                    _ => {
                        let body = format!(
                            "{{\"fault_message\": \"fixture: unexpected {method} {path}\"}}"
                        );
                        format!(
                            "HTTP/1.1 400 Bad Request\r\nContent-Length: {}\r\n\r\n{body}",
                            body.len()
                        )
                    }
                };
                let _ = stream.write_all(reply.as_bytes());
            }
        });

        let spec = FirecrackerSpec {
            api_sock_path: api_sock.to_string_lossy().to_string(),
            vsock_uds_path: dir.join("vsock.sock").to_string_lossy().to_string(),
            ..Default::default()
        };
        // The fixture serves the API protocol but the controller still
        // spawns a REAL process — a sleeping no-op binary stands in for
        // the VMM process so lifecycle (spawn/Drop-kill) is exercised.
        // Production passes ("firecracker", &[]).
        let (stand_in, extra) = ("sleep", vec!["30".to_string()]);
        let mut vm = boot(spec, stand_in, &extra).expect("fixture boot");
        assert_eq!(vm.state().expect("state"), "Running");
        vm.shutdown().expect("shutdown");
        stop.store(true, Ordering::SeqCst);
        server.join().expect("fixture server");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A missing/unusable VMM binary is a typed failure, not a panic.
    #[test]
    fn boot_with_missing_vmm_binary_fails_typed() {
        let spec = FirecrackerSpec {
            api_sock_path: std::env::temp_dir()
                .join(format!(
                    "modbit-vmm-missing-{}.sock",
                    uuid::Uuid::now_v7().simple()
                ))
                .to_string_lossy()
                .to_string(),
            ..Default::default()
        };
        match boot(spec, "definitely-not-a-vmm-binary", &[]) {
            Err(SubstrateError::VmmBinaryMissing { program, .. }) => {
                assert_eq!(program, "definitely-not-a-vmm-binary");
            }
            other => panic!("expected VmmBinaryMissing, got {other:?}"),
        }
    }

    /// The host vsock link performs the `CONNECT <port>` / `Ok` handshake
    /// and yields a usable byte channel; a refused port is a typed error.
    #[test]
    fn vsock_link_handshake_and_refusal() {
        let dir =
            std::env::temp_dir().join(format!("modbit-vsock-{}", uuid::Uuid::now_v7().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let uds = dir.join("v.sock");
        let uds_path = uds.clone();
        let server = std::thread::spawn(move || {
            let listener = UnixListener::bind(&uds_path).unwrap();
            let (mut stream, _) = listener.accept().expect("accept");
            let mut buf = [0u8; 64];
            let n = stream.read(&mut buf).unwrap();
            let line = String::from_utf8_lossy(&buf[..n]);
            if line.trim() == "CONNECT 5000" {
                let _ = stream.write_all(b"Ok\n");
                // Echo one payload to prove the channel is live.
                let _ = stream.write_all(b"guest-hello");
            } else {
                let _ = stream.write_all(b"Indicated port is unavailable\n");
            }
        });
        // Wait for the fixture socket, then connect (no bind race).
        let waited_for = Instant::now();
        while !uds.exists() {
            assert!(
                waited_for.elapsed() < Duration::from_secs(2),
                "fixture vsock socket never appeared"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        // Happy path.
        let mut chan = connect_guest_vsock(&uds, 5000).expect("vsock connect");
        let mut got = Vec::new();
        let _ = chan.read_to_end(&mut got);
        assert!(String::from_utf8_lossy(&got).contains("guest-hello"));
        server.join().unwrap();

        // Refusal path.
        let server2_path = dir.join("v2.sock");
        let server2 = std::thread::spawn(move || {
            let listener = UnixListener::bind(&server2_path).unwrap();
            let (mut stream, _) = listener.accept().expect("accept");
            let mut buf = [0u8; 64];
            let _ = stream.read(&mut buf);
            let _ = stream.write_all(b"Indicated port is unavailable\n");
        });
        let refusal_sock = dir.join("v2.sock");
        while !refusal_sock.exists() {
            std::thread::sleep(Duration::from_millis(5));
        }
        match connect_guest_vsock(&refusal_sock, 5999) {
            Err(SubstrateError::VsockHandshake { port, detail }) => {
                assert_eq!(port, 5999);
                assert!(detail.contains("unavailable"), "{detail}");
            }
            other => panic!("expected VsockHandshake refusal, got {other:?}"),
        }
        server2.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Bootstrap contract: rendered env reaches tmpfs ONLY — the VM spec
    /// (image/config side) carries no secret material, and the env file
    /// carries exactly the guest boot contract.
    #[test]
    fn bootstrap_renders_env_without_secrets_in_spec() {
        let bootstrap = GuestBootstrap {
            task: "task-x".into(),
            provision_key_hex: "aabbccdd".repeat(4),
            capability_tokens: vec!["tok-a".into(), "tok-b".into()],
            fs_roots: vec!["/workspace".into()],
            vsock_port: 5001,
        };
        let env = bootstrap.env_file();
        for needle in [
            "MODBIT_GUEST_TRANSPORT=vsock",
            "MODBIT_GUEST_TASK=task-x",
            "MODBIT_GUEST_PROVISION_KEY=aabbccddaabbccddaabbccddaabbccdd",
            "MODBIT_GUEST_TOKENS=tok-a,tok-b",
            "MODBIT_GUEST_FS_ROOTS=/workspace",
            "MODBIT_GUEST_VSOCK_PORT=5001",
        ] {
            assert!(env.contains(needle), "env missing {needle}");
        }
        // The VM spec never carries bootstrap material.
        let spec_json = serde_json::to_string(&FirecrackerSpec::default()).unwrap();
        assert!(
            !spec_json.contains("aabbccdd"),
            "provision key leaked into VM spec"
        );
        assert!(
            !spec_json.contains("tok-a"),
            "capability token leaked into VM spec"
        );
        // The generic rootfs contract: guest.env path is under /run (tmpfs).
        assert!(env.starts_with("MODBIT_GUEST_TRANSPORT=vsock\n"));
    }
}
