//! One test per blocker, the healthy verdict, boundaries, ordering and fail-closed reads.

use spot_journal_audit::{
    BlobCrossCheck, DurabilityPolicy, ExclusionReason, JournalTarget, KeyBackupStatus, Position,
    VerifiedPosition,
};

use super::{Fixture, NOW, defaults, tampering};
use crate::{
    AuditFailure, BlockerKind, Cadence, ChainFacts, ChainFinding, ChainState, DAY_MILLIS,
    ExclusionEntry, SnapshotFacts, Thresholds, Verdict, assess, build_report, verdict,
};

fn run(fx: &Fixture) -> Verdict {
    assess(&fx.inputs(), NOW, &defaults()).verdict
}

fn keys(v: &Verdict) -> Vec<&str> {
    v.blockers().iter().map(|b| b.key.as_str()).collect()
}

fn only(fx: &Fixture, key: &str, kind: BlockerKind) {
    let v = run(fx);
    assert_eq!(keys(&v), vec![key], "{v:?}");
    assert_eq!(v.blockers()[0].kind, kind);
    match kind {
        BlockerKind::Definite => assert!(matches!(v, Verdict::NotRecoverable { .. })),
        BlockerKind::Unknown => assert!(matches!(v, Verdict::CannotConfirm { .. })),
    }
}

#[test]
fn a_healthy_tenant_is_recoverable() {
    let v = run(&Fixture::healthy());
    assert_eq!(v, Verdict::Recoverable);
    assert!(v.is_recoverable());
    assert!(v.blockers().is_empty());
}

#[test]
fn no_journal_target_blocks() {
    let mut fx = Fixture::healthy();
    fx.target = None;
    only(
        &fx,
        concat!("recovery", ".blocker.journal_target_missing"),
        BlockerKind::Definite,
    );
    fx.target = Some(JournalTarget {
        target: "  ".to_owned(),
        region: "eu".to_owned(),
    });
    only(
        &fx,
        concat!("recovery", ".blocker.journal_target_missing"),
        BlockerKind::Definite,
    );
}

#[test]
fn an_empty_region_blocks() {
    let mut fx = Fixture::healthy();
    fx.target = Some(JournalTarget {
        target: "b/acme".to_owned(),
        region: String::new(),
    });
    only(
        &fx,
        concat!("recovery", ".blocker.region_missing"),
        BlockerKind::Definite,
    );
}

#[test]
fn a_missing_tenant_blocks() {
    let mut fx = Fixture::healthy();
    fx.tenant = " ".to_owned();
    fx.chain = ChainFacts::new(" ", ChainState::Complete, []);
    only(
        &fx,
        concat!("recovery", ".blocker.tenant_missing"),
        BlockerKind::Definite,
    );
}

#[test]
fn a_missing_policy_blocks() {
    let mut fx = Fixture::healthy();
    fx.policy = None;
    only(
        &fx,
        concat!("recovery", ".blocker.durability_policy_missing"),
        BlockerKind::Definite,
    );
}

#[test]
fn local_policy_without_offsite_history_blocks() {
    let mut fx = Fixture::healthy();
    fx.policy = Some(DurabilityPolicy::Local);
    fx.offsite = false;
    only(
        &fx,
        concat!("recovery", ".blocker.local_policy_without_offsite_history"),
        BlockerKind::Definite,
    );
    // The same policy with history on the object store is acceptable.
    fx.offsite = true;
    assert_eq!(run(&fx), Verdict::Recoverable);
}

#[test]
fn no_offsite_history_is_fine_for_a_remote_policy() {
    let mut fx = Fixture::healthy();
    fx.offsite = false;
    assert_eq!(run(&fx), Verdict::Recoverable);
}

#[test]
fn tail_lag_blocks_only_when_a_limit_is_set() {
    let mut fx = Fixture::healthy();
    fx.lag = Some(10_000);
    assert_eq!(run(&fx), Verdict::Recoverable);
    let limits = Thresholds {
        tail_lag_max_millis: Some(5_000),
        ..defaults()
    };
    let v = assess(&fx.inputs(), NOW, &limits).verdict;
    assert_eq!(
        keys(&v),
        vec![concat!("recovery", ".blocker.tail_lag_exceeded")]
    );
    assert_eq!(v.blockers()[0].params["lag_millis"], "10000");
    // At the limit is fine; an unmeasured lag cannot be confirmed.
    fx.lag = Some(5_000);
    assert!(assess(&fx.inputs(), NOW, &limits).verdict.is_recoverable());
    fx.lag = None;
    let v = assess(&fx.inputs(), NOW, &limits).verdict;
    assert_eq!(
        keys(&v),
        vec![concat!("recovery", ".blocker.input_unreadable")]
    );
}

