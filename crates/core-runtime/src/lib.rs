//! modbit-core-runtime — scheduler, WorkGraph/AgentGraph/StateGraph.
//!
//! M1.4 slice: hosts the canonical Core service loop — SurfaceProtocol
//! request dispatch to the idempotent command processor and fleet snapshot
//! reads over the docs/31 projections. The `modbit-core` binary wires this
//! to the authenticated local transport (docs/30 § Local SurfaceProtocol).
//!
//! Canonical owner subsystem: core-runtime (docs/81). Layout: docs/12.

pub mod surface;

pub use surface::CoreServices;
