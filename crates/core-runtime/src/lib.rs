//! modbit-core-runtime — scheduler, WorkGraph/AgentGraph/StateGraph.
//!
//! M1.4 slice: hosts the canonical Core service loop — SurfaceProtocol
//! request dispatch to the idempotent command processor and fleet snapshot
//! reads over the docs/31 projections. The `modbit-core` binary wires this
//! to the authenticated local transport (docs/30 § Local SurfaceProtocol).
//!
//! Canonical owner subsystem: core-runtime (docs/81). Layout: docs/12.

pub mod agent_fleet;
pub mod agent_profiles_plans;
pub mod agent_runtime_batch2;
pub mod config;
pub mod daemon;
pub mod delegation;
pub mod fleet_admission;
pub mod one_agent;
pub mod surface;

pub use config::{resolve, Authority, ConfigLayer, ResolvedConfig};
pub use daemon::Daemon;
pub use delegation::{decide, Delegation, DelegationDecision, DependencySignal, Reason};
pub use surface::CoreServices;
