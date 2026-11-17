//! Scenario 2: a fork at position 400 (generation 2 starts again at 1), 20 more writes, then
//! roll-forward and point-in-time rebuilds, with verification pinned to the newest generation.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;

use serde_json::Value;
use spot_events_contract::envelope::Position;
use spot_journal_e2e::audit::{catalog, pin, stored_manifest};
use spot_journal_e2e::events::{Model, SOURCE, TENANT, state_line};
use spot_journal_e2e::keys::{Signer, manifest_keys, snapshot_keys};
use spot_journal_e2e::rig::relay_config;
use spot_journal_e2e::store::TestStore;
use spot_journal_manifest::Fork;
use spot_journal_rebuild::{Call, RebuildKeys, RebuildOptions, RecordingSink, execute};
use spot_journal_rebuild_plan::{RebuildPlan, Target, plan};
use spot_journal_relay::{Batch, Relay};
use spot_journal_store::TenantId;
use spot_journal_verify::{FindingKind, Mode, verify_pinned};

struct Built {
    store: TestStore,
    /// The database after the 20 writes of generation 2.
    at_end: Model,
    /// The database after the first 10 of them.
    at_ten: Model,
}

fn batch(
    id: u64,
    generation: u64,
    first: u64,
    model: &mut Model,
    first_change: u64,
    count: u64,
) -> Batch {
    let mut entries = Vec::new();
    for k in 0..count {
        let seq = first + k;
        let change = model.next_change(first_change + k);
        entries.push((
            Position { generation, seq },
            state_line(SOURCE, generation, seq, &change),
        ));
        model.apply(&change, seq);
    }
    Batch {
        batch_id: id,
        entries,
    }
}

/// 500 writes in generation 1 (sealed at 400 and at 500), a fork at 400, 20 writes in generation 2.
async fn build() -> (Built, Model) {
    let store = TestStore::default();
    let mut first = Relay::open(
        store.clone(),
        Box::new(Signer),
        manifest_keys(),
        relay_config(TENANT, SOURCE),
        1_000,
    )
    .await
    .unwrap();
    let mut model = Model::default();
    for b in 0..4u64 {
        let batch = batch(b + 1, 1, b * 100 + 1, &mut model, b * 100, 100);
        first.append_batch(batch, 2_000).await.unwrap();
    }
    assert!(first.seal_now(3_000).await.unwrap());
    let at_400 = model.clone();
    let tail = batch(5, 1, 401, &mut model, 400, 100);
    first.append_batch(tail, 4_000).await.unwrap();
    assert!(first.seal_now(5_000).await.unwrap());

    let fork = Fork::new(
        2,
        400,
        "point-in-time rebuild to before a bad import",
        6_000,
    );
    let mut second = Relay::open_fork(
        store.clone(),
        Box::new(Signer),
        manifest_keys(),
        relay_config(TENANT, SOURCE),
        fork,
        7_000,
    )
    .await
    .unwrap();
    assert_eq!(
        second.next_position(),
        Position {
            generation: 2,
            seq: 1
        }
    );
    // The new generation continues from the database as it was at 400.
    let mut m2 = at_400.clone();
    let b1 = batch(1, 2, 1, &mut m2, 400, 10);
    second.append_batch(b1, 8_000).await.unwrap();
    let at_ten = m2.clone();
    let b2 = batch(2, 2, 11, &mut m2, 410, 10);
    second.append_batch(b2, 8_500).await.unwrap();
    assert!(second.seal_now(9_000).await.unwrap());
    (
        Built {
            store,
            at_end: m2,
            at_ten,
        },
        at_400,
    )
}

async fn replay_async(store: &TestStore, planned: &RebuildPlan) -> RecordingSink {
    let manifest = manifest_keys();
    let snapshot = snapshot_keys();
    let keys = RebuildKeys {
        manifest: &manifest,
        snapshot: &snapshot,
    };
    let options = RebuildOptions::new(TenantId::new(TENANT).unwrap(), SOURCE, 1);
    let mut sink = RecordingSink::new();
    execute(
        store,
        &keys,
        &spot_image_upcast::Registry::new(),
        planned,
        &mut sink,
        &options,
    )
    .await
    .unwrap();
    sink
}

