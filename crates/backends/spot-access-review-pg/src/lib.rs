//! PostgreSQL stores for the access review and segregation-of-duties data (WP-SEC-02.T3,
//! ADR-070 section 8).
//!
//! `spot-access-review` is a pure state machine and `spot-sod` holds rules and overrides as
//! values; this crate persists them behind the ports those crates define:
//!
//! - [`PgCampaignStore`] implements `CampaignStore`: open, load, decide with compare-and-set on
//!   the item version, the deadline tick as one stored-state operation, list, history.
//! - [`PgSodStore`] implements `SodStore`: the versioned rule set (validated on the way in and
//!   on the way out) and expiring, never-edited overrides with append-only revocations.
//!
//! # Tables
//!
//! See [`schema`]. Every row is keyed by the tenant first; the pool the caller passes may be
//! shared between tenants, and no statement is ever built without the tenant in its `WHERE`.
//!
//! # Rules
//!
//! - **Tenant isolation.** Every statement carries the tenant; a campaign, override or history
//!   of another tenant answers exactly like a missing one.
//! - **Append-only evidence.** Triggers refuse UPDATE, DELETE and TRUNCATE on the history, the
//!   rule-set versions, the overrides and the revocations, and refuse any change to a
//!   campaign's spec or an item's snapshot, even for the table owner. The application role gets
//!   no UPDATE or DELETE privilege on the append-only tables.
//! - **One writer at a time per campaign (or tenant).** Mutations lock the campaign row (or
//!   take the tenant's advisory lock) first, then decide on committed state; the item version
//!   in the `UPDATE` is a second, independent guard.
//! - **Fails closed.** A pool or database failure is `Unavailable` with a fixed sentence; no
//!   driver text, SQLSTATE or value reaches an error, a log line or a metric. A stored campaign
//!   or rule set that no longer validates is refused (`Corrupt`), never served.
//! - **Time is an argument.** The stores never read a clock.
//!
//! # Tests and the database
//!
//! The integration tests need a PostgreSQL server and skip cleanly (printing `SKIPPED:`) when
//! `SPOT_TEST_PG_URL` is unset; with `SPOT_REQUIRE_PG=1` a missing server fails, as in CI.

#![warn(missing_docs)]

mod campaign;
mod error;
pub mod schema;
mod sod;

pub use campaign::PgCampaignStore;
pub use error::MIGRATION_FAILED;
pub use sod::PgSodStore;

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-access-review-pg";
