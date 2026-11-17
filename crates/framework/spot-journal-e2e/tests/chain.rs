//! Scenario 1, the happy path: 500 commits through the commit driver under remote-sync, relayed
//! into tail chunks and sealed segments, each acknowledged only after its chunk is durable; a
//! snapshot at position 300; the manifest verifies; a rebuild into a scratch side agrees with the
//! live records through the scheduled audit; the recovery report says the tenant could be
//! recovered. A second tenant on the same store is never touched.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use serde_json::json;
use spot_journal_audit::{
    BlobCrossCheck, DurabilityPolicy, KeyBackupStatus, Position, RecoveryReportBuilder,
};
use spot_journal_commit::CommitAnswer;
use spot_journal_e2e::audit::{audit, catalog, live_of, pin, stored_manifest, write_snapshot};
use spot_journal_e2e::events::{
    Model, OTHER_SOURCE, OTHER_TENANT, SOURCE, TENANT, state_line, uuid,
};
use spot_journal_e2e::keys::{Signer, manifest_keys, snapshot_keys, snapshot_signer};
use spot_journal_e2e::rig::{Rig, relay_config};
use spot_journal_e2e::store::{EventLog, TestStore};
use spot_journal_rebuild_plan::Base;
use spot_journal_relay::{Batch, Relay};
use spot_journal_snapshot::mark_verified;
use spot_journal_verify::{Mode, verify_pinned};

const ROUNDS: u64 = 50;
const PER_ROUND: u64 = 10;

/// A small journal of another tenant in the same store.
async fn seed_other_tenant(store: &TestStore) {
    let mut relay = Relay::open(
        store.clone(),
        Box::new(Signer),
        manifest_keys(),
        relay_config(OTHER_TENANT, OTHER_SOURCE),
        1_000,
    )
    .await
    .unwrap();
    let mut model = Model::default();
    let mut entries = Vec::new();
    for i in 0..30u64 {
        let change = model.next_change(i);
        entries.push((
            spot_events_contract::envelope::Position {
                generation: 1,
                seq: i + 1,
            },
            state_line(OTHER_SOURCE, 1, i + 1, &change),
        ));
        model.apply(&change, i + 1);
    }
    relay
        .append_batch(
            Batch {
                batch_id: 1,
                entries,
            },
            2_000,
        )
        .await
        .unwrap();
    assert!(relay.seal_now(3_000).await.unwrap());
}

fn count(log: &EventLog, prefix: &str) -> usize {
    log.lock()
        .unwrap()
        .iter()
        .filter(|e| e.starts_with(prefix))
        .count()
}

#[tokio::test]
async fn five_hundred_commits_are_acknowledged_after_their_chunk_and_the_journal_recovers() {
    let log: EventLog = EventLog::default();
    let store = TestStore::with_log(log.clone());
    seed_other_tenant(&store).await;
    log.lock().unwrap().clear();
    let other_before = store.dump(&format!("{OTHER_TENANT}/")).await;
    assert!(!other_before.is_empty());
    store.clear_access();

    let rig = Rig::open(store.clone(), log.clone()).await;
    assert_eq!(rig.next_seq().await, 1);
    let mut model = Model::default();
    let mut model_at_300 = None;
    let mut tickets = Vec::new();
    for round in 0..ROUNDS {
        for k in 0..PER_ROUND {
            let i = round * PER_ROUND + k;
            let change = model.next_change(i);
            tickets.push(rig.commit(i + 1, state_line(SOURCE, 1, i + 1, &change)));
            model.apply(&change, i + 1);
        }
        // Nothing of this round is acknowledged before its chunk is durable.
        assert_eq!(count(&log, "ack "), (round * PER_ROUND) as usize);
        rig.flush().await;
        assert_eq!(count(&log, "ack "), ((round + 1) * PER_ROUND) as usize);
        if round + 1 == 30 {
            model_at_300 = Some(model.clone());
            write_snapshot(&store, &model, 1, 300).await;
            mark_verified(&store, TENANT, 1, 300, &snapshot_keys(), &snapshot_signer())
                .await
                .unwrap();
        }
    }
    for (i, ticket) in tickets.into_iter().enumerate() {
        assert_eq!(ticket.wait().await, CommitAnswer::Acked(i as u64 + 1));
    }

    // Order of events: round r's chunk is durable before any of its acknowledgements, and its
    // acknowledgements come before the next chunk.
    let events = log.lock().unwrap().clone();
    let tails: Vec<usize> = (0..events.len())
        .filter(|&at| events[at].starts_with("tail "))
        .collect();
    assert_eq!(tails.len(), ROUNDS as usize, "one chunk per round");
    let ack_at = |seq: u64| {
        events
            .iter()
            .position(|e| *e == format!("ack {seq}"))
            .unwrap()
    };
    for round in 0..ROUNDS {
        let first = round * PER_ROUND + 1;
        let last = first + PER_ROUND - 1;
        assert!(tails[round as usize] < ack_at(first), "round {round}");
        if let Some(next) = tails.get(round as usize + 1) {
            assert!(ack_at(last) < *next, "round {round}");
        }
    }

    // The rest is sealed; the manifest verifies deeply and lists several segments.
    assert!(rig.seal().await);
    let report = verify_pinned(&store, &manifest_keys(), &pin(1), Mode::Deep)
        .await
        .unwrap();
    assert!(report.is_clean(), "{:?}", report.findings);
    let manifest = stored_manifest(&store, 1).await;
    assert_eq!(manifest.last_seq(), 500);
    assert!(manifest.segments().len() >= 2);

    // The scheduled audit rebuilds snapshot(300) plus the journal and agrees with the live side.
    let ranges = catalog(&[&manifest], Vec::new()).segments;
    let live = live_of(&model, 1, 500);
    let run = audit(&store, &live, ranges.clone()).await;
    assert!(
        run.is_clean(),
        "{:?}",
        run.report().map(|r| r.result.clone())
    );
    let report = run.report().unwrap();
    assert_eq!(
        report.base,
        Base::Snapshot {
            position: 300,
            generation: 1
        }
    );
    assert_eq!(report.as_of, Position::new(1, 500));
    assert_eq!(model_at_300.unwrap().0.len(), 15);
    let verified = run.verified_position().unwrap();

    // A tampered live record is found (the comparison is not vacuous).
    let mut bad = live_of(&model, 1, 500);
    bad.put(500, "account", &uuid(1), 1, &json!({"name": "tampered"}));
    bad.set_head(Position::new(1, 500));
    assert!(!audit(&store, &bad, ranges).await.is_clean());

    // The recovery report says the tenant could be recovered.
    let recovery = RecoveryReportBuilder::new(TENANT)
        .journal_target("memory-store", "test-region")
        .durability_policy(DurabilityPolicy::RemoteSync)
        .last_verified(verified.position, verified.at_millis, 86_400_000)
        .oldest_snapshot(Position::new(1, 300), 1_700_000_000_000)
        .key_backup(KeyBackupStatus::Confirmed {
            verified_at_millis: verified.at_millis,
        })
        .blob_cross_check(BlobCrossCheck::clean(0))
        .build(verified.at_millis + 1_000);
    assert!(recovery.could_recover(), "{:?}", recovery.blockers());

    // The other tenant was never read, written or listed, and its objects are unchanged.
    for (operation, key) in store.accessed() {
        assert!(!key.is_empty(), "{operation} of the whole store");
        assert!(
            !key.starts_with(OTHER_TENANT),
            "{operation} touched the other tenant: {key}"
        );
    }
    assert_eq!(store.dump(&format!("{OTHER_TENANT}/")).await, other_before);
}
