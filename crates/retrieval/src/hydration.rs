//! Read-through freshness hydration (M3, REQ-EV-0002): indexes rank
//! candidates, but source bytes are re-read from the ACTIVE revision
//! before prompt inclusion. A stale index entry can therefore never put
//! stale bytes in front of the model — the re-read is the source of truth
//! and the staleness is recorded as provenance.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Reads CURRENT bytes for a path from the active workspace revision.
pub trait FreshnessSource {
    /// Returns (current bytes, current revision) or None if deleted.
    fn current(&self, path: &str) -> Option<(Vec<u8>, u64)>;
}

/// Hydrated bytes plus provenance (REQ-EV-0003: provenance survives).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HydratedBytes {
    pub path: String,
    pub bytes: Vec<u8>,
    /// Revision the bytes were read from (the ACTIVE one, always).
    pub revision: u64,
    /// sha256 of `bytes` — what the context pack records as provenance.
    pub sha256: String,
    /// True when the index digest was out of date and the re-read saved
    /// stale bytes from reaching the model.
    pub was_stale: bool,
}

#[derive(Debug)]
pub enum HydrationError {
    /// The index proposed a path that no longer exists in the workspace.
    Deleted { path: String },
}

impl fmt::Display for HydrationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HydrationError::Deleted { path } => {
                write!(f, "candidate {path:?} was deleted since indexing")
            }
        }
    }
}

impl std::error::Error for HydrationError {}

/// Hydrates one candidate with read-through freshness: bytes are re-read
/// from the active revision; `indexed_sha256` is compared only to RECORD
/// staleness, never to decide which bytes to use.
pub fn hydrate_fresh<S: FreshnessSource>(
    source: &S,
    path: &str,
    indexed_sha256: &str,
) -> Result<HydratedBytes, HydrationError> {
    let (bytes, revision) = source
        .current(path)
        .ok_or_else(|| HydrationError::Deleted {
            path: path.to_string(),
        })?;
    let sha256 = crate::sha256_hex(&bytes);
    let was_stale = sha256 != indexed_sha256;
    Ok(HydratedBytes {
        path: path.to_string(),
        bytes,
        revision,
        sha256,
        was_stale,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    /// An in-memory workspace whose contents a test mutates after
    /// indexing.
    struct Workspace {
        files: RefCell<BTreeMap<String, Vec<u8>>>,
        revision: RefCell<u64>,
    }

    impl Workspace {
        fn new() -> Self {
            Self {
                files: RefCell::new(BTreeMap::new()),
                revision: RefCell::new(1),
            }
        }
        fn write(&self, path: &str, bytes: &[u8]) {
            self.files
                .borrow_mut()
                .insert(path.to_string(), bytes.to_vec());
            *self.revision.borrow_mut() += 1;
        }
    }

    impl FreshnessSource for Workspace {
        fn current(&self, path: &str) -> Option<(Vec<u8>, u64)> {
            self.files
                .borrow()
                .get(path)
                .map(|b| (b.clone(), *self.revision.borrow()))
        }
    }

    /// QUAL-EV-0002: mutate an indexed file after indexing; stale bytes
    /// must never reach model context.
    #[test]
    fn mutated_file_never_returns_stale_bytes() {
        let ws = Workspace::new();
        ws.write("src/lib.rs", b"fn old() {}\n");

        // "Index" the file: digest of the bytes AS INDEXED.
        let indexed_digest = crate::sha256_hex(b"fn old() {}\n");

        // The task mutates the file AFTER indexing.
        ws.write("src/lib.rs", b"fn renamed_and_rewritten() {}\n");

        // Hydration re-reads from the ACTIVE revision: fresh bytes win,
        // staleness is recorded — never the stale indexed content.
        let hydrated = hydrate_fresh(&ws, "src/lib.rs", &indexed_digest).unwrap();
        assert_eq!(
            hydrated.bytes,
            b"fn renamed_and_rewritten() {}\n".to_vec(),
            "stale bytes must never reach the model"
        );
        assert!(hydrated.was_stale);
        assert_eq!(hydrated.revision, 3, "active revision after both writes");
        assert_eq!(hydrated.sha256, crate::sha256_hex(&hydrated.bytes));

        // An unchanged file hydrates with was_stale = false.
        ws.write("README.md", b"# fine\n");
        let stable_digest = crate::sha256_hex(b"# fine\n");
        let hydrated = hydrate_fresh(&ws, "README.md", &stable_digest).unwrap();
        assert!(!hydrated.was_stale);
        assert_eq!(hydrated.bytes, b"# fine\n".to_vec());

        // A deleted candidate is a typed error — not empty bytes.
        let err = hydrate_fresh(&ws, "gone.rs", &stable_digest).unwrap_err();
        assert!(matches!(err, HydrationError::Deleted { .. }));
    }
}
