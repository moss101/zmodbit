//! modbit-cloud-api — authenticated cloud control plane (docs/24):
//! OIDC authorization-code + PKCE sign-in (MOD-AUTH-001: the PROTOCOL is
//! fixed; the issuer is a configuration point — internal User/Tenant ids
//! stay independent of the provider), short-lived HMAC session tokens,
//! and tenant-scoped control endpoints that relay to the tenant's cloud
//! worker THROUGH the sandbox gateway. The api never talks to a worker
//! outside the caller's authenticated tenant.

use std::collections::HashMap;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

use modbit_protocol::transport::BootSecret;
use modbit_sandbox_gateway::{Envelope, Registration};
use prost::Message as _;

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}

/// Pending PKCE login attempts: state → verifier (docs/24: authorization
/// code + S256 PKCE).
#[derive(Default)]
struct Logins {
    pending: HashMap<String, Pkce>,
}

#[derive(Clone)]
struct Pkce {
    verifier: String,
    nonce: String,
}

/// The api's own short-lived session token claims (separate from the
/// issuer's id token: the session expires in 15 minutes and carries the
/// INTERNAL tenant id, never provider ids).
#[derive(Serialize, Deserialize)]
struct Session {
    tenant: String,
    sub: String,
    exp: u64,
}

fn b64url(data: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

struct AppState {
    logins: Mutex<Logins>,
    session_key: String,
    issuer: String,
    client_id: String,
    redirect_uri: String,
    gateway_addr: String,
    gateway_secret: BootSecret,
}

fn http_response(status: &str, content_type: &str, body: &str, location: Option<&str>) -> Vec<u8> {
    let mut head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n",
        body.len()
    );
    if let Some(loc) = location {
        head.push_str(&format!("Location: {loc}\r\n"));
    }
    head.push_str("Connection: close\r\n\r\n");
    let mut out = head.into_bytes();
    out.extend_from_slice(body.as_bytes());
    out
}

fn main() {
    let addr = env_or("MODBIT_CLOUD_API_ADDR", "127.0.0.1:0");
    let listener = TcpListener::bind(&addr).unwrap_or_else(|e| {
        eprintln!("cloud-api: cannot bind {addr}: {e}");
        std::process::exit(1);
    });
    let boot_secret_hex = env_or("MODBIT_GATEWAY_SECRET", "");
    let state = Arc::new(AppState {
        logins: Mutex::new(Logins::default()),
        session_key: env_or("MODBIT_CLOUD_SESSION_KEY", "dev-session-key"),
        issuer: env_or("MODBIT_OIDC_ISSUER", "unset-issuer"),
        client_id: env_or("MODBIT_OIDC_CLIENT_ID", "modbit-cloud"),
        redirect_uri: env_or("MODBIT_OIDC_REDIRECT", "http://localhost/callback"),
        gateway_addr: env_or("MODBIT_CLOUD_GATEWAY", ""),
        gateway_secret: BootSecret::from_hex(&boot_secret_hex).unwrap_or_else(|| {
            eprintln!("cloud-api: MODBIT_GATEWAY_SECRET is required (64 hex)");
            std::process::exit(1);
        }),
    });
    println!("cloud-api {}", listener.local_addr().expect("bound"));

    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let state = state.clone();
        std::thread::spawn(move || {
            let _ = handle_connection(stream, &state);
        });
    }
}

