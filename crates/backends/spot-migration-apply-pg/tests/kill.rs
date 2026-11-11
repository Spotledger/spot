//! The kill-mid-apply acceptance on a real database (WP-MIG-07.T2, the outcome rule from T1).
//!
//! Each round runs against its own scratch database:
//!
//! 1. the parent takes a snapshot and starts a **child process** (this same test binary, the
//!    `apply_child` test) that applies a six-step plan and appends a marker before and after
//!    every step;
//! 2. the parent watches the step markers and `SIGKILL`s the child at a randomised step;
//! 3. a probe loop asks a **second** [`PgAdmissionGate`](spot_migration_apply_pg::PgAdmissionGate)
//!    on the same database while the child runs, and records every request-path decision. Once
//!    the probe has seen the gate closed, a served request is a violation — the test asserts
//!    there are none until recovery starts;
//! 4. the parent restarts the engine in its own process over the same journal and runs
//!    [`ApplyEngine::recover`](spot_migration_apply::ApplyEngine::recover);
//! 5. the outcome rule is asserted: **new version verified and open, or old version restored
//!    and open — never open-unverified**, no step completed twice, and the site is open only
//!    after verification passed or the restore finished.
//!
//! Kill points come from the child's own step markers (never from a clock), so a run is
//! deterministic per seed; the seed is the parent's pid, printed for reproduction.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::io::Write;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use spot_core::SystemClock;
use spot_migration_apply::{
    ApplyEngine, ApplyJournal, ApplyOptions, ApplyRequest, ApplyState, ProgressSink,
    RecoveryAction, SnapshotPort, Version,
};
use spot_migration_apply_pg::{
    PgAdmissionGate, PgApplyJournal, PgApplySchema, PgDriftVerifier, PgStepRunner,
    RecordingTimings, TimingPhase,
};
use support::*;

/// How many kills one run performs (one scratch database each).
const KILLS: usize = 3;

/// How long the parent waits for one child step marker before giving up.
const MARKER_WAIT: Duration = Duration::from_secs(60);

/// The env var carrying `host|port|db|user|pass` to the child.
const CHILD_DB: &str = "SPOT_MIG_APPLY_CHILD_DB";
/// The env var carrying the marker file path.
const CHILD_MARKERS: &str = "SPOT_MIG_APPLY_CHILD_MARKERS";
/// The env var carrying the plan digest the child must apply.
const CHILD_DIGEST: &str = "SPOT_MIG_APPLY_CHILD_DIGEST";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_sigkill_mid_apply_never_serves_a_request_and_recovery_obeys_the_outcome_rule() {
    let mut seed = std::process::id() as u64 | 1;
    println!("KILL seed={seed}");
    for round in 0..KILLS {
        let Some(db) = db().await else { return };
        seed = next_seed(seed);
        let kill_at = 1 + (seed % 5) as usize;
        println!("KILL round={round} kill_after_step_marker={kill_at}");
        one_round(&db, kill_at, seed).await;
    }
}

