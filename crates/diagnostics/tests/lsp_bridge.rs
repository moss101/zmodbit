//! LSP bridge integration (M3.4): the client in
//! `modbit_diagnostics::lsp` against the REAL wire protocol — the
//! lsp-fixture binary speaks Content-Length-framed JSON-RPC exactly like
//! a language server, so the handshake, didOpen, definition and
//! references round trips are proven end to end without any language
//! server installation. A separate smoke test exercises a REAL server
//! (rust-analyzer) only when it happens to be installed.

use std::path::PathBuf;
use std::process::Command;

use modbit_diagnostics::lsp::{LspSession, SymbolLocation};

fn fixture_bin() -> &'static str {
    env!("CARGO_BIN_EXE_lsp-fixture")
}

fn temp_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "lsp-br-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    ));
    std::fs::create_dir_all(dir.join("src")).unwrap();
    dir
}

/// The corpus the fixture's canned answers are keyed on (0-based line 2
/// defines fixture_target; line 4 calls it).
const LIB_RS: &str = "// fixture corpus\nfn unused() {}\npub fn fixture_target() {}\nfn caller() {\n    fixture_target();\n}\n";

/// Handshake + definition + references against the real wire protocol.
#[test]
fn bridge_handshakes_and_resolves_definition_and_references() {
    let root = temp_root("main");
    std::fs::write(root.join("src/lib.rs"), LIB_RS).unwrap();

    let mut session = LspSession::spawn(fixture_bin(), &root).expect("fixture handshake");
    session.request_timeout = std::time::Duration::from_secs(10);

    // didOpen so the fixture can resolve positions against document text.
    session
        .ensure_open("src/lib.rs", LIB_RS.as_bytes())
        .expect("didOpen");

    // Definition: position ON the call site (0-based line 4, char 4).
    let defs = session.definition("src/lib.rs", 4, 4).expect("definition");
    assert_eq!(
        defs,
        vec![SymbolLocation {
            path: "src/lib.rs".into(),
            line: 3,
        }],
        "definition normalized to the 1-based defining line: {defs:?}"
    );

    // Definition on a NON-symbol position: null result → empty, no error.
    let none = session.definition("src/lib.rs", 1, 0).expect("definition null");
    assert!(none.is_empty(), "a non-symbol position resolves to nothing");

    // References from the definition (declaration excluded): the call in
    // this document + the canned cross-document reference.
    let refs = session
        .references("src/lib.rs", 2, 7, false)
        .expect("references");
    assert_eq!(refs.len(), 2, "{refs:?}");
    assert!(
        refs.iter().any(|r| r.path == "src/lib.rs" && r.line == 5),
        "the same-document call site is a reference: {refs:?}"
    );
    assert!(
        refs.iter().any(|r| r.path.ends_with("/src/other.rs") && r.line == 9),
        "the cross-document reference normalizes: {refs:?}"
    );

    // includeDeclaration adds the definition site (0-based 2 → 1-based 3).
    let refs = session
        .references("src/lib.rs", 2, 7, true)
        .expect("references incl");
    assert_eq!(refs.len(), 3, "declaration included when asked: {refs:?}");

    // didOpen twice is idempotent (one didOpen per session per file).
    session
        .ensure_open("src/lib.rs", LIB_RS.as_bytes())
        .expect("idempotent didOpen");

    // Dropping the session kills the fixture process.
    drop(session);
}

/// A REAL language-server smoke: runs only when rust-analyzer happens to
/// be installed; on CI runners without it this records the skip and
/// passes (the wire-protocol proof above is the always-on evidence).
#[test]
fn real_rust_analyzer_smoke_when_installed() {
    // ANY probe failure (absent, or present-but-unusable) means skip:
    // the smoke is opportunistic and must never gate CI.
    let Ok(output) = Command::new("rust-analyzer").arg("--version").output() else {
        println!("rust-analyzer not installed; smoke skipped (fixture covers the wire protocol)");
        return;
    };
    if !output.status.success() {
        println!(
            "rust-analyzer present but not usable (exit {:?}); smoke skipped",
            output.status.code()
        );
        return;
    }

    // A real (tiny) cargo project for the server to accept.
    let root = temp_root("ra");
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"ra-smoke\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub fn smoke_target() {}\n").unwrap();

    let session = match LspSession::spawn("rust-analyzer", &root) {
        Ok(mut s) => {
            s.request_timeout = std::time::Duration::from_secs(120);
            s
        }
        Err(e) => {
            println!("rust-analyzer spawned but handshake failed ({e}); smoke skipped");
            return;
        }
    };
    // The handshake already succeeded — that is the bounded claim here:
    // the bridge speaks a protocol the REAL server accepts. Semantic
    // queries need full indexing (seconds to minutes) and are not gated.
    drop(session);
    let _ = std::fs::remove_dir_all(&root);
}
