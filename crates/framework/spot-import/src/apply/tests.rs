// The back half's mode tests: insert, update, upsert, dry run and the idempotent rerun, each
// against the app API's reference host (`StubHost`).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::testkit::*;
use super::*;
use crate::error::KEY_NOT_FOUND;
use spot_app_api::document::DocState;

// ---------------------------------------------------------------------------------------
// Modes
// ---------------------------------------------------------------------------------------

/// Insert creates one draft per parent, with the child rows the file described.
#[test]
fn insert_creates_one_draft_per_parent() {
    let host = host_with_link_target();
    let items = vec![
        invoice_item(2, "SI-1", "Paid"),
        invoice_item(3, "SI-2", "Open"),
    ];
    let mut progress = Recorder::default();
    let outcome = block_on(apply_import(
        &host,
        &invocation(),
        items.into_iter(),
        &options(ApplyMode::Insert),
        &mut progress,
    ));
    let summary = outcome.summary();
    assert!(matches!(outcome, ApplyOutcome::Applied(_)), "{outcome:?}");
    assert_eq!(summary.created, 2);
    assert_eq!(summary.failed, 0);
    assert_eq!(summary.created_drafts.len(), 2, "both ids are kept");
    // One engine call per document: the drafts exist, in draft state, with their item row.
    for draft in &summary.created_drafts {
        let stored = host
            .stored("sales_invoice", draft.id)
            .expect("the draft is stored");
        assert_eq!(draft.doctype, "sales_invoice");
        assert_eq!(draft.version, Version::FIRST);
        assert_eq!(stored.state(), Some(DocState::Draft));
        assert_eq!(stored.rows("items").len(), 1, "the child row came along");
        assert!(
            matches!(stored.get("status"), Value::Text(text) if text == "Paid" || text == "Open"),
            "the file's status came along: {:?}",
            stored.get("status")
        );
    }
    assert!(!summary.created_drafts[0].posted);
    assert_eq!(
        progress.calls.last(),
        Some(&(2, None)),
        "the end is reported"
    );
}

/// Update by key loads the named document, patches it against its version, and reports a key
/// that names nothing instead of guessing.
#[test]
fn update_by_key_patches_the_named_document() {
    let host = host();
    let seeded = seed_customer(&host, 41, "Acme", "A");
    let index = MapIndex::default();
    index.add("customer", "name", Value::Text("Acme".into()), seeded.id());

    let mut options = options(ApplyMode::Update {
        key_field: "name".to_owned(),
    });
    options.index = Some(Arc::new(index));
    let mut progress = Recorder::default();
    let outcome = block_on(apply_import(
        &host,
        &invocation(),
        vec![
            customer_item(2, "Acme", Some("B")),
            customer_item(3, "Unknown Co", Some("A")),
        ]
        .into_iter(),
        &options,
        &mut progress,
    ));
    let summary = outcome.summary();
    assert!(matches!(outcome, ApplyOutcome::Applied(_)), "{outcome:?}");
    assert_eq!(summary.updated, 1);
    assert_eq!(summary.failed, 1, "the key that names nothing is a failure");
    let not_found = entry(summary, KEY_NOT_FOUND).expect("the report names the missing key");
    assert_eq!(not_found.row, 3);
    assert_eq!(not_found.column, "name");
    assert_eq!(not_found.cell, "Unknown Co", "the user sees their own text");
    // The stored document changed through the engine: the version moved on.
    let stored = host.stored("customer", seeded.id()).unwrap();
    assert_eq!(stored.get("rating"), &Value::Text("B".into()));
    assert_eq!(stored.version(), Some(Version::FIRST.next()));
}

/// A keyed update that would write the same values skips the engine call entirely.
#[test]
fn update_by_key_skips_an_unchanged_document() {
    let host = host();
    let seeded = seed_customer(&host, 42, "Acme", "A");
    let index = MapIndex::default();
    index.add("customer", "name", Value::Text("Acme".into()), seeded.id());
    let mut options = options(ApplyMode::Update {
        key_field: "name".to_owned(),
    });
    options.index = Some(Arc::new(index));
    let mut progress = Recorder::default();
    let outcome = block_on(apply_import(
        &host,
        &invocation(),
        vec![customer_item(2, "Acme", Some("A"))].into_iter(),
        &options,
        &mut progress,
    ));
    let summary = outcome.summary();
    assert_eq!(summary.updated, 0);
    assert_eq!(summary.skipped, 1, "nothing to change");
    assert_eq!(
        host.stored("customer", seeded.id()).unwrap().version(),
        Some(Version::FIRST),
        "no write, no version bump"
    );
}

