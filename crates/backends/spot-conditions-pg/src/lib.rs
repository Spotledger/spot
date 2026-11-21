//! `spot-conditions-pg`: the durable PostgreSQL store behind `spot-conditions` (WP-RTB-01.T2).
//!
//! [`PgConditionStore`] is what lets the site host `ConditionEngine` with open conditions that
//! survive a restart: [`PgConditionStore::load_open`] is the engine's `restore` input, and
//! [`PgConditionStore::apply`] writes the engine's audit events into `spot_condition_open`,
//! `spot_condition_history` and the outbox, all in one transaction. See the `store` module for
//! the detail and the module-level notes on the two decisions this crate makes that are not
//! obvious from the type signatures: why an `Arc<Catalog>` with no entity in it, and why the
//! outbox `NodeRef` carries a fresh id rather than a real document's.
//!
//! This crate owns no HTTP surface and authorises nothing: it is a storage adapter only, called
//! by the site's own `services/conditions.rs` host (`spot-cli`).

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod error;
pub mod schema;
mod store;

pub use error::ConditionsPgError;
pub use store::PgConditionStore;

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-conditions-pg";
