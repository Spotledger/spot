//! `spot-rescue-pg` (WP-MIG-10.T3, ADR-71): the durable [`RescueJournal`](spot_rescue::RescueJournal).
//!
//! `spot-rescue` promises that a rescue killed in the middle resumes: a completed restore is not
//! repeated and a `done` item is not presented to the old engine again. That promise holds only if
//! the journal survives the kill; its in-memory journal does not. [`PgRescueJournal`] keeps it in
//! `spot_rescue_control.spot_rescue_journal`, a schema of its own: `spot-pg-migrate` reads the current schema
//! (`public`) only, so a migration or a restore of a tenant snapshot never touches the journal that
//! records the restore.
//!
//! # Rules
//!
//! - One row per record: `(site, seq)` is the key, `record` is `jsonb`, `at` is unix microseconds
//!   in `bigint`. A record round-trips exactly, including the `serde(default)` fields of `Started`.
//! - Appends are serialised per site by a transaction-scoped advisory lock and are **consecutive**:
//!   `seq` is accepted only as the last stored `seq` plus one (1 for an empty journal). The same
//!   record again at the same `seq` is idempotent success (a retry after a lost acknowledgement); a
//!   different record there, or a gap, is refused with `rescue_pg.journal_sequence` and nothing is
//!   written.
//! - Every statement takes the site as a bound parameter: a site reads and writes its own rows only.
//! - The journal stores run identity, snapshot ids, item keys and outcome codes, never document
//!   content, and an error never echoes a record or a statement.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod codes;
mod error;
mod journal;
mod schema;

pub use deadpool_postgres::Pool;
pub use error::{Error, Result};
pub use journal::PgRescueJournal;
pub use schema::{CONTROL_SCHEMA, PgRescueSchema, TABLES};
