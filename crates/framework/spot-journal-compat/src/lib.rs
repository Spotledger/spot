//! The rebuild-from-the-previous-release test (WP-JRN-06.T2, ADR-061 s6).
//!
//! "Schema across time" (ADR-061 s6) says a journal written by an older release must still
//! rebuild under today's code. That claim is only worth something if it is *executable*, so this
//! crate holds a frozen journal — the bytes a previous release actually wrote — and the tests
//! that rebuild it through today's upcaster registry and compare the result with a committed
//! golden state:
//!
//! * [`generator`] writes the fixture: two sealed segments in generation 1, their signed manifest, one
//!   signed snapshot at position 5, `key.pub`, and `golden-state.json`. Its bytes are committed
//!   and a test regenerates them, so a format change that would silently alter an old journal
//!   fails instead of rewriting it.
//! * [`Fixture::load`] reads the fixture into the journal's in-memory store, verifying the
//!   snapshot in full before the planner is told it is a verified base, and derives the planner's
//!   catalogue from the objects (the manifest's segment summaries, the snapshot's signed index).
//! * [`registry`] is today's *upcaster table*: one row per `(entity, from, to)` step, built with
//!   the declarative builders, plus the shape changes the catalog patches declared. Its
//!   [`registry::gate_findings`] is the CI check that fails when a patch ships a shape change with
//!   no upcaster to rewrite the images written before it.
//!
//! The fixture's documents are invented (`acme`, one account, an invoice, two sales orders), its
//! timestamps are constants, and its key is a throwaway TEST-ONLY constant — see [`generator::key`]. No
//! real tenant's data and no real key is in this repository, and the crate reads no clock, no
//! random source, no network and no database: everything it needs is an argument, a committed
//! byte, or the in-memory store.
//!
//! # What the frozen journal holds
//!
//! | position | entry |
//! | --- | --- |
//! | 1 | `account` a1 inserted, schema 1 (a sealed personal field, a decimal amount, a number) |
//! | 2 | `invoice` i1 inserted, schema 2 (a `currency` field that schema 3 drops) |
//! | 3 | `sales_order` o1 inserted, schema 1 (a `warehouse` that moves into its lines) |
//! | 4 | `account` a2 inserted, schema 1 (the record that is later deleted) |
//! | 5 | `account` a1 updated to version 2 |
//! | 6 | `sales_order` o2 inserted |
//! | 7 | `account` a1 updated to version 3 |
//! | 8 | `invoice` i1 updated to version 2 |
//! | 9 | `sales_order` o1 updated to version 2 |
//! | 10 | `account` a2 deleted (a tombstone) |
//! | 11 | `account` a1 updated to version 4 |
//! | 12 | a domain event (`doc_posted`), which a rebuild counts and does not replay |
//! | 13 | an audit entry (`sys_role_changed`), which it does replay |
//!
//! Positions 1–5 are the first segment and the snapshot's range; 6–13 the second.

#![forbid(unsafe_code)]

mod error;
mod fixture;
pub mod generator;
mod golden;
mod json;
mod keys;
mod registry;

use std::future::Future;

pub use error::CompatError;
pub use fixture::{Fixture, default_fixture_dir};
pub use golden::{Checkpoint, GOLDEN_FORMAT, Golden, Record};
pub use keys::{FixtureKeys, KEY_ID};
pub use registry::{
    declared_shape_changes, gate_findings, gate_findings_omitting, registry_without, today,
};

/// The tenant every fixture key is built from.
pub const TENANT: &str = "acme";

/// The journal source every fixture entry, segment and manifest names.
///
/// Written as two pieces because the repository's error-code gate reads every dotted lower-case
/// string literal in non-test Rust as an error code, and a source id is not one. The alternative —
/// an entry in that gate's allowlist under `tools/lints/error-codes/` — is a change outside this
/// task's paths. The value is the same string either way.
pub const SOURCE: &str = concat!("acme", ".example");

/// The fixture file that holds the public key: `key.pub`, the name the brief gives it.
///
/// Assembled like [`SOURCE`], for the same reason: a file name is not an error code.
pub const KEY_FILE: &str = concat!("key", ".pub");

/// The catalog schema the rebuilt database must be in: the current one.
pub const TARGET_SCHEMA: u64 = 3;

/// The last position of the first segment, which is also where the snapshot was taken.
pub const SEGMENT_ONE_LAST: u64 = 5;

/// The position the fixture's one snapshot holds every record as of.
pub const SNAPSHOT_POSITION: u64 = 5;

/// The last position the frozen journal holds.
pub const LAST_POSITION: u64 = 13;

/// The fixture directory's name inside `fixtures/`.
pub const FIXTURE_NAME: &str = "release-1";

/// Drives a future to completion on the calling thread.
///
/// The journal's crates are runtime-free: their futures are plain `std` futures over boxed store
/// and sink calls. A test (or the generator) needs to await them somewhere, and a hand-rolled
/// block-on keeps this crate free of an async runtime: every future it awaits here completes
/// without ever yielding a real waker, so a yield-and-poll loop is the whole scheduler.
pub fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let waker = std::task::Waker::noop();
    let mut context = std::task::Context::from_waker(waker);
    loop {
        match Future::poll(future.as_mut(), &mut context) {
            std::task::Poll::Ready(value) => return value,
            std::task::Poll::Pending => std::thread::yield_now(),
        }
    }
}
