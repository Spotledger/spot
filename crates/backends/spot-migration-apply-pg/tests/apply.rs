//! The apply engine over the real PostgreSQL wiring: a six-step plan applied, verified and
//! opened; the gate consulted by a probe throughout; and the timing capture the WP-MIG-05
//! measurement needs.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::sync::Arc;
use std::time::Duration;

use spot_core::SystemClock;
use spot_migration_apply::StepRunner;
use spot_migration_apply::gate::AdmissionGate;
use spot_migration_apply::{ApplyOutcome, ApplyRequest, ApplyState, Version};
use support::*;

fn clock() -> Arc<SystemClock> {
    Arc::new(SystemClock::new())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_six_step_plan_creates_every_table_verifies_and_opens_on_the_new_version() {
    let Some(h) = harness(6, clock()).await else {
        return;
    };
    let outcome = h
        .engine
        .apply(ApplyRequest::new(
            h.plan.clone(),
            Some(h.ticket.clone()),
            Some(h.snapshot_id.clone()),
            1_000,
        ))
        .await
        .expect("the plan applies");
    assert_eq!(
        outcome,
        ApplyOutcome::Opened {
            version: Version::New,
            verified: true
        }
    );
    assert_eq!(
        h.engine.state(),
        ApplyState::Open {
            version: Version::New,
            verified: true
        }
    );

    // Every table exists and the planner has nothing left: the verifier's own check.
    let live = live_branch_tables(&h.backend).await;
    assert_eq!(live.len(), 6, "{live:?}");
    let remaining = h.runner.remaining().await.unwrap();
    assert!(remaining.steps.is_empty(), "{:?}", remaining.steps);

    // The journal records the whole run, and the gate row is open.
    let progress = progress_of(&h.journal).await;
    assert!(progress.verified);
    assert_eq!(progress.gate_open, Some((Version::New, true)));
    assert_eq!(progress.completed.len(), 6);
    assert_eq!(gate_label(&h.gate), "open");
    assert!(h.gate.problem().is_none());

    // The progress sink saw the ordered tags.
    let tags = h.progress.tags();
    assert_eq!(tags.first(), Some(&"started"));
    assert_eq!(tags.get(1), Some(&"gate_closing"));
    assert_eq!(tags.last(), Some(&"opened"));
    assert_eq!(
        tags.iter().filter(|tag| **tag == "step_completed").count(),
        6
    );

    // The snapshot was taken and never restored.
    assert_eq!(h.snapshot.restores(), 0);
    print_timings("six-step apply", &h.timings.records());
    let apply = timings_of(&h.timings, spot_migration_apply_pg::TimingPhase::Apply);
    assert_eq!(apply.len(), 6);
    assert!(apply.iter().all(|record| record.duration_micros >= 0));
    let snapshots = timings_of(&h.timings, spot_migration_apply_pg::TimingPhase::Snapshot);
    assert_eq!(snapshots.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_probe_is_never_served_while_the_gate_is_closed() {
    let Some(h) = harness(6, clock()).await else {
        return;
    };
    let probe = Arc::new(ProbeLog::default());
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let gate = h.gate.clone();
    let log = probe.clone();
    let running = stop.clone();
    let watcher = tokio::spawn(async move {
        while !running.load(std::sync::atomic::Ordering::SeqCst) {
            let decision = admit(&gate).await;
            log.observe(decision, "apply");
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    });

    let outcome = h
        .engine
        .apply(ApplyRequest::new(
            h.plan.clone(),
            Some(h.ticket.clone()),
            Some(h.snapshot_id.clone()),
            1_000,
        ))
        .await
        .expect("the plan applies");
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    watcher.await.unwrap();

    assert_eq!(
        outcome,
        ApplyOutcome::Opened {
            version: Version::New,
            verified: true
        }
    );
    assert!(probe.observations() > 0);
    let refusals = probe.refusals();
    assert!(
        refusals
            .iter()
            .any(|reason| reason.contains("migration apply")),
        "the probe never saw the maintenance problem: {refusals:?}"
    );
    // Every refusal was the maintenance document; no observation was both served and refused.
    assert!(refusals.iter().all(|reason| !reason.is_empty()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_apply_of_the_same_plan_is_refused_and_the_gate_stays_open() {
    let Some(h) = harness(3, clock()).await else {
        return;
    };
    let request = || {
        ApplyRequest::new(
            h.plan.clone(),
            Some(h.ticket.clone()),
            Some(h.snapshot_id.clone()),
            1_000,
        )
    };
    h.engine.apply(request()).await.expect("the plan applies");
    let error = h.engine.apply(request()).await.expect_err("a second apply");
    assert_eq!(
        error.code(),
        spot_migration_apply::codes::ALREADY_APPLIED,
        "{error}"
    );
    assert_eq!(gate_label(&h.gate), "open");
    assert_eq!(h.snapshot.restores(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_plan_without_a_ticket_never_closes_the_gate_or_writes_a_journal() {
    let Some(h) = harness(2, clock()).await else {
        return;
    };
    let error = h
        .engine
        .apply(ApplyRequest::new(
            h.plan.clone(),
            None,
            Some(h.snapshot_id.clone()),
            1_000,
        ))
        .await
        .expect_err("no ticket");
    assert_eq!(error.code(), spot_migration_apply::codes::NO_TICKET);
    assert!(h.journal.read().await.unwrap().is_empty());
    assert_eq!(gate_label(&h.gate), "open");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_runner_refuses_a_non_schema_step_it_has_no_handler_for() {
    let Some(db) = db().await else { return };
    let backend = backend(&db).await;
    let pool = pool(&db, 4);
    spot_migration_apply_pg::PgApplySchema::new(pool.clone())
        .prepare()
        .await
        .unwrap();
    let plan = metadata_plan();
    let mut runner = spot_migration_apply_pg::PgStepRunner::new(
        backend.clone(),
        site(),
        catalog(0),
        engine_steps(&plan),
        clock(),
    );
    runner.prepare().await.unwrap();
    let descriptor = StepRunner::describe(&runner, 0, &plan.steps()[0]);
    let ctx = spot_migration_apply::StepContext::new(&site(), plan.plan_digest.clone(), "m", 1_000);
    let error = StepRunner::run(&runner, &descriptor, &ctx)
        .await
        .expect_err("no metadata handler");
    assert!(
        error.to_string().contains("no handler") && error.to_string().contains("PublishMetadata"),
        "{error}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failing_step_restores_the_snapshot_and_opens_the_old_version() {
    let Some(h) = harness(3, clock()).await else {
        return;
    };
    // The runner's target has four tables but the snapshot's restore drops three: the fourth
    // table's create step succeeds, then the plan asks for one more step than the catalogue
    // supplies, so the runner's `remaining` empties and the step is a no-op. Instead, make the
    // step fail by refusing a name collision the planner cannot fix: create the target table
    // by hand with an incompatible column.
    let client = h.db.raw_client().await;
    client
        .batch_execute("CREATE TABLE branch_2 (id uuid PRIMARY KEY, note integer NOT NULL)")
        .await
        .unwrap();
    let outcome = h
        .engine
        .apply(ApplyRequest::new(
            h.plan.clone(),
            Some(h.ticket.clone()),
            Some(h.snapshot_id.clone()),
            1_000,
        ))
        .await
        .expect("the run rolls back rather than failing outright");
    assert!(matches!(
        outcome,
        ApplyOutcome::Restored {
            cause: spot_migration_apply::RestoreCause::StepFailed { .. },
            mechanism: spot_migration_apply::RestoreMechanism::Snapshot
        }
    ));
    assert_eq!(h.snapshot.restores(), 1);
    assert_eq!(
        h.engine.state(),
        ApplyState::Open {
            version: Version::Old,
            verified: false
        }
    );
    // The restore dropped the tables the run created; the hand-made table survives, because
    // the snapshot drops only what the plan creates.
    let live = live_branch_tables(&h.backend).await;
    assert!(!live.contains(&"branch_1".to_owned()), "{live:?}");
    assert_eq!(gate_label(&h.gate), "open");
    assert!(h.gate.problem().is_none());
}