#[test]
fn never_verified_blocks() {
    let mut fx = Fixture::healthy();
    fx.verified = None;
    only(
        &fx,
        concat!("recovery", ".blocker.never_verified"),
        BlockerKind::Definite,
    );
}

#[test]
fn a_stale_verification_blocks_with_its_age() {
    let mut fx = Fixture::healthy();
    fx.verified = Some(VerifiedPosition {
        position: Position::new(1, 1),
        at_millis: NOW - 10 * DAY_MILLIS,
    });
    let v = run(&fx);
    assert_eq!(
        keys(&v),
        vec![concat!("recovery", ".blocker.verification_stale")]
    );
    assert_eq!(v.blockers()[0].params["age_days"], "10");
    assert_eq!(v.blockers()[0].params["limit_days"], "8");
}

#[test]
fn the_verification_limit_depends_on_the_cadence_and_the_boundary_passes() {
    let mut fx = Fixture::healthy();
    let at = |age: i64| {
        Some(VerifiedPosition {
            position: Position::new(1, 1),
            at_millis: NOW - age,
        })
    };
    // Weekly tenants: exactly 8 days is fine, one millisecond more is stale.
    fx.verified = at(8 * DAY_MILLIS);
    assert_eq!(run(&fx), Verdict::Recoverable);
    fx.verified = at(8 * DAY_MILLIS + 1);
    assert_eq!(
        keys(&run(&fx)),
        vec![concat!("recovery", ".blocker.verification_stale")]
    );
    // Daily tenants: exactly 2 days is fine, one millisecond more is stale.
    fx.cadence = Cadence::Daily;
    fx.verified = at(2 * DAY_MILLIS);
    assert_eq!(run(&fx), Verdict::Recoverable);
    fx.verified = at(2 * DAY_MILLIS + 1);
    assert_eq!(
        keys(&run(&fx)),
        vec![concat!("recovery", ".blocker.verification_stale")]
    );
    // Eight days is stale for a daily tenant.
    fx.verified = at(8 * DAY_MILLIS);
    assert_eq!(
        keys(&run(&fx)),
        vec![concat!("recovery", ".blocker.verification_stale")]
    );
}

#[test]
fn a_verification_in_the_future_cannot_be_confirmed() {
    let mut fx = Fixture::healthy();
    fx.verified = Some(VerifiedPosition {
        position: Position::new(1, 1),
        at_millis: NOW + 1,
    });
    only(
        &fx,
        concat!("recovery", ".blocker.clock_skew"),
        BlockerKind::Unknown,
    );
}

#[test]
fn the_last_audit_mismatch_blocks_until_a_later_clean_verification() {
    let mut fx = Fixture::healthy();
    fx.failure = Some(AuditFailure {
        at_millis: NOW - 2 * DAY_MILLIS,
        differing_records: 7,
    });
    // The healthy verification is a day old, after the failure: superseded.
    assert_eq!(run(&fx), Verdict::Recoverable);
    // A failure after the last clean verification stands.
    fx.failure = Some(AuditFailure {
        at_millis: NOW - 3_600_000,
        differing_records: 7,
    });
    let v = run(&fx);
    assert_eq!(
        keys(&v),
        vec![concat!("recovery", ".blocker.last_audit_mismatch")]
    );
    assert_eq!(v.blockers()[0].params["differing_records"], "7");
}

#[test]
fn no_snapshot_blocks() {
    let mut fx = Fixture::healthy();
    fx.snapshots = SnapshotFacts::from_infos(&[]);
    only(
        &fx,
        concat!("recovery", ".blocker.no_snapshot"),
        BlockerKind::Definite,
    );
}

#[test]
fn an_old_snapshot_blocks_and_its_boundary_passes() {
    let mut fx = Fixture::healthy();
    let oldest = |age: i64| {
        Some(spot_journal_audit::OldestSnapshot {
            position: Position::new(1, 1),
            taken_at_millis: NOW - age,
        })
    };
    fx.snapshots.oldest = oldest(35 * DAY_MILLIS);
    assert_eq!(run(&fx), Verdict::Recoverable);
    fx.snapshots.oldest = oldest(35 * DAY_MILLIS + 1);
    let v = run(&fx);
    assert_eq!(
        keys(&v),
        vec![concat!("recovery", ".blocker.snapshot_too_old")]
    );
    assert_eq!(v.blockers()[0].params["age_days"], "35");
}

#[test]
fn a_missing_journal_key_blocks() {
    let mut fx = Fixture::healthy();
    fx.key_present = false;
    only(
        &fx,
        concat!("recovery", ".blocker.journal_key_missing"),
        BlockerKind::Definite,
    );
}

