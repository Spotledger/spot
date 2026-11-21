//! The PostgreSQL store of the node role a site first started as (WP-RTB-03.T5).
//!
//! One table, `spot_node_role_state`, one row per site (see [`schema`]). The host records the role
//! a site first started as, and a later start in another role is refused; that decision is the
//! caller's. This crate only reads and writes the row, async over the caller's pool, with bound
//! parameters only. Reads fail closed: a pool or database failure is an error, never `Ok(None)`.
//!
//! The table lives in the site's own database, keyed by site. The crate reads no clock: the time of
//! a write is an argument.

#![warn(missing_docs)]

mod error;
pub mod schema;
mod store;

pub use error::{NodeRoleStoreError, TABLE_MISSING, UNAVAILABLE};
pub use store::{PgNodeRoleStore, StoredRole};
#[cfg(feature = "testing")]
pub use store::{create_table_for_tests, drop_table_for_tests};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-node-role-pg";
