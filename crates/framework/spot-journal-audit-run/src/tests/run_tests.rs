use serde_json::json;
use spot_image_upcast::Registry;
use spot_journal_audit::{
    Amount, DerivedEntities, DifferenceKind, Fingerprint, Position, RtoVerdict,
};
use spot_journal_rebuild_plan::Base;
use spot_journal_snapshot::{list_snapshots, mark_verified};

use super::block_on;
use super::fixture::{Journal, TENANT, Toy, delete, put, uuid};
use crate::memory::{MemoryLive, MemoryScratch, SteppingClock};
use crate::{AuditRun, AuditRunError, FailureStage, SnapshotMarking, run_audit};

fn script() -> Vec<super::fixture::Ev> {
    vec![
        put(1, 1),
        put(2, 1),
        put(1, 2),
        put(3, 1),
        delete(2, 2),
        put(4, 1),
    ]
}

fn audit(
    journal: &Journal,
    live: &MemoryLive,
    scratch: &mut MemoryScratch,
    options: &crate::AuditOptions<'_>,
) -> Result<AuditRun, AuditRunError> {
    block_on(run_audit(
        &journal.store,
        &journal.keys(),
        &Registry::new(),
        live,
        scratch,
        options,
        &SteppingClock::new(10_000, 5),
    ))
}

fn fingerprint(total: &str) -> Vec<Fingerprint> {
    vec![
        Fingerprint::new(
            "trial_balance",
            [("cash".to_owned(), Amount::decimal(total).unwrap())],
        )
        .unwrap(),
    ]
}

#[test]
fn identical_sides_match_and_return_the_verified_position() {
    let journal = Journal::new(script(), 2);
    let live = journal.live();
    let mut scratch = MemoryScratch::new(TENANT);
    let run = audit(&journal, &live, &mut scratch, &journal.options(1_000)).unwrap();
    let report = run.report().unwrap();
    assert!(run.is_clean());
    assert_eq!(report.base, Base::Empty);
    assert_eq!(report.as_of, Position::new(1, 6));
    assert_eq!(report.result.totals.live, 3);
    assert_eq!(report.result.totals.rebuilt, 3);
    assert_eq!(report.result.entity("account").unwrap().live, 3);
    let verified = run.verified_position().unwrap();
    assert_eq!(verified.position, Position::new(1, 6));
    assert_eq!(verified.at_millis, 10_005);
    assert_eq!(report.rebuild_millis, 5);
    assert_eq!(report.rto, RtoVerdict::Within);
    assert_eq!(scratch.discard_count(), 1);
    assert_eq!(scratch.record_count(), 0);
}

#[test]
fn a_trusted_snapshot_is_the_base() {
    let journal = Journal::new(script(), 2);
    journal.snapshot(4, 1);
    block_on(mark_verified(&journal.store, TENANT, 1, 4, &Toy, &Toy)).unwrap();
    let live = journal.live();
    let mut scratch = MemoryScratch::new(TENANT);
    let run = audit(&journal, &live, &mut scratch, &journal.options(1_000)).unwrap();
    let report = run.report().unwrap();
    assert!(run.is_clean());
    assert_eq!(
        report.base,
        Base::Snapshot {
            position: 4,
            generation: 1
        }
    );
    assert!(!report.snapshot_was_untrusted);
    assert_eq!(report.marking, SnapshotMarking::NotNeeded);
}

#[test]
fn an_altered_record_is_the_first_difference() {
    let journal = Journal::new(script(), 2);
    // Record 3 was inserted at position 4; the live copy now says something else.
    let mut live = journal.live();
    live.put(4, "account", &uuid(3), 1, &json!({"name": "tampered"}));
    live.set_head(Position::new(1, 6));
    let mut scratch = MemoryScratch::new(TENANT);
    let run = audit(&journal, &live, &mut scratch, &journal.options(1_000)).unwrap();
    assert!(!run.is_clean());
    let first = run.first_difference().unwrap();
    assert_eq!(first.entity, "account");
    assert_eq!(first.id, uuid(3));
    assert_eq!(first.kind, DifferenceKind::ContentDiffers);
    assert!(run.verified_position().is_none());
    let counts = run.report().unwrap().result.entity("account").unwrap();
    assert_eq!(counts.content_differs, 1);
    assert_eq!(scratch.discard_count(), 1);
}

