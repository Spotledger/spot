//! Batch start: one request owns many records; per-item outcomes, checkpoints, resume.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use common::now;
use serde_json::{Value as Json, json};
use spot_bpm_entry::{
    BatchControl, BatchIdentity, BatchInput, BatchRunner, BatchSpec, BatchStatus, BatchStore,
    EntryError, FailurePolicy, ItemApplier, ItemContext, ItemFailure, ItemOutcome,
    MemoryBatchStore, MemoryRequests, ProcessRef, RequestAuthority, RequestStarter,
};

const TOTAL: usize = 5_000;

/// Starts requests, remembering how many it started and how each ended.
#[derive(Default)]
struct Starter {
    started: Mutex<Vec<String>>,
    finished: Mutex<Vec<(String, Json)>>,
}

impl RequestStarter for Starter {
    fn start(
        &self,
        _process: &ProcessRef,
        identity: &BatchIdentity,
        _now: spot_app_api::prelude::OffsetDateTime,
    ) -> Result<String, EntryError> {
        // Idempotent on the start key: the same key is the same request.
        let id = format!("req_{}", identity.key);
        let mut started = self.started.lock().unwrap();
        if !started.contains(&id) {
            started.push(id.clone());
        }
        Ok(id)
    }

    fn finish(
        &self,
        request: &str,
        _node: &str,
        summary: &Json,
        _now: spot_app_api::prelude::OffsetDateTime,
    ) -> Result<(), EntryError> {
        self.finished
            .lock()
            .unwrap()
            .push((request.to_owned(), summary.clone()));
        Ok(())
    }
}

/// An applier whose lifecycle rejects every 33rd... item with invalid content (3%), dedupes on
/// the item key like the lifecycle's idempotency does, and can be told to die mid-run.
struct Applier {
    /// item key -> how many times its effect was applied.
    effects: Mutex<BTreeMap<String, usize>>,
    /// Every call, including repeats.
    calls: AtomicUsize,
    /// Command ids seen, to prove the request context rode along.
    contexts: Mutex<BTreeSet<String>>,
    /// Panic (the process dies) when this item index is reached, after its effect applied.
    die_after_effect_at: Mutex<Option<usize>>,
    /// Fail with a transient error on these items.
    broken: Mutex<BTreeSet<String>>,
}

impl Applier {
    fn new() -> Self {
        Self {
            effects: Mutex::default(),
            calls: AtomicUsize::new(0),
            contexts: Mutex::default(),
            die_after_effect_at: Mutex::new(None),
            broken: Mutex::default(),
        }
    }

    fn effects_total(&self) -> usize {
        self.effects.lock().unwrap().values().sum()
    }

    fn max_effects_per_item(&self) -> usize {
        self.effects
            .lock()
            .unwrap()
            .values()
            .copied()
            .max()
            .unwrap_or(0)
    }
}

fn is_invalid(index: usize) -> bool {
    index % 100 < 3
}

impl ItemApplier for Applier {
    fn apply(&self, item: &BatchInput, ctx: &ItemContext) -> Result<(), ItemFailure> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.contexts.lock().unwrap().insert(ctx.command_id.clone());
        if is_invalid(ctx.index) {
            return Err(ItemFailure::Invalid {
                code: "document.required".to_owned(),
                detail: format!("row {} has no name", ctx.index + 1),
            });
        }
        if self.broken.lock().unwrap().contains(&item.key) {
            return Err(ItemFailure::Failed {
                code: "server.unavailable".to_owned(),
                detail: "the store was busy".to_owned(),
            });
        }
        // The lifecycle dedupes on the idempotency key: a repeat changes nothing.
        let mut effects = self.effects.lock().unwrap();
        let count = effects.entry(item.key.clone()).or_insert(0);
        if *count == 0 {
            *count = 1;
        }
        drop(effects);
        if *self.die_after_effect_at.lock().unwrap() == Some(ctx.index) {
            panic!("the process died");
        }
        Ok(())
    }
}

fn items(count: usize) -> impl Iterator<Item = BatchInput> {
    (0..count).map(|index| BatchInput {
        key: format!("row{index:05}"),
        values: json!({"name": format!("Person {index}")}),
    })
}

fn identity(key: &str) -> BatchIdentity {
    BatchIdentity {
        actor: "u1".to_owned(),
        scope: "acme".to_owned(),
        key: key.to_owned(),
    }
}

fn spec() -> BatchSpec {
    let mut spec = BatchSpec::new(
        ProcessRef {
            id: "entry_employee".to_owned(),
            generated: true,
        },
        "employee",
        "enter",
    );
    spec.chunk_size = 500;
    spec
}

struct Rig {
    store: Arc<MemoryBatchStore>,
    starter: Arc<Starter>,
    applier: Arc<Applier>,
    runner: BatchRunner,
    requests: Arc<MemoryRequests>,
}

