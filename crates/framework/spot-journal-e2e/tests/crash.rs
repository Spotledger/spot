//! Scenario 4: crashes between the steps of a commit, of a seal and of a snapshot. After each
//! restart the journal converges to a state `verify` accepts and the scheduled audit confirms.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use spot_journal_commit::{CommitAnswer, CommitRequest};
use spot_journal_e2e::audit::{
    audit, catalog, live_of, pin, stored_manifest, try_write_snapshot, write_snapshot,
};
use spot_journal_e2e::events::{Model, SOURCE, TENANT, domain_line, state_line};
use spot_journal_e2e::keys::{manifest_keys, snapshot_keys, snapshot_signer};
use spot_journal_e2e::rig::Rig;
use spot_journal_e2e::store::{EventLog, TestStore};
use spot_journal_snapshot::{INDEX_NAME, list_snapshots, mark_verified};
use spot_journal_verify::{Mode, verify_pinned};

/// Commits changes `from..from + count` as sequence numbers `from + 1..` and flushes.
async fn commit_state(rig: &Rig, model: &mut Model, from: u64, count: u64) {
    let mut tickets = Vec::new();
    for i in from..from + count {
        let change = model.next_change(i);
        tickets.push(rig.commit(i + 1, state_line(SOURCE, 1, i + 1, &change)));
        model.apply(&change, i + 1);
    }
    rig.flush().await;
    for (offset, ticket) in tickets.into_iter().enumerate() {
        assert_eq!(
            ticket.wait().await,
            CommitAnswer::Acked(from + offset as u64 + 1)
        );
    }
}

/// The state every scenario must reach: the manifest verifies deeply and lists `head`, and the
/// audit of the live side (which holds `model`) agrees with the rebuild.
async fn converged(store: &TestStore, model: &Model, head: u64) {
    let report = verify_pinned(store, &manifest_keys(), &pin(1), Mode::Deep)
        .await
        .unwrap();
    assert!(report.is_clean(), "{:?}", report.findings);
    let manifest = stored_manifest(store, 1).await;
    assert_eq!(manifest.last_seq(), head);
    let ranges = catalog(&[&manifest], Vec::new()).segments;
    let run = audit(store, &live_of(model, 1, head), ranges).await;
    assert!(
        run.is_clean(),
        "{:?}",
        run.report().map(|r| r.result.clone())
    );
}

async fn fresh() -> (TestStore, EventLog, Rig, Model) {
    let log = EventLog::default();
    let store = TestStore::with_log(log.clone());
    let rig = Rig::open(store.clone(), log.clone()).await;
    (store, log, rig, Model::default())
}

#[tokio::test]
async fn a_crash_after_the_chunk_is_durable_but_before_the_local_commit_returns() {
    let (store, log, rig, mut model) = fresh().await;
    commit_state(&rig, &mut model, 0, 20).await;

    // Commit 21 is shipped, then the local commit fails: the commit never happened.
    let ticket = rig
        .driver
        .submit(CommitRequest {
            command_id: "cmd-21".to_owned(),
            seq: 21,
            entry: domain_line(SOURCE, 1, 21),
        })
        .unwrap();
    rig.flush().await;
    rig.driver.local_commit_failed(21).unwrap();
    assert_eq!(ticket.wait().await, CommitAnswer::Withdrawn(21));
    assert_eq!(rig.driver.status().withdrawn_shipped, 1);
    drop(rig);

    // Restart: the relay resumes after the durable chunk and the next commit takes seq 22.
    let rig = Rig::open(store.clone(), log).await;
    assert_eq!(rig.next_seq().await, 22);
    commit_state(&rig, &mut model, 21, 10).await;
    rig.seal().await;
    converged(&store, &model, 31).await;
}

#[tokio::test]
async fn a_withdrawn_state_entry_shows_in_the_audit() {
    // Open decision (no `journal_void` entry yet): a state entry shipped for a commit that never
    // happened is in the journal, so the audit must see the live side differ. The audit does not
    // hide it.
    let (store, _log, rig, mut model) = fresh().await;
    commit_state(&rig, &mut model, 0, 20).await;
    let ghost = model.next_change(20);
    rig.driver
        .submit(CommitRequest {
            command_id: "cmd-21".to_owned(),
            seq: 21,
            entry: state_line(SOURCE, 1, 21, &ghost),
        })
        .unwrap();
    rig.flush().await;
    rig.driver.local_commit_failed(21).unwrap();
    rig.seal().await;
    let manifest = stored_manifest(&store, 1).await;
    let ranges = catalog(&[&manifest], Vec::new()).segments;
    let run = audit(&store, &live_of(&model, 1, 21), ranges).await;
    assert!(!run.is_clean());
    assert!(run.first_difference().is_some());
}

#[tokio::test]
async fn a_crash_after_the_segment_is_sealed_but_before_the_manifest_is_signed() {
    let (store, log, rig, mut model) = fresh().await;
    commit_state(&rig, &mut model, 0, 30).await;

    // The segment object is written, then the manifest replace fails: the process dies.
    store.fail_next_write("/manifest/");
    assert!(rig.relay.0.lock().await.seal_now(9_000).await.is_err());
    drop(rig);

    // Restart: the relay adopts the segment the manifest lacks and carries on from seq 31.
    let rig = Rig::open(store.clone(), log).await;
    assert_eq!(rig.next_seq().await, 31);
    commit_state(&rig, &mut model, 30, 10).await;
    rig.seal().await;
    converged(&store, &model, 40).await;
}

#[tokio::test]
async fn a_crash_in_the_middle_of_a_snapshot() {
    let (store, _log, rig, mut model) = fresh().await;
    commit_state(&rig, &mut model, 0, 30).await;
    rig.seal().await;

    // The parts are written, the signed index is not: the snapshot does not exist yet.
    store.fail_next_write(INDEX_NAME);
    assert!(try_write_snapshot(&store, &model, 1, 30).await.is_err());
    let keys = snapshot_keys();
    assert!(
        list_snapshots(&store, TENANT, &keys)
            .await
            .unwrap()
            .is_empty()
    );

    // The rerun converges: identical parts are accepted and the index is written.
    let info = write_snapshot(&store, &model, 1, 30).await;
    assert_eq!(info.position, 30);
    mark_verified(&store, TENANT, 1, 30, &keys, &snapshot_signer())
        .await
        .unwrap();
    assert_eq!(
        list_snapshots(&store, TENANT, &keys).await.unwrap().len(),
        1
    );
    converged(&store, &model, 30).await;
}