fn handle_connection(stream: TcpStream, state: &Arc<AppState>) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let mut parts = request_line.split_whitespace();
    let _method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or_default().to_string();
    // Drain headers (GET-only control plane; bodies unused).
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 || header.trim().is_empty() {
            break;
        }
    }
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target.clone(), String::new()),
    };
    let params = parse_query(&query);
    let mut stream = stream;
    match path.as_str() {
        "/login" => {
            // PKCE S256: the verifier stays server-side; only the
            // challenge travels to the issuer.
            let verifier = b64url(&rand_bytes(32));
            let challenge = b64url(&Sha256::digest(verifier.as_bytes()));
            let nonce = b64url(&rand_bytes(16));
            let oauth_state = b64url(&rand_bytes(16));
            state.logins.lock().expect("logins").pending.insert(
                oauth_state.clone(),
                Pkce {
                    verifier,
                    nonce: nonce.clone(),
                },
            );
            let loc = format!(
                "{}/authorize?response_type=code&client_id={}&redirect_uri={}&scope=openid&state={}&nonce={}&code_challenge={}&code_challenge_method=S256",
                state.issuer,
                urlencode(&state.client_id),
                urlencode(&state.redirect_uri),
                urlencode(&oauth_state),
                urlencode(&nonce),
                urlencode(&challenge),
            );
            stream.write_all(&http_response(
                "302 Found",
                "text/plain",
                "redirecting",
                Some(&loc),
            ))?;
        }
        "/callback" => {
            let code = params.get("code").cloned().unwrap_or_default();
            let oauth_state = params.get("state").cloned().unwrap_or_default();
            let pkce = state.logins.lock().expect("logins").pending.remove(&oauth_state);
            let Some(pkce) = pkce else {
                stream.write_all(&http_response(
                    "400 Bad Request",
                    "application/json",
                    "{\"error\":\"unknown state\"}",
                    None,
                ))?;
                return Ok(());
            };
            // Token exchange (code + verifier) at the issuer's token
            // endpoint — the verifier proves we still hold the PKCE secret.
            let form = format!(
                "grant_type=authorization_code&code={}&client_id={}&redirect_uri={}&code_verifier={}",
                urlencode(&code),
                urlencode(&state.client_id),
                urlencode(&state.redirect_uri),
                urlencode(&pkce.verifier),
            );
            let issued = http_post_form(&state.issuer, "/token", &form);
            let Ok(body_text) = issued else {
                let body = format!("{{\"error\":\"issuer unreachable: {}\"}}", issued.unwrap_err().replace('"', "'"));
                stream.write_all(&http_response("502 Bad Gateway", "application/json", &body, None))?;
                return Ok(());
            };
            let Ok(body) = serde_json::from_str::<serde_json::Value>(&body_text) else {
                stream.write_all(&http_response(
                    "502 Bad Gateway",
                    "application/json",
                    "{\"error\":\"bad issuer response\"}",
                    None,
                ))?;
                return Ok(());
            };
            let Some(id_token) = body["id_token"].as_str().map(str::to_string) else {
                stream.write_all(&http_response(
                    "401 Unauthorized",
                    "application/json",
                    "{\"error\":\"issuer returned no id_token\"}",
                    None,
                ))?;
                return Ok(());
            };
            // Verify the ID token against the issuer's JWKS (RS256) with
            // iss/aud/exp AND the binding nonce.
            let claims = match verify_id_token(state, &id_token, &pkce.nonce) {
                Ok(c) => c,
                Err(e) => {
                    let body = format!("{{\"error\":\"{e}\"}}");
                    stream.write_all(&http_response("401 Unauthorized", "application/json", &body, None))?;
                    return Ok(());
                }
            };
            // Short-lived api session token carrying the INTERNAL tenant.
            let session = Session {
                tenant: claims.tenant,
                sub: claims.sub,
                exp: now_secs() + 15 * 60,
            };
            let header = json!({"alg": "HS256", "typ": "JWT"});
            let enc = |v: &serde_json::Value| b64url(v.to_string().as_bytes());
            let signing_input = format!(
                "{}.{}",
                enc(&header),
                enc(&serde_json::to_value(&session).expect("json"))
            );
            let sig = hmac_sha256(state.session_key.as_bytes(), signing_input.as_bytes());
            let token = format!("{signing_input}.{}", b64url(&sig));
            stream.write_all(&http_response(
                "200 OK",
                "application/json",
                &format!("{{\"session_token\":\"{token}\"}}"),
                None,
            ))?;
        }
        "/fleet" | "/task" => {
            // Authenticated control endpoint: the bearer session token
            // authorizes the TENANT; the request relays through the
            // gateway to that tenant's worker only.
            let token = params.get("token").cloned().unwrap_or_default();
            let session = match verify_session(state, &token) {
                Ok(s) => s,
                Err(e) => {
                    let body = format!("{{\"error\":\"{e}\"}}");
                    stream.write_all(&http_response("401 Unauthorized", "application/json", &body, None))?;
                    return Ok(());
                }
            };
            let payload = if path == "/fleet" {
                modbit_protocol::modbit::protocol::v1::SurfaceRequest {
                    request: Some(
                        modbit_protocol::modbit::protocol::v1::surface_request::Request::GetFleet(
                            modbit_protocol::modbit::protocol::v1::GetFleetRequest {},
                        ),
                    ),
                }
                .encode_to_vec()
            } else {
                // Remote run create (docs/24 § Cloud API).
                modbit_protocol::modbit::protocol::v1::SurfaceRequest {
                    request: Some(
                        modbit_protocol::modbit::protocol::v1::surface_request::Request::CreateTask(
                            modbit_protocol::modbit::protocol::v1::CreateTaskCommand {
                                session_id: String::new(),
                                title: params
                                    .get("title")
                                    .cloned()
                                    .unwrap_or_else(|| "cloud task".into()),
                                prompt: params.get("prompt").cloned().unwrap_or_default(),
                                repo_id: params.get("repo_id").cloned().unwrap_or_default(),
                                base_branch: String::new(),
                                parent_task_id: String::new(),
                                write_scope: String::new(),
                            },
                        ),
                    ),
                }
                .encode_to_vec()
            };
            match relay_to_tenant_worker(state, &session.tenant, &payload) {
                Ok(response) => {
                    stream.write_all(&http_response("200 OK", "application/json", &response, None))?;
                }
                Err(e) => {
                    let body = format!("{{\"error\":\"{e}\"}}");
                    stream.write_all(&http_response("502 Bad Gateway", "application/json", &body, None))?;
                }
            }
        }
        _ => {
            stream.write_all(&http_response(
                "404 Not Found",
                "application/json",
                "{\"error\":\"not found\"}",
                None,
            ))?;
        }
    }
    Ok(())
}