fn rig() -> Rig {
    let store = MemoryBatchStore::new().shared();
    let starter = Arc::new(Starter::default());
    let applier = Arc::new(Applier::new());
    let requests = MemoryRequests::new().shared();
    let runner = BatchRunner::new(
        store.clone(),
        starter.clone(),
        applier.clone(),
        requests.clone(),
    );
    Rig {
        store,
        starter,
        applier,
        runner,
        requests,
    }
}

#[test]
fn a_batch_of_5000_with_3_percent_invalid_is_one_request_with_reported_items() {
    let r = rig();
    let report = r
        .runner
        .start_batch(
            &spec(),
            items(TOTAL),
            &identity("import-1"),
            &BatchControl::none(),
            now(),
        )
        .unwrap();

    // One request owns all of it.
    assert_eq!(r.starter.started.lock().unwrap().len(), 1);
    assert_eq!(report.request, "req_import-1");

    // 150 invalid rows are skipped and reported; the run goes on to the end.
    assert_eq!(report.counters.read, TOTAL);
    assert_eq!(report.counters.skipped, 150);
    assert_eq!(report.counters.failed, 0);
    assert_eq!(report.counters.done, 4_850);
    assert_eq!(report.status, BatchStatus::SucceededWithSkips);
    assert_eq!(
        report.problems.len(),
        100,
        "the report lists the first problems"
    );
    assert_eq!(report.problems[0].key, "row00000");
    assert!(matches!(
        report.problems[0].outcome,
        ItemOutcome::Skipped { .. }
    ));
    assert_eq!(r.store.outcome_count(&report.request), TOTAL);

    // The records were created under the request's own context, as the calling principal.
    let contexts = r.applier.contexts.lock().unwrap().clone();
    assert_eq!(contexts.len(), 1, "one grant for the attempt");
    let key = contexts.iter().next().unwrap();
    let step = r
        .requests
        .authenticate(key, "u1")
        .expect("the grant is the calling principal's for the current attempt");
    assert_eq!(
        (step.request.as_str(), step.node.as_str(), step.attempt),
        ("req_import-1", "enter", 1)
    );
    assert!(
        r.requests.authenticate(key, "u2").is_none(),
        "no one else can use it"
    );
    assert_eq!(r.applier.effects_total(), 4_850);

    // The request's batch step is finished with the summary.
    let finished = r.starter.finished.lock().unwrap().clone();
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0].1["skipped"], json!(150));
}

#[test]
fn a_finished_batch_is_not_run_or_resumed_again() {
    let r = rig();
    // Indexes 0 to 2 are in the invalid block, so 3 of these 10 rows are skipped.
    let report = r
        .runner
        .start_batch(
            &spec(),
            items(10),
            &identity("tiny"),
            &BatchControl::none(),
            now(),
        )
        .unwrap();
    assert_eq!(report.counters.skipped, 3);
    assert_eq!(report.status, BatchStatus::SucceededWithSkips);
    let again = r
        .runner
        .start_batch(
            &spec(),
            items(10),
            &identity("tiny"),
            &BatchControl::none(),
            now(),
        )
        .expect_err("the same start key is the same batch");
    assert_eq!(again.code(), "bpm_entry.batch_state");
    assert_eq!(
        r.starter.started.lock().unwrap().len(),
        1,
        "no second request"
    );
    let resumed = r
        .runner
        .resume("req_tiny", items(10), &BatchControl::none(), now())
        .expect_err("already ran to the end");
    assert_eq!(resumed.code(), "bpm_entry.batch_state");
}

#[test]
fn a_killed_batch_resumes_without_applying_any_item_twice() {
    let r = rig();
    // The process dies at item 2350, after its effect committed and before its outcome row.
    *r.applier.die_after_effect_at.lock().unwrap() = Some(2_350);
    let crashed = catch_unwind(AssertUnwindSafe(|| {
        r.runner.start_batch(
            &spec(),
            items(TOTAL),
            &identity("import-2"),
            &BatchControl::none(),
            now(),
        )
    }));
    assert!(crashed.is_err(), "the run died mid-batch");

    let run = r.store.load("req_import-2").unwrap().unwrap();
    assert_eq!(
        run.status,
        BatchStatus::Running,
        "a dead driver leaves the run running"
    );
    assert_eq!(
        run.checkpoint, 2_000,
        "the checkpoint is the last chunk boundary"
    );
    let calls_before = r.applier.calls.load(Ordering::SeqCst);

    // A new process resumes from the stream's start; nothing that has an outcome is presented
    // again, and the item whose outcome was lost is retried under the same key.
    *r.applier.die_after_effect_at.lock().unwrap() = None;
    let report = r
        .runner
        .resume("req_import-2", items(TOTAL), &BatchControl::none(), now())
        .unwrap();
    assert_eq!(report.attempt, 2);
    assert_eq!(report.status, BatchStatus::SucceededWithSkips);
    assert_eq!(report.counters.read, TOTAL);
    assert_eq!(report.counters.skipped, 150);
    assert_eq!(report.counters.done, 4_850);

    // Items before the checkpoint were never presented again; those between the checkpoint and
    // the crash were found decided; only the lost one and the undone remainder were applied.
    let calls_after = r.applier.calls.load(Ordering::SeqCst) - calls_before;
    assert_eq!(
        calls_after,
        TOTAL - 2_350,
        "item 2350 and everything after it"
    );
    assert_eq!(r.applier.effects_total(), 4_850);
    assert_eq!(
        r.applier.max_effects_per_item(),
        1,
        "no item took effect twice"
    );
    assert_eq!(r.store.outcome_count("req_import-2"), TOTAL);
    assert_eq!(
        r.starter.started.lock().unwrap().len(),
        1,
        "still the one request"
    );
}

