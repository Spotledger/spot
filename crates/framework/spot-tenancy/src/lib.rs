//! The control-plane core (WP-CTL-01.T1, ADR-34, ADR-058).
//!
//! - [`model`]: the tenant record, validated identifiers and the checked state machine.
//! - [`store`]: [`ControlStore`] (registry with unique names, optimistic versions, the workflow
//!   journal) and its in-memory implementation.
//! - [`placement`]: [`PlacementMap::choose`], deterministic, region-strict, capacity-aware, spread.
//! - [`ports`]: the traits the workflows drive (schema, seeder, IAM, licence, telemetry, archive,
//!   approval, audit).
//! - [`plane`]: [`ControlPlane`]: `provision` and `run` (journaled, resumable), `rollback`,
//!   `suspend`, `archive`, `delete` (retention delay and two-person approval).
//!
//! Time is always an argument; nothing here reads a clock, a random source, a secret or the
//! network.

pub mod error;
pub mod model;
pub mod placement;
pub mod plane;
pub mod ports;
pub mod store;

pub use error::{Error, Result, codes};
pub use model::{
    ClusterId, DatabaseId, Edition, Placement, Region, Retention, Tenant, TenantFlags, TenantId,
    TenantName, TenantState,
};
pub use placement::{Cluster, PlacementMap};
pub use plane::{Config, ControlPlane, ProvisionPlan, ProvisionRequest, StepCosts};
pub use store::{ControlStore, JournalEntry, JournalKind, MemoryStore, Step};
