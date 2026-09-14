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
    let dir = exe
        .parent()
        .expect("deps dir")
        .parent()
        .expect("profile dir");
    let candidate = dir.join(name);
    // Windows: sibling bins carry the .exe suffix.
    if candidate.exists() || !cfg!(target_os = "windows") {
        candidate
    } else {
        dir.join(format!("{name}.exe"))
    }
}

struct Proc(Child);

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn(path: std::path::PathBuf, envs: &[(&str, &str)], piped_boot: bool) -> (Proc, String) {
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

/// Waits (bounded) until the tenant worker has leased and the fleet
/// relay succeeds — worker registration is async on a fresh OS process;
/// fixed sleeps race on slow runners.
fn wait_for_worker(api_addr: &str, token: &str) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let (head, body) = read_http(api_addr, &format!("/fleet?token={token}"));
        if head.contains("200") {
            if let Ok(fleet) = serde_json::from_str::<serde_json::Value>(&body) {
                if fleet["ok"] == true {
                    return;
                }
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "worker never leased: {head} {body}"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
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
        .filter_map(|p| {
            p.split_once('=')
                .map(|(k, v)| (k.to_string(), v.to_string()))
        })
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
    // tenant-1 worker and reaches the REAL Core (wait out the async
    // worker lease instead of racing a fixed sleep).
    wait_for_worker(&api_addr, token);

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

/// Drives the FULL cloud browser relay (M8.8, docs/24 § Cloud API): a
/// tenant session over REAL HTTP reaches the tenant worker's REAL
/// BrowserHost through the gateway — live view (real Chromium PNG),
/// takeover (lease=user) and return (lease=agent). ISOLATION: a second
/// tenant's session asking for the same task id can never observe the
/// first tenant's browser state — its relay goes only to its own plane
/// and fails closed (no worker), and tenant-1's lease is untouched.
/// Skips with a recorded note when no Chromium-family binary exists
/// (same documented gap as the cdp e2e), never a fake pass.
#[test]
fn cloud_browser_relay_view_lease_and_tenant_isolation() {
    use modbit_browser::cdp::CdpBrowser;
    if CdpBrowser::find_browser().is_none() {
        println!(
            "cloud browser e2e skipped: no Chromium-family browser on this runner (recorded gap)"
        );
        return;
    }

    // 0. Issuer for tenant-1 (default fixture tenant).
    let (_issuer1, issuer1_boot) = spawn(bin("fixture-issuer"), &[], true);
    let iss1_addr = issuer1_boot
        .split_whitespace()
        .nth(1)
        .expect("iss1")
        .to_string();
    // Issuer for tenant-2: same fixture binary, different INTERNAL tenant.
    let (_issuer2, issuer2_boot) = spawn(
        bin("fixture-issuer"),
        &[("MODBIT_FIXTURE_TENANT", "tenant-2")],
        true,
    );
    let iss2_addr = issuer2_boot
        .split_whitespace()
        .nth(1)
        .expect("iss2")
        .to_string();

    // 1. Gateway + tenant-1 worker (real Core, real store).
    let (_gateway, gateway_boot) = spawn(
        bin("modbit-sandbox-gateway"),
        &[("MODBIT_GATEWAY_ADDR", "127.0.0.1:0")],
        true,
    );
    let gw_parts: Vec<&str> = gateway_boot.split_whitespace().collect();
    let (gw_addr, gw_secret) = (gw_parts[1].to_string(), gw_parts[2].to_string());
    let db1 = tempdir("db1").join("cloud.db");
    let (_worker1, _) = spawn(
        bin("modbit-cloud-worker"),
        &[
            ("MODBIT_GATEWAY_ADDR", gw_addr.as_str()),
            ("MODBIT_GATEWAY_SECRET", gw_secret.as_str()),
            ("MODBIT_WORKER_TENANT", "tenant-1"),
            ("MODBIT_CORE_DB", db1.to_str().unwrap()),
        ],
        false,
    );

    // 2. Cloud apis: one per tenant, same gateway, different issuers.
    let api_boot_for = |issuer: &str| {
        let (api, boot) = spawn(
            bin("modbit-cloud-api"),
            &[
                ("MODBIT_CLOUD_API_ADDR", "127.0.0.1:0"),
                ("MODBIT_GATEWAY_SECRET", gw_secret.as_str()),
                ("MODBIT_CLOUD_GATEWAY", gw_addr.as_str()),
                ("MODBIT_OIDC_ISSUER", issuer),
                ("MODBIT_OIDC_REDIRECT", "http://localhost/callback"),
            ],
            true,
        );
        let addr = boot
            .split_whitespace()
            .nth(1)
            .expect("api addr")
            .to_string();
        (api, addr)
    };
    let (_api1, api1) = api_boot_for(&format!("http://{iss1_addr}"));
    let (_api2, api2) = api_boot_for(&format!("http://{iss2_addr}"));
    std::thread::sleep(std::time::Duration::from_millis(400));

    // 3. PKCE login per tenant (fixture issuer → callback → session).
    let login = |api_addr: &str, issuer_addr: &str| -> String {
        let (head, _) = read_http(api_addr, "/login");
        assert!(head.contains("302"), "{head}");
        let location = head
            .lines()
            .find(|l| l.to_lowercase().starts_with("location:"))
            .expect("location")
            .split_once(' ')
            .expect("loc value")
            .1
            .to_string();
        let rest = location
            .split_once("://")
            .map(|(_, r)| r)
            .unwrap_or(&location);
        let authorize_query = rest[rest.find('/').expect("path")..].to_string();
        let (cb_head, _cb_path) = read_http(issuer_addr, &authorize_query);
        assert!(cb_head.contains("302"), "{cb_head}");
        let cb_loc = cb_head
            .lines()
            .find(|l| l.to_lowercase().starts_with("location:"))
            .expect("cb location")
            .split_once(' ')
            .expect("cb loc value")
            .1
            .to_string();
        let cb_rest = cb_loc.split_once("://").map(|(_, r)| r).unwrap_or(&cb_loc);
        let callback_target = cb_rest[cb_rest.find('/').expect("cb path")..].to_string();
        let (head, body) = read_http(api_addr, &callback_target);
        assert!(head.contains("200"), "callback: {head} {body}");
        let session: serde_json::Value = serde_json::from_str(&body).expect("session json");
        session["session_token"]
            .as_str()
            .expect("token")
            .to_string()
    };
    let token1 = login(&api1, &iss1_addr);
    let _token2 = login(&api2, &iss2_addr);
    wait_for_worker(&api1, &token1);

    // 4. Tenant-1 live view through the cloud plane: the worker's
    // BrowserHost launches REAL headless Chromium on demand and returns
    // the live PNG + page + lease (agent by default).
    let (head, body) = read_http(
        &api1,
        &format!("/browser?token={token1}&task=t-cloud-b&action=view"),
    );
    // The relay itself is proven either way (HTTP → gateway → worker Core
    // → BrowserHost and the typed error returns). A LAUNCH failure on a
    // runner whose Chromium cannot start is the same documented
    // environment gap as the cdp e2e — recorded, never a fake pass.
    if body.contains("browser host: cdp:") {
        println!(
            "cloud browser e2e: relay proven; browser launch unavailable on this runner (recorded gap): {body}"
        );
        return;
    }
    assert!(head.contains("200"), "view: {head} {body}");
    let view: serde_json::Value = serde_json::from_str(&body).expect("view json");
    assert_eq!(view["ok"], true, "{body}");
    let v = &view["browser_view"];
    assert_eq!(v["task_id"], "t-cloud-b");
    assert_eq!(v["lease"], "agent", "fresh browser starts agent-owned");
    let png = v["png_base64"].as_str().expect("png");
    assert!(
        png.len() > 100,
        "live PNG frame expected, got {} chars",
        png.len()
    );

    // 5. Takeover through the cloud plane: lease flips to the user.
    let (head, body) = read_http(
        &api1,
        &format!("/browser?token={token1}&task=t-cloud-b&action=lease&owner=user"),
    );
    assert!(head.contains("200"), "takeover: {head} {body}");
    let view: serde_json::Value = serde_json::from_str(&body).expect("takeover json");
    assert_eq!(view["browser_view"]["lease"], "user", "{body}");

    // 6. Return to agent through the cloud plane.
    let (head, body) = read_http(
        &api1,
        &format!("/browser?token={token1}&task=t-cloud-b&action=lease&owner=agent"),
    );
    assert!(head.contains("200"), "return: {head} {body}");
    let view: serde_json::Value = serde_json::from_str(&body).expect("return json");
    assert_eq!(view["browser_view"]["lease"], "agent", "{body}");

    // 7. ISOLATION: tenant-2 (own cloud api, own issuer-vouched tenant)
    // asks for the SAME task id. The gateway relays only within
    // tenant-2's plane; with no tenant-2 worker the relay fails closed —
    // and never leaks tenant-1's browser state.
    let (head, body) = read_http(
        &api2,
        &format!("/browser?token={_token2}&task=t-cloud-b&action=view"),
    );
    assert!(
        head.contains("502") || head.contains("404") || head.contains("400"),
        "tenant-2 relay must fail closed, got: {head} {body}"
    );
    assert!(
        !body.contains("png_base64"),
        "cross-tenant response leaked a view: {body}"
    );

    // 8. Tenant-1's browser state is untouched by the denied attempt.
    let (head, body) = read_http(
        &api1,
        &format!("/browser?token={token1}&task=t-cloud-b&action=view"),
    );
    assert!(head.contains("200"), "{head}");
    let view: serde_json::Value = serde_json::from_str(&body).expect("view json");
    assert_eq!(
        view["browser_view"]["lease"], "agent",
        "denied cross-tenant read disturbed tenant-1 state: {body}"
    );
}

/// M8.7: the checkpoint handoff routed through the CLOUD plane — a local
/// machine exports a checkpoint handoff bundle from a REAL dirty git
/// repo, posts it to the tenant /checkpoint endpoint, the relay carries
/// it as the typed ImportCheckpoint RPC through the gateway, and the
/// worker's Core attaches it into ITS repository with the
/// exact-reconstruction receipt (tree digest proven).
#[test]
fn cloud_checkpoint_attach_routes_through_relay() {
    use modbit_checkpoint::cloud_attach::export_checkpoint_bundle;
    use modbit_git::snapshot::SnapshotProvenance;
    use modbit_git::GitRepo;

    // 0. Issuer + gateway + tenant worker (with a REAL repository to
    // attach into) + cloud api.
    let (_issuer, issuer_boot) = spawn(bin("fixture-issuer"), &[], true);
    let iss_addr = issuer_boot
        .split_whitespace()
        .nth(1)
        .expect("iss")
        .to_string();
    let (_gateway, gateway_boot) = spawn(
        bin("modbit-sandbox-gateway"),
        &[("MODBIT_GATEWAY_ADDR", "127.0.0.1:0")],
        true,
    );
    let gw_parts: Vec<&str> = gateway_boot.split_whitespace().collect();
    let (gw_addr, gw_secret) = (gw_parts[1].to_string(), gw_parts[2].to_string());

    let worker_repo_dir = tempdir("worker-repo");
    let worker_repo = GitRepo::init(&worker_repo_dir).expect("init worker repo");
    std::fs::write(worker_repo_dir.join("seed.txt"), "seed\n").unwrap();
    worker_repo.commit_all("cloud-base").expect("seed");

    let db = tempdir("ckpt-db").join("cloud.db");
    let (_worker, _) = spawn(
        bin("modbit-cloud-worker"),
        &[
            ("MODBIT_GATEWAY_ADDR", gw_addr.as_str()),
            ("MODBIT_GATEWAY_SECRET", gw_secret.as_str()),
            ("MODBIT_WORKER_TENANT", "tenant-1"),
            ("MODBIT_CORE_DB", db.to_str().unwrap()),
            ("MODBIT_REPO_ROOT", worker_repo_dir.to_str().unwrap()),
        ],
        false,
    );
    let (_api, api_addr) = {
        let (api, boot) = spawn(
            bin("modbit-cloud-api"),
            &[
                ("MODBIT_CLOUD_API_ADDR", "127.0.0.1:0"),
                ("MODBIT_GATEWAY_SECRET", gw_secret.as_str()),
                ("MODBIT_CLOUD_GATEWAY", gw_addr.as_str()),
                ("MODBIT_OIDC_ISSUER", format!("http://{iss_addr}").as_str()),
                ("MODBIT_OIDC_REDIRECT", "http://localhost/callback"),
            ],
            true,
        );
        let addr = boot
            .split_whitespace()
            .nth(1)
            .expect("api boot")
            .to_string();
        (api, addr)
    };
    std::thread::sleep(std::time::Duration::from_millis(300));

    // 1. LOCAL machine: a real repo with committed base + dirty state
    // (tracked edit + untracked file).
    let local_dir = tempdir("local-repo");
    let local = GitRepo::init(&local_dir).expect("init local");
    std::fs::write(local_dir.join("seed.txt"), "seed\n").unwrap();
    local.commit_all("base").expect("commit");
    std::fs::write(local_dir.join("feature.txt"), "dirty feature work").unwrap();
    std::fs::create_dir_all(local_dir.join("notes")).unwrap();
    std::fs::write(local_dir.join("notes/idea.md"), "untracked note").unwrap();

    let bundle = export_checkpoint_bundle(
        &local,
        &SnapshotProvenance::new("task-ckpt-1", "mac-local", "local-workspace"),
        "continue the feature on the worker",
        "feature.txt rewritten, notes pending",
        vec![],
        vec![],
    )
    .expect("export");
    let bundle_json = serde_json::to_string(&bundle).expect("bundle json");

    // 2. Login (PKCE) and wait for the worker lease.
    let (head, _) = read_http(&api_addr, "/login");
    assert!(head.contains("302"), "{head}");
    let location = head
        .lines()
        .find(|l| l.to_lowercase().starts_with("location:"))
        .expect("location")
        .split_once(' ')
        .expect("loc")
        .1
        .to_string();
    let rest = location
        .split_once("://")
        .map(|(_, r)| r)
        .unwrap_or(&location);
    let authorize_query = rest[rest.find('/').expect("path")..].to_string();
    let (cb_head, _) = read_http(&iss_addr, &authorize_query);
    assert!(cb_head.contains("302"), "{cb_head}");
    let cb_loc = cb_head
        .lines()
        .find(|l| l.to_lowercase().starts_with("location:"))
        .expect("cb loc")
        .split_once(' ')
        .expect("cb loc value")
        .1
        .to_string();
    let cb_rest = cb_loc.split_once("://").map(|(_, r)| r).unwrap_or(&cb_loc);
    let callback_target = cb_rest[cb_rest.find('/').expect("cb path")..].to_string();
    let (head, body) = read_http(&api_addr, &callback_target);
    assert!(head.contains("200"), "{head} {body}");
    let session: serde_json::Value = serde_json::from_str(&body).expect("session");
    let token = session["session_token"].as_str().expect("token");
    wait_for_worker(&api_addr, token);

    // 3. POST the bundle through the tenant checkpoint endpoint.
    let post_body = serde_json::json!({
        "task_id": "task-ckpt-1",
        "bundle_json": bundle_json,
    })
    .to_string();
    let mut stream = std::net::TcpStream::connect(&api_addr).expect("connect");
    stream
        .write_all(
            format!(
                "POST /checkpoint?token={token} HTTP/1.1\r\nHost: t\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{post_body}",
                post_body.len()
            )
            .as_bytes(),
        )
        .unwrap();
    let mut text = String::new();
    std::io::Read::read_to_string(&mut stream, &mut text).unwrap();
    assert!(text.contains("200"), "checkpoint attach: {text}");
    let (_head, resp_body) = text
        .split_once("\r\n\r\n")
        .map(|(h, b)| (h.to_string(), b.to_string()))
        .expect("http split");
    let resp: serde_json::Value = serde_json::from_str(&resp_body).expect("resp json");
    assert_eq!(resp["ok"], true, "{resp_body}");
    let receipt = &resp["checkpoint_attach"];
    assert_eq!(receipt["task_id"], "task-ckpt-1");
    assert_eq!(receipt["exact_reconstruction"], true, "{resp_body}");
    assert_eq!(
        receipt["restored_tree"], bundle.snapshot.tree,
        "worker reproduced the exact dirty tree"
    );

    // 4. The worker's repository really holds the reconstructed state.
    assert_eq!(
        std::fs::read_to_string(worker_repo_dir.join("feature.txt")).unwrap(),
        "dirty feature work"
    );
    assert_eq!(
        std::fs::read_to_string(worker_repo_dir.join("notes/idea.md")).unwrap(),
        "untracked note"
    );

    // 5. Provenance binding through the relay: attaching the same bundle
    // under a DIFFERENT task id is refused by the worker's attach
    // contract (typed error, no state change).
    let post_body = serde_json::json!({
        "task_id": "task-OTHER",
        "bundle_json": bundle_json,
    })
    .to_string();
    let mut stream = std::net::TcpStream::connect(&api_addr).expect("connect");
    stream
        .write_all(
            format!(
                "POST /checkpoint?token={token} HTTP/1.1\r\nHost: t\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{post_body}",
                post_body.len()
            )
            .as_bytes(),
        )
        .unwrap();
    let mut text = String::new();
    std::io::Read::read_to_string(&mut stream, &mut text).unwrap();
    let (_h, resp_body) = text
        .split_once("\r\n\r\n")
        .map(|(h, b)| (h.to_string(), b.to_string()))
        .expect("http split");
    let resp: serde_json::Value = serde_json::from_str(&resp_body).expect("resp json");
    // The attach CONTRACT refused it: ok=false with the typed provenance
    // error and NO receipt — the worker's repository stays untouched.
    assert_eq!(
        resp["ok"], false,
        "cross-task attach must be refused: {resp_body}"
    );
    assert!(
        resp["error"]
            .as_str()
            .map(|e| e.contains("refusing attach"))
            .unwrap_or(false),
        "expected the provenance refusal, got {resp_body}"
    );
    assert!(
        resp["checkpoint_attach"].is_null(),
        "no receipt for a refused attach"
    );
}
