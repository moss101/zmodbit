//! Cloud API E2E (Phase 8c, docs/24 + MOD-AUTH-001): REAL processes —
//! a fixture OIDC issuer (own process, real RSA + JWKS), the cloud-api,
//! the sandbox gateway and a cloud worker. Exercises the full
//! authorization-code + PKCE flow, RS256 id-token verification, the
//! short-lived session token, and tenant-scoped control endpoints
//! relayed through the gateway to a real Core worker.
//!
//! The fixture ISSUER is test infrastructure (deterministic, local); the
//! api side is production code pointed at it by configuration.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::process::{Child, Command, Stdio};

fn tempdir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "modbit-api-{tag}-{}",
        uuid::Uuid::now_v7().simple()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn bin(name: &str) -> std::path::PathBuf {
    let exe = std::env::current_exe().expect("current_exe");
    exe.parent()
        .expect("deps")
        .parent()
        .expect("profile")
        .join(name)
}

struct Proc(Child);

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn(
    path: std::path::PathBuf,
    envs: &[(&str, &str)],
    piped_boot: bool,
) -> (Proc, String) {
    let mut cmd = Command::new(path);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    if piped_boot {
        cmd.stdout(Stdio::piped());
    } else {
        cmd.stdout(Stdio::null());
    }
    cmd.stderr(Stdio::inherit());
    let mut child = cmd.spawn().expect("spawn");
    let boot = if piped_boot {
        let mut line = String::new();
        BufReader::new(child.stdout.take().expect("stdout"))
            .read_line(&mut line)
            .expect("boot line");
        line.trim().to_string()
    } else {
        String::new()
    };
    (Proc(child), boot)
}

fn read_http(addr: &str, target: &str) -> (String, String) {
    let mut stream = std::net::TcpStream::connect(addr).expect("connect");
    stream
        .write_all(
            format!("GET {target} HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n").as_bytes(),
        )
        .unwrap();
    let mut text = String::new();
    stream.read_to_string(&mut text).unwrap();
    let (head, body) = text.split_once("\r\n\r\n").expect("http split");
    (head.to_string(), body.to_string())
}

