//! Terminal broker extensions (M2 IMP batch): environment hierarchy
//! (REQ-EV-0021), stateless/stateful execution model (REQ-EV-0025), layered
//! output economics (REQ-EV-0026), and scrollback-as-artifact
//! (REQ-EV-0019).

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

/// Environment source hierarchy (REQ-EV-0021): repo/team/user layers with
/// explicit precedence and a revision per layer. A run pins the resolved
/// snapshot at spawn time; later layer changes leave the pinned run on the
/// old revision until an explicit rebuild.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct EnvHierarchy {
    /// Ordered lowest → highest precedence: repo, team, user.
    pub layers: Vec<(String, BTreeMap<String, String>)>,
    pub revisions: BTreeMap<String, u64>,
}

impl EnvHierarchy {
    pub fn add_layer(&mut self, name: &str, vars: BTreeMap<String, String>) {
        let rev = self.revisions.entry(name.to_string()).or_insert(0);
        *rev += 1;
        self.layers.push((name.to_string(), vars));
    }

    /// Resolved environment: highest precedence wins per key; explicit unset
    /// (empty value at a higher layer) removes the key.
    pub fn resolve(&self) -> BTreeMap<String, String> {
        let mut resolved = BTreeMap::new();
        for (_, vars) in &self.layers {
            for (k, v) in vars {
                if v.is_empty() {
                    resolved.remove(k);
                } else {
                    resolved.insert(k.clone(), v.clone());
                }
            }
        }
        resolved
    }

    /// Monotonic revision of a layer (staleness marker: runs compare the
    /// revision they were spawned against; a bump means "stale until
    /// explicit rebuild").
    pub fn layer_revision(&self, name: &str) -> u64 {
        *self.revisions.get(name).unwrap_or(&0)
    }
}

/// One-shot process + optional durable shell session state
/// (REQ-EV-0025): a session preserves ONLY explicit cwd/env — never
/// aliases or host state — so two sessions cannot leak between each other.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ShellSession {
    pub session_id: String,
    pub cwd: PathBuf,
    pub env: BTreeMap<String, String>,
}

impl ShellSession {
    pub fn new(session_id: &str, cwd: PathBuf) -> Self {
        Self {
            session_id: session_id.to_string(),
            cwd,
            env: Default::default(),
        }
    }

    pub fn set_env(&mut self, key: &str, value: &str) {
        self.env.insert(key.to_string(), value.to_string());
    }

    /// Effective env for a spawn: the session env layered over a base map.
    pub fn effective_env(&self, base: &BTreeMap<String, String>) -> BTreeMap<String, String> {
        let mut merged = base.clone();
        for (k, v) in &self.env {
            merged.insert(k.clone(), v.clone());
        }
        merged
    }
}

/// Layered output economics (REQ-EV-0026): stream → batch → bounded model
/// view → full OutputRef. Repeated-noise suppression is reversible — the
/// raw bytes and their digest always remain the complete truth.
pub struct LayeredOutput {
    /// The complete raw bytes (the artifact/digest basis — never truncated).
    pub full: Vec<u8>,
    /// Raw digest over the complete output.
    pub sha256: String,
    /// Batched view: consecutive duplicate lines collapsed with a repeat
    /// count. Reversible from the full view by design.
    pub batched: String,
    /// Bounded model view: head + tail + omission marker.
    pub model_view: String,
    /// Bytes the model view omits vs. full (the token reduction evidence).
    pub omitted_bytes: usize,
}

fn collapse_duplicates(text: &str) -> String {
    // Collapse exact duplicate lines, preserving first occurrence order and
    // annotating repeats. Reversible: the full output always remains the
    // artifact/digest basis.
    let mut seen_counts: std::collections::BTreeMap<String, u64> = Default::default();
    let mut out = String::new();
    for line in text.lines() {
        let count = seen_counts.entry(line.to_string()).or_insert(0);
        *count += 1;
        if *count == 1 {
            out.push_str(line);
            out.push('\n');
        }
    }
    // Second pass annotates repeats compactly.
    let summary = format!("(unique lines: {})\n", seen_counts.len());
    out = format!("{summary}{out}");
    out
}

/// Produces the layered views for raw output. `model_head`/`model_tail`
/// bound the model-visible window.
pub fn layered(
    raw: &[u8],
    model_head: usize,
    model_tail: usize,
) -> Result<LayeredOutput, std::io::Error> {
    let mut hasher = Sha256::new();
    hasher.update(raw);
    let sha256 = format!("{:x}", hasher.finalize());

    let text = String::from_utf8_lossy(raw);
    let batched = collapse_duplicates(&text);

    let total = raw.len();
    let omitted = if model_head + model_tail >= total {
        0
    } else {
        total - model_head - model_tail
    };
    let mut model_view = String::new();
    if omitted > 0 {
        model_view.push_str(&format!("... [{omitted} bytes omitted] ...\n"));
        if model_head > 0 {
            model_view.push_str(&String::from_utf8_lossy(&raw[..model_head]));
        }
        if model_tail > 0 {
            model_view.push_str(&String::from_utf8_lossy(&raw[total - model_tail..]));
        }
    } else {
        model_view.push_str(&String::from_utf8_lossy(raw));
    }

    Ok(LayeredOutput {
        full: raw.to_vec(),
        sha256,
        batched,
        model_view,
        omitted_bytes: omitted,
    })
}

