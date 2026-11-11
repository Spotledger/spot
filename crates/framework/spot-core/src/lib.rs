//! Shared types for the Spotledger Platform.
//!
//! The full contents are defined by the app API contract (WP-CON-02). The types here were
//! published ahead of it by the lead so that framework crates built in parallel (HTTP, auth,
//! realtime, telemetry, cache) share one definition instead of each inventing its own:
//!
//! - [`SiteId`]: which tenant a request, job or cache entry belongs to (ADR-34).
//! - [`Clock`]: the only way framework code reads the time, so tests control it.

pub mod clock;
pub mod site;

pub use clock::{Clock, ManualClock, SystemClock};
pub use site::{InvalidSiteId, SiteId};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-core";