#[test]
fn a_record_missing_from_the_rebuild_and_one_extra_are_found() {
    let journal = Journal::new(script(), 2);
    // The live side holds a record the journal never had.
    let mut live = journal.live();
    live.put(3, "account", &uuid(99), 1, &json!({"name": "ghost"}));
    live.set_head(Position::new(1, 6));
    let mut scratch = MemoryScratch::new(TENANT);
    let run = audit(&journal, &live, &mut scratch, &journal.options(1_000)).unwrap();
    let first = run.first_difference().unwrap();
    assert_eq!(first.id, uuid(99));
    assert_eq!(first.kind, DifferenceKind::MissingInRebuild);

    // The live side lacks a record the journal has.
    let mut live = MemoryLive::new(TENANT);
    live.put(1, "account", &uuid(1), 2, &json!({"name": "a1v2"}));
    live.set_head(Position::new(1, 6));
    let mut scratch = MemoryScratch::new(TENANT);
    let run = audit(&journal, &live, &mut scratch, &journal.options(1_000)).unwrap();
    let first = run.first_difference().unwrap();
    assert_eq!(first.kind, DifferenceKind::ExtraInRebuild);
    assert_eq!(first.id, uuid(3));
}

#[test]
fn a_live_side_that_moved_past_the_pinned_position_still_matches() {
    let journal = Journal::new(script(), 2);
    let mut live = journal.live();
    live.set_head(Position::new(1, 6));
    live.put(7, "account", &uuid(50), 1, &json!({"name": "later"}));
    live.remove(8, "account", &uuid(1), 3);
    live.set_head(Position::new(1, 6));
    let mut scratch = MemoryScratch::new(TENANT);
    let run = audit(&journal, &live, &mut scratch, &journal.options(1_000)).unwrap();
    assert!(run.is_clean());
}

#[test]
fn a_fingerprint_mismatch_with_equal_records_is_not_clean() {
    let journal = Journal::new(script(), 2);
    let mut live = journal.live();
    live.set_fingerprints(fingerprint("10.00"));
    let mut scratch = MemoryScratch::new(TENANT);
    scratch.set_fingerprints(fingerprint("10.01"));
    let run = audit(&journal, &live, &mut scratch, &journal.options(1_000)).unwrap();
    let report = run.report().unwrap();
    assert!(report.result.is_match());
    assert!(!report.fingerprints.is_match());
    assert_eq!(report.fingerprints.names(), vec!["trial_balance"]);
    assert!(!run.is_clean());
    assert!(run.verified_position().is_none());
}

#[test]
fn a_rebuild_over_the_target_breaches_the_rto() {
    let journal = Journal::new(script(), 2);
    let live = journal.live();
    let mut scratch = MemoryScratch::new(TENANT);
    let run = block_on(run_audit(
        &journal.store,
        &journal.keys(),
        &Registry::new(),
        &live,
        &mut scratch,
        &journal.options(60),
        &SteppingClock::new(0, 100),
    ))
    .unwrap();
    let report = run.report().unwrap();
    assert_eq!(report.rebuild_millis, 100);
    assert_eq!(report.rto, RtoVerdict::Breached { over_by_millis: 40 });
    assert!(report.is_clean());
}

#[test]
fn derived_entities_are_left_out_of_the_compare() {
    let journal = Journal::new(script(), 2);
    let mut live = journal.live();
    live.put(2, "balance", "cash", 9, &json!({"total": "1.00"}));
    live.set_head(Position::new(1, 6));
    let mut scratch = MemoryScratch::new(TENANT);
    let mut options = journal.options(1_000);
    let run = audit(&journal, &live, &mut scratch, &options).unwrap();
    assert!(!run.is_clean());
    let mut scratch = MemoryScratch::new(TENANT);
    options.derived = DerivedEntities::from_names(["balance"]);
    let run = audit(&journal, &live, &mut scratch, &options).unwrap();
    assert!(run.is_clean());
    assert_eq!(
        run.report().unwrap().result.derived_skipped,
        vec!["balance"]
    );
}

#[test]
fn an_untrusted_snapshot_is_marked_verified_only_after_a_clean_run() {
    let journal = Journal::new(script(), 2);
    journal.snapshot(4, 1);
    let listed = block_on(list_snapshots(&journal.store, TENANT, &Toy)).unwrap();
    assert!(!listed[0].verified);

    // Trusted only: the snapshot is not used, the whole journal is replayed.
    let live = journal.live();
    let mut scratch = MemoryScratch::new(TENANT);
    let run = audit(&journal, &live, &mut scratch, &journal.options(1_000)).unwrap();
    assert_eq!(run.report().unwrap().base, Base::Empty);

    // Untrusted allowed but the run is not clean: no marker.
    let mut broken = journal.live();
    broken.put(6, "account", &uuid(4), 1, &json!({"name": "other"}));
    broken.set_head(Position::new(1, 6));
    let mut options = journal.options(1_000);
    options.allow_untrusted_snapshot = true;
    options.signer = Some(&Toy);
    let mut scratch = MemoryScratch::new(TENANT);
    let run = audit(&journal, &broken, &mut scratch, &options).unwrap();
    let report = run.report().unwrap();
    assert!(!report.is_clean());
    assert!(report.snapshot_was_untrusted);
    assert_eq!(report.marking, SnapshotMarking::NotNeeded);
    let listed = block_on(list_snapshots(&journal.store, TENANT, &Toy)).unwrap();
    assert!(!listed[0].verified);

    // A clean run marks it.
    let mut scratch = MemoryScratch::new(TENANT);
    let run = audit(&journal, &live, &mut scratch, &options).unwrap();
    let report = run.report().unwrap();
    assert!(report.is_clean());
    assert_eq!(
        report.base,
        Base::Snapshot {
            position: 4,
            generation: 1
        }
    );
    assert_eq!(report.marking, SnapshotMarking::Marked);
    let listed = block_on(list_snapshots(&journal.store, TENANT, &Toy)).unwrap();
    assert!(listed[0].verified);
}