#[test]
fn a_cooperative_stop_pauses_between_items_and_resumes_exactly() {
    let r = rig();
    let seen = AtomicUsize::new(0);
    let stop = || seen.fetch_add(1, Ordering::SeqCst) >= 1_200;
    let paused = r
        .runner
        .start_batch(
            &spec(),
            items(TOTAL),
            &identity("import-3"),
            &BatchControl::stop_when(&stop),
            now(),
        )
        .unwrap();
    assert_eq!(paused.status, BatchStatus::Paused);
    assert_eq!(paused.counters.read, 1_200);
    let run = r.store.load("req_import-3").unwrap().unwrap();
    assert_eq!(
        run.checkpoint, 1_200,
        "the position is the first undecided item"
    );

    let done = r
        .runner
        .resume("req_import-3", items(TOTAL), &BatchControl::none(), now())
        .unwrap();
    assert_eq!(done.status, BatchStatus::SucceededWithSkips);
    assert_eq!(done.counters.read, TOTAL);
    assert_eq!(done.counters.done + done.counters.skipped, TOTAL);
    assert_eq!(
        r.applier.calls.load(Ordering::SeqCst),
        TOTAL,
        "each item presented once"
    );
}

#[test]
fn per_item_failures_are_reported_and_do_not_abort_unless_configured() {
    let r = rig();
    r.applier
        .broken
        .lock()
        .unwrap()
        .insert("row00550".to_owned());
    let report = r
        .runner
        .start_batch(
            &spec(),
            items(1_000),
            &identity("import-4"),
            &BatchControl::none(),
            now(),
        )
        .unwrap();
    assert_eq!(
        report.counters.failed, 1,
        "the busy item failed and the run went on"
    );
    assert_eq!(report.counters.read, 1_000);
    assert!(
        report
            .problems
            .iter()
            .any(|p| p.key == "row00550" && matches!(p.outcome, ItemOutcome::Failed { .. }))
    );

    // Configured to abort on the first problem: the run stops at it and resumes there.
    let r = rig();
    let mut strict = spec();
    strict.failure = FailurePolicy {
        abort_on_problem: true,
        max_problems: None,
    };
    let stopped = r
        .runner
        .start_batch(
            &strict,
            items(1_000),
            &identity("import-5"),
            &BatchControl::none(),
            now(),
        )
        .unwrap();
    assert_eq!(stopped.status, BatchStatus::Failed);
    assert_eq!(
        stopped.counters.read, 1,
        "row 0 is invalid, so it stops there"
    );
    assert_eq!(
        stopped.stopped_by.as_deref(),
        Some("bpm_entry.batch_failed")
    );
}

#[test]
fn a_problem_limit_stops_the_run_with_its_code() {
    let r = rig();
    let mut limited = spec();
    limited.failure = FailurePolicy {
        abort_on_problem: false,
        max_problems: Some(10),
    };
    let report = r
        .runner
        .start_batch(
            &limited,
            items(TOTAL),
            &identity("import-6"),
            &BatchControl::none(),
            now(),
        )
        .unwrap();
    assert_eq!(report.status, BatchStatus::Failed);
    assert_eq!(report.stopped_by.as_deref(), Some("bpm_entry.batch_limit"));
    assert_eq!(report.counters.problems(), 11);
    // Not finished, so the request's step is not completed.
    assert!(r.starter.finished.lock().unwrap().is_empty());
}

#[test]
fn an_unstarted_or_unknown_batch_cannot_be_resumed() {
    let r = rig();
    let error = r
        .runner
        .resume("req_none", items(1), &BatchControl::none(), now())
        .expect_err("no such batch");
    assert_eq!(error.code(), "bpm_entry.batch_state");
}
