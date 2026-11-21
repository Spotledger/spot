//! The streaming compare: per-entity counts, each kind of difference, and the first differing
//! record in sort order.

use crate::tests::{derived, stream};
use crate::{
    AuditOutcome, AuditResult, DerivedEntities, DifferenceKind, EntityCount, FirstDifference,
    RecordSummary, compare, compare_all,
};

/// One seeded divergence: the live rows, the rebuilt rows, the record's id, the kind, and both
/// sides' versions.
type Case<'a> = (
    &'a [(&'a str, &'a str, u64, u8)],
    &'a [(&'a str, &'a str, u64, u8)],
    &'a str,
    DifferenceKind,
    Option<u64>,
    Option<u64>,
);

/// The first difference of a result, or a panic naming what was compared instead.
fn first(result: &AuditResult) -> &FirstDifference {
    result
        .outcome
        .first_difference()
        .expect("a mismatch with a first differing record")
}

#[test]
fn identical_streams_match() {
    let live = stream(&[
        ("account", "a1", 3, 1),
        ("account", "a2", 1, 2),
        ("invoice", "i1", 7, 3),
    ]);
    let rebuilt = live.clone();
    let result = compare(live, rebuilt, &DerivedEntities::none()).expect("sorted streams");

    assert_eq!(result.outcome, AuditOutcome::Match);
    assert!(result.is_match());
    assert_eq!(
        (
            result.totals.live,
            result.totals.rebuilt,
            result.totals.differences()
        ),
        (3, 3, 0)
    );
    assert!(result.derived_skipped.is_empty());
    assert_eq!(result.entity("account").expect("counted").live, 2);
    assert_eq!(result.entity("invoice").expect("counted").rebuilt, 1);
    assert!(result.entity("warehouse").is_none());
}

#[test]
fn two_empty_streams_match() {
    let result = compare_all(Vec::new(), Vec::new()).expect("empty streams");
    assert!(result.is_match());
    assert_eq!(result.totals, EntityCount::default());
    assert!(result.per_entity.is_empty());
}

#[test]
fn each_kind_of_divergence_names_its_first_record_exactly() {
    // live, rebuilt, the record's id, the kind, and both sides' versions.
    let cases: [Case<'_>; 4] = [
        (
            &[("account", "a1", 1, 1), ("account", "a2", 2, 2)],
            &[("account", "a1", 1, 1)],
            "a2",
            DifferenceKind::MissingInRebuild,
            Some(2),
            None,
        ),
        (
            &[("account", "a1", 1, 1)],
            &[("account", "a1", 1, 1), ("account", "a2", 3, 2)],
            "a2",
            DifferenceKind::ExtraInRebuild,
            None,
            Some(3),
        ),
        (
            &[("account", "a1", 2, 1)],
            &[("account", "a1", 3, 1)],
            "a1",
            DifferenceKind::VersionDiffers,
            Some(2),
            Some(3),
        ),
        (
            &[("account", "a1", 2, 1)],
            &[("account", "a1", 2, 2)],
            "a1",
            DifferenceKind::ContentDiffers,
            Some(2),
            Some(2),
        ),
    ];
    for (live, rebuilt, id, kind, live_version, rebuilt_version) in cases {
        let result = compare_all(stream(live), stream(rebuilt)).expect("sorted streams");
        assert_eq!(
            result.outcome,
            AuditOutcome::Mismatch {
                first_difference: FirstDifference {
                    entity: "account".to_owned(),
                    id: id.to_owned(),
                    kind,
                    live_version,
                    rebuilt_version,
                },
            },
            "{kind:?}"
        );
    }
}

#[test]
fn the_first_difference_is_the_first_in_sort_order() {
    // By entity: `account` is reported before `invoice`, whatever was seeded first.
    let result = compare_all(
        stream(&[
            ("account", "a9", 1, 1),
            ("invoice", "a1", 2, 2),
            ("invoice", "a2", 1, 1),
        ]),
        stream(&[
            ("account", "a9", 1, 1),
            ("invoice", "a1", 1, 2),
            ("invoice", "a2", 1, 9),
        ]),
    )
    .expect("sorted streams");
    assert_eq!(first(&result).entity, "invoice");
    assert_eq!(first(&result).id, "a1");

    // By id within one entity: a1 before a3.
    let result = compare_all(
        stream(&[
            ("account", "a1", 1, 1),
            ("account", "a2", 1, 2),
            ("account", "a3", 1, 3),
        ]),
        stream(&[
            ("account", "a1", 1, 9),
            ("account", "a2", 1, 2),
            ("account", "a3", 1, 8),
        ]),
    )
    .expect("sorted streams");
    assert_eq!(first(&result).id, "a1");
    assert_eq!(result.totals.content_differs, 2);
}

