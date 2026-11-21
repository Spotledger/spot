// The batched write path: the same results as one engine call per document, in batches.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Mutex;

use super::testkit::*;
use super::*;
use crate::error::TOO_MANY_ERRORS;
use spot_app_api::error::ErrorClass;
use spot_app_api::stub::StubHost;

/// A batch writer over the stub host: one `create` per call, a refusal of the calls whose
/// position in the whole run is listed, and a log of the batch sizes.
struct Batcher<'a> {
    host: &'a StubHost,
    refuse: Vec<usize>,
    sizes: Mutex<Vec<usize>>,
    position: Mutex<usize>,
}

impl<'a> Batcher<'a> {
    fn new(host: &'a StubHost, refuse: &[usize]) -> Self {
        Self {
            host,
            refuse: refuse.to_vec(),
            sizes: Mutex::new(Vec::new()),
            position: Mutex::new(0),
        }
    }
}

impl BatchCreate for Batcher<'_> {
    fn create_many<'a>(
        &'a self,
        calls: Vec<(Invocation, Document)>,
    ) -> spot_app_api::BoxFuture<'a, Vec<Result<Outcome<Document>, AppError>>> {
        Box::pin(async move {
            self.sizes.lock().unwrap().push(calls.len());
            let mut results = Vec::new();
            for (inv, doc) in calls {
                let at = {
                    let mut position = self.position.lock().unwrap();
                    *position += 1;
                    *position
                };
                if self.refuse.contains(&at) {
                    results.push(Err(AppError::new(
                        ErrorClass::Validation,
                        "document.validation_failed",
                        "refused for the test",
                    )));
                } else {
                    results.push(self.host.create(&inv, doc).await);
                }
            }
            results
        })
    }
}

fn batch_options(batcher: Arc<dyn BatchCreate>, rows: usize) -> ApplyOptions {
    let mut options = options(ApplyMode::Insert);
    options.batch = Some(batcher);
    options.batch_rows = rows;
    options
}

fn customers(count: u64) -> Vec<ImportItem> {
    (0..count)
        .map(|n| customer_item(n + 2, &format!("Customer {n}"), Some("A")))
        .collect()
}

/// A batcher the options own, over a host kept alive by the test.
struct Shared(Arc<Batcher<'static>>);

impl BatchCreate for Shared {
    fn create_many<'a>(
        &'a self,
        calls: Vec<(Invocation, Document)>,
    ) -> spot_app_api::BoxFuture<'a, Vec<Result<Outcome<Document>, AppError>>> {
        self.0.create_many(calls)
    }
}

fn leak_host() -> &'static StubHost {
    Box::leak(Box::new(host()))
}

/// Documents go to the host in batches of the configured size, and the summary is the one a
/// run of single calls gives.
#[test]
fn documents_are_created_in_batches_with_the_single_call_summary() {
    let host = leak_host();
    let batcher = Arc::new(Batcher::new(host, &[]));
    let options = batch_options(Arc::new(Shared(batcher.clone())), 100);
    let mut progress = Recorder::default();
    let outcome = block_on(apply_import(
        host,
        &invocation(),
        customers(250).into_iter(),
        &options,
        &mut progress,
    ));
    assert!(matches!(outcome, ApplyOutcome::Applied(_)), "{outcome:?}");
    let summary = outcome.summary();
    assert_eq!((summary.created, summary.failed), (250, 0));
    assert_eq!(summary.created_drafts.len(), 250);
    assert_eq!(*batcher.sizes.lock().unwrap(), vec![100, 100, 50]);
    for draft in &summary.created_drafts {
        assert!(host.stored("customer", draft.id).is_some());
    }
    assert_eq!(progress.calls.last(), Some(&(250, None)));
}