#[test]
fn the_untrusted_path_needs_a_signer() {
    let journal = Journal::new(script(), 2);
    let live = journal.live();
    let mut scratch = MemoryScratch::new(TENANT);
    let mut options = journal.options(1_000);
    options.allow_untrusted_snapshot = true;
    let error = audit(&journal, &live, &mut scratch, &options).unwrap_err();
    assert_eq!(error, AuditRunError::SignerMissing);
    assert_eq!(error.code(), "journal_audit_run.signer_missing");
}

#[test]
fn a_side_of_another_tenant_is_refused_before_anything_runs() {
    let journal = Journal::new(script(), 2);
    let live = MemoryLive::new("other");
    let mut scratch = MemoryScratch::new(TENANT);
    let error = audit(&journal, &live, &mut scratch, &journal.options(1_000)).unwrap_err();
    assert_eq!(error, AuditRunError::TenantMismatch { side: "live" });
    assert_eq!(scratch.discard_count(), 0);

    let live = journal.live();
    let mut scratch = MemoryScratch::new("other");
    let error = audit(&journal, &live, &mut scratch, &journal.options(1_000)).unwrap_err();
    assert_eq!(error, AuditRunError::TenantMismatch { side: "scratch" });
    assert_eq!(scratch.discard_count(), 0);
}

#[test]
fn a_gap_in_the_journal_is_a_failed_run_and_the_scratch_is_discarded() {
    let journal = Journal::new(script(), 2);
    let live = journal.live();
    let mut options = journal.options(1_000);
    options.segments.remove(1);
    let mut scratch = MemoryScratch::new(TENANT);
    let run = audit(&journal, &live, &mut scratch, &options).unwrap();
    let AuditRun::Failed(failure) = run else {
        panic!("expected a failed run");
    };
    assert_eq!(failure.stage, FailureStage::Plan);
    assert!(failure.code.starts_with("journal_rebuild_plan."));
    assert_eq!(scratch.discard_count(), 1);
}

#[test]
fn a_missing_upcaster_fails_the_run_and_the_scratch_is_discarded() {
    let journal = Journal::new(script(), 2);
    let live = journal.live();
    let mut options = journal.options(1_000);
    options.target_schema = 2;
    let mut scratch = MemoryScratch::new(TENANT);
    let run = audit(&journal, &live, &mut scratch, &options).unwrap();
    let AuditRun::Failed(failure) = run else {
        panic!("expected a failed run");
    };
    assert_eq!(failure.stage, FailureStage::Rebuild);
    assert_eq!(failure.code, "journal_rebuild.missing_upcast");
    assert_eq!(scratch.discard_count(), 1);
    assert_eq!(scratch.record_count(), 0);
}

#[test]
fn an_unsorted_side_is_an_error_and_the_scratch_is_still_discarded() {
    struct Unsorted(MemoryLive);
    impl crate::LiveSide for Unsorted {
        fn tenant(&self) -> &str {
            TENANT
        }
        fn head(&self) -> Position {
            self.0.head()
        }
        fn summaries(&self, as_of: Position) -> crate::Summaries<'_> {
            let mut rows: Vec<_> = self.0.summaries(as_of).collect();
            rows.reverse();
            Box::new(rows.into_iter())
        }
        fn fingerprints(&self, _as_of: Position) -> Vec<Fingerprint> {
            Vec::new()
        }
    }
    let journal = Journal::new(script(), 2);
    let live = Unsorted(journal.live());
    let mut scratch = MemoryScratch::new(TENANT);
    let error = block_on(run_audit(
        &journal.store,
        &journal.keys(),
        &Registry::new(),
        &live,
        &mut scratch,
        &journal.options(1_000),
        &SteppingClock::new(0, 1),
    ))
    .unwrap_err();
    assert_eq!(error.code(), "journal_audit_run.compare_failed");
    assert_eq!(scratch.discard_count(), 1);
}
