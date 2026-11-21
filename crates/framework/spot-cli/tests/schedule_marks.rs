//! WP-PLT-07.T4: the durable schedule mark. A table keeps the last enqueued firing of each
//! schedule across a restart (the in-process coordinator forgets it), it never goes down, the
//! later of table and coordinator wins, and a crash between the enqueue and the mark replays the
//! slot as a duplicate, never as a second firing. No database: the table is the in-memory one.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use jiff::Timestamp;
use spot_cli::services::jobs::{SlotOutcome, enqueue_slot};
use spot_cli::services::schedule_marks::{
    CoordinatorMarks, MarkTable, MemoryMarkTable, install_table, remove_table,
};
use spot_coordinator::{Coordinator, InMemoryPresence, InProcessCoordinator, NodeId};
use spot_core::{SiteId, SystemClock};
use spot_jobs::InMemoryJobStore;
use spot_jobs::store::JobStore;
use spot_scheduler::{RunSlot, SlotOrigin, SlotRef};

async fn coordinator(name: &str) -> Arc<dyn Coordinator> {
    InProcessCoordinator::start(
        Arc::new(InMemoryPresence::new()),
        NodeId::new(name.to_owned()).unwrap(),
        Arc::new(SystemClock::new()),
    )
    .await
    .unwrap()
}

fn at(text: &str) -> Timestamp {
    text.parse().unwrap()
}

/// Removes the installed table when the test ends, even on a failure.
struct Installed(String);
impl Drop for Installed {
    fn drop(&mut self) {
        remove_table(&self.0);
    }
}

#[tokio::test]
async fn the_table_never_goes_down_and_a_missing_row_is_none() {
    let table = MemoryMarkTable::new();
    assert_eq!(table.read("acme", "nightly").await.unwrap(), None);
    table
        .raise("acme", "nightly", at("2026-11-12T02:00:00Z"))
        .await
        .unwrap();
    table
        .raise("acme", "nightly", at("2026-11-12T01:00:00Z"))
        .await
        .unwrap();
    assert_eq!(
        table.read("acme", "nightly").await.unwrap(),
        Some(at("2026-11-12T02:00:00Z"))
    );
    assert_eq!(
        table.read("acme", "other").await.unwrap(),
        None,
        "keyed by schedule"
    );
    assert_eq!(
        table.read("other", "nightly").await.unwrap(),
        None,
        "keyed by app"
    );
}

#[tokio::test]
async fn a_restart_resumes_from_the_table_and_without_one_forgets() {
    let site = "marks-restart";
    let _guard = Installed(site.to_owned());
    install_table(site, Arc::new(MemoryMarkTable::new()) as Arc<dyn MarkTable>);

    let before = CoordinatorMarks::new(coordinator("a").await, site);
    assert_eq!(before.load("acme", "nightly").await.unwrap(), None);
    before
        .store("acme", "nightly", at("2026-11-12T02:00:00Z"))
        .await
        .unwrap();

    // The restarted process: a fresh coordinator that remembers nothing, the same table.
    let after = CoordinatorMarks::new(coordinator("b").await, site);
    assert_eq!(
        after.load("acme", "nightly").await.unwrap(),
        Some(at("2026-11-12T02:00:00Z")),
        "the mark survives the restart"
    );

    // A site without a table is the old behaviour: the fresh coordinator has no mark.
    let plain = CoordinatorMarks::new(coordinator("c").await, "marks-no-table");
    plain
        .store("acme", "nightly", at("2026-11-12T02:00:00Z"))
        .await
        .unwrap();
    let restarted = CoordinatorMarks::new(coordinator("d").await, "marks-no-table");
    assert_eq!(restarted.load("acme", "nightly").await.unwrap(), None);
}

#[tokio::test]
async fn the_later_of_the_table_and_the_coordinator_wins() {
    let site = "marks-later";
    let _guard = Installed(site.to_owned());
    let table = Arc::new(MemoryMarkTable::new());
    table
        .raise("acme", "nightly", at("2026-11-12T01:00:00Z"))
        .await
        .unwrap();
    install_table(site, table.clone() as Arc<dyn MarkTable>);
    let shared = coordinator("a").await;
    // The coordinator is ahead (another node stored a later mark there only).
    CoordinatorMarks::new(Arc::clone(&shared), "marks-later-plain")
        .store("acme", "nightly", at("2026-11-12T03:00:00Z"))
        .await
        .unwrap();
    let marks = CoordinatorMarks::new(Arc::clone(&shared), site);
    assert_eq!(
        marks.load("acme", "nightly").await.unwrap(),
        Some(at("2026-11-12T01:00:00Z")),
        "other sites' counters are not read"
    );
    marks
        .store("acme", "nightly", at("2026-11-12T00:30:00Z"))
        .await
        .unwrap();
    assert_eq!(
        table.read("acme", "nightly").await.unwrap(),
        Some(at("2026-11-12T01:00:00Z")),
        "a lower mark changes nothing"
    );
}

#[tokio::test]
async fn a_crash_between_the_enqueue_and_the_mark_replays_as_a_duplicate_not_a_second_firing() {
    let site = "marks-crash";
    let _guard = Installed(site.to_owned());
    let table = Arc::new(MemoryMarkTable::new());
    install_table(site, table.clone() as Arc<dyn MarkTable>);
    let store = InMemoryJobStore::new();
    let store: &dyn JobStore = &store;
    let id = SiteId::new(site).unwrap();
    let slot_time = at("2026-11-12T02:00:00Z");
    let slot = RunSlot::new(
        "acme",
        "nightly",
        slot_time,
        SlotOrigin::Due,
        SlotRef::new("acme", "nightly"),
    );

    // The old leader inserted the job and died before it raised the mark.
    assert_eq!(enqueue_slot(store, &id, &slot).await, SlotOutcome::Enqueued);
    let restarted = CoordinatorMarks::new(coordinator("a").await, site);
    assert_eq!(
        restarted.load("acme", "nightly").await.unwrap(),
        None,
        "the mark is behind the job"
    );

    // The new leader plans the slot again under the same key: the store says it is done.
    assert_eq!(
        enqueue_slot(store, &id, &slot).await,
        SlotOutcome::Duplicate
    );
    restarted.store("acme", "nightly", slot_time).await.unwrap();
    assert_eq!(
        table.read("acme", "nightly").await.unwrap(),
        Some(slot_time),
        "the mark moves once the job is known to exist"
    );
}
