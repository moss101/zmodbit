//! Cursor metadata interfaces (M4, M4.5): every stateful external
//! surface — terminal, browser, sandbox — exposes the SAME cursor
//! metadata contract, so recovery can reattach to any of them with one
//! code path. A cursor answers: which surface, where was I, at which
//! revision, and is it still live?

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The surface kinds with durable cursors.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SurfaceKind {
    Terminal,
    Browser,
    Sandbox,
}

/// Unified cursor metadata for one surface.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CursorMeta {
    pub surface: SurfaceKind,
    /// Surface handle (run id / session id / sandbox id).
    pub handle: String,
    /// Byte offset (terminal) / navigation index (browser) / step counter
    /// (sandbox) — the surface's own addressing unit.
    pub position: u64,
    /// Workspace revision the surface state belongs to.
    pub revision: u64,
    pub live: bool,
}

/// The interface each surface implements for recovery.
pub trait CursorSource {
    fn cursor_meta(&self) -> CursorMeta;
}

/// A registry of cursor metadata captured at checkpoint time, keyed by
/// surface.
#[derive(Default)]
pub struct CursorRegistry {
    cursors: BTreeMap<(SurfaceKind, String), CursorMeta>,
}

impl CursorRegistry {
    pub fn new() -> Self {
        Default::default()
    }

    pub fn capture(&mut self, meta: CursorMeta) {
        self.cursors
            .insert((meta.surface, meta.handle.clone()), meta);
    }

    pub fn get(&self, surface: SurfaceKind, handle: &str) -> Option<&CursorMeta> {
        self.cursors.get(&(surface, handle.to_string()))
    }

    /// All cursors for reattachment during recovery.
    pub fn all(&self) -> Vec<&CursorMeta> {
        self.cursors.values().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeTerminal {
        offset: u64,
    }

    impl CursorSource for FakeTerminal {
        fn cursor_meta(&self) -> CursorMeta {
            CursorMeta {
                surface: SurfaceKind::Terminal,
                handle: "run-1".into(),
                position: self.offset,
                revision: 41,
                live: true,
            }
        }
    }

    struct FakeBrowser {
        nav_index: u64,
    }

    impl CursorSource for FakeBrowser {
        fn cursor_meta(&self) -> CursorMeta {
            CursorMeta {
                surface: SurfaceKind::Browser,
                handle: "sess-1".into(),
                position: self.nav_index,
                revision: 41,
                live: false,
            }
        }
    }

    struct FakeSandbox;

    impl CursorSource for FakeSandbox {
        fn cursor_meta(&self) -> CursorMeta {
            CursorMeta {
                surface: SurfaceKind::Sandbox,
                handle: "sbx-1".into(),
                position: 7,
                revision: 40,
                live: true,
            }
        }
    }

    /// M4.5: all three surfaces expose the same metadata contract, and the
    /// registry recovers them uniformly.
    #[test]
    fn all_surfaces_expose_unified_cursor_metadata() {
        let mut registry = CursorRegistry::new();
        registry.capture(FakeTerminal { offset: 2048 }.cursor_meta());
        registry.capture(FakeBrowser { nav_index: 3 }.cursor_meta());
        registry.capture(FakeSandbox.cursor_meta());

        let terminal = registry.get(SurfaceKind::Terminal, "run-1").unwrap();
        assert_eq!(terminal.position, 2048);
        assert_eq!(terminal.surface, SurfaceKind::Terminal);
        assert!(terminal.live);

        let browser = registry.get(SurfaceKind::Browser, "sess-1").unwrap();
        assert_eq!(browser.position, 3);
        assert!(!browser.live);

        let sandbox = registry.get(SurfaceKind::Sandbox, "sbx-1").unwrap();
        assert_eq!(sandbox.position, 7);
        assert_eq!(sandbox.revision, 40);

        assert_eq!(registry.all().len(), 3, "every surface recovered");
    }
}