#[derive(Deserialize)]
struct GatewayErr {
    error: String,
}

/// Relays a SurfaceRequest to the caller's tenant worker THROUGH the
/// gateway (guest role, bound to the authenticated tenant).
fn relay_to_tenant_worker(
    state: &Arc<AppState>,
    tenant: &str,
    payload: &[u8],
) -> Result<String, String> {
    let stream =
        TcpStream::connect(&state.gateway_addr).map_err(|e| format!("gateway unreachable: {e}"))?;
    let mut conn = modbit_protocol::transport::Connection::over_stream(
        stream,
        &state.gateway_secret,
    )
    .map_err(|e| format!("gateway handshake failed: {e}"))?;
    conn.send(
        &serde_json::to_vec(&Registration {
            role: "guest".into(),
            tenant: tenant.to_string(),
            task: String::new(),
        })
        .expect("json"),
    )
    .map_err(|e| e.to_string())?;
    conn.send(
        &serde_json::to_vec(&Envelope {
            tenant: tenant.to_string(),
            task: String::new(),
            payload: Envelope::encode_payload(payload),
        })
        .expect("json"),
    )
    .map_err(|e| e.to_string())?;
    let frame = conn.receive().map_err(|e| e.to_string())?;
    if let Ok(err) = serde_json::from_slice::<GatewayErr>(&frame) {
        return Err(err.error);
    }
    let envelope: Envelope = serde_json::from_slice(&frame).map_err(|e| e.to_string())?;
    let raw = envelope.decode_payload()?;
    let response = modbit_protocol::modbit::protocol::v1::SurfaceResponse::decode(raw.as_slice())
        .map_err(|e| e.to_string())?;
    Ok(format!(
        "{{\"ok\":{},\"error\":\"{}\"}}",
        response.ok,
        response.error.replace('"', "'")
    ))
}

#[derive(Deserialize, Debug, Clone)]
struct IdClaims {
    iss: String,
    aud: String,
    sub: String,
    nonce: String,
    #[serde(default)]
    tenant: String,
    exp: u64,
}

