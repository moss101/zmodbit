# ADR-0002: tokio + reqwest + rustls as the async runtime and model HTTP transport

- **ID:** ADR-0002
- **Status:** ACCEPTED
- **Date:** 2026-09-05
- **Affects:** crates/providers/Cargo.toml, crates/core-runtime/Cargo.toml, Cargo.toml
- **Decides:** The model provider gateway's HTTP transport and the process-wide async runtime. Modbit standardizes on `tokio` (multi-thread async runtime), `reqwest` (HTTP/1.1 + HTTP/2 client) with the `rustls` TLS backend (no OpenSSL), and `tokio`-native streaming for incremental SSE. The sync alternative (`ureq` + OS threads) is rejected.

## Trigger / Evidence

Future-tasks.md (audit of 2026-09-05, section 5 Phase 1 item 1): the workspace has **no HTTP client in the product**; the only model transport is a `curl` subprocess inside two test files, so no production caller can stream a model response (M2 classifies as WIRED, not COMPLETE). REQ-EV-0028/0112 and docs/15 require a normalized **streaming** inference contract with cancellation, timeout, bounded retry on 429/5xx and usage capture. docs/15 § Live provider proof requires nightly CI to perform a real streaming call, a real tool-call round trip and cancellation against a production endpoint. Incremental SSE parsing and cancellation are only tractable with non-blocking incremental reads; blocking a thread per in-flight stream does not compose with the single scheduler's concurrent turns, terminal broker output pumping and event streaming (M1 daemon already streams SSE to desktop clients).

## Current Behavior

`modbit-providers` defines request bodies and SSE line parsers but owns no socket. Integration tests shell out to `curl -N` to move bytes; the product binary cannot reach the provider gateway's transport at all. No async runtime exists in the workspace; the M1 daemon uses blocking I/O threads.

## Proposed Replacement

1. `Cargo.toml` (workspace) gains `tokio` (features: `rt-multi-thread`, `macros`, `time`, `sync`, `io-util`, `net`), `tokio-util` (cancellation tokens), `reqwest` (features: `stream`, `json`, `rustls-tls`, `gzip`; `default-features = false`), and `futures` (stream utilities). TLS is `rustls` via `reqwest`'s `rustls-tls` feature; no dynamic OpenSSL linkage (macOS/Windows CI and future MicroVM images stay self-contained).
2. `crates/providers/src/transport.rs` defines the object-safe `ModelTransport` trait (sync method returning a stream handle — no async-trait needed) and `HttpStreamTransport` implementing it over `reqwest`: incremental SSE (bytes pushed to a `tokio::sync::mpsc` channel as they arrive, never buffered into a `Vec`), per-request total timeout + connect timeout, a `tokio_util::sync::CancellationToken`, bounded retry with exponential backoff + jitter on 429/5xx/connection errors before the first token byte, and usage capture from the provider's usage-bearing final frames.
3. Credentials are handed to the transport through a `SecretBroker` trait (env-backed implementation now; keychain implementation in Phase 2). Raw secrets never enter logs, event payloads or model context (docs/15 § Credentials).
4. `core-runtime` runs the scheduler on the shared tokio runtime; blocking subsystems (SQLite event store, process spawns) stay on `tokio::task::spawn_blocking` so the SSE path never stalls behind them.

## Migration

No stored state changes. The two `curl`-subprocess tests are replaced or supplemented by tests against `HttpStreamTransport` talking to a local fixture HTTP server; the curl path is deleted once coverage is equivalent. The M1 daemon keeps its blocking event loop for now; it gains a tokio handle when the scheduler lands (Phase 1 item 3), which is additive.

## Compatibility

No protocol/schema/API/event changes. `ModelTransport` is a new internal boundary in `modbit-providers`; provider-specific semantics stay inside the crate (docs/81 single model-provider-gateway owner). OpenAI-compatible and Anthropic adapters keep their existing request/SSE types.

## Security Impact

`rustls` avoids OpenSSL version drift and its C attack surface. `reqwest` redirect behavior will be configured to strip `Authorization` on cross-origin redirect (default `Policy::none` is replaced by a limited same-origin policy in the transport). Retry-with-backoff must not replay streaming bodies containing tenant data beyond the original endpoint. Secrets flow only through `SecretBroker`; the transport redacts `Authorization` headers from any error/debug output.

## Test Impact

- Unit: SSE chunk splitting across buffer boundaries, cancel-before-first-byte, timeout, 429 retry then success, 5xx exhausts bounded retries, usage event extraction.
- Integration: local TCP fixture server driving `HttpStreamTransport` through production routing; env-backed `SecretBroker`.
- Live (nightly CI, docs/15): real streaming call + real tool-call round trip + cancellation against the production endpoint with a repository secret, log committed under `docs/evidence/`.

## Rollback

The transport is behind the `ModelTransport` trait; reverting to the curl-subprocess test path or to a blocking transport is a crate-local change. No data or protocol migration to undo. Dependency removal is a workspace `Cargo.toml` revert.

## Explicit User Approval

Approved by mohsin, 2026-09-05, via the session goal instruction: "ADR-A (docs/decisions, before Phase 1 code): async runtime and HTTP client. Recommendation: tokio + reqwest + rustls. State the alternative (sync ureq) and why it was rejected."
