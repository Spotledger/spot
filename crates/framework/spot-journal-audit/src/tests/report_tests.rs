//! The recovery report: every missing input degrades "could I recover?" with its own reason, and a
//! complete set of inputs answers yes.

use crate::{
    BlobCrossCheck, Blocker, DurabilityPolicy, ExcludedEntity, ExclusionReason, KeyBackupStatus,
    Position, RecoveryReportBuilder, Verdict, VerifiedPosition,
};

/// The instant the reports are built at: a fixed argument, never a clock.
const NOW: i64 = 1_800_000_000_000;
/// One hour in milliseconds.
const HOUR: i64 = 60 * 60 * 1000;
/// One day in milliseconds.
const DAY: i64 = 24 * HOUR;
/// The verification window the tenant set: a week.
const WEEK: i64 = 7 * DAY;

/// One field of the report's inputs, so a test can leave exactly that one out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Input {
    /// The tenant.
    Tenant,
    /// The journal target.
    Target,
    /// The journal region.
    Region,
    /// The durability policy.
    Policy,
    /// The last verified position and time.
    Verified,
    /// The verification window.
    Window,
    /// The oldest snapshot.
    Snapshot,
    /// The key backup status.
    KeyBackup,
    /// The blob cross-check.
    Blobs,
    /// Nothing: every input is supplied.
    Nothing,
}

/// A complete set of inputs with one of them left out or spoiled.
fn without(input: Input) -> RecoveryReportBuilder {
    let tenant = if input == Input::Tenant { "" } else { "acme" };
    let mut builder = RecoveryReportBuilder::new(tenant);
    builder = match input {
        Input::Target => builder.journal_target("", "eu-central-1"),
        Input::Region => builder.journal_target("s3://acme-journal", ""),
        _ => builder.journal_target("s3://acme-journal", "eu-central-1"),
    };
    if input != Input::Policy {
        builder = builder.durability_policy(DurabilityPolicy::RemoteSync);
    }
    if input != Input::Verified {
        let within = if input == Input::Window { 0 } else { WEEK };
        builder = builder.last_verified(Position::new(2, 1000), NOW - HOUR, within);
    }
    if input != Input::Snapshot {
        builder = builder.oldest_snapshot(Position::new(2, 900), NOW - DAY);
    }
    if input != Input::KeyBackup {
        builder = builder.key_backup(KeyBackupStatus::Confirmed {
            verified_at_millis: NOW - DAY,
        });
    }
    if input != Input::Blobs {
        builder = builder.blob_cross_check(BlobCrossCheck::clean(17));
    }
    builder
}

/// A complete set of inputs.
fn complete() -> RecoveryReportBuilder {
    without(Input::Nothing).exclude("balance", "app/accounting", ExclusionReason::Derived)
}

#[test]
fn a_complete_report_says_the_tenant_could_be_recovered() {
    let report = complete().build(NOW);

    assert_eq!(report.could_recover, Verdict::Yes);
    assert!(report.could_recover());
    assert!(report.blockers().is_empty());
    assert_eq!(report.tenant, "acme");
    assert_eq!(report.journal.target, "s3://acme-journal");
    assert_eq!(report.journal.region, "eu-central-1");
    assert_eq!(report.durability_policy, Some(DurabilityPolicy::RemoteSync));
    assert_eq!(
        report.last_verified,
        Some(VerifiedPosition {
            position: Position::new(2, 1000),
            at_millis: NOW - HOUR,
        })
    );
    assert_eq!(
        report.oldest_snapshot.expect("a snapshot").position,
        Position::new(2, 900)
    );
    assert_eq!(
        report.key_backup,
        KeyBackupStatus::Confirmed {
            verified_at_millis: NOW - DAY
        }
    );
    assert_eq!(
        report.excluded_entities,
        vec![ExcludedEntity::new(
            "balance",
            "app/accounting",
            ExclusionReason::Derived
        )]
    );
    assert_eq!(report.blob_cross_check, Some(BlobCrossCheck::clean(17)));
    assert_eq!(
        complete().build(NOW),
        report,
        "the same inputs, the same report"
    );
}

#[test]
fn an_empty_report_names_every_missing_input() {
    let report = RecoveryReportBuilder::new("").build(NOW);

    assert!(!report.could_recover());
    assert_eq!(
        report.blockers(),
        &[
            Blocker::TenantMissing,
            Blocker::JournalTargetMissing,
            Blocker::RegionMissing,
            Blocker::DurabilityPolicyUnknown,
            Blocker::NeverVerified,
            Blocker::NoSnapshot,
            Blocker::KeyBackupUnverified,
            Blocker::BlobCrossCheckNotRun,
        ]
    );
}