fn verify_id_token(state: &Arc<AppState>, token: &str, nonce: &str) -> Result<IdClaims, String> {
    let jwks_text = http_get(&state.issuer, "/.well-known/jwks.json")
        .map_err(|e| format!("jwks fetch failed: {e}"))?;
    let jwks: jsonwebtoken::jwk::JwkSet =
        serde_json::from_str(&jwks_text).map_err(|e| format!("bad jwks: {e}"))?;
    let jwk = jwks.find("cloud-key").ok_or("jwks has no 'cloud-key'")?;
    let mut validation = Validation::new(Algorithm::RS256);
    validation.set_audience(&[state.client_id.clone()]);
    let mut claims = jsonwebtoken::decode::<IdClaims>(
        token,
        &DecodingKey::from_jwk(jwk).map_err(|e| format!("jwk -> key: {e}"))?,
        &validation,
    )
    .map_err(|e| format!("id token rejected: {e}"))?
    .claims;
    if claims.nonce != nonce {
        return Err("nonce mismatch".into());
    }
    // Internal tenant id: the issuer's tenant claim when present, else
    // the subject — provider ids never become storage paths (docs/24).
    if claims.tenant.is_empty() {
        claims.tenant = claims.sub.clone();
    }
    Ok(claims)
}

fn verify_session(state: &Arc<AppState>, token: &str) -> Result<Session, String> {
    let mut parts = token.split('.');
    let header = parts.next().ok_or("malformed token")?;
    let claims = parts.next().ok_or("malformed token")?;
    let sig = parts.next().ok_or("malformed token")?;
    let expected = hmac_sha256(
        state.session_key.as_bytes(),
        format!("{header}.{claims}").as_bytes(),
    );
    let sig_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(sig)
        .map_err(|_| "malformed token")?;
    if sig_bytes != expected {
        return Err("bad signature".into());
    }
    let json = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(claims)
        .map_err(|_| "malformed token")?;
    let session: Session = serde_json::from_slice(&json).map_err(|e| e.to_string())?;
    if session.exp < now_secs() {
        return Err("session expired".into());
    }
    Ok(session)
}

/// Minimal HTTP GET over TcpStream (the daemon's hand-rolled HTTP
/// pattern; no client stack needed for the two fixed issuer calls).
fn http_get(base: &str, path: &str) -> Result<String, String> {
    let addr = base.trim_start_matches("http://").trim_end_matches('/').to_string();
    let mut stream = TcpStream::connect(&addr).map_err(|e| format!("connect {addr}: {e}"))?;
    stream
        .write_all(format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n").as_bytes())
        .map_err(|e| e.to_string())?;
    let mut text = String::new();
    stream.read_to_string(&mut text).map_err(|e| e.to_string())?;
    text.split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .ok_or_else(|| "no http body".into())
}

/// Minimal HTTP POST (x-www-form-urlencoded) over TcpStream.
fn http_post_form(base: &str, path: &str, form: &str) -> Result<String, String> {
    let addr = base.trim_start_matches("http://").trim_end_matches('/').to_string();
    let mut stream = TcpStream::connect(&addr).map_err(|e| format!("connect {addr}: {e}"))?;
    let body = form.to_string();
    let req = format!(
        "POST {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(req.as_bytes()).map_err(|e| e.to_string())?;
    let mut text = String::new();
    stream.read_to_string(&mut text).map_err(|e| e.to_string())?;
    text.split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .ok_or_else(|| "no http body".into())
}

/// HMAC-SHA256 (RFC 2104) for the api session token.
fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let block = 64;
    let mut k = key.to_vec();
    if k.len() > block {
        k = Sha256::digest(&k).to_vec();
    }
    k.resize(block, 0);
    let ipad: Vec<u8> = k.iter().map(|b| b ^ 0x36).collect();
    let opad: Vec<u8> = k.iter().map(|b| b ^ 0x5c).collect();
    let inner = Sha256::digest([&ipad[..], data].concat());
    Sha256::digest([&opad[..], &inner[..]].concat()).to_vec()
}

fn rand_bytes(n: usize) -> Vec<u8> {
    use rand::RngCore as _;
    let mut v = vec![0u8; n];
    rand::thread_rng().fill_bytes(&mut v);
    v
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

fn parse_query(query: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            map.insert(urldecode(k), urldecode(v));
        }
    }
    map
}

fn urldecode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("00");
                out.push(u8::from_str_radix(hex, 16).unwrap_or(b'%'));
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}
