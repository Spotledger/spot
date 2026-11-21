// The back half's policy tests: skip-bad-rows, all-or-nothing, limits, cancellation, posting and
// rollback, and the 100k-row run lives in `scale_tests`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::testkit::*;
use super::*;
use crate::error::{SPOOL_FULL, TOO_MANY_ERRORS};
use spot_app_api::document::DocState;

#[test]
fn all_or_nothing_writes_nothing_on_a_bad_row() {
    let host = host();
    let (items, _report) = stream_customers(
        "apply-all-or-nothing-csv",
        b"name,rating\nAcme,A\nBeta,ZZZ\n",
    );
    let mut options = options(ApplyMode::Insert);
    options.policy = ImportPolicy::AllOrNothing;
    let mut progress = Recorder::default();
    let outcome = block_on(apply_import(
        &host,
        &invocation(),
        items.into_iter(),
        &options,
        &mut progress,
    ));
    let summary = outcome.summary();
    assert!(
        matches!(outcome, ApplyOutcome::NothingWritten(_)),
        "{outcome:?}"
    );
    assert_eq!(summary.stopped, Some(StopReason::PolicyRefused));
    assert_eq!(summary.created, 0);
    assert_eq!(summary.failed, 1);
    assert_eq!(summary.skipped, 1, "the good parent was not written either");
    assert_eq!(host.outbox().len(), 0, "not one write reached the host");
}

/// All-or-nothing refuses past the spool's cap with a clear code, writing nothing.
#[test]
fn all_or_nothing_refuses_past_the_spool_cap() {
    let host = host();
    let mut options = options(ApplyMode::Insert);
    options.policy = ImportPolicy::AllOrNothing;
    options.spool = Box::new(MemorySpool::new(2));
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
        &options,
        &mut progress,
    ));
    let summary = outcome.summary();
    assert!(
        matches!(outcome, ApplyOutcome::NothingWritten(_)),
        "{outcome:?}"
    );
    assert_eq!(summary.stopped, Some(StopReason::SpoolFull));
    let full = entry(summary, SPOOL_FULL).expect("the spool's cap is named");
    assert_eq!(full.row, 0);
    assert_eq!(
        summary.skipped, 3,
        "all three validated parents stayed unwritten"
    );
    assert_eq!(host.outbox().len(), 0);
}

/// A write that fails half-way stops the run; the outcome names the row, and the rollback
/// deletes exactly the drafts the run created.
#[test]
fn partial_failure_rolls_back_exactly_the_created_drafts() {
    let host = host();
    let wrapped = Wrapped::failing(&host, 2);
    let mut progress = Recorder::default();
    let outcome = block_on(apply_import(
        &wrapped,
        &invocation(),
        vec![
            customer_item(2, "One", Some("A")),
            customer_item(3, "Two", Some("B")),
            customer_item(4, "Three", Some("C")),
        ]
        .into_iter(),
        &options(ApplyMode::Insert),
        &mut progress,
    ));
    let (summary, failed_at) = match &outcome {
        ApplyOutcome::PartiallyApplied { summary, failed_at } => (summary, *failed_at),
        other => panic!("expected a partial application: {other:?}"),
    };
    assert_eq!(summary.created, 1);
    assert_eq!(summary.failed, 1);
    assert_eq!(failed_at, 3, "the second document's source row");
    let created = summary.created_drafts.clone();
    assert_eq!(created.len(), 1);

    let rollback = block_on(rollback_created(&wrapped, &invocation(), &created));
    assert_eq!(rollback.deleted, 1);
    assert!(rollback.failures.is_empty(), "{:?}", rollback.failures);
    assert!(rollback.needs_cancel.is_empty());
    assert!(
        host.stored("customer", created[0].id).is_none(),
        "the draft the run created is gone"
    );
}

/// Cancellation stops between documents: the documents already applied stay, and the outcome
/// says the run was cancelled.
#[test]
fn cancellation_stops_between_documents() {
    let host = host();
    let token = CancelToken::new();
    let mut progress = Recorder {
        cancel_at: Some((2, token.clone())),
        ..Recorder::default()
    };
    let mut options = options(ApplyMode::Insert);
    options.progress_every = 1;
    options.cancel = token;
    let outcome = block_on(apply_import(
        &host,
        &invocation(),
        (2..7).map(|row| customer_item(row, &format!("C{row}"), Some("A"))),
        &options,
        &mut progress,
    ));
    let summary = outcome.summary();
    assert!(summary.cancelled, "{outcome:?}");
    assert_eq!(summary.stopped, Some(StopReason::Cancelled));
    assert_eq!(summary.created, 2, "the two documents before the cancel");
    assert_eq!(summary.created_drafts.len(), 2);
    assert!(
        matches!(outcome, ApplyOutcome::PartiallyApplied { .. }),
        "{outcome:?}"
    );
}

/// Cancelling before the first document writes nothing at all.
#[test]
fn cancellation_before_the_first_write_writes_nothing() {
    let host = host();
    let token = CancelToken::new();
    token.cancel();
    let mut options = options(ApplyMode::Insert);
    options.cancel = token;
    let mut progress = Recorder::default();
    let outcome = block_on(apply_import(
        &host,
        &invocation(),
        vec![customer_item(2, "One", Some("A"))].into_iter(),
        &options,
        &mut progress,
    ));
    let summary = outcome.summary();
    assert!(
        matches!(outcome, ApplyOutcome::NothingWritten(_)),
        "{outcome:?}"
    );
    assert!(summary.cancelled);
    assert_eq!(summary.created, 0);
    assert_eq!(host.outbox().len(), 0);
}