/// One kill, probe and recovery against `db`.
async fn one_round(db: &spot_postgres::testing::TestDb, kill_at: usize, seed: u64) {
    let pool = pool(db, 8);
    PgApplySchema::new(pool.clone())
        .prepare()
        .await
        .expect("the control schema prepares");
    let backend = backend(db).await;
    let catalog = catalog(6);
    let plan = schema_plan(6);
    let site = site();

    // The snapshot is taken before the child starts, in the parent, so the parent's recovery
    // can restore it. Its timing is one of the measurements printed below.
    let timings = Arc::new(RecordingTimings::new());
    let snapshot = Arc::new(TestSnapshotPort::new(
        pool.clone(),
        site.clone(),
        branch_tables(6),
        Arc::new(SystemClock::new()),
        timings.clone(),
    ));
    let snapshot_id = snapshot.take("before-kill", 1_000).await.unwrap();

    let markers = marker_path(round_id(seed));
    let _ = std::fs::remove_file(&markers);

    // The probe: a request path's gate on the same row, in the parent process.
    let probe = Arc::new(ProbeLog::default());
    let stop_probe = Arc::new(AtomicBool::new(false));
    let recovery_started = Arc::new(AtomicBool::new(false));
    let probe_gate = Arc::new(PgAdmissionGate::new(
        pool.clone(),
        site.clone(),
        Arc::new(SystemClock::new()),
        Duration::from_millis(5),
    ));
    probe_gate.refresh(1_000).await.unwrap();
    let probe_task = {
        let gate = probe_gate.clone();
        let log = probe.clone();
        let stop = stop_probe.clone();
        let recovering = recovery_started.clone();
        tokio::spawn(async move {
            while !stop.load(Ordering::SeqCst) {
                let decision = admit(&gate).await;
                if decision.is_err() {
                    log.note_closed();
                } else if log.has_seen_closed() && !recovering.load(Ordering::SeqCst) {
                    // Served while the gate was closed and before recovery: a violation.
                    log.note_violation("served while the gate was closed");
                }
                log.observe(decision, "kill-test");
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
    };

    // Start the child and wait for its `kill_at`-th step marker.
    let mut child = spawn_child(db, &markers, &plan.plan_digest);
    let step_markers = wait_for_step_markers(&mut child, &markers, kill_at);
    let exited = child.try_wait().expect("try_wait").is_some();
    let status = kill(&mut child);
    println!("KILL step_markers={step_markers} exited_before_kill={exited} status={status:?}");
    assert!(
        !exited,
        "the child finished before the kill point; the plan is too fast for the marker"
    );
    assert!(
        step_markers >= kill_at,
        "the child did not reach the kill point: {step_markers} markers"
    );

    // The gate row is closed after the child's death.
    let status = probe_gate.refresh(2_000).await.unwrap();
    assert!(
        matches!(status, spot_migration_apply::GateStatus::Closed { .. }),
        "the gate was not closed after the kill: {status:?}"
    );

    // Recovery in this process, over the same journal.
    let mut runner = PgStepRunner::new(
        backend.clone(),
        site.clone(),
        catalog.clone(),
        engine_steps(&plan),
        Arc::new(SystemClock::new()),
    )
    .with_timings(timings.clone());
    runner.prepare().await.unwrap();
    let runner = Arc::new(runner);
    let journal = Arc::new(PgApplyJournal::new(pool.clone(), site.clone()));
    let before = progress_of(&journal).await;
    assert!(before.started, "the child never started a run");
    assert!(
        before.gate_open.is_none(),
        "the child opened the site before the kill: {before:?}"
    );

    let gate = Arc::new(PgAdmissionGate::new(
        pool.clone(),
        site.clone(),
        Arc::new(SystemClock::new()),
        Duration::from_millis(5),
    ));
    gate.refresh(2_000).await.unwrap();
    let hold = Arc::new(spot_migration_apply::RecordingHold::new());
    let progress = Arc::new(spot_migration_apply::RecordingProgress::new());
    let engine = ApplyEngine::new(
        site.clone(),
        journal.clone() as Arc<dyn ApplyJournal>,
        gate.clone(),
        hold,
        snapshot.clone(),
        runner.clone(),
        Arc::new(PgDriftVerifier::new(backend.clone(), catalog.clone())),
    )
    .with_progress(progress.clone())
    .with_options(ApplyOptions {
        drain_deadline_micros: 10_000_000,
        ..ApplyOptions::default()
    });

    recovery_started.store(true, Ordering::SeqCst);
    let recovered = engine.recover(&plan, 3_000).await.expect("recovery runs");
    println!("KILL recovered={recovered:?}");
    assert!(
        matches!(
            recovered.action,
            RecoveryAction::Resumed { .. }
                | RecoveryAction::Restored
                | RecoveryAction::AlreadyOpen { .. }
        ),
        "unexpected recovery action: {recovered:?}"
    );

    // The outcome rule.
    let after = progress_of(&journal).await;
    match engine.state() {
        ApplyState::Open {
            version: Version::New,
            verified,
        } => {
            assert!(verified, "the new version opened unverified");
            assert!(after.verified, "the journal does not record a verified run");
            assert!(
                matches!(recovered.action, RecoveryAction::Resumed { .. }),
                "{recovered:?}"
            );
            assert_eq!(
                live_branch_tables(&backend).await.len(),
                6,
                "the new version is open but the schema is incomplete"
            );
        }
        ApplyState::Open {
            version: Version::Old,
            verified,
        } => {
            assert!(!verified, "the old version is open marked verified");
            assert!(
                after.restore_done,
                "the journal records no completed restore"
            );
            assert_eq!(recovered.action, RecoveryAction::Restored);
            assert!(
                live_branch_tables(&backend).await.is_empty(),
                "the old version is open but the plan's tables survived"
            );
        }
        other => panic!("the site is not open after recovery: {other:?}"),
    }

    // The gate is open now and a request path is served.
    let status = gate.refresh(4_000).await.unwrap();
    assert!(status.is_open(), "{status:?}");
    assert!(admit(&gate).await.is_ok());
    assert_eq!(
        journal
            .entries()
            .await
            .unwrap()
            .iter()
            .filter(|entry| matches!(
                entry.record,
                spot_migration_apply::JournalRecord::GateOpened { .. }
            ))
            .count(),
        1,
        "the gate opened more than once"
    );

    stop_probe.store(true, Ordering::SeqCst);
    probe_task.await.unwrap();
    assert!(probe.observations() > 0);
    assert!(
        probe.has_seen_closed(),
        "the probe never observed the closed gate"
    );
    assert!(
        probe
            .refusals()
            .iter()
            .any(|reason| reason.contains("migration")),
        "the probe was never refused: {:?}",
        probe.refusals()
    );
    assert!(
        probe.violations().is_empty(),
        "a probe was served while the gate was closed: {:?}",
        probe.violations()
    );

    // No step completed twice, and every completion has a start.
    let entries = journal.read().await.unwrap();
    let mut started = std::collections::BTreeSet::new();
    let mut completed = std::collections::BTreeSet::new();
    for entry in &entries {
        match &entry.record {
            spot_migration_apply::JournalRecord::StepStarted { index, .. } => {
                started.insert(*index);
            }
            spot_migration_apply::JournalRecord::StepCompleted { index, .. } => {
                assert!(
                    completed.insert(*index),
                    "step {index} completed twice: {entries:?}"
                );
            }
            _ => {}
        }
    }
    for index in &completed {
        assert!(
            started.contains(index),
            "step {index} completed without a start"
        );
    }

    // The measurements WP-MIG-05 asks for: snapshot, apply and restore, in integers.
    let records = timings.records();
    print_timings("kill apply", &records);
    println!(
        "TIMING kill summary snapshot={} apply={} restore={}",
        records
            .iter()
            .filter(|r| r.phase == TimingPhase::Snapshot)
            .count(),
        records
            .iter()
            .filter(|r| r.phase == TimingPhase::Apply)
            .count(),
        records
            .iter()
            .filter(|r| r.phase == TimingPhase::Restore)
            .count()
    );
    // The snapshot the parent took is the one the run restores from.
    assert!(snapshot.exists(&snapshot_id).await.unwrap());
}

/// The child test: apply the plan with a marker before and after every step, and never exit on
/// its own before the parent's kill point unless the plan finishes.
///
/// It runs only when the parent set [`CHILD_DB`]; otherwise it returns immediately, so a normal
/// `cargo test` runs it as a no-op and it never touches a database.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn apply_child() {
    let Ok(descriptor) = std::env::var(CHILD_DB) else {
        return;
    };
    let config = config_from_child(&descriptor);
    let markers = std::env::var(CHILD_MARKERS).ok().map(PathBuf::from);
    let digest = std::env::var(CHILD_DIGEST).ok();
    let backend = spot_postgres::PgBackend::connect(config.clone())
        .await
        .expect("the child connects");
    let pool = pool_for(&config, 8, MIGRATION_APP);
    PgApplySchema::new(pool.clone())
        .prepare()
        .await
        .expect("the child prepares the control schema");
    let site = site();
    let catalog = catalog(6);
    let plan = schema_plan(6);
    if let Some(expected) = digest {
        assert_eq!(
            plan.plan_digest, expected,
            "the parent and child disagree on the plan"
        );
    }
    let ticket = ticket(&plan);
    let timings = Arc::new(RecordingTimings::new());
    let snapshot = Arc::new(TestSnapshotPort::new(
        pool.clone(),
        site.clone(),
        branch_tables(6),
        Arc::new(SystemClock::new()),
        timings,
    ));
    // The parent took the snapshot; resolve it from the control schema.
    let client = pool.get().await.unwrap();
    let row = client
        .query_one(
            "SELECT snapshot_id FROM spot_migration_control.snapshot \
             WHERE site = $1 ORDER BY taken_at LIMIT 1",
            &[&site.as_str()],
        )
        .await
        .expect("the parent's snapshot is indexed");
    let snapshot_id = spot_migration_apply::SnapshotId::new(row.get::<_, String>(0)).unwrap();

    let mut runner = PgStepRunner::new(
        backend.clone(),
        site.clone(),
        catalog.clone(),
        engine_steps(&plan),
        Arc::new(SystemClock::new()),
    );
    runner.prepare().await.expect("the child's runner prepares");
    let hooked = Arc::new(HookedRunner::new(
        Arc::new(runner),
        markers.clone(),
        Duration::from_millis(150),
    ));
    let gate = Arc::new(PgAdmissionGate::new(
        pool.clone(),
        site.clone(),
        Arc::new(SystemClock::new()),
        Duration::from_millis(5),
    ));
    gate.refresh(1_000).await.unwrap();
    let hold = Arc::new(spot_migration_apply::RecordingHold::new());
    let inner_progress = Arc::new(spot_migration_apply::RecordingProgress::new());
    let progress = Arc::new(FileProgress::new(inner_progress, markers.clone()));
    let engine = ApplyEngine::new(
        site.clone(),
        Arc::new(PgApplyJournal::new(pool.clone(), site.clone())) as Arc<dyn ApplyJournal>,
        gate,
        hold,
        snapshot,
        hooked,
        Arc::new(PgDriftVerifier::new(backend, catalog)),
    )
    .with_progress(progress as Arc<dyn ProgressSink>)
    .with_options(ApplyOptions {
        drain_deadline_micros: 10_000_000,
        ..ApplyOptions::default()
    });
    let outcome = engine
        .apply(ApplyRequest::new(
            plan,
            Some(ticket),
            Some(snapshot_id),
            1_000,
        ))
        .await
        .expect("the child applies");
    println!("CHILD outcome={outcome:?}");
    mark(&markers, "child:finished");
}

/// A seed step for the kill points: a small deterministic LCG, no clock and no `rand`.
fn next_seed(seed: u64) -> u64 {
    seed.wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407)
}

/// A unique marker path for one round.
fn marker_path(round: u64) -> PathBuf {
    std::env::temp_dir().join(format!(
        "spot-migration-apply-pg-kill-{}-{round}.markers",
        std::process::id()
    ))
}

/// A round id derived from the seed (never the clock).
fn round_id(seed: u64) -> u64 {
    seed & 0xffff_ffff
}

/// Spawns the child test in a process of its own.
fn spawn_child(db: &spot_postgres::testing::TestDb, markers: &PathBuf, digest: &str) -> Child {
    let exe = std::env::current_exe().expect("the test binary path");
    Command::new(exe)
        .args(["--exact", "apply_child", "--nocapture", "--test-threads=1"])
        .env(CHILD_DB, child_database(db))
        .env(CHILD_MARKERS, markers)
        .env(CHILD_DIGEST, digest)
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("the child process starts")
}

/// Waits until the marker file holds at least `target` **step** markers or the child exits;
/// returns how many step markers it saw.
fn wait_for_step_markers(child: &mut Child, markers: &PathBuf, target: usize) -> usize {
    let deadline = Instant::now() + MARKER_WAIT;
    loop {
        let count = count_step_markers(markers);
        if count >= target {
            return count;
        }
        if let Ok(Some(_)) = child.try_wait() {
            return count;
        }
        if Instant::now() > deadline {
            return count;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// How many step-start markers the file holds (progress markers are not counted).
fn count_step_markers(path: &PathBuf) -> usize {
    std::fs::read_to_string(path)
        .map(|text| {
            text.lines()
                .filter(|line| line.starts_with("step:") && line.ends_with(":start"))
                .count()
        })
        .unwrap_or(0)
}

/// `SIGKILL`s the child (`Child::kill` sends `SIGKILL` on Unix) and reaps it.
fn kill(child: &mut Child) -> std::process::ExitStatus {
    let _ = child.kill();
    child.wait().expect("the killed child is reaped")
}

/// Appends one marker line, ignoring a failed write (the parent is the reader).
fn mark(path: &Option<PathBuf>, line: &str) {
    if let Some(path) = path
        && let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
    {
        let _ = writeln!(file, "{line}");
    }
}

/// A verification failure after the steps restores the old version and opens it, and the gate
/// is closed for the whole restore.
///
/// The verifier is given a catalogue with one table the plan never creates, so verification
/// fails after every step; the engine's rollback path then restores the snapshot on a real
/// database. The outcome rule's second branch is asserted, not assumed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_verification_restores_and_opens_the_old_version() {
    let Some(db) = db().await else { return };
    let pool = pool(&db, 8);
    PgApplySchema::new(pool.clone())
        .prepare()
        .await
        .expect("the control schema prepares");
    let backend = backend(&db).await;
    let plan = schema_plan(3);
    let site = site();
    // The plan applies three tables; the verifier expects four, so it must fail.
    let applied = catalog(3);
    let wanted = catalog(4);
    let timings = Arc::new(RecordingTimings::new());
    let snapshot = Arc::new(TestSnapshotPort::new(
        pool.clone(),
        site.clone(),
        branch_tables(3),
        Arc::new(SystemClock::new()),
        timings.clone(),
    ));
    let snapshot_id = snapshot.take("before-apply", 1_000).await.unwrap();
    let ticket = ticket(&plan);

    let mut runner = PgStepRunner::new(
        backend.clone(),
        site.clone(),
        applied.clone(),
        engine_steps(&plan),
        Arc::new(SystemClock::new()),
    )
    .with_timings(timings.clone());
    runner.prepare().await.unwrap();
    let journal = Arc::new(PgApplyJournal::new(pool.clone(), site.clone()));
    let gate = Arc::new(PgAdmissionGate::new(
        pool.clone(),
        site.clone(),
        Arc::new(SystemClock::new()),
        Duration::from_millis(5),
    ));
    gate.refresh(1_000).await.unwrap();
    let engine = ApplyEngine::new(
        site.clone(),
        journal.clone() as Arc<dyn ApplyJournal>,
        gate.clone(),
        Arc::new(spot_migration_apply::RecordingHold::new()),
        snapshot.clone(),
        Arc::new(runner),
        Arc::new(PgDriftVerifier::new(backend.clone(), wanted)),
    )
    .with_options(ApplyOptions {
        drain_deadline_micros: 10_000_000,
        ..ApplyOptions::default()
    });
    let outcome = engine
        .apply(ApplyRequest::new(
            plan.clone(),
            Some(ticket),
            Some(snapshot_id),
            1_000,
        ))
        .await
        .expect("the run restores rather than failing outright");
    println!("KILL verify-fail outcome={outcome:?}");
    assert!(matches!(
        outcome,
        spot_migration_apply::ApplyOutcome::Restored {
            cause: spot_migration_apply::RestoreCause::VerifyFailed,
            mechanism: spot_migration_apply::RestoreMechanism::Snapshot
        }
    ));
    assert_eq!(
        engine.state(),
        ApplyState::Open {
            version: Version::Old,
            verified: false
        }
    );
    assert_eq!(snapshot.restores(), 1);
    assert!(live_branch_tables(&backend).await.is_empty());
    let progress = progress_of(&journal).await;
    assert!(progress.verify_failed);
    assert!(progress.restore_done);
    assert_eq!(progress.gate_open, Some((Version::Old, false)));
    assert!(gate.refresh(4_000).await.unwrap().is_open());
    print_timings("verify-fail", &timings.records());
}