#[test]
fn counts_are_per_entity() {
    let result = compare_all(
        stream(&[
            ("account", "a1", 1, 1),
            ("account", "a2", 1, 2),
            ("invoice", "i1", 1, 3),
        ]),
        stream(&[("account", "a1", 2, 1), ("warehouse", "w1", 1, 4)]),
    )
    .expect("sorted streams");

    assert_eq!(
        result.entity("account").copied(),
        Some(EntityCount {
            live: 2,
            rebuilt: 1,
            missing_in_rebuild: 1,
            extra_in_rebuild: 0,
            version_differs: 1,
            content_differs: 0,
        })
    );
    let invoice = result.entity("invoice").expect("counted");
    assert_eq!(
        (invoice.live, invoice.rebuilt, invoice.missing_in_rebuild),
        (1, 0, 1)
    );
    let warehouse = result.entity("warehouse").expect("counted");
    assert_eq!(
        (
            warehouse.live,
            warehouse.rebuilt,
            warehouse.extra_in_rebuild
        ),
        (0, 1, 1)
    );

    assert_eq!(result.totals.live, 3);
    assert_eq!(result.totals.rebuilt, 2);
    assert_eq!(result.totals.missing_in_rebuild, 2);
    assert_eq!(result.totals.extra_in_rebuild, 1);
    assert_eq!(result.totals.version_differs, 1);
    assert_eq!(result.totals.differences(), 4);
    assert_eq!(
        result
            .per_entity
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        vec!["account", "invoice", "warehouse"]
    );
}

#[test]
fn derived_entities_are_skipped_and_listed() {
    // The balance table is derived: a rebuild's projector fills it, so its differences are not
    // findings, and the result says it was left out.
    let result = compare(
        stream(&[
            ("account", "a1", 1, 1),
            ("balance", "b1", 5, 2),
            ("balance", "b2", 1, 3),
        ]),
        stream(&[("account", "a1", 1, 1), ("balance", "b1", 9, 4)]),
        &derived(&["balance", "search_index"]),
    )
    .expect("sorted streams");

    assert!(result.is_match());
    assert_eq!(result.derived_skipped, vec!["balance".to_owned()]);
    assert!(result.entity("balance").is_none());
    assert_eq!((result.totals.live, result.totals.rebuilt), (1, 1));
}

#[test]
fn a_stream_that_breaks_its_order_is_refused() {
    let cases: [Vec<RecordSummary>; 3] = [
        // An entity out of order.
        stream(&[("invoice", "i1", 1, 1), ("account", "a1", 1, 1)]),
        // An id out of order.
        stream(&[("account", "a2", 1, 1), ("account", "a1", 1, 1)]),
        // The same record twice.
        stream(&[("account", "a1", 1, 1), ("account", "a1", 1, 1)]),
    ];
    for records in cases {
        let error = compare_all(records, Vec::new()).expect_err("an unsorted stream");
        assert_eq!(error.code(), "journal_audit.stream_not_sorted");
        assert!(error.to_string().contains("account/a1"), "{error}");
        assert!(error.to_string().contains("live"), "{error}");
    }

    // The rebuilt side is named as such.
    let error = compare_all(
        Vec::new(),
        stream(&[("invoice", "i1", 1, 1), ("account", "a1", 1, 1)]),
    )
    .expect_err("an unsorted stream");
    assert!(error.to_string().contains("rebuilt"), "{error}");
}

#[test]
fn a_record_may_repeat_across_entities() {
    let live = stream(&[("account", "a1", 1, 1), ("invoice", "a1", 1, 1)]);
    let result = compare_all(live.clone(), live).expect("sorted streams");
    assert!(result.is_match());
    assert_eq!(result.totals.live, 2);
}
