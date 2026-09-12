//! CDP bridge (Phase 7 item 3): the real Chromium transport under the
//! semantic browser layer. Launches the local Chromium-family binary with
//! `--remote-debugging-port=0`, reads the `DevToolsActivePort` handshake
//! file, attaches to the page target over WebSocket (flattened session),
//! and serves the agent verbs: navigate / snapshot / action / network /
//! console / capture.
//!
//! Reuses the existing semantic machinery (`PageState`, `SemanticElement`,
//! fingerprints) — this module only moves bytes to and from the browser.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::WebSocket;

#[derive(Debug)]
pub struct CdpError {
    pub message: String,
}

impl std::fmt::Display for CdpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "cdp: {}", self.message)
    }
}

impl std::error::Error for CdpError {}

fn err<E: std::fmt::Display>(e: E) -> CdpError {
    CdpError {
        message: e.to_string(),
    }
}

type Ws = WebSocket<MaybeTlsStream<std::net::TcpStream>>;

/// Console + network observations collected from domain events while the
/// session runs (the `console` / `network` verbs drain these).
#[derive(Default, Debug, Clone)]
pub struct Observations {
    pub console: Vec<String>,
    pub requests: Vec<String>,
    pub responses: Vec<String>,
}

pub struct CdpBrowser {
    socket: Ws,
    session_id: String,
    target_id: String,
    next_id: u64,
    observations: Observations,
    child: Child,
    user_data_dir: PathBuf,
}

impl CdpBrowser {
    /// Locates a usable Chromium-family binary: explicit override first
    /// (`MODBIT_BROWSER_BIN`), then common per-OS names/paths.
    pub fn find_browser() -> Option<PathBuf> {
        if let Ok(from_env) = std::env::var("MODBIT_BROWSER_BIN") {
            if !from_env.is_empty() {
                return Some(PathBuf::from(from_env));
            }
        }
        let candidates: &[&str] = if cfg!(target_os = "macos") {
            &[
                "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
                "/Applications/Chromium.app/Contents/MacOS/Chromium",
                "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
                "/Applications/Google Chrome Canary.app/Contents/MacOS/Google Chrome Canary",
            ]
        } else if cfg!(target_os = "windows") {
            &[
                "C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe",
                "C:\\Program Files (x86)\\Google\\Chrome\\Application\\chrome.exe",
                "C:\\Program Files (x86)\\Microsoft\\Edge\\Application\\msedge.exe",
            ]
        } else {
            &[
                "/usr/bin/chromium",
                "/usr/bin/chromium-browser",
                "/usr/bin/google-chrome",
                "/usr/bin/google-chrome-stable",
                "/snap/bin/chromium",
            ]
        };
        candidates
            .iter()
            .map(PathBuf::from)
            .find(|p| p.exists() || which_exists(&p.display().to_string()))
    }

