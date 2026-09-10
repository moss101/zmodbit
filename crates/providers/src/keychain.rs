//! Keychain-backed [`SecretBroker`] (Phase 4 item 3, docs/36 § Selected
//! dependencies: native OS secret stores via the `keyring` crate —
//! macOS Keychain, Windows Credential Manager, Linux Secret Service).
//!
//! Policy (docs/31 § Secrets):
//! - lookups hit the OS keychain FIRST, the environment SECOND — the env
//!   broker remains the fallback so CI and headless boots keep working;
//! - values are set/delete through the same store; they NEVER enter the
//!   environment, the settings document, or the event store;
//! - `has_credential` reports keychain storage only (the settings screen's
//!   "stored in the system keychain" indicator).
//!
//! The service name is namespaced ("modbit-core") and the account is the
//! broker name the transport asks for (e.g. `OPENAI_API_KEY`) — exactly
//! the names the env broker would read.

use crate::transport::{EnvSecretBroker, SecretBroker, TransportError};
use std::sync::Mutex;

pub const SERVICE_NAME: &str = "modbit-core";

/// The service namespace for this process. Tests and E2E runs set
/// `MODBIT_KEYCHAIN_SERVICE` to a run-scoped name: keychain ACLs are
/// per-creating-binary, so a rebuilt binary touching an entry created by
/// an older signature would otherwise block on a user prompt.
fn service_name() -> String {
    std::env::var("MODBIT_KEYCHAIN_SERVICE").unwrap_or_else(|_| SERVICE_NAME.to_string())
}

/// Set/delete operations can fail for environmental reasons (no keychain
/// daemon, locked keychain). The BROKER lookups must not — callers get a
/// typed "unavailable" flag instead, and the env fallback applies.
#[derive(Debug)]
pub enum KeychainError {
    Unavailable(String),
    Operation(String),
}

impl std::fmt::Display for KeychainError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KeychainError::Unavailable(e) => write!(f, "keychain unavailable: {e}"),
            KeychainError::Operation(e) => write!(f, "keychain operation failed: {e}"),
        }
    }
}

fn entry_for(name: &str) -> Result<keyring::Entry, KeychainError> {
    keyring::Entry::new(&service_name(), name)
        .map_err(|e| KeychainError::Unavailable(e.to_string()))
}

/// Serializes keychain mutations: concurrent set/delete on the same entry
/// can race the platform store's UI/daemon.
static MUTATION_LOCK: Mutex<()> = Mutex::new(());

/// Reads one secret from the OS keychain. `Ok(None)` = not stored
/// (distinct from a locked/unavailable keychain, which is an error the
/// caller may treat as a miss — the env fallback makes that safe).
pub fn read_secret(name: &str) -> Result<Option<String>, KeychainError> {
    let entry = entry_for(name)?;
    match entry.get_password() {
        Ok(v) => Ok(Some(v)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(KeychainError::Operation(e.to_string())),
    }
}

/// Stores one secret in the OS keychain.
pub fn store_secret(name: &str, value: &str) -> Result<(), KeychainError> {
    let _guard = MUTATION_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let entry = entry_for(name)?;
    entry
        .set_password(value)
        .map_err(|e| KeychainError::Operation(e.to_string()))
}

/// Deletes one secret from the OS keychain (NoEntry is success).
pub fn delete_secret(name: &str) -> Result<(), KeychainError> {
    let _guard = MUTATION_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let entry = entry_for(name)?;
    match entry.delete_credential() {
        Ok(()) => Ok(()),
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(KeychainError::Operation(e.to_string())),
    }
}

/// Whether the secret is stored in the keychain (settings indicator).
pub fn has_secret(name: &str) -> bool {
    matches!(read_secret(name), Ok(Some(_)))
}

/// The keychain-first broker: OS keychain, then the environment broker as
/// the documented fallback (Phase 4 item 3).
pub struct KeychainSecretBroker;

impl SecretBroker for KeychainSecretBroker {
    fn credential(&self, name: &str) -> Result<String, TransportError> {
        let outcome = read_secret(name);
        if std::env::var("MODBIT_DEBUG_BROKER").is_ok() {
            eprintln!(
                "broker: keychain read of {name} under service {:?} -> {:?}",
                service_name(),
                outcome.as_ref().map(|o| o.as_ref().map(|_| "<redacted>")),
            );
        }
        match outcome {
            Ok(Some(v)) if !v.trim().is_empty() => return Ok(v),
            // A locked/unavailable keychain is treated as a miss: the env
            // fallback keeps headless boots and CI working.
            _ => {}
        }
        EnvSecretBroker.credential(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unique per run so parallel/leaked entries never collide; the test
    /// cleans up after itself.
    fn scratch_name(tag: &str) -> String {
        format!(
            "MODBIT_TEST_{tag}_{}",
            &uuid::Uuid::now_v7().simple().to_string()[..12]
        )
    }

    /// The platform store round-trips set → has → get → delete.
    #[test]
    fn keychain_round_trip() {
        let name = scratch_name("rt");
        // Probe availability first: a headless Linux CI without a secret
        // service skips this test (the env fallback path is covered below
        // and the E2E asserts the real flow where a keychain exists).
        if let Err(KeychainError::Unavailable(e)) = store_secret(&name, "probe") {
            println!("keychain unavailable ({e}); round-trip skipped");
            return;
        }
        store_secret(&name, "probe").unwrap();
        assert!(has_secret(&name));
        assert_eq!(read_secret(&name).unwrap().as_deref(), Some("probe"));
        // Overwrite (the settings screen re-saving a key).
        store_secret(&name, "rotated").unwrap();
        assert_eq!(read_secret(&name).unwrap().as_deref(), Some("rotated"));
        delete_secret(&name).unwrap();
        assert!(!has_secret(&name), "deleted means gone");
        assert_eq!(read_secret(&name).unwrap(), None);
    }

    /// The documented fallback: a keychain miss reads the environment.
    /// (The broker treats a locked/unavailable keychain the same way.)
    #[test]
    fn broker_falls_back_to_env_on_keychain_miss() {
        let name = scratch_name("env");
        // SAFETY: single-threaded test touch of this unique name.
        std::env::set_var(&name, "from-env");
        let broker = KeychainSecretBroker;
        assert_eq!(
            broker.credential(&name).unwrap(),
            "from-env",
            "keychain miss falls back to the environment broker"
        );
        std::env::remove_var(&name);
    }

    /// Keychain-first ordering: when BOTH exist, the keychain wins.
    #[test]
    fn broker_prefers_keychain_over_env() {
        let name = scratch_name("pref");
        if store_secret(&name, "from-keychain").is_err() {
            println!("keychain unavailable; preference test skipped");
            return;
        }
        std::env::set_var(&name, "from-env");
        let broker = KeychainSecretBroker;
        assert_eq!(broker.credential(&name).unwrap(), "from-keychain");
        delete_secret(&name).unwrap();
        std::env::remove_var(&name);
    }
}
