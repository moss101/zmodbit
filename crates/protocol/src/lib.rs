//! modbit-protocol — local/cloud framing + generated schemas.
//!
//! Rust bindings are generated from the canonical protobuf schemas in
//! `proto/` (docs/30) by build.rs. TypeScript bindings in
//! packages/surface-protocol are generated from the same schema source;
//! wire compatibility is proven by golden-fixture round-trip tests on both
//! sides (crates/protocol/tests/wire_compat.rs and packages/surface-protocol).
//!
//! Canonical owner subsystem: domain-events (docs/81). Layout: docs/12.

pub mod transport;

pub mod modbit {
    pub mod protocol {
        pub mod v1 {
            include!(concat!(env!("OUT_DIR"), "/modbit.protocol.v1.rs"));
        }
    }
}