    /// Launches headless Chromium and attaches to its page target.
    pub fn launch(browser_bin: &Path) -> Result<Self, CdpError> {
        let user_data_dir = std::env::temp_dir().join(format!(
            "modbit-cdp-{}-{}",
            std::process::id(),
            uuid_like_suffix()
        ));
        std::fs::create_dir_all(&user_data_dir).map_err(err)?;
        let child = Command::new(browser_bin)
            .arg("--headless=new")
            .arg("--remote-debugging-port=0")
            .arg("--no-first-run")
            .arg("--no-default-browser-check")
            .arg("--disable-gpu")
            .arg(format!("--user-data-dir={}", user_data_dir.display()))
            .arg("about:blank")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(err)?;

        // Chromium writes the handshake file once the debug server binds.
        let port_file = user_data_dir.join("DevToolsActivePort");
        let deadline = Instant::now() + Duration::from_secs(30);
        let (port, ws_path) = loop {
            if Instant::now() >= deadline {
                let mut child = child;
                let _ = child.kill();
                return Err(CdpError {
                    message: "browser never wrote DevToolsActivePort".into(),
                });
            }
            if let Ok(content) = std::fs::read_to_string(&port_file) {
                let mut lines = content.lines();
                if let (Some(p), Some(path)) = (lines.next(), lines.next()) {
                    if let Ok(port) = p.parse::<u16>() {
                        break (port, path.trim().to_string());
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(100));
        };

        let socket = tungstenite::connect(format!("ws://127.0.0.1:{port}{ws_path}"))
            .map_err(err)?
            .0;

        let mut browser = Self {
            socket,
            session_id: String::new(),
            target_id: String::new(),
            next_id: 0,
            observations: Observations::default(),
            child,
            user_data_dir,
        };

        // Attach to the initial page target (about:blank from the command
        // line); flattened sessions keep ONE socket.
        let targets = browser.command_value(None, "Target.getTargets", json!({}))?;
        let page = targets["targetInfos"]
            .as_array()
            .and_then(|list| {
                list.iter()
                    .find(|t| t["type"] == "page" && t["url"] != "devtools://devtools")
            })
            .and_then(|t| t["targetId"].as_str().map(str::to_string))
            .ok_or_else(|| CdpError {
                message: "no page target after launch".into(),
            })?;
        browser.target_id = page;
        let attached = browser.command_value(
            None,
            "Target.attachToTarget",
            json!({ "targetId": browser.target_id, "flatten": true }),
        )?;
        browser.session_id = attached["sessionId"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        if browser.session_id.is_empty() {
            return Err(CdpError {
                message: "attach produced no session id".into(),
            });
        }
        // Domains we observe events from — sent ON the attached session
        // (browser-level sockets have no page domains).
        browser.command_value(
            Some(browser.session_id.clone()),
            "Runtime.enable",
            json!({}),
        )?;
        browser.command_value(
            Some(browser.session_id.clone()),
            "Page.enable",
            json!({}),
        )?;
        browser.command_value(
            Some(browser.session_id.clone()),
            "Network.enable",
            json!({}),
        )?;
        Ok(browser)
    }

    /// `Page.navigate` + wait for the document to finish loading.
    pub fn navigate(&mut self, url: &str) -> Result<(), CdpError> {
        self.command_value(
            Some(self.session_id.clone()),
            "Page.navigate",
            json!({ "url": url }),
        )?;
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if Instant::now() >= deadline {
                return Err(CdpError {
                    message: format!("navigation to {url} never settled"),
                });
            }
            let ready = self.evaluate("document.readyState")?;
            if ready.as_str() == Some("complete") {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// `Runtime.evaluate` returning the raw JSON value (returnByValue).
    pub fn evaluate(&mut self, expression: &str) -> Result<Value, CdpError> {
        let out = self.command_value(
            Some(self.session_id.clone()),
            "Runtime.evaluate",
            json!({ "expression": expression, "returnByValue": true }),
        )?;
        Ok(out["result"]["value"].clone())
    }

    /// The `snapshot` verb: agent-native semantic state — url, title,
    /// visible text and interactive elements as `SemanticElement`s with
    /// stable refs (feeds `PageState` fingerprints/deltas unchanged).
    pub fn snapshot(&mut self) -> Result<crate::PageState, CdpError> {
        let js = r#"JSON.stringify({
            url: location.href,
            title: document.title,
            text: document.body ? document.body.innerText.slice(0, 20000) : "",
            elements: [...document.querySelectorAll(
                "a,button,input,select,textarea,[role=button],[onclick]"
            )].slice(0, 200).map((e, i) => ({
                tag: e.tagName.toLowerCase(),
                text: ((e.innerText || e.value || e.getAttribute("aria-label") || "") + "").slice(0, 120),
                href: e.href || null,
                i
            }))
        })"#;
        let raw = self.evaluate(js)?;
        let page = serde_json::from_str::<Value>(
            raw.as_str().unwrap_or_default(),
        )
        .map_err(err)?;
        let elements = page["elements"]
            .as_array()
            .map(|list| {
                list.iter()
                    .filter_map(|e| {
                        let tag = e["tag"].as_str()?;
                        let text = e["text"].as_str().unwrap_or_default();
                        let name = if text.is_empty() {
                            format!("<{tag}>")
                        } else {
                            text.to_string()
                        };
                        Some(crate::SemanticElement {
                            element_ref: format!("{tag}[{}]:{name}", e["i"]),
                            role: tag.to_string(),
                            name,
                            value: e["href"].as_str().map(str::to_string),
                            clickable: true,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(crate::PageState::from_elements(
            page["url"].as_str().unwrap_or_default(),
            page["title"].as_str().unwrap_or_default(),
            elements,
        ))
    }

    /// The `action` verb: run a JS expression in the page (click, fill,
    /// dispatch) and return its value.
    pub fn action(&mut self, expression: &str) -> Result<Value, CdpError> {
        self.evaluate(expression)
    }

    /// Reads socket messages (filing domain events) for up to
    /// `max_wait_ms` without sending any command — the async counterpart
    /// to command-driven reads, so events that arrive while nobody is
    /// awaiting a response still land in the buffers.
    pub fn pump(&mut self, max_wait_ms: u64) {
        let deadline = Instant::now() + Duration::from_millis(max_wait_ms);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return;
            }
            if !set_read_timeout(&self.socket, Some(remaining)) {
                return;
            }
            match self.socket.read() {
                Ok(msg) => {
                    let text = match msg {
                        tungstenite::Message::Text(t) => t.to_string(),
                        tungstenite::Message::Binary(b) => {
                            String::from_utf8_lossy(&b).to_string()
                        }
                        _ => continue,
                    };
                    if let Ok(value) = serde_json::from_str::<Value>(&text) {
                        if value["id"].is_null() {
                            self.file_event(&value);
                        }
                    }
                }
                Err(tungstenite::Error::Io(e))
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    return;
                }
                Err(_) => return,
            }
        }
    }

    /// The `console` verb: drains buffered `Runtime.consoleAPICalled`
    /// entries as `level: text` lines.
    pub fn console(&mut self) -> Vec<String> {
        std::mem::take(&mut self.observations.console)
    }

    /// The `network` verb: drains buffered request/response summaries.
    pub fn network(&mut self) -> (Vec<String>, Vec<String>) {
        (
            std::mem::take(&mut self.observations.requests),
            std::mem::take(&mut self.observations.responses),
        )
    }

    /// The `capture` verb: full-page PNG.
    pub fn capture(&mut self) -> Result<Vec<u8>, CdpError> {
        let out = self.command_value(
            Some(self.session_id.clone()),
            "Page.captureScreenshot",
            json!({ "format": "png" }),
        )?;
        use base64::Engine as _;
        let data = out["data"]
            .as_str()
            .ok_or_else(|| CdpError {
                message: "screenshot produced no data".into(),
            })?;
        base64::engine::general_purpose::STANDARD
            .decode(data)
            .map_err(err)
    }

    /// Kills the browser process and removes the throwaway profile.
    pub fn shutdown(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.user_data_dir);
    }

    fn send_cmd(
        &mut self,
        session: Option<String>,
        id: u64,
        method: &str,
        params: Value,
    ) -> Result<(), CdpError> {
        let mut msg = json!({ "id": id, "method": method, "params": params });
        if let Some(sid) = session {
            msg["sessionId"] = json!(sid);
        }
        self.socket
            .send(tungstenite::Message::text(msg.to_string()))
            .map_err(err)
    }

    /// Sends a command and reads until ITS response arrives, filing any
    /// domain events into the observation buffers on the way.
    fn command_value(&mut self, session: Option<String>, method: &str, params: Value) -> Result<Value, CdpError> {
        let id = self.next_id;
        self.next_id += 1;
        self.send_cmd(session, id, method, params)?;
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(CdpError {
                    message: format!("{method} timed out"),
                });
            }
            // Command reads own their timeout: pump() may have left a
            // short one, and a slow response (e.g. screenshot) must not
            // surface as EAGAIN. A WouldBlock retry resumes the frame
            // buffer; the deadline is the hard stop.
            if !set_read_timeout(&self.socket, Some(remaining)) {
                return Err(CdpError {
                    message: "socket does not support read timeouts".into(),
                });
            }
            let msg = match self.socket.read() {
                Ok(msg) => msg,
                Err(tungstenite::Error::Io(e))
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    continue;
                }
                Err(e) => return Err(err(e)),
            };
            let text = match msg {
                tungstenite::Message::Text(t) => t.to_string(),
                tungstenite::Message::Binary(b) => String::from_utf8_lossy(&b).to_string(),
                _ => continue,
            };
            let value: Value = match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if value["id"].as_u64() == Some(id) {
                if let Some(e) = value["error"].as_object() {
                    return Err(CdpError {
                        message: format!("{method} failed: {e:?}"),
                    });
                }
                return Ok(value["result"].clone());
            }
            self.file_event(&value);
        }
    }

    fn file_event(&mut self, value: &Value) {
        let method = value["method"].as_str().unwrap_or_default();
        match method {
            "Runtime.consoleAPICalled" => {
                let level = value["params"]["type"].as_str().unwrap_or("log");
                let text: Vec<String> = value["params"]["args"]
                    .as_array()
                    .map(|args| {
                        args.iter()
                            .map(|a| {
                                a["value"].as_str().map(str::to_string).unwrap_or_else(|| {
                                    a["description"]
                                        .as_str()
                                        .unwrap_or("?")
                                        .to_string()
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                self.observations
                    .console
                    .push(format!("{level}: {}", text.join(" ")));
            }
            "Network.requestWillBeSent" => {
                let r = &value["params"]["request"];
                self.observations.requests.push(format!(
                    "{} {}",
                    r["method"].as_str().unwrap_or("?"),
                    r["url"].as_str().unwrap_or("?")
                ));
            }
            "Network.responseReceived" => {
                let r = &value["params"]["response"];
                self.observations.responses.push(format!(
                    "{} {}",
                    r["status"].as_u64().unwrap_or(0),
                    r["url"].as_str().unwrap_or("?")
                ));
            }
            _ => {}
        }
    }
}

impl Drop for CdpBrowser {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.user_data_dir);
    }
}

fn set_read_timeout(socket: &Ws, d: Option<Duration>) -> bool {
    match socket.get_ref() {
        MaybeTlsStream::Plain(s) => s.set_read_timeout(d).is_ok(),
        // TLS-gateway variants only exist with tungstenite TLS features
        // (not enabled in this workspace); the catch-all covers them.
        _ => false,
    }
}

fn uuid_like_suffix() -> u128 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static SEQ: AtomicU32 = AtomicU32::new(0);
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    // Full nanos + a per-process sequence: two launches in the SAME
    // process must never share a profile dir (a colliding --user-data-dir
    // makes the second Chrome delegate to the first singleton instance
    // and both sessions then drive one browser).
    let seq = SEQ.fetch_add(1, Ordering::Relaxed) as u128;
    nanos << 32 | seq
}

/// PATH lookup only — never executes the binary (a GUI shell could flash
/// an app window just to answer --version).
fn which_exists(bin: &str) -> bool {
    if bin.contains('/') {
        return Path::new(bin).exists();
    }
    std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths)
                .any(|dir| dir.join(bin).exists())
        })
        .unwrap_or(false)
}
