//! The contract suite a durable [`RefusalStore`] must pass (feature `testing`).
//!
//! A hub's refusal marks are money-critical: an entry that was *offered* to the quarantine sink
//! and not yet in the queue has to be offered again after a restart, and an entry that is
//! *held* must not be applied on a redelivery. A store that loses a mark, or that writes a mark
//! without the queue item it describes, breaks both. This module states the contract once, so
//! every durable implementation runs the same proof instead of writing its own weaker one:
//!
//! ```no_run
//! use spot_shared_balance::{MemoryRefusalStore, testing::assert_refusal_store_contract};
//!
//! assert_refusal_store_contract(MemoryRefusalStore::new);
//! ```
//!
//! Run it from the implementation's own test suite, against the real store (a real
//! transaction, not a mock), and from the hub that is built on it.

// This module is a test suite shipped behind the `testing` feature: every `expect` here is an
// assertion about a store's contract, and a failure is the suite telling the implementer what
// broke. It never runs inside the hub.
#![allow(clippy::expect_used)]

use crate::hub::Mark;
use crate::ledger::{EntryId, NodeId};
use crate::quarantine::RejectReason;
use crate::store::{MemoryRefusalStore, RefusalRecord, RefusalStore};

/// A [`RefusalStore`] whose next writes can be made to fail, so the contract can prove that a
/// failed transaction changes nothing.
///
/// A durable implementation does this the way its database fails: dropping the connection, an
/// aborted transaction, a constraint violation. The records given to the failing save must not
/// reach the store.
pub trait FailingRefusalStore: RefusalStore {
    /// Makes the next `calls` saves fail.
    fn fail_next_save(&mut self, calls: u32);
}

/// The reference store can be made to fail, so the suite runs against it as it would against a
/// durable store.
impl FailingRefusalStore for MemoryRefusalStore {
    fn fail_next_save(&mut self, calls: u32) {
        self.fail_next_saves(calls);
    }
}

fn record(node: &str, seq: u64, mark: Mark, reason: RejectReason) -> RefusalRecord {
    RefusalRecord {
        id: EntryId {
            node: NodeId::new(node),
            seq,
        },
        mark,
        reason,
    }
}

/// Proves `make()` produces a store that keeps the [`RefusalStore`] contract.
///
/// `make` is called several times, so every check starts from an empty store; each store it
/// returns must be independent of the others. Panics with what failed and which check.
pub fn assert_refusal_store_contract<S, F>(mut make: F)
where
    S: FailingRefusalStore,
    F: FnMut() -> S,
{
    empty_store_loads_nothing(&mut make);
    a_save_round_trips_every_field(&mut make);
    a_save_is_an_upsert_keyed_by_entry_id(&mut make);
    a_failed_save_changes_nothing(&mut make);
    a_save_after_a_failure_still_lands(&mut make);
}

/// A store with nothing in it loads nothing, and says so without an error.
fn empty_store_loads_nothing<S, F>(make: &mut F)
where
    S: FailingRefusalStore,
    F: FnMut() -> S,
{
    let store = make();
    let loaded = store.load().expect("an empty store loads");
    assert!(loaded.is_empty(), "an empty store holds no records");
}

/// Every field of a record survives a save and a load: the mark, the reason and the id.
///
/// A store that keeps only the id — or only the mark — cannot re-offer an entry with its
/// original reason, and a `Held` mark that came back as `Offered` would re-offer an entry the
/// queue already holds.
fn a_save_round_trips_every_field<S, F>(make: &mut F)
where
    S: FailingRefusalStore,
    F: FnMut() -> S,
{
    let mut store = make();
    let held = record("branch-01", 1, Mark::Held, RejectReason::OverQuota);
    let offered = record("branch-01", 2, Mark::Offered, RejectReason::AfterExpiry);
    store
        .save(&[held.clone(), offered.clone()])
        .expect("the first save lands");
    let loaded = store.load().expect("the store loads");
    assert_eq!(
        loaded,
        vec![held, offered],
        "every field survives, ordered by entry id"
    );
}

/// A second save of the same entry id replaces the record instead of adding one.
///
/// The hub marks an entry offered and later holds it: the store must end with one record whose
/// mark is `Held`, not two records the next reader has to reconcile.
fn a_save_is_an_upsert_keyed_by_entry_id<S, F>(make: &mut F)
where
    S: FailingRefusalStore,
    F: FnMut() -> S,
{
    let mut store = make();
    let mut record_v1 = record("branch-02", 7, Mark::Offered, RejectReason::NoGrant);
    store
        .save(std::slice::from_ref(&record_v1))
        .expect("the first save");
    record_v1.mark = Mark::Held;
    store
        .save(&[record_v1.clone()])
        .expect("the confirming save");
    let loaded = store.load().expect("the store loads");
    assert_eq!(
        loaded,
        vec![record_v1],
        "one record per entry id, carrying the newest state"
    );
}

/// A save that fails writes nothing: a mark and the queue item it describes cannot part.
///
/// This is the transaction rule. A store that wrote some of the records and then failed would
/// leave a held mark with no queue item (a refusal lost) or an offered mark under an item that
/// exists (an entry both applied and queued).
fn a_failed_save_changes_nothing<S, F>(make: &mut F)
where
    S: FailingRefusalStore,
    F: FnMut() -> S,
{
    let mut store = make();
    let before = record("branch-03", 1, Mark::Held, RejectReason::OverQuota);
    store
        .save(std::slice::from_ref(&before))
        .expect("the first save");
    store.fail_next_save(1);
    let failed = store.save(&[
        record("branch-03", 2, Mark::Offered, RejectReason::Insufficient),
        record("branch-03", 3, Mark::Held, RejectReason::NotPermitted),
    ]);
    assert!(failed.is_err(), "the failing save reports the failure");
    let loaded = store.load().expect("the store still loads");
    assert_eq!(
        loaded,
        vec![before],
        "a failed transaction leaves the previous state in place"
    );
}

/// A failure does not leave the store broken: the next save lands.
fn a_save_after_a_failure_still_lands<S, F>(make: &mut F)
where
    S: FailingRefusalStore,
    F: FnMut() -> S,
{
    let mut store = make();
    store.fail_next_save(1);
    let lost = record("branch-04", 1, Mark::Offered, RejectReason::NoGrant);
    assert!(store.save(&[lost]).is_err(), "the first save fails");
    let retried = record("branch-04", 1, Mark::Offered, RejectReason::NoGrant);
    store
        .save(std::slice::from_ref(&retried))
        .expect("the retry lands: a failure is not sticky");
    assert_eq!(store.load().expect("the store loads"), vec![retried]);
}