/// `max_errors` stops the run and says why.
#[test]
fn max_errors_stops_the_run() {
    let host = host();
    let mut options = options(ApplyMode::Insert);
    options.max_errors = 2;
    let bad = || {
        (2..7).map(|row| {
            ImportItem::Error(ImportRowError {
                row,
                column: "rating".to_owned(),
                cell: "ZZZ".to_owned(),
                code: "import.bad_value".to_owned(),
                message: "not an option".to_owned(),
            })
        })
    };
    let mut progress = Recorder::default();
    let outcome = block_on(apply_import(
        &host,
        &invocation(),
        bad(),
        &options,
        &mut progress,
    ));
    let summary = outcome.summary();
    assert!(
        matches!(outcome, ApplyOutcome::NothingWritten(_)),
        "{outcome:?}"
    );
    assert_eq!(summary.failed, 2);
    assert_eq!(summary.stopped, Some(StopReason::TooManyErrors));
    let note = entry(summary, TOO_MANY_ERRORS).expect("the limit is named");
    assert_eq!(note.row, 0);
}

/// A whole-file failure (a torn CSV) is the stream's last item; all-or-nothing writes nothing,
/// and a streaming run still reports the file's own code.
#[test]
fn a_fatal_stream_writes_nothing_under_all_or_nothing() {
    let host = host();
    let (items, report) = stream_customers("apply_fatal.csv", b"name,rating\nAcme,A\n\xFF\xFE\n");
    assert_eq!(report.fatal.len(), 1);
    assert!(
        items
            .iter()
            .any(|item| matches!(item, ImportItem::Fatal(_))),
        "the writer must see the fatal: {items:?}"
    );
    let mut options = options(ApplyMode::Insert);
    options.policy = ImportPolicy::AllOrNothing;
    let mut progress = Recorder::default();
    let outcome = block_on(apply_import(
        &host,
        &invocation(),
        items.into_iter(),
        &options,
        &mut progress,
    ));
    let summary = outcome.summary();
    assert!(
        matches!(outcome, ApplyOutcome::NothingWritten(_)),
        "{outcome:?}"
    );
    assert_eq!(summary.stopped, Some(StopReason::StreamFailed));
    assert_eq!(
        summary.created, 0,
        "the parent before the tear is not written"
    );
    assert_eq!(
        summary.errors.first().map(|error| error.code.as_str()),
        Some(crate::error::BAD_ENCODING)
    );
    assert_eq!(host.outbox().len(), 0);
}

// ---------------------------------------------------------------------------------------
// Posting and rollback
// ---------------------------------------------------------------------------------------

/// Posting after import posts each created draft, and the rollback then reports them for a
/// cancel instead of deleting a posted document.
#[test]
fn post_after_import_posts_and_the_rollback_refuses_posted_drafts() {
    let host = host_with_link_target();
    let mut options = options(ApplyMode::Insert);
    options.post_after_import = true;
    let mut progress = Recorder::default();
    let outcome = block_on(apply_import(
        &host,
        &invocation(),
        vec![invoice_item(2, "SI-1", "Paid")].into_iter(),
        &options,
        &mut progress,
    ));
    let summary = outcome.summary();
    assert_eq!(summary.created, 1, "{summary:?}");
    assert!(summary.errors.is_empty(), "{:?}", summary.errors);
    let draft = &summary.created_drafts[0];
    assert!(draft.posted);
    assert_eq!(
        draft.version,
        Version::FIRST.next(),
        "posting bumped the version"
    );
    let stored = host.stored("sales_invoice", draft.id).unwrap();
    assert_eq!(stored.state(), Some(DocState::Posted));

    let rollback = block_on(rollback_created(
        &host,
        &invocation(),
        &summary.created_drafts,
    ));
    assert_eq!(rollback.deleted, 0);
    assert_eq!(
        rollback.needs_cancel.len(),
        1,
        "a posted document needs a cancel"
    );
    assert!(
        host.stored("sales_invoice", draft.id).is_some(),
        "never deleted"
    );
}

/// A failed post leaves the draft created and its failure in the report; the rollback deletes
/// the now-unposted draft.
#[test]
fn a_failed_post_keeps_the_draft_and_reports_the_failure() {
    // `Wrapped` fails its second write: the first is the create, the second the post.
    let host = host_with_link_target();
    let wrapped = Wrapped::failing(&host, 2);
    let mut options = options(ApplyMode::Insert);
    options.post_after_import = true;
    let mut progress = Recorder::default();
    let outcome = block_on(apply_import(
        &wrapped,
        &invocation(),
        vec![invoice_item(2, "SI-1", "Paid")].into_iter(),
        &options,
        &mut progress,
    ));
    let summary = outcome.summary();
    assert_eq!(summary.created, 1);
    assert!(!summary.created_drafts[0].posted);
    assert_eq!(summary.failed, 0, "the document was written");
    assert_eq!(
        summary.errors.first().map(|error| error.code.as_str()),
        Some("server.unavailable"),
        "the failed post is reported"
    );
    let rollback = block_on(rollback_created(
        &wrapped,
        &invocation(),
        &summary.created_drafts,
    ));
    assert_eq!(rollback.deleted, 1);
    assert!(
        host.stored("sales_invoice", summary.created_drafts[0].id)
            .is_none()
    );
}