/// Upsert patches a known key and creates an unknown one, in the same run.
#[test]
fn upsert_updates_or_creates() {
    let host = host();
    let seeded = seed_customer(&host, 43, "Acme", "A");
    let index = MapIndex::default();
    index.add("customer", "name", Value::Text("Acme".into()), seeded.id());
    let mut options = options(ApplyMode::Upsert {
        key_field: "name".to_owned(),
    });
    options.index = Some(Arc::new(index));
    let mut progress = Recorder::default();
    let outcome = block_on(apply_import(
        &host,
        &invocation(),
        vec![
            customer_item(2, "Acme", Some("C")),
            customer_item(3, "Beta", Some("B")),
        ]
        .into_iter(),
        &options,
        &mut progress,
    ));
    let summary = outcome.summary();
    assert_eq!(summary.updated, 1, "{summary:?}");
    assert_eq!(summary.created, 1, "{summary:?}");
    assert_eq!(
        host.stored("customer", seeded.id()).unwrap().get("rating"),
        &Value::Text("C".into())
    );
    assert_eq!(summary.created_drafts.len(), 1);
}

/// A dry run makes no writes at all — not even for the parents it would create — and reports
/// what each row would do, including the ones whose values already match.
#[test]
fn dry_run_changes_nothing() {
    let host = host();
    let seeded = seed_customer(&host, 44, "Acme", "A");
    let index = MapIndex::default();
    index.add("customer", "name", Value::Text("Acme".into()), seeded.id());

    // An insert dry run: three would be created, nothing is.
    let mut progress = Recorder::default();
    let outcome = block_on(apply_import(
        &host,
        &invocation(),
        vec![
            customer_item(2, "One", Some("A")),
            customer_item(3, "Two", Some("B")),
            customer_item(4, "Three", Some("C")),
        ]
        .into_iter(),
        &options(ApplyMode::DryRun { key_field: None }),
        &mut progress,
    ));
    let summary = outcome.summary();
    assert!(summary.dry_run);
    assert_eq!(summary.created, 3, "what would be created");
    assert!(summary.created_drafts.is_empty(), "nothing was created");
    assert_eq!(
        host.stored("customer", rid(44)).unwrap().version(),
        Some(Version::FIRST)
    );

    // A keyed dry run: Acme is unchanged (skip), Beta would be created.
    let mut options = options(ApplyMode::DryRun {
        key_field: Some("name".to_owned()),
    });
    options.index = Some(Arc::new(index));
    let mut progress = Recorder::default();
    let outcome = block_on(apply_import(
        &host,
        &invocation(),
        vec![
            customer_item(2, "Acme", Some("A")),
            customer_item(3, "Beta", Some("B")),
        ]
        .into_iter(),
        &options,
        &mut progress,
    ));
    let summary = outcome.summary();
    assert_eq!(summary.skipped, 1, "Acme already matches");
    assert_eq!(summary.created, 1, "Beta would be created");
    assert_eq!(
        host.stored("customer", seeded.id()).unwrap().get("rating"),
        &Value::Text("A".into())
    );
}

// ---------------------------------------------------------------------------------------
// Idempotency, policies, limits
// ---------------------------------------------------------------------------------------

/// Rerunning the same batch creates once: the derived command ids are stable and the engine's
/// dedupe returns the first result.
#[test]
fn idempotent_rerun_creates_once() {
    let host = host();
    let wrapped = Wrapped::deduping(&host);
    let options = options(ApplyMode::Insert);
    let items = || vec![customer_item(2, "Acme", Some("A"))].into_iter();

    let mut progress = Recorder::default();
    let first = block_on(apply_import(
        &wrapped,
        &invocation(),
        items(),
        &options,
        &mut progress,
    ));
    let mut progress = Recorder::default();
    let second = block_on(apply_import(
        &wrapped,
        &invocation(),
        items(),
        &options,
        &mut progress,
    ));

    let (first, second) = (first.into_summary(), second.into_summary());
    assert_eq!(first.created, 1);
    assert_eq!(second.created, 1);
    assert_eq!(
        first.created_drafts[0].id, second.created_drafts[0].id,
        "the rerun got the first run's document"
    );
    assert_eq!(
        first.created_drafts[0].command_id, second.created_drafts[0].command_id,
        "the key is derived from the batch label and the row"
    );
    assert_eq!(
        host.outbox().len(),
        1,
        "one create event in total: one document"
    );
    assert!(
        host.stored("customer", first.created_drafts[0].id)
            .is_some()
    );
}

/// Skip-bad-rows imports the good parents and reports the bad rows with row, column and cell.
#[test]
fn skip_bad_rows_imports_the_good_rows() {
    let host = host();
    let (items, report) = stream_customers(
        "apply_skip_bad.csv",
        b"name,rating\nAcme,A\nBeta,ZZZ\nGamma,B\n",
    );
    // The stream itself reported the bad row; apply must carry it into its own summary.
    assert_eq!(report.count("import.bad_value"), 1);
    let mut progress = Recorder::default();
    let outcome = block_on(apply_import(
        &host,
        &invocation(),
        items.into_iter(),
        &options(ApplyMode::Insert),
        &mut progress,
    ));
    let summary = outcome.summary();
    assert!(matches!(outcome, ApplyOutcome::Applied(_)), "{outcome:?}");
    assert_eq!(summary.created, 2, "Acme and Gamma");
    assert_eq!(summary.failed, 1, "Beta's rating");
    let bad = entry(summary, "import.bad_value").expect("the bad cell is reported");
    assert_eq!(bad.row, 3);
    assert_eq!(bad.column, "rating");
    assert_eq!(bad.cell, "ZZZ");
}