fn applied(sink: &RecordingSink) -> Vec<(u64, u64)> {
    sink.calls()
        .iter()
        .filter_map(|call| match call {
            Call::Image(image) => Some((image.position.generation, image.position.seq)),
            _ => None,
        })
        .collect()
}

type State = BTreeMap<(String, String), (u64, Value)>;

fn state_of(sink: &RecordingSink) -> State {
    sink.state()
        .expect("the rebuild committed")
        .iter()
        .map(|(key, r)| {
            (
                key.clone(),
                (r.version, Value::Object(r.image.body().clone())),
            )
        })
        .collect()
}

fn model_state(model: &Model) -> State {
    model
        .0
        .iter()
        .map(|(key, (version, body, _))| (key.clone(), (*version, body.clone())))
        .collect()
}

#[tokio::test]
async fn verification_is_pinned_to_the_newest_generation() {
    let (built, _) = build().await;
    for mode in [Mode::Light, Mode::Deep] {
        let newest = verify_pinned(&built.store, &manifest_keys(), &pin(2), mode)
            .await
            .unwrap();
        assert!(newest.is_clean(), "{mode:?}: {:?}", newest.findings);
        // Pinned to the abandoned generation: intact, but a newer generation exists.
        let old = verify_pinned(&built.store, &manifest_keys(), &pin(1), mode)
            .await
            .unwrap();
        let kinds: Vec<_> = old.findings.iter().map(|f| f.kind).collect();
        assert_eq!(kinds, vec![FindingKind::NewerGenerationPresent], "{mode:?}");
        assert!(!old.has_tampering());
    }
}

#[tokio::test]
async fn roll_forward_applies_generation_one_to_the_fork_then_generation_two() {
    let (built, _) = build().await;
    let first = stored_manifest(&built.store, 1).await;
    let second = stored_manifest(&built.store, 2).await;
    assert_eq!(first.last_seq(), 500);
    assert_eq!(second.last_seq(), 20);
    let planned = plan(&catalog(&[&first, &second], Vec::new()), None, Target::End).unwrap();
    assert_eq!(planned.ends_at, 20);
    assert!(planned.preserves_positions);
    let sink = replay_async(&built.store, &planned).await;
    let want: Vec<(u64, u64)> = (1..=400)
        .map(|seq| (1, seq))
        .chain((1..=20).map(|seq| (2, seq)))
        .collect();
    // Exactly 1..=400 of generation 1 and the 20 new ones, once, in order: nothing of 401..=500.
    assert_eq!(applied(&sink), want);
    assert_eq!(state_of(&sink), model_state(&built.at_end));
}

#[tokio::test]
async fn a_point_in_time_rebuild_forks_a_third_generation() {
    let (built, _) = build().await;
    let first = stored_manifest(&built.store, 1).await;
    let second = stored_manifest(&built.store, 2).await;
    let planned = plan(
        &catalog(&[&first, &second], Vec::new()),
        None,
        Target::At(10),
    )
    .unwrap();
    let fork = planned.fork_point.unwrap();
    assert_eq!((fork.from_position, fork.abandoned_generation), (10, 2));
    assert_eq!(planned.new_generation, Some(3));
    assert!(!planned.preserves_positions);
    let sink = replay_async(&built.store, &planned).await;
    assert_eq!(applied(&sink).len(), 410);
    assert_eq!(applied(&sink).last(), Some(&(2, 10)));
    assert_eq!(state_of(&sink), model_state(&built.at_ten));

    // The manifest of the new generation holds both forks.
    let third = second
        .manifest()
        .fork(fork.from_position, "second rebuild", 10_000, 10_000)
        .unwrap();
    let recorded: Vec<(u64, u64)> = third
        .forks
        .iter()
        .map(|f| (f.generation, f.forked_from_position))
        .collect();
    assert_eq!(recorded, vec![(2, 400), (3, 10)]);
}
