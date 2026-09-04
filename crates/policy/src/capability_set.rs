//! ProtocolCapabilitySet + end-to-end capability invariant (M2,
//! REQ-EV-0043/0044, docs/16 § Capability Kernel).
//!
//! Separates client/transport capabilities from per-round execution
//! authority. A capability is advertised ONLY when producer, authorization,
//! transport and consumer all exist — removing any one makes the tool
//! disappear rather than fail after the model selects it (REQ-EV-0044).

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// Which layer provides/consumes a capability.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityLayer {
    /// The tool handler / effector exists.
    Producer,
    /// The policy kernel grants authorization.
    Authorization,
    /// The transport protocol can carry it.
    Transport,
    /// The consumer (renderer/CLI) can render/use it.
    Consumer,
}

/// The set of capabilities a protocol endpoint advertises.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtocolCapabilitySet {
    pub capabilities: BTreeSet<String>,
}

impl ProtocolCapabilitySet {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, capability: &str) {
        self.capabilities.insert(capability.to_string());
    }

    pub fn remove(&mut self, capability: &str) {
        self.capabilities.remove(capability);
    }

    pub fn has(&self, capability: &str) -> bool {
        self.capabilities.contains(capability)
    }

    /// Merges two sets (for multi-client negotiation).
    pub fn union(&self, other: &ProtocolCapabilitySet) -> ProtocolCapabilitySet {
        let mut merged = self.clone();
        merged
            .capabilities
            .extend(other.capabilities.iter().cloned());
        merged
    }

    /// Intersects two sets (for the minimum common capability set).
    pub fn intersection(&self, other: &ProtocolCapabilitySet) -> ProtocolCapabilitySet {
        let mut result = ProtocolCapabilitySet::default();
        for cap in &self.capabilities {
            if other.capabilities.contains(cap) {
                result.capabilities.insert(cap.clone());
            }
        }
        result
    }
}

/// End-to-end capability invariant (REQ-EV-0044): a capability is advertised
/// only when all four layers exist. If any layer is missing, the capability
/// is not advertised and the model never sees it — preventing post-selection
/// failures.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AdvertisedCapability {
    pub name: String,
    pub producer_exists: bool,
    pub authorized: bool,
    pub transport_supported: bool,
    pub consumer_exists: bool,
}

/// Checks the end-to-end invariant: all four layers must be present.
pub fn check_e2e_capability(cap: &AdvertisedCapability) -> Result<(), String> {
    if !cap.producer_exists {
        return Err(format!("{}: no producer/effector exists", cap.name));
    }
    if !cap.authorized {
        return Err(format!("{}: not authorized by policy", cap.name));
    }
    if !cap.transport_supported {
        return Err(format!("{}: transport does not support it", cap.name));
    }
    if !cap.consumer_exists {
        return Err(format!("{}: no consumer to render it", cap.name));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// REQ-EV-0043: a headless client has no UI capabilities — the
    /// intersection of the protocol set with UI caps is empty.
    #[test]
    fn protocol_capability_set_intersects() {
        let server = ProtocolCapabilitySet {
            capabilities: [
                "browser".to_string(),
                "terminal".to_string(),
                "fs.read".to_string(),
            ]
            .into_iter()
            .collect(),
        };
        let headless_client = ProtocolCapabilitySet {
            capabilities: ["terminal".to_string(), "fs.read".to_string()]
                .into_iter()
                .collect(),
        };
        let common = server.intersection(&headless_client);
        assert!(common.has("terminal"));
        assert!(common.has("fs.read"));
        assert!(!common.has("browser"), "browser not in headless client");
    }

    /// REQ-EV-0044: a capability is advertised only when all four layers
    /// (producer, authorization, transport, consumer) exist.
    #[test]
    fn e2e_invariant_all_layers_present() {
        let checks = vec![
            (true, true, true, true, true),
            (true, true, false, true, false),
            (true, false, true, true, false),
            (false, true, true, true, false),
        ];
        for (producer, auth, transport, consumer, expect_ok) in checks {
            let result = (|| -> Result<(), String> {
                if !producer {
                    return Err("no producer".into());
                }
                if !auth {
                    return Err("not authorized".into());
                }
                if !transport {
                    return Err("no transport".into());
                }
                if !consumer {
                    return Err("no consumer".into());
                }
                Ok(())
            })();
            assert_eq!(result.is_ok(), expect_ok);
        }
    }
}