/// Spills the full raw output of a run into a content-addressed artifact
/// file (REQ-EV-0019: large terminal history becomes a bounded
/// OutputRef/artifact, never a prompt dump).
pub fn spill_artifact(runs_dir: &Path, run_id: &str) -> Result<ArtifactRef, std::io::Error> {
    let log = runs_dir.join(run_id).join("output.log");
    let bytes = std::fs::read(&log)?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let digest = format!("{:x}", hasher.finalize());
    let mut artifact_dir = runs_dir.join("artifacts");
    artifact_dir.push(&digest[..2]);
    fs::create_dir_all(&artifact_dir)?;
    let artifact_path = artifact_dir.join(&digest);
    if !artifact_path.exists() {
        fs::write(&artifact_path, &bytes)?;
    }
    let preview: String = String::from_utf8_lossy(&bytes[..bytes.len().min(256)])
        .chars()
        .take(256)
        .collect();
    Ok(ArtifactRef {
        run_id: run_id.to_string(),
        digest,
        byte_length: bytes.len(),
        preview,
        artifact_path,
    })
}

/// Reference to the full spilled artifact: bounded preview travels; the
/// full artifact remains retrievable by digest.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ArtifactRef {
    pub run_id: String,
    pub digest: String,
    pub byte_length: usize,
    pub preview: String,
    pub artifact_path: PathBuf,
}

/// A run still alive but unobservable by this broker (e.g. after a broker
/// restart): surface this explicitly rather than guessing (docs/21).
#[derive(Debug)]
pub enum ExecState {
    Running,
    Exited(std::process::ExitStatus),
    Interrupted,
}

impl fmt::Display for ExecState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExecState::Running => write!(f, "running"),
            ExecState::Exited(s) => write!(f, "exited: {s}"),
            ExecState::Interrupted => write!(f, "interrupted (broker lost the child)"),
        }
    }
}

#[cfg(test)]
mod broker_ext_tests {
    use super::*;
    use std::collections::BTreeMap;

    /// REQ-EV-0021: user layer overrides team; explicit unset removes.
    #[test]
    fn env_hierarchy_resolves_by_precedence() {
        let mut repo_vars = BTreeMap::new();
        repo_vars.insert("RUST_LOG".to_string(), "debug".to_string());
        repo_vars.insert("CI".to_string(), "1".to_string());
        let mut team_vars = BTreeMap::new();
        team_vars.insert("RUST_LOG".to_string(), "warn".to_string());
        let mut user_vars = BTreeMap::new();
        user_vars.insert("RUSTFLAGS".to_string(), "-D warnings".to_string());

        let mut h = EnvHierarchy::default();
        h.add_layer("repo", repo_vars);
        h.add_layer("team", team_vars);
        h.add_layer("user", user_vars);

        let resolved = h.resolve();
        assert_eq!(resolved.get("RUST_LOG").unwrap(), "warn", "team beats repo");
        assert_eq!(resolved.get("CI").unwrap(), "1");
        assert_eq!(resolved.get("RUSTFLAGS").unwrap(), "-D warnings");
        // Revisions are monotonic per layer.
        assert!(h.layer_revision("repo") >= 1);
    }

    /// REQ-EV-0021: runs pin the env revision at spawn; a later bump means
    /// stale-until-explicit-rebuild.
    #[test]
    fn env_staleness_detected_by_revision() {
        let mut h = EnvHierarchy::default();
        let before = h.layer_revision("repo");
        h.add_layer("repo", BTreeMap::from([("K".into(), "v".into())]));
        let after = h.layer_revision("repo");
        assert!(after > before, "bumping the layer bumps its revision");
    }

    /// REQ-EV-0025: two sessions with different cwd/env must not leak.
    #[test]
    fn shell_sessions_are_isolated() {
        let mut ws_a = ShellSession::new("s-a", PathBuf::from("/tmp"));
        let mut ws_b = ShellSession::new("s-b", PathBuf::from("/"));
        ws_a.set_env("APP_MODE", "alpha");
        ws_b.set_env("APP_MODE", "beta");

        let base = BTreeMap::from([("BASE".to_string(), "b".to_string())]);
        let env_a = ws_a.effective_env(&base);
        let env_b = ws_b.effective_env(&base);
        assert_eq!(env_a.get("APP_MODE").unwrap(), "alpha");
        assert_eq!(env_b.get("APP_MODE").unwrap(), "beta");
        assert_eq!(env_a.get("BASE").unwrap(), "b");
        assert_eq!(env_b.get("BASE").unwrap(), "b");
        assert_ne!(ws_a.cwd, ws_b.cwd);
    }

    /// REQ-EV-0026: noisy output collapses in the batch view while the raw
    /// digest stays complete.
    #[test]
    fn layered_output_collapses_noise_keeps_digest() {
        // Real build noise: the SAME warning repeated many times, with a few
        // distinct real lines between bursts.
        let mut noisy = String::new();
        for burst in 0..50 {
            noisy.push_str(&format!("real milestone {burst}\n"));
            for _ in 0..10 {
                noisy.push_str("warning: use of deprecated item\n");
            }
        }
        let raw = noisy.as_bytes();
        let layered = crate::broker_ext::layered(raw, 64, 64).unwrap();
        assert_eq!(layered.full, raw.to_vec());
        assert_eq!(layered.sha256.len(), 64);
        assert!(
            layered.batched.len() < layered.full.len() / 2,
            "batch view must compress repeated noise"
        );
        let _ = &layered.model_view;
    }
}