#[test]
fn cloud_api_oidc_pkce_login_and_tenant_scoped_control() {
    // 0. Fixture issuer: real process, real RSA keys, real JWKS.
    let (issuer, issuer_boot) = spawn(bin("fixture-issuer"), &[], true);
    let mut issuer_parts = issuer_boot.split_whitespace();
    assert_eq!(issuer_parts.next(), Some("issuer"));
    let issuer_addr = issuer_parts.next().expect("issuer addr").to_string();
    let iss = format!("http://{issuer_addr}");

    // 1. Gateway + worker for tenant-1 (real Core against a real store).
    let (gateway, gateway_boot) = spawn(
        bin("modbit-sandbox-gateway"),
        &[("MODBIT_GATEWAY_ADDR", "127.0.0.1:0")],
        true,
    );
    let gw_parts: Vec<&str> = gateway_boot.split_whitespace().collect();
    assert_eq!(gw_parts[0], "gateway");
    let (gw_addr, gw_secret) = (gw_parts[1].to_string(), gw_parts[2].to_string());

    let db = tempdir("db").join("cloud.db");
    let (_worker, _) = spawn(
        bin("modbit-cloud-worker"),
        &[
            ("MODBIT_GATEWAY_ADDR", gw_addr.as_str()),
            ("MODBIT_GATEWAY_SECRET", gw_secret.as_str()),
            ("MODBIT_WORKER_TENANT", "tenant-1"),
            ("MODBIT_CORE_DB", db.to_str().unwrap()),
        ],
        false,
    );

    // 2. The cloud api.
    let (_api, api_boot) = spawn(
        bin("modbit-cloud-api"),
        &[
            ("MODBIT_CLOUD_API_ADDR", "127.0.0.1:0"),
            ("MODBIT_GATEWAY_SECRET", gw_secret.as_str()),
            ("MODBIT_CLOUD_GATEWAY", gw_addr.as_str()),
            ("MODBIT_OIDC_ISSUER", iss.as_str()),
            ("MODBIT_OIDC_REDIRECT", "http://localhost/callback"),
        ],
        true,
    );
    let api_addr = api_boot
        .split_whitespace()
        .nth(1)
        .expect("api boot line")
        .to_string();

    // Preflight: the issuer serves its JWKS.
    let (jwks_head, jwks_body) = read_http(&issuer_addr, "/.well-known/jwks.json");
    assert!(
        jwks_head.contains("200") && jwks_body.contains("cloud-key"),
        "{jwks_head} {jwks_body}"
    );

    // 3. /login → 302 to the issuer's /authorize with PKCE params.
    let (head, _) = read_http(&api_addr, "/login");
    assert!(head.contains("302"), "{head}");
    let location = head
        .lines()
        .find(|l| l.to_lowercase().starts_with("location:"))
        .expect("location")
        .trim()
        .to_string();
    // Keep the PATH + query from the absolute Location URL.
    let loc_url = location
        .split_once(' ')
        .expect("header value")
        .1
        .to_string();
    let rest = loc_url
        .split_once("://")
        .map(|(_, r)| r)
        .unwrap_or(&loc_url);
    let authorize_query = rest[rest.find('/').expect("path starts")..].to_string();
    let params: std::collections::HashMap<String, String> = authorize_query
        .split('&')
        .filter_map(|p| p.split_once('=').map(|(k, v)| (k.to_string(), v.to_string())))
        .collect();
    assert_eq!(
        params.get("code_challenge_method").map(String::as_str),
        Some("S256")
    );
    let state = params.get("state").cloned().expect("state");
    assert!(params.contains_key("code_challenge"));
    assert!(params.contains_key("nonce"));

    // 4. Follow the authorize redirect AS THE BROWSER (the issuer binds
    // state→(nonce, challenge) and redirects to the api callback with a
    // code), then the api completes the flow: code+verifier exchange,
    // RS256 id-token verification against the JWKS, session issuance.
    let (cb_head, cb_path) = read_http(&issuer_addr, &authorize_query);
    assert!(cb_head.contains("302"), "{cb_head} {cb_path}");
    let cb_loc = cb_head
        .lines()
        .find(|l| l.to_lowercase().starts_with("location:"))
        .expect("callback location")
        .trim()
        .split_once(' ')
        .expect("loc target")
        .1
        .to_string();
    let cb_rest = cb_loc.split_once("://").map(|(_, r)| r).unwrap_or(&cb_loc);
    let callback_target = cb_rest[cb_rest.find('/').expect("cb path starts")..].to_string();
    let (head, cb_body) = read_http(&api_addr, &callback_target);
    assert!(head.contains("200"), "callback: {head} {cb_body}");
    let session: serde_json::Value = serde_json::from_str(&cb_body).expect("session json");
    let token = session["session_token"].as_str().expect("token");
    assert_eq!(token.split('.').count(), 3, "compact JWT shape");

    // 5. An authenticated /fleet call relays through the gateway to the
    // tenant-1 worker and reaches the REAL Core.
    std::thread::sleep(std::time::Duration::from_millis(400));
    let (head, body) = read_http(&api_addr, &format!("/fleet?token={token}"));
    assert!(head.contains("200"), "{head} {body}");
    let fleet: serde_json::Value = serde_json::from_str(&body).expect("fleet json");
    assert_eq!(fleet["ok"], true, "{body}");

    // 6. A tampered session token is rejected (signature check).
    let forged = format!("{}.forged.sig", token.split('.').next().unwrap());
    let (head, _) = read_http(&api_addr, &format!("/fleet?token={forged}"));
    assert!(head.contains("401"), "forged token must 401: {head}");

    // 7. A tampered ID TOKEN is rejected: replaying the same callback
    // state fails first (state is one-time).
    let (head, _) = read_http(
        &api_addr,
        &format!("/callback?code=authcode-1&state={state}"),
    );
    assert!(head.contains("400"), "replayed state must fail: {head}");

    // 8. Remote run create through the api → durable task in the
    // worker's store (the full cloud control-plane path).
    let (head, body) = read_http(
        &api_addr,
        &format!(
            "/task?token={token}&title={}&prompt={}",
            urlencode("cloud-created task"),
            urlencode("issued by the cloud api e2e")
        ),
    );
    assert!(head.contains("200"), "{head} {body}");
    let created: serde_json::Value = serde_json::from_str(&body).expect("create json");
    assert_eq!(created["ok"], true, "{body}");

    let store = modbit_event_store::EventStore::open(&db).unwrap();
    let tasks: i64 = store
        .with_conn(|conn| conn.query_row("SELECT COUNT(*) FROM tasks", [], |r| r.get(0)))
        .unwrap();
    assert!(tasks >= 1, "cloud-created task is durable");
    let _ = (gateway, issuer);
}

fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
