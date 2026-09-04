//! modbit-event-store — append-only event store + projections.
//!
//! Implements the authoritative persistence of docs/13/docs/31: the
//! append-only `events` table with per-aggregate sequence ordering and
//! integrity hashing, explicit versioned migrations over SQLite (WAL,
//! foreign_keys, synchronous=FULL), and the idempotent command processor
//! (docs/30 § Commands, docs/33 § Idempotency).
//!
//! Canonical owner subsystem: domain-events (docs/81). Layout: docs/12.

pub mod commands;
pub mod migrations;
pub mod projections;
pub mod store;

pub use commands::{CommandProcessor, Outcome};
pub use projections::rebuild;
pub use store::{envelope_for, EventStore, StoreError};
