//! A post-fork journal (generation 2 starts again at position 1, ADR-061 A.0): the audit passes
//! on the pair, and a live side that names the wrong generation fails with the right fact.

use spot_image_upcast::Registry;
use spot_journal_audit::Position;
use spot_journal_manifest::{Manifest, VerifyingKeys, append_segments, sign};
use spot_journal_rebuild::RebuildKeys;
use spot_journal_rebuild_plan::{ForkInfo, SegmentRange};
use spot_journal_store::{JournalStore, Layout, MemoryStore, TenantId};

use super::block_on;
use super::fixture::{Ev, KEY_ID, SOURCE, TENANT, Toy, put, seal_in, signing_key, summary, uuid};
use crate::memory::{MemoryLive, MemoryScratch, SteppingClock};
use crate::{AuditOptions, AuditRun, FailureStage, LiveSide, run_audit};

/// Generation 1 holds six entries (sealed 1..=4 and 5..=6); the fork keeps 1..=4; generation 2
/// holds two entries at positions 1 and 2.
struct Forked {
    store: MemoryStore,
    keys: VerifyingKeys,
    kept: Vec<Ev>,
    second: Vec<Ev>,
}

fn forked() -> Forked {
    let first = vec![
        put(1, 1),
        put(2, 1),
        put(3, 1),
        put(1, 2),
        put(4, 1),
        put(2, 2),
    ];
    let second = vec![put(5, 1), put(3, 2)];
    let sealed_first = seal_in(&first, 4, 1);
    let sealed_second = seal_in(&second, 8, 2);
    let summaries_first: Vec<_> = sealed_first.iter().map(summary).collect();
    let manifest_first =
        append_segments(&Manifest::first(SOURCE, 1_000).unwrap(), &summaries_first).unwrap();
    let manifest_second = manifest_first
        .fork(4, "point-in-time rebuild", 5_000, 6_000)
        .unwrap();
    let summaries_second: Vec<_> = sealed_second.iter().map(summary).collect();
    let manifest_second = append_segments(&manifest_second, &summaries_second).unwrap();

    let store = MemoryStore::new();
    for (generation, manifest) in [(1, &manifest_first), (2, &manifest_second)] {
        let signed = sign(manifest, &signing_key(), KEY_ID);
        block_on(store.put_new(
            &Layout::manifest(TENANT, generation).unwrap().to_key(),
            signed.to_bytes(),
        ))
        .unwrap();
    }
    for segment in sealed_first.iter().chain(sealed_second.iter()) {
        let h = &segment.header;
        let key = Layout::segment(TENANT, h.generation, h.first_seq, h.last_seq)
            .unwrap()
            .to_key();
        block_on(store.put_new(&key, segment.bytes.clone())).unwrap();
    }
    let mut keys = VerifyingKeys::new();
    keys.add_key(KEY_ID, signing_key().verifying_key(), 1, u64::MAX)
        .unwrap();
    Forked {
        store,
        keys,
        kept: first[..4].to_vec(),
        second,
    }
}

fn options() -> AuditOptions<'static> {
    let range = |generation, first_seq, last_seq| SegmentRange {
        generation,
        first_seq,
        last_seq,
    };
    let mut options = AuditOptions::new(
        TenantId::new(TENANT).unwrap(),
        SOURCE,
        1,
        vec![range(1, 1, 4), range(1, 5, 6), range(2, 1, 2)],
        1_000,
    );
    options.forks = vec![ForkInfo {
        generation: 2,
        from_position: 4,
    }];
    options
}

fn record(live: &mut MemoryLive, generation: u64, events: &[Ev]) {
    for (index, event) in events.iter().enumerate() {
        let seq = index as u64 + 1;
        if let Ev::Put {
            entity,
            n,
            version,
            body,
        } = event
        {
            live.put_in(generation, seq, entity, &uuid(*n), *version, body);
        }
    }
}

fn live(journal: &Forked) -> MemoryLive {
    let mut live = MemoryLive::new(TENANT);
    record(&mut live, 1, &journal.kept);
    record(&mut live, 2, &journal.second);
    live.set_head(Position::new(2, 2));
    live
}

fn audit(journal: &Forked, live: &MemoryLive) -> AuditRun {
    let mut scratch = MemoryScratch::new(TENANT);
    let keys = RebuildKeys {
        manifest: &journal.keys,
        snapshot: &Toy,
    };
    block_on(run_audit(
        &journal.store,
        &keys,
        &Registry::new(),
        live,
        &mut scratch,
        &options(),
        &SteppingClock::new(10_000, 5),
    ))
    .unwrap()
}

#[test]
fn the_audit_passes_on_a_post_fork_journal() {
    let journal = forked();
    let run = audit(&journal, &live(&journal));
    assert!(run.is_clean());
    let report = run.report().unwrap();
    assert_eq!(report.as_of, Position::new(2, 2));
    assert_eq!(
        run.verified_position().unwrap().position,
        Position::new(2, 2)
    );
    // Four records: 1 (v2), 2 (v1), 3 (v2) and 5 (v1); the abandoned puts of records 4 and 2 at
    // generation 1 positions 5 and 6 are not in the live history.
    assert_eq!(report.result.totals.live, 4);
    assert_eq!(report.result.totals.rebuilt, 4);
}

#[test]
fn a_live_side_naming_the_wrong_generation_fails_in_the_planner() {
    // The same position number (2), but of the abandoned generation: the live head says a
    // rollback happened that the journal does not record.
    let journal = forked();
    let mut wrong = live(&journal);
    wrong.set_head(Position::new(1, 2));
    let run = audit(&journal, &wrong);
    let AuditRun::Failed(failure) = run else {
        panic!("the audit must not complete");
    };
    assert_eq!(failure.stage, FailureStage::Plan);
    assert_eq!(failure.code, "journal_rebuild_plan.target_before_fork");
    assert_eq!(failure.as_of, Position::new(1, 2));
}

#[test]
fn live_changes_are_ordered_by_generation_then_seq() {
    let journal = forked();
    let live = live(&journal);
    // As of generation 1 position 4 the live side holds records 1 (v2), 2 and 3 only.
    assert_eq!(live.summaries(Position::new(1, 4)).count(), 3);
    // Position 1 of generation 2 is later than every generation 1 position: record 5 joins.
    assert_eq!(live.summaries(Position::new(2, 1)).count(), 4);
    assert_eq!(live.summaries(Position::new(2, 0)).count(), 3);
}