/// Row problems and refused documents come back in file order; the first refused write stops
/// the run, the documents committed after it in the same batch are still counted.
#[test]
fn a_refused_write_stops_the_run_and_the_committed_rest_of_its_batch_is_counted() {
    let host = leak_host();
    // The 3rd document of the run is refused.
    let batcher = Arc::new(Batcher::new(host, &[3]));
    let options = batch_options(Arc::new(Shared(batcher.clone())), 100);
    let mut items = customers(5);
    items.insert(
        1,
        ImportItem::Error(ImportRowError {
            row: 99,
            column: "rating".to_owned(),
            cell: "Z".to_owned(),
            code: "import.cell_too_long".to_owned(),
            message: "bad".to_owned(),
        }),
    );
    let outcome = block_on(apply_import(
        host,
        &invocation(),
        items.into_iter(),
        &options,
        &mut NoProgress,
    ));
    let ApplyOutcome::PartiallyApplied { summary, failed_at } = outcome else {
        panic!("expected a partial run");
    };
    assert_eq!(summary.stopped, Some(StopReason::WriteFailed));
    // Row 4 held the third document (rows 2, 3, 4 ...).
    assert_eq!(failed_at, 4);
    assert_eq!(summary.created, 4, "the four others were committed");
    assert_eq!(summary.failed, 2, "the bad row and the refused document");
    assert_eq!(summary.errors[0].row, 99, "file order is kept");
    assert_eq!(summary.errors[1].row, 4);
    assert_eq!(summary.created_drafts.len(), 4);
}

/// The error limit counts row problems before a batch is written, so a run stops at the same
/// document as the single-call path.
#[test]
fn the_error_limit_stops_between_batches_at_the_same_row() {
    let host = leak_host();
    let batcher = Arc::new(Batcher::new(host, &[]));
    let mut options = batch_options(Arc::new(Shared(batcher.clone())), 100);
    options.max_errors = 2;
    let bad = |row| {
        ImportItem::Error(ImportRowError {
            row,
            column: "rating".to_owned(),
            cell: String::new(),
            code: "import.cell_too_long".to_owned(),
            message: "bad".to_owned(),
        })
    };
    let items = vec![
        customer_item(2, "A", Some("A")),
        bad(3),
        customer_item(4, "B", Some("A")),
        bad(5),
        customer_item(6, "C", Some("A")),
    ];
    let outcome = block_on(apply_import(
        host,
        &invocation(),
        items.into_iter(),
        &options,
        &mut NoProgress,
    ));
    let summary = outcome.summary();
    assert_eq!(summary.stopped, Some(StopReason::TooManyErrors));
    assert_eq!((summary.created, summary.failed), (2, 2));
    assert!(entry(summary, TOO_MANY_ERRORS).is_some());
}

/// An all-or-nothing run writes its spool in batches too, and stops at a refusal.
#[test]
fn all_or_nothing_writes_its_spool_in_batches() {
    let host = leak_host();
    let batcher = Arc::new(Batcher::new(host, &[]));
    let mut options = batch_options(Arc::new(Shared(batcher.clone())), 4);
    options.policy = ImportPolicy::AllOrNothing;
    let outcome = block_on(apply_import(
        host,
        &invocation(),
        customers(10).into_iter(),
        &options,
        &mut NoProgress,
    ));
    assert!(matches!(outcome, ApplyOutcome::Applied(_)), "{outcome:?}");
    assert_eq!(outcome.summary().created, 10);
    assert_eq!(*batcher.sizes.lock().unwrap(), vec![4, 4, 2]);

    let host = leak_host();
    let batcher = Arc::new(Batcher::new(host, &[6]));
    let mut options = batch_options(Arc::new(Shared(batcher.clone())), 4);
    options.policy = ImportPolicy::AllOrNothing;
    let outcome = block_on(apply_import(
        host,
        &invocation(),
        customers(10).into_iter(),
        &options,
        &mut NoProgress,
    ));
    let ApplyOutcome::PartiallyApplied { summary, failed_at } = outcome else {
        panic!("expected a partial run");
    };
    assert_eq!(failed_at, 7);
    assert_eq!(
        summary.created,
        4 + 3,
        "the first batch and the three committed beside the refused document"
    );
    assert_eq!(summary.stopped, Some(StopReason::WriteFailed));
    assert_eq!(summary.skipped, 2, "the last batch was never sent");
}

/// A host that answers the wrong number of results is a failure, never a guess.
#[test]
fn a_host_that_miscounts_its_results_fails_the_batch() {
    struct Short;
    impl BatchCreate for Short {
        fn create_many<'a>(
            &'a self,
            _calls: Vec<(Invocation, Document)>,
        ) -> spot_app_api::BoxFuture<'a, Vec<Result<Outcome<Document>, AppError>>> {
            Box::pin(async { Vec::new() })
        }
    }
    let host = host();
    let options = batch_options(Arc::new(Short), 10);
    let outcome = block_on(apply_import(
        &host,
        &invocation(),
        customers(3).into_iter(),
        &options,
        &mut NoProgress,
    ));
    let summary = outcome.summary();
    assert_eq!(summary.created, 0);
    assert_eq!(summary.stopped, Some(StopReason::WriteFailed));
}
