//! The scheduler: validation, duplicate collapse, bounded concurrency, stop rules.

use std::collections::HashSet;

use futures::stream::{FuturesUnordered, StreamExt};
use spot_app_api::context::{DocumentService, Invocation};
use spot_app_api::prelude::RecordId;

use crate::command;
use crate::error::{BulkError, codes};
use crate::exec::run_one;
use crate::types::{
    BulkAction, BulkOptions, BulkOutcome, BulkResult, BulkStatus, BulkTarget, CancellationToken,
    MAX_CONCURRENCY, MAX_LABEL_CHARS, ProgressSink, SkipReason, StopReason,
};

/// Checks the whole request. Nothing is called on the service before this passes.
pub fn validate_request(
    action: &BulkAction,
    targets: &[BulkTarget],
    options: &BulkOptions,
) -> Result<(), BulkError> {
    let label_len = options.batch_label.chars().count();
    if label_len == 0
        || label_len > MAX_LABEL_CHARS
        || options.batch_label.chars().any(char::is_control)
    {
        return Err(BulkError::new(
            codes::BAD_LABEL,
            "the batch label must be 1 to 64 characters with no control character",
        ));
    }
    if options.concurrency == 0 || options.concurrency > MAX_CONCURRENCY {
        return Err(BulkError::new(
            codes::BAD_CONCURRENCY,
            "the concurrency must be between 1 and 16",
        ));
    }
    match action {
        BulkAction::Method { name, .. }
            if name.is_empty()
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_') =>
        {
            return Err(BulkError::new(
                codes::ACTION_NOT_ALLOWED,
                "a method name is lower-case letters, digits and underscores",
            ));
        }
        BulkAction::Update(patch) if patch.fields.is_empty() && patch.tables.is_empty() => {
            return Err(BulkError::new(
                codes::ACTION_NOT_ALLOWED,
                "an update needs a patch that changes something",
            ));
        }
        _ => {}
    }
    if targets.is_empty() {
        return Err(BulkError::new(
            codes::EMPTY_BATCH,
            "the request names no document",
        ));
    }
    if targets.len() > options.max_targets {
        return Err(BulkError::new(
            codes::TOO_MANY_TARGETS,
            "the request names more documents than the batch limit",
        ));
    }
    Ok(())
}

/// Applies `action` to every target, one engine call each, and reports each one.
///
/// See the crate documentation for the semantics. In short: no batch transaction; a failure
/// of one document never stops the others (except by `max_failures`); results are in the order
/// of `targets`; at most `options.concurrency` calls are in flight; re-running the same
/// request with the same `batch_label` changes nothing twice, while the engine's command ledger
/// still holds the derived command ids.
///
/// # Errors
///
/// A [`BulkError`] when the request as a whole is refused (see [`validate_request`]); then no
/// call was made.
pub async fn apply_bulk(
    service: &dyn DocumentService,
    inv: &Invocation,
    action: &BulkAction,
    targets: Vec<BulkTarget>,
    options: &BulkOptions,
    progress: &dyn ProgressSink,
    cancel: &CancellationToken,
) -> Result<BulkOutcome, BulkError> {
    validate_request(action, &targets, options)?;
    let total = targets.len();
    let mut statuses: Vec<Option<BulkStatus>> = vec![None; total];

    // Duplicates collapse: the first occurrence runs, the rest are skipped.
    let mut seen: HashSet<(&str, RecordId)> = HashSet::with_capacity(total);
    let mut queue: Vec<usize> = Vec::with_capacity(total);
    let mut finished: u64 = 0;
    for (index, target) in targets.iter().enumerate() {
        if seen.insert((target.doctype.as_str(), target.id)) {
            queue.push(index);
        } else {
            statuses[index] = Some(BulkStatus::Skipped {
                reason: SkipReason::Duplicate,
            });
            finished += 1;
        }
    }

    let action_key = action.key();
    let every = options.progress_every.max(1);
    let total_u64 = total as u64;
    let mut in_flight = FuturesUnordered::new();
    let mut next = 0usize;
    let mut failures = 0usize;
    let mut stopped: Option<StopReason> = None;

    loop {
        // Start documents while there is room, nothing has stopped the batch, and work remains.
        while in_flight.len() < options.concurrency && next < queue.len() {
            if cancel.is_cancelled() {
                stopped = Some(StopReason::Cancelled);
                break;
            }
            if options.max_failures > 0 && failures >= options.max_failures {
                stopped = Some(StopReason::MaxFailures);
                break;
            }
            let index = queue[next];
            next += 1;
            let target = &targets[index];
            let mut call_inv = inv.clone();
            call_inv.command_id = command::derive(&options.batch_label, &action_key, &target.id);
            in_flight.push(async move {
                let status = run_one(service, &call_inv, action, target, options).await;
                (index, status)
            });
        }
        let Some((index, status)) = in_flight.next().await else {
            break;
        };
        if matches!(status, BulkStatus::Failed { .. }) {
            failures += 1;
        }
        statuses[index] = Some(status);
        finished += 1;
        if finished.is_multiple_of(every) {
            progress.progress(finished, total_u64);
        }
    }
    progress.progress(finished, total_u64);

    let mut outcome = BulkOutcome {
        succeeded: 0,
        would_succeed: 0,
        skipped: 0,
        failed: 0,
        not_run: 0,
        results: Vec::with_capacity(total),
        stopped,
        dry_run: options.dry_run,
    };
    for (index, (target, status)) in targets.iter().zip(statuses).enumerate() {
        let status = status.unwrap_or(BulkStatus::NotRun);
        match status {
            BulkStatus::Succeeded { .. } => outcome.succeeded += 1,
            BulkStatus::WouldSucceed => outcome.would_succeed += 1,
            BulkStatus::Skipped { .. } => outcome.skipped += 1,
            BulkStatus::Failed { .. } => outcome.failed += 1,
            BulkStatus::NotRun => outcome.not_run += 1,
        }
        outcome.results.push(BulkResult {
            index,
            doctype: target.doctype.clone(),
            id: target.id,
            status,
        });
    }
    Ok(outcome)
}
