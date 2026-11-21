//! The result as plain data: the condition a mismatch raises for the conditions framework
//! (ADR-067 s2), and the stable names a console reads.

use crate::tests::record;
use crate::{
    AuditOutcome, ConditionParams, DifferenceKind, EntityCount, FirstDifference,
    JOURNAL_AUDIT_MISMATCH, compare_all,
};

/// The mismatch the condition tests start from: `account/a2` is missing from the rebuild.
fn mismatch() -> AuditOutcome {
    compare_all(
        vec![record("account", "a1", 1, 1), record("account", "a2", 4, 2)],
        vec![record("account", "a1", 1, 1)],
    )
    .expect("sorted streams")
    .outcome
}

#[test]
fn a_mismatch_raises_the_journal_audit_mismatch_condition() {
    let condition = mismatch()
        .to_condition("tenant/acme")
        .expect("a mismatch raises a condition");

    assert_eq!(condition.id, "JOURNAL_AUDIT_MISMATCH");
    assert_eq!(condition.id, JOURNAL_AUDIT_MISMATCH);
    assert_eq!(condition.subject, "tenant/acme");
    // One parameter, the record's identity: no free text a translation template could not brace.
    assert_eq!(
        condition.params,
        ConditionParams {
            first_differing_record: FirstDifference {
                entity: "account".to_owned(),
                id: "a2".to_owned(),
                kind: DifferenceKind::MissingInRebuild,
                live_version: Some(4),
                rebuilt_version: None,
            },
        }
    );
    assert_eq!(
        condition.params.first_differing_record.record(),
        "account/a2".to_owned()
    );
}

#[test]
fn a_match_raises_no_condition() {
    let live = vec![record("account", "a1", 1, 1)];
    let outcome = compare_all(live.clone(), live)
        .expect("sorted streams")
        .outcome;
    assert_eq!(outcome, AuditOutcome::Match);
    assert_eq!(outcome.to_condition("tenant/acme"), None);
}

#[test]
fn difference_kinds_have_stable_names() {
    assert_eq!(
        DifferenceKind::MissingInRebuild.name(),
        "missing_in_rebuild"
    );
    assert_eq!(DifferenceKind::ExtraInRebuild.name(), "extra_in_rebuild");
    assert_eq!(DifferenceKind::VersionDiffers.name(), "version_differs");
    assert_eq!(DifferenceKind::ContentDiffers.name(), "content_differs");
}

#[test]
fn a_count_knows_its_differences() {
    let mut totals = EntityCount::default();
    totals.add(&EntityCount {
        live: 3,
        rebuilt: 1,
        missing_in_rebuild: 1,
        extra_in_rebuild: 1,
        version_differs: 1,
        content_differs: 0,
    });
    assert_eq!(totals.differences(), 3);
    assert!(!totals.is_match());
    assert!(EntityCount::default().is_match());

    // A side alone does not match, however equal the difference counts are.
    totals.add(&EntityCount {
        live: 0,
        rebuilt: 3,
        ..EntityCount::default()
    });
    assert_eq!((totals.live, totals.rebuilt), (3, 4));
    assert!(!totals.is_match());
}
