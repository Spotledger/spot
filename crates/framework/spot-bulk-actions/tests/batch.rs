//! Whole-batch behaviour: order, mixed results, stops, cancellation, idempotent re-run,
//! concurrency bound.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::atomic::{AtomicU64, Ordering};

use common::{DOCTYPE, Fake, invocation, rid};
use spot_app_api::document::DocState;
use spot_app_api::error::{AppError, ErrorClass};
use spot_app_api::prelude::Version;
use spot_bulk_actions::{
    BulkAction, BulkOptions, BulkStatus, BulkTarget, CancellationToken, NoProgress, ProgressSink,
    SkipReason, StopReason, apply_bulk,
};

fn targets(range: std::ops::RangeInclusive<u8>) -> Vec<BulkTarget> {
    range
        .map(|n| BulkTarget::current(DOCTYPE, rid(n)))
        .collect()
}

fn code(status: &BulkStatus) -> &str {
    match status {
        BulkStatus::Failed { code, .. } => code,
        other => panic!("not failed: {other:?}"),
    }
}

#[tokio::test]
async fn fifty_drafts_post_in_request_order_even_when_calls_finish_in_reverse() {
    let mut fake = Fake::plain();
    fake.reverse_completion = true;
    fake.add_drafts(1..=50);
    let options = BulkOptions::new("march");
    let outcome = apply_bulk(
        &fake,
        &invocation(),
        &BulkAction::Post,
        targets(1..=50),
        &options,
        &NoProgress,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(outcome.succeeded, 50);
    assert_eq!(outcome.failed + outcome.skipped + outcome.not_run, 0);
    for (i, result) in outcome.results.iter().enumerate() {
        assert_eq!(result.index, i);
        assert_eq!(result.id, rid(i as u8 + 1));
        assert_eq!(
            result.status,
            BulkStatus::Succeeded {
                version: Some(Version(2))
            }
        );
        assert_eq!(fake.version_of(result.id), Some(2));
    }
    assert!(outcome.stopped.is_none());
}

#[tokio::test]
async fn mixed_batch_reports_each_document_and_continues_past_every_failure() {
    let fake = Fake::plain();
    fake.add_drafts(1..=4);
    for n in 5..=7 {
        fake.add(n, Some(DocState::Posted));
    }
    fake.add_drafts(8..=9); // targets with a stale `expected`
    let denied = fake.add(10, Some(DocState::Draft));
    fake.inject(
        denied,
        AppError::new(
            ErrorClass::PermissionDenied,
            "permission.denied",
            "you may not do this",
        ),
    );
    // 11 does not exist.
    let mut list = targets(1..=7);
    for n in 8..=9 {
        list.push(BulkTarget {
            doctype: DOCTYPE.to_owned(),
            id: rid(n),
            expected: Some(Version(7)),
        });
    }
    list.extend(targets(10..=11));
    let options = BulkOptions::new("mixed");
    let outcome = apply_bulk(
        &fake,
        &invocation(),
        &BulkAction::Post,
        list,
        &options,
        &NoProgress,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(outcome.succeeded, 4);
    assert_eq!(outcome.skipped, 3);
    assert_eq!(outcome.failed, 4);
    assert_eq!(outcome.not_run, 0);
    for r in &outcome.results[4..7] {
        assert_eq!(
            r.status,
            BulkStatus::Skipped {
                reason: SkipReason::AlreadyPosted
            }
        );
    }
    assert_eq!(code(&outcome.results[7].status), "document.stale_version");
    assert_eq!(code(&outcome.results[8].status), "document.stale_version");
    assert_eq!(code(&outcome.results[9].status), "permission.denied");
    assert_eq!(code(&outcome.results[10].status), "document.not_found");
}

#[tokio::test]
async fn skip_already_done_off_makes_a_posted_document_a_failure() {
    let fake = Fake::plain();
    fake.add(1, Some(DocState::Posted));
    let mut options = BulkOptions::new("strict");
    options.skip_already_done = false;
    let outcome = apply_bulk(
        &fake,
        &invocation(),
        &BulkAction::Post,
        targets(1..=1),
        &options,
        &NoProgress,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(code(&outcome.results[0].status), "document.invalid_state");
}

fn failing_fake(total: u8) -> Fake {
    let mut fake = Fake::plain();
    fake.yields = 5;
    fake.add_drafts(1..=total);
    for n in 1..=2 {
        fake.inject(
            rid(n),
            AppError::new(ErrorClass::Validation, "document.validation_failed", "no"),
        );
    }
    fake
}

#[tokio::test]
async fn max_failures_stops_scheduling_exactly_with_one_call_in_flight() {
    let fake = failing_fake(20);
    let mut options = BulkOptions::new("stop1");
    options.max_failures = 2;
    options.concurrency = 1;
    let outcome = apply_bulk(
        &fake,
        &invocation(),
        &BulkAction::Post,
        targets(1..=20),
        &options,
        &NoProgress,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(outcome.failed, 2);
    assert_eq!(outcome.not_run, 18);
    assert_eq!(outcome.stopped, Some(StopReason::MaxFailures));
    assert_eq!(fake.writes.load(Ordering::SeqCst), 2);
    assert!(
        outcome.results[2..]
            .iter()
            .all(|r| r.status == BulkStatus::NotRun)
    );
}

#[tokio::test]
async fn max_failures_with_concurrency_starts_at_most_concurrency_minus_one_extra() {
    let fake = failing_fake(30);
    let mut options = BulkOptions::new("stop8");
    options.max_failures = 2;
    options.concurrency = 8;
    let outcome = apply_bulk(
        &fake,
        &invocation(),
        &BulkAction::Post,
        targets(1..=30),
        &options,
        &NoProgress,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    let started = fake.writes.load(Ordering::SeqCst);
    // The two failures finish first, so only the calls already in flight (and the one slot the
    // first failure freed) were started: the bound, not a number.
    assert!(
        started <= 2 + (options.concurrency - 1),
        "started {started}"
    );
    assert_eq!(outcome.stopped, Some(StopReason::MaxFailures));
    assert_eq!(outcome.not_run, 30 - started);
    assert_eq!(outcome.failed, 2);
}

#[tokio::test]
async fn unlimited_failures_never_stop() {
    let fake = failing_fake(10);
    let mut options = BulkOptions::new("unlimited");
    options.max_failures = 0;
    let outcome = apply_bulk(
        &fake,
        &invocation(),
        &BulkAction::Post,
        targets(1..=10),
        &options,
        &NoProgress,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(outcome.failed, 2);
    assert_eq!(outcome.succeeded, 8);
    assert!(outcome.stopped.is_none());
}

/// Cancels once `at` documents have a result.
struct CancelAt {
    at: u64,
    token: CancellationToken,
    last: AtomicU64,
}

impl ProgressSink for CancelAt {
    fn progress(&self, done: u64, _total: u64) {
        self.last.store(done, Ordering::SeqCst);
        if done >= self.at {
            self.token.cancel();
        }
    }
}

#[tokio::test]
async fn cancellation_after_ten_results_lets_in_flight_finish_and_marks_the_rest_not_run() {
    for concurrency in [1usize, 4] {
        let mut fake = Fake::plain();
        fake.yields = 3;
        fake.add_drafts(1..=40);
        let token = CancellationToken::new();
        let sink = CancelAt {
            at: 10,
            token: token.clone(),
            last: AtomicU64::new(0),
        };
        let mut options = BulkOptions::new("cancel");
        options.concurrency = concurrency;
        options.progress_every = 1;
        let outcome = apply_bulk(
            &fake,
            &invocation(),
            &BulkAction::Post,
            targets(1..=40),
            &options,
            &sink,
            &token,
        )
        .await
        .unwrap();
        assert_eq!(outcome.stopped, Some(StopReason::Cancelled));
        assert_eq!(outcome.succeeded + outcome.not_run, 40);
        if concurrency == 1 {
            assert_eq!(outcome.succeeded, 10);
        } else {
            assert!((10..10 + concurrency).contains(&outcome.succeeded));
        }
        // Every document that started was finished and is really posted.
        for r in &outcome.results {
            match r.status {
                BulkStatus::Succeeded { .. } => {
                    assert_eq!(fake.state_of(r.id), Some(DocState::Posted));
                }
                BulkStatus::NotRun => assert_eq!(fake.state_of(r.id), Some(DocState::Draft)),
                ref other => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!(sink.last.load(Ordering::SeqCst), outcome.succeeded as u64);
    }
}

async fn run_post(fake: &Fake, options: &BulkOptions) -> spot_bulk_actions::BulkOutcome {
    let inv = invocation();
    apply_bulk(
        fake,
        &inv,
        &BulkAction::Post,
        targets(1..=5),
        options,
        &NoProgress,
        &CancellationToken::new(),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn re_running_the_same_batch_changes_nothing_twice() {
    let fake = Fake::plain();
    fake.add_drafts(1..=5);
    let options = BulkOptions::new("rerun");
    let first = run_post(&fake, &options).await;
    assert_eq!(first.succeeded, 5);
    let applied = fake.applied.load(std::sync::atomic::Ordering::SeqCst);
    assert_eq!(applied, 5);

    let second = run_post(&fake, &options).await;
    assert_eq!(second.results, first.results);
    assert_eq!(fake.applied.load(std::sync::atomic::Ordering::SeqCst), 5);
    for n in 1..=5 {
        assert_eq!(fake.version_of(rid(n)), Some(2));
    }

    // A different label is a different batch: the documents are posted already.
    let third = run_post(&fake, &BulkOptions::new("another")).await;
    assert_eq!(third.skipped, 5);
    assert_eq!(fake.applied.load(std::sync::atomic::Ordering::SeqCst), 5);
}

#[tokio::test]
async fn every_call_carries_a_derived_command_id_not_the_callers() {
    let fake = Fake::plain();
    fake.add_drafts(1..=3);
    let mut inv = invocation();
    inv.command_id = Some(spot_app_api::prelude::CommandId::new("callers-key").unwrap());
    apply_bulk(
        &fake,
        &inv,
        &BulkAction::Post,
        targets(1..=3),
        &BulkOptions::new("ids"),
        &NoProgress,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    let ids = fake.command_ids.lock().unwrap().clone();
    assert_eq!(ids.len(), 3);
    let distinct: std::collections::HashSet<_> = ids.iter().collect();
    assert_eq!(distinct.len(), 3);
    assert!(ids.iter().all(|id| id.starts_with("bulk:ids:post:")));
}

#[tokio::test]
async fn calls_in_flight_never_exceed_the_concurrency() {
    for concurrency in [1usize, 3, 16] {
        let mut fake = Fake::plain();
        fake.yields = 4;
        fake.add_drafts(1..=60);
        let mut options = BulkOptions::new("bound");
        options.concurrency = concurrency;
        let outcome = apply_bulk(
            &fake,
            &invocation(),
            &BulkAction::Post,
            targets(1..=60),
            &options,
            &NoProgress,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(outcome.succeeded, 60);
        let max = fake.max_in_flight.load(Ordering::SeqCst);
        assert!(max <= concurrency, "max {max} over {concurrency}");
        if concurrency > 1 {
            assert!(max > 1, "the calls never overlapped");
        }
    }
}

#[tokio::test]
async fn duplicates_collapse_and_the_first_wins() {
    let fake = Fake::plain();
    fake.add_drafts(1..=2);
    let mut list = targets(1..=2);
    list.push(BulkTarget::current(DOCTYPE, rid(1)));
    list.push(BulkTarget::current(DOCTYPE, rid(2)));
    let outcome = apply_bulk(
        &fake,
        &invocation(),
        &BulkAction::Post,
        list,
        &BulkOptions::new("dups"),
        &NoProgress,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(outcome.succeeded, 2);
    assert_eq!(outcome.skipped, 2);
    assert_eq!(
        outcome.results[2].status,
        BulkStatus::Skipped {
            reason: SkipReason::Duplicate
        }
    );
    assert_eq!(fake.writes.load(Ordering::SeqCst), 2);
}
