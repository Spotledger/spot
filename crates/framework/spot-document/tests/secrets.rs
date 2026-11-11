//! The secret path of the engine (WP-SEC-03.T3): a credential is written sealed, in the same
//! transaction as its record, and is read or searched only through a `SecretAccess`.
//!
//! Every case runs on the in-memory backend and, when `SPOT_TEST_PG_URL` is set, on PostgreSQL.
//! The suite is split by what it proves: `write`, `exposure`, `open`, `guards`, `lookup` and `rewrap`,
//! over the shared rig in `support`. The stored row is read back through the storage contract,
//! so the suite holds no SQL of its own (ADR-045).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;
mod flaky;

#[path = "secrets/exposure.rs"]
mod exposure;
#[path = "secrets/guards.rs"]
mod guards;
#[path = "secrets/journal.rs"]
mod journal;
#[path = "secrets/lookup.rs"]
mod lookup;
#[path = "secrets/open.rs"]
mod open;
#[path = "secrets/rewrap.rs"]
mod rewrap;
#[path = "secrets/support.rs"]
mod support;
#[path = "secrets/write.rs"]
mod write;