#[test]
fn each_missing_input_degrades_the_verdict_with_its_own_reason() {
    let rows = [
        (Input::Tenant, Blocker::TenantMissing),
        (Input::Target, Blocker::JournalTargetMissing),
        (Input::Region, Blocker::RegionMissing),
        (Input::Policy, Blocker::DurabilityPolicyUnknown),
        (Input::Verified, Blocker::NeverVerified),
        (Input::Window, Blocker::VerificationWindowInvalid),
        (Input::Snapshot, Blocker::NoSnapshot),
        (Input::KeyBackup, Blocker::KeyBackupUnverified),
        (Input::Blobs, Blocker::BlobCrossCheckNotRun),
    ];
    for (input, reason) in &rows {
        let report = without(*input).build(NOW);
        assert!(!report.could_recover(), "{input:?}");
        assert_eq!(
            report.blockers(),
            std::slice::from_ref(reason),
            "the report for a missing {input:?} must name exactly that reason"
        );
    }
}

#[test]
fn a_verification_is_judged_against_its_window() {
    let verify = |at_millis: i64| {
        without(Input::Verified)
            .last_verified(Position::new(2, 1000), at_millis, WEEK)
            .build(NOW)
    };

    // Exactly at the window's edge is still within it; an hour older is not.
    assert!(verify(NOW - WEEK).could_recover());
    let stale = verify(NOW - WEEK - HOUR);
    assert_eq!(
        stale.blockers(),
        &[Blocker::VerificationStale {
            age_millis: WEEK + HOUR,
            limit_millis: WEEK,
        }]
    );

    // A verification stamped after the caller's now cannot be trusted.
    assert_eq!(
        verify(NOW + 1).blockers(),
        &[Blocker::VerifiedAtInTheFuture]
    );
}

#[test]
fn a_key_backup_that_never_ran_or_failed_is_a_blocker() {
    for (status, reason) in [
        (KeyBackupStatus::Never, Blocker::KeyBackupNever),
        (KeyBackupStatus::Failed, Blocker::KeyBackupFailed),
        (KeyBackupStatus::Unverified, Blocker::KeyBackupUnverified),
    ] {
        let report = complete().key_backup(status).build(NOW);
        assert_eq!(report.blockers(), std::slice::from_ref(&reason));
        assert_eq!(report.key_backup, status);
    }
}

#[test]
fn missing_blobs_are_counted_once_each() {
    let report = complete()
        .blob_cross_check(BlobCrossCheck::new(3, ["b2", "b1", "b2"]))
        .build(NOW);

    assert_eq!(
        report.blockers(),
        &[Blocker::BlobsMissing { count: 2 }],
        "a hash reported twice is one missing blob"
    );
    let cross_check = report.blob_cross_check.expect("a cross-check");
    assert_eq!(cross_check.checked, 3);
    assert_eq!(cross_check.missing, vec!["b1".to_owned(), "b2".to_owned()]);
    assert!(!cross_check.is_clean());
}

#[test]
fn exclusions_are_listed_once_each_and_must_name_an_owner() {
    let sorted = complete()
        .exclude("tax_code", "app/tax", ExclusionReason::LocalOnly)
        .exclude("balance", "app/accounting", ExclusionReason::Derived)
        .build(NOW);
    assert!(sorted.could_recover());
    let entities: Vec<&str> = sorted
        .excluded_entities
        .iter()
        .map(|excluded| excluded.entity.as_str())
        .collect();
    assert_eq!(entities, vec!["balance", "tax_code"]);
    // The first exclusion of an entity is kept, so a repeated one cannot change the page.
    assert_eq!(sorted.excluded_entities[0].owner, "app/accounting");

    let without_owner = complete()
        .exclude("tax_code", "  ", ExclusionReason::LocalOnly)
        .build(NOW);
    assert_eq!(
        without_owner.blockers(),
        &[Blocker::ExcludedWithoutOwner {
            entity: "tax_code".to_owned(),
        }]
    );

    let nameless = complete()
        .exclude("", "app/accounting", ExclusionReason::LocalOnly)
        .build(NOW);
    assert_eq!(nameless.blockers(), &[Blocker::ExcludedEntityNameless]);
}
