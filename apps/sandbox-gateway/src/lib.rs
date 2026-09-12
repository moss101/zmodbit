//! Sandbox gateway shared surface. The wire types live in the canonical
//! protocol crate (`modbit_protocol::cloud`); they are re-exported here
//! for convenience — the gateway crate adds no second envelope.
pub use modbit_protocol::cloud::{Envelope, GatewayError, Registration};
