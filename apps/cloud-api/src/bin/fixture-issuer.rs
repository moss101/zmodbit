//! Fixture OIDC issuer (TEST INFRASTRUCTURE ONLY — never shipped as the
//! production identity provider; MOD-AUTH-001 keeps the issuer a
//! configuration point). A real HTTP process: /authorize binds
//! state→(nonce, code_challenge), /token verifies the PKCE verifier and
//! signs an RS256 id_token with a real per-boot RSA key, and
//! /.well-known/jwks.json publishes the public half.
//!
//! Boot line: `issuer <addr>`.

use std::collections::HashMap;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use rsa::pkcs1v15::SigningKey;
use rsa::signature::{RandomizedSigner as _, SignatureEncoding as _};
use rsa::RsaPrivateKey;
use serde_json::json;
use sha2::Digest as _;

struct State {
    key: SigningKey<sha2::Sha256>,
    pending: Mutex<HashMap<String, (String, String)>>,
    iss: String,
}

fn b64url(data: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
}

fn http_reply(stream: &mut TcpStream, status: &str, body: &str, location: Option<&str>) {
    let mut head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n",
        body.len()
    );
    if let Some(loc) = location {
        head.push_str(&format!("Location: {loc}\r\n"));
    }
    head.push_str("Connection: close\r\n\r\n");
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body.as_bytes());
}

fn urldecode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes.get(i) {
            Some(b'%') if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("00");
                out.push(u8::from_str_radix(hex, 16).unwrap_or(b'%'));
                i += 3;
            }
            Some(b'+') => {
                out.push(b' ');
                i += 1;
            }
            Some(&b) => {
                out.push(b);
                i += 1;
            }
            None => break,
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

fn parse_query(query: &str) -> HashMap<String, String> {
    query
        .split('&')
        .filter_map(|p| p.split_once('=').map(|(k, v)| (k.to_string(), urldecode(v))))
        .collect()
}

fn main() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind issuer");
    let addr = listener.local_addr().expect("addr");
    let key = {
        use rand::SeedableRng as _;
        let mut rng = rand::rngs::StdRng::from_entropy();
        RsaPrivateKey::new(&mut rng, 2048).expect("rsa keygen")
    };
    let state = Arc::new(State {
        key: SigningKey::<sha2::Sha256>::new(key),
        pending: Mutex::new(HashMap::new()),
        iss: format!("http://{addr}"),
    });
    println!("issuer {addr}");

    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let state = state.clone();
        if let Err(e) = serve_conn(stream, state) {
            eprintln!("fixture-issuer: conn error: {e}");
        }
    }
}

fn serve_conn(mut stream: TcpStream, state: Arc<State>) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let target = request_line.split_whitespace().nth(1).unwrap_or("").to_string();
    let mut content_length = 0usize;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 || header.trim().is_empty() {
            break;
        }
        if let Some(v) = header
            .to_ascii_lowercase()
            .strip_prefix("content-length:")
            .map(|v| v.trim().to_string())
        {
            content_length = v.parse().unwrap_or(0);
        }
    }
    let mut buf = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut buf)?;
    }
    let body = String::from_utf8_lossy(&buf).to_string();

    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target.clone(), String::new()),
    };
    let params = parse_query(&query);
    match path.as_str() {
        "/authorize" => {
            // The issued CODE is the grant handle: /token looks the grant
            // up by code (state is never sent to the token endpoint).
            let code = params.get("code").cloned().unwrap_or_else(|| "authcode-1".into());
            state.pending.lock().unwrap().insert(
                code,
                (
                    params.get("nonce").cloned().unwrap_or_default(),
                    params.get("code_challenge").cloned().unwrap_or_default(),
                ),
            );
            let redirect = params.get("redirect_uri").cloned().unwrap_or_default();
            http_reply(
                &mut stream,
                "302 Found",
                "",
                Some(&format!(
                    "{redirect}?code=authcode-1&state={}",
                    params.get("state").cloned().unwrap_or_default()
                )),
            );
        }
        "/token" => {
            let form: HashMap<String, String> = body
                .split('&')
                .filter_map(|p| p.split_once('=').map(|(k, v)| (k.to_string(), urldecode(v))))
                .collect();
            let pending = form
                .get("code")
                .and_then(|c| state.pending.lock().unwrap().remove(c));
            let Some((nonce, challenge)) = pending else {
                http_reply(
                    &mut stream,
                    "400 Bad Request",
                    "{\"error\":\"unknown state\"}",
                    None,
                );
                return Ok(());
            };
            let verifier = form.get("code_verifier").cloned().unwrap_or_default();
            let computed = b64url(&sha2::Sha256::digest(verifier.as_bytes()));
            if computed != challenge {
                http_reply(
                    &mut stream,
                    "401 Unauthorized",
                    "{\"error\":\"pkce verification failed\"}",
                    None,
                );
                return Ok(());
            }
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs();
            let header = json!({"alg": "RS256", "typ": "JWT"});
            let claims = json!({
                "iss": state.iss,
                "aud": "modbit-cloud",
                "sub": "user-42",
                "tenant": "tenant-1",
                "nonce": nonce,
                "exp": now + 300,
                "iat": now,
            });
            let enc = |v: &serde_json::Value| b64url(v.to_string().as_bytes());
            let signing_input = format!("{}.{}", enc(&header), enc(&claims));
            let mut rng = rand::thread_rng();
            let sig = state
                .key
                .sign_with_rng(&mut rng, signing_input.as_bytes());
            let id_token = format!(
                "{signing_input}.{}",
                b64url(&sig.to_bytes())
            );
            http_reply(
                &mut stream,
                "200 OK",
                &json!({ "id_token": id_token }).to_string(),
                None,
            );
        }
        "/.well-known/jwks.json" => {
            use rsa::traits::PublicKeyParts as _;
                        let public = state.key.as_ref().clone().to_public_key();
            let enc = |d: &[u8]| b64url(d);
            http_reply(
                &mut stream,
                "200 OK",
                &json!({
                    "keys": [{
                        "kty": "RSA",
                        "use": "sig",
                        "alg": "RS256",
                        "kid": "cloud-key",
                        "n": enc(&public.n().to_bytes_be()),
                        "e": enc(&public.e().to_bytes_be()),
                    }]
                })
                .to_string(),
                None,
            );
        }
        _ => http_reply(&mut stream, "404 Not Found", "{\"error\":\"nf\"}", None),
    }
    Ok(())
}