#[test]
fn key_backup_statuses_block() {
    let mut fx = Fixture::healthy();
    fx.key_backup = KeyBackupStatus::Never;
    only(
        &fx,
        concat!("recovery", ".blocker.key_backup_missing"),
        BlockerKind::Definite,
    );
    fx.key_backup = KeyBackupStatus::Failed;
    only(
        &fx,
        concat!("recovery", ".blocker.key_backup_failed"),
        BlockerKind::Definite,
    );
    fx.key_backup = KeyBackupStatus::Unverified;
    only(
        &fx,
        concat!("recovery", ".blocker.key_backup_unverified"),
        BlockerKind::Unknown,
    );
}

#[test]
fn a_stale_key_backup_blocks_and_its_boundary_passes() {
    let mut fx = Fixture::healthy();
    fx.key_backup = KeyBackupStatus::Confirmed {
        verified_at_millis: NOW - 90 * DAY_MILLIS,
    };
    assert_eq!(run(&fx), Verdict::Recoverable);
    fx.key_backup = KeyBackupStatus::Confirmed {
        verified_at_millis: NOW - 100 * DAY_MILLIS,
    };
    let v = run(&fx);
    assert_eq!(
        keys(&v),
        vec![concat!("recovery", ".blocker.key_backup_stale")]
    );
    assert_eq!(v.blockers()[0].params["age_days"], "100");
    assert_eq!(v.blockers()[0].params["limit_days"], "90");
}

#[test]
fn missing_blobs_block_and_a_blob_check_that_did_not_run_cannot_confirm() {
    let mut fx = Fixture::healthy();
    fx.blobs = Some(BlobCrossCheck::new(12, ["h2", "h1", "h2"]));
    let v = run(&fx);
    assert_eq!(
        keys(&v),
        vec![concat!("recovery", ".blocker.blobs_missing")]
    );
    assert_eq!(v.blockers()[0].params["count"], "2");
    fx.blobs = None;
    only(
        &fx,
        concat!("recovery", ".blocker.blob_cross_check_not_run"),
        BlockerKind::Unknown,
    );
}

#[test]
fn an_exclusion_without_owner_or_reason_blocks() {
    let mut fx = Fixture::healthy();
    fx.exclusions
        .push(ExclusionEntry::new("orphan", "", ExclusionReason::Derived));
    let v = run(&fx);
    assert_eq!(
        keys(&v),
        vec![concat!(
            "recovery",
            ".blocker.excluded_without_owner_reason"
        )]
    );
    assert_eq!(v.blockers()[0].params["entity"], "orphan");

    let mut fx = Fixture::healthy();
    fx.exclusions.push(ExclusionEntry {
        entity: "mute".to_owned(),
        owner: "platform".to_owned(),
        reason: None,
    });
    only(
        &fx,
        concat!("recovery", ".blocker.excluded_without_owner_reason"),
        BlockerKind::Definite,
    );

    let mut fx = Fixture::healthy();
    fx.exclusions.push(ExclusionEntry::new(
        "",
        "platform",
        ExclusionReason::Derived,
    ));
    only(
        &fx,
        concat!("recovery", ".blocker.excluded_entity_nameless"),
        BlockerKind::Definite,
    );
}

#[test]
fn tampering_findings_block_and_benign_ones_do_not() {
    let mut fx = Fixture::healthy();
    fx.chain = ChainFacts::new(
        "acme",
        ChainState::Complete,
        [tampering("journal_verify.hash_mismatch", 10, 20)],
    );
    let v = run(&fx);
    assert_eq!(
        keys(&v),
        vec![concat!("recovery", ".blocker.chain_tampering")]
    );
    assert_eq!(
        v.blockers()[0].params["code"],
        "journal_verify.hash_mismatch"
    );
    assert_eq!(v.blockers()[0].params["first_seq"], "10");

    // An unlisted segment is the ordinary crash between sealing and the manifest update.
    fx.chain = ChainFacts::new(
        "acme",
        ChainState::Complete,
        [ChainFinding {
            code: "journal_verify.unlisted_segment".to_owned(),
            generation: 1,
            first_seq: 30,
            last_seq: 40,
            tampering: false,
        }],
    );
    assert_eq!(run(&fx), Verdict::Recoverable);
}

#[test]
fn a_partial_chain_check_cannot_confirm_and_a_foreign_one_is_unreadable() {
    let mut fx = Fixture::healthy();
    fx.chain = ChainFacts::new("acme", ChainState::Partial, []);
    only(
        &fx,
        concat!("recovery", ".blocker.chain_check_incomplete"),
        BlockerKind::Unknown,
    );
    fx.chain = ChainFacts::new("other-tenant", ChainState::Complete, []);
    let v = run(&fx);
    assert_eq!(
        keys(&v),
        vec![concat!("recovery", ".blocker.input_unreadable")]
    );
    assert_eq!(v.blockers()[0].params["input"], "chain");
}

