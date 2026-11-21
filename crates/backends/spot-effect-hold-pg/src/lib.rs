//! The PostgreSQL [`LedgerStore`](spot_effect_hold::LedgerStore) for the side-effect hold
//! (WP-MIG-08.T2, ADR-22).
//!
//! `spot-effect-hold` (WP-MIG-08.T1) defines the hold, the gate every external dispatcher passes
//! through, and the [`LedgerStore`](spot_effect_hold::LedgerStore) trait, with an in-memory
//! reference ledger. A hold that only lived in memory would lose its effects on the next restart,
//! and a lost effect is either never sent or sent twice. [`PgLedger`] makes the ledger durable:
//!
//! * [`LedgerStore::append_effect`](spot_effect_hold::LedgerStore::append_effect) deduplicates on
//!   `(site, key)` in one insert; a duplicate returns the existing id and changes nothing. A
//!   per-site sequence is allocated inside the same transaction and never reused, so
//!   [`LedgerStore::list_effects`](spot_effect_hold::LedgerStore::list_effects) returns the
//!   original order even while several tasks append at once.
//! * [`LedgerStore::compare_and_set_hold`](spot_effect_hold::LedgerStore::compare_and_set_hold) is
//!   one conditional `UPDATE ... WHERE state = expected`, or an insert for a site that was never
//!   held, so two `begin`s racing on one site cannot both succeed.
//! * [`LedgerStore::checkpoint`](spot_effect_hold::LedgerStore::checkpoint) stores a release's
//!   progress per `(site, migration)`, so a crash mid-release resumes rather than restarts.
//!
//! # Fail closed
//!
//! A database or pool failure is always [`Error::LedgerUnavailable`], the error the hold treats as
//! "stay held". This crate never answers `Open` for a state it could not read and never answers an
//! empty effect list for a read that failed: [`get_hold`](spot_effect_hold::LedgerStore::get_hold)
//! returns `Open` only when the row is *known* to be absent. A stored row that cannot be decoded is
//! an error too, not a default.
//!
//! # The ledger lives outside the restorable business database
//!
//! A migration's rollback restores the business database. These four tables are the ledger's own,
//! named apart from every business table, and nothing in this crate reads or writes a business
//! table; an effect whose source rows disappeared in a restore is still listed until it is
//! discarded. The contract suite's `ledger_survives_business_reset` case proves the observable half
//! of that rule.
//!
//! # Tests and the database
//!
//! The integration tests need a PostgreSQL server and skip cleanly (printing `SKIPPED:`) when
//! `SPOT_TEST_PG_URL` is unset, exactly as the other `*-pg` crates do. In CI the Postgres service
//! sets it and the suite runs. The ledger's contract suite,
//! [`spot_effect_hold::contract::run_all`], runs against this store in `tests/contract.rs`.

#![warn(missing_docs)]

mod convert;
pub mod error;
pub mod schema;
mod sql;
mod store;

pub use error::MIGRATION_FAILED;
pub use store::PgLedger;

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-effect-hold-pg";