#[test]
fn an_invalid_threshold_cannot_confirm() {
    let fx = Fixture::healthy();
    let bad = Thresholds {
        snapshot_max_age_millis: 0,
        ..defaults()
    };
    let v = assess(&fx.inputs(), NOW, &bad).verdict;
    assert_eq!(
        keys(&v),
        vec![concat!("recovery", ".blocker.threshold_invalid")]
    );
}

#[test]
fn every_unreadable_input_cannot_confirm_never_recoverable() {
    for input in [
        "journal_target",
        "durability_policy",
        "audit_cadence",
        "journal_key",
        "key_backup",
        "exclusions",
        "last_verified",
        "last_failure",
    ] {
        let mut fx = Fixture::healthy();
        fx.broken = vec![input];
        let v = run(&fx);
        assert!(!v.is_recoverable(), "{input}");
        assert!(matches!(v, Verdict::CannotConfirm { .. }), "{input}: {v:?}");
        assert_eq!(
            keys(&v),
            vec![concat!("recovery", ".blocker.input_unreadable")],
            "{input}"
        );
        assert_eq!(v.blockers()[0].params["input"], input);
    }
    // The off-site history only matters under the local policy.
    let mut fx = Fixture::healthy();
    fx.broken = vec!["offsite_history"];
    assert!(run(&fx).is_recoverable());
    fx.policy = Some(DurabilityPolicy::Local);
    assert!(matches!(run(&fx), Verdict::CannotConfirm { .. }));
}

#[test]
fn unreadable_snapshots_and_chain_cannot_confirm() {
    let mut fx = Fixture::healthy();
    fx.snapshots = SnapshotFacts::unreadable();
    only(
        &fx,
        concat!("recovery", ".blocker.input_unreadable"),
        BlockerKind::Unknown,
    );
    let mut fx = Fixture::healthy();
    fx.chain = ChainFacts::unreadable("acme");
    only(
        &fx,
        concat!("recovery", ".blocker.input_unreadable"),
        BlockerKind::Unknown,
    );
}

#[test]
fn a_definite_blocker_wins_over_unknown_ones_and_all_are_named_in_order() {
    let mut fx = Fixture::healthy();
    fx.target = None;
    fx.verified = None;
    fx.key_backup = KeyBackupStatus::Unverified;
    fx.blobs = None;
    let v = run(&fx);
    assert!(matches!(v, Verdict::NotRecoverable { .. }));
    assert_eq!(
        keys(&v),
        vec![
            concat!("recovery", ".blocker.journal_target_missing"),
            concat!("recovery", ".blocker.never_verified"),
            concat!("recovery", ".blocker.key_backup_unverified"),
            concat!("recovery", ".blocker.blob_cross_check_not_run"),
        ]
    );
}

#[test]
fn the_verdict_is_the_same_whatever_order_the_inputs_arrive_in() {
    let mut fx = Fixture::healthy();
    fx.exclusions = vec![
        ExclusionEntry::new("zeta", "", ExclusionReason::Derived),
        ExclusionEntry::new("alpha", "", ExclusionReason::Derived),
        ExclusionEntry::new("mid", "platform", ExclusionReason::LocalOnly),
    ];
    fx.chain = ChainFacts::new(
        "acme",
        ChainState::Complete,
        [
            tampering("journal_verify.hash_mismatch", 50, 60),
            tampering("journal_verify.missing_segment", 10, 20),
        ],
    );
    fx.blobs = Some(BlobCrossCheck::new(5, ["b", "a"]));
    let first = assess(&fx.inputs(), NOW, &defaults());

    let mut shuffled = fx.clone();
    shuffled.exclusions.reverse();
    shuffled.chain = ChainFacts::new(
        "acme",
        ChainState::Complete,
        [
            tampering("journal_verify.missing_segment", 10, 20),
            tampering("journal_verify.hash_mismatch", 50, 60),
        ],
    );
    shuffled.blobs = Some(BlobCrossCheck::new(5, ["a", "b", "a"]));
    let second = assess(&shuffled.inputs(), NOW, &defaults());
    assert_eq!(first, second);
    assert_eq!(
        serde_json::to_string(&first).unwrap(),
        serde_json::to_string(&second).unwrap()
    );
}

#[test]
fn the_report_is_a_pure_function_of_its_inputs() {
    let fx = Fixture::healthy();
    assert_eq!(
        build_report(&fx.inputs(), NOW),
        build_report(&fx.inputs(), NOW)
    );
    let report = build_report(&fx.inputs(), NOW);
    assert_eq!(verdict(&report, &defaults()), verdict(&report, &defaults()));
}
