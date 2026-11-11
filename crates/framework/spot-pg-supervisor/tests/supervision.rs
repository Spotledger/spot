//! Supervision round-trips against the fake postmaster: start → ready → stop, the
//! timeout path, the probe veto, the forced kill, crash restarts with budget, and the
//! stale-lock gate.

mod common;

use common::{TestEnv, must};
use spot_pg_supervisor::Supervisor;
use spot_pg_supervisor::config::{RestartBudget, SupervisorConfig};
use spot_pg_supervisor::error::SupervisorError;
use spot_pg_supervisor::events::SupervisorEvent;
use spot_pg_supervisor::supervisor::{StoppedHow, SupervisorState};
use std::sync::Arc;
use std::time::Duration;

fn supervisor(env: &TestEnv) -> Supervisor {
    Supervisor::new(env.layout(), env.settings(), env.config())
}

fn supervisor_with(env: &TestEnv, config: SupervisorConfig) -> Supervisor {
    Supervisor::new(env.layout(), env.settings(), config)
}

async fn wait_for_state(
    rx: &tokio::sync::watch::Receiver<SupervisorState>,
    predicate: impl Fn(&SupervisorState) -> bool,
) -> SupervisorState {
    let mut rx = rx.clone();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let state = rx.borrow().clone();
        if predicate(&state) {
            return state;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("state never matched; last was {state:?}");
        }
        if tokio::time::timeout(Duration::from_millis(500), rx.changed())
            .await
            .is_err()
        {
            continue; // re-borrow and re-check once more before the deadline
        }
    }
}

/// Start, become ready through a genuine TCP accept, stop gracefully, stop twice.
#[tokio::test]
async fn start_ready_stop_and_stop_twice() {
    let env = TestEnv::new();
    let supervisor = supervisor(&env);
    let mut events = supervisor.events();
    let rx = supervisor.subscribe();

    let handle = must(supervisor.start().await);
    let state = wait_for_state(&rx, |state| matches!(state, SupervisorState::Ready)).await;
    assert!(matches!(state, SupervisorState::Ready));

    // The fake wrote its pid file; readiness means the port accepted a connection.
    assert!(env.data_dir.join("postmaster.pid").is_file());

    let stopped = must(handle.stop(Duration::from_secs(5)).await);
    assert!(matches!(stopped, StoppedHow::Graceful));
    let state = wait_for_state(&rx, |state| {
        matches!(
            state,
            SupervisorState::Stopped | SupervisorState::Failed { .. }
        )
    })
    .await;
    assert!(
        matches!(state, SupervisorState::Stopped),
        "expected Stopped after graceful stop, got {state:?}"
    );

    // Calling stop twice is fine and stays graceful.
    let again = must(handle.stop(Duration::from_secs(5)).await);
    assert!(matches!(again, StoppedHow::Graceful));

    // The event stream saw the full arc: Starting/Ready at least.
    let mut seen = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let SupervisorEvent::StateChanged { state, .. } = event {
            seen.push(state);
        }
    }
    assert!(
        seen.contains(&SupervisorState::Starting),
        "events lacked Starting: {seen:?}"
    );
    assert!(
        seen.contains(&SupervisorState::Ready),
        "events lacked Ready: {seen:?}"
    );
    common::assert_no_orphans(&env);
}

/// A probe returning false holds the state at `Starting` until the timeout, which
/// becomes `Failed`.
#[tokio::test]
async fn start_timeout_and_probe_veto() {
    let env = TestEnv::new();
    let mut config = env.config();
    config.start_timeout = Duration::from_millis(400);
    let veto: spot_pg_supervisor::config::ReadyProbe = Arc::new(|| {
        Box::pin(async {
            tokio::time::sleep(Duration::from_millis(40)).await;
            false
        })
    });
    config.ready_probe = Some(veto);
    let supervisor = supervisor_with(&env, config);
    let rx = supervisor.subscribe();
    let handle = must(supervisor.start().await);

    let mut events = handle.events();
    let state = wait_for_state(&rx, |state| {
        matches!(
            state,
            SupervisorState::Failed { .. } | SupervisorState::Stopped
        )
    })
    .await;
    match &state {
        SupervisorState::Failed { reason } => {
            assert!(reason.contains("start timeout"), "{reason}");
        }
        other => panic!("expected Failed(start timeout), got {other:?}"),
    }
    // The probe veto means the port may have accepted: no restarts were scheduled.
    let mut restarts = 0;
    while let Ok(event) = events.try_recv() {
        if matches!(event, SupervisorEvent::Restart { .. }) {
            restarts += 1;
        }
    }
    assert_eq!(restarts, 0, "a vetoed start must not schedule restarts");
    must(handle.stop(Duration::from_millis(100)).await);
    common::assert_no_orphans(&env);
}

/// A probe returning true does not block readiness (control for the veto test).
#[tokio::test]
async fn probe_veto_passes_when_probe_returns_true() {
    let env = TestEnv::new();
    let mut config = env.config();
    let approve: spot_pg_supervisor::config::ReadyProbe = Arc::new(|| Box::pin(async { true }));
    config.ready_probe = Some(approve);
    let supervisor = supervisor_with(&env, config);
    let rx = supervisor.subscribe();
    let handle = must(supervisor.start().await);
    let state = wait_for_state(&rx, |state| {
        matches!(
            state,
            SupervisorState::Ready | SupervisorState::Failed { .. }
        )
    })
    .await;
    assert!(matches!(state, SupervisorState::Ready), "got {state:?}");
    must(handle.stop(Duration::from_secs(3)).await);
    common::assert_no_orphans(&env);
}

/// The forced path: a child that ignores SIGINT is killed after the grace period and
/// the outcome is `StoppedForced` with a `ForcedStop` event and no surviving child.
#[tokio::test]
async fn forced_kill_reports_stopped_forced() {
    let env = TestEnv::new();
    // The fake reads its behaviours from the per-data-dir .env file (process-global
    // env vars are gone in edition 2024 and race across parallel tests).
    env.write_env(&[("FAKE_IGNORE_SIGINT", "1")]);
    let supervisor = supervisor(&env);
    let rx = supervisor.subscribe();
    let handle = must(supervisor.start().await);
    let state = wait_for_state(&rx, |state| matches!(state, SupervisorState::Ready)).await;
    assert!(matches!(state, SupervisorState::Ready));

    let mut events = handle.events();
    let stopped = must(handle.stop(Duration::from_millis(150)).await);
    assert!(
        matches!(stopped, StoppedHow::StoppedForced),
        "expected StoppedForced, got {stopped:?}"
    );
    // The ForcedStop event was published.
    let mut forced_seen = false;
    while let Ok(event) = events.try_recv() {
        if matches!(event, SupervisorEvent::ForcedStop { .. }) {
            forced_seen = true;
        }
    }
    assert!(forced_seen, "ForcedStop event missing");
    // Give the kill a moment, then assert the fake is really dead.
    tokio::time::sleep(Duration::from_millis(100)).await;
    common::assert_no_orphans(&env);
}

/// Crash restarts: the fake crashes, the supervisor restarts with backoff, and the
/// state machine walks Starting → Ready → Restarting → Ready.
#[tokio::test]
async fn crash_restarts_with_backoff() {
    let env = TestEnv::new();
    let mut config = env.config();
    // Crash 250 ms after each bind; the first restart must bring it back.
    config
        .child_env
        .insert("FAKE_EXIT_AFTER_MS".to_string(), "250".to_string());
    config
        .child_env
        .insert("FAKE_EXIT_CODE".to_string(), "7".to_string());
    let supervisor = supervisor_with(&env, config);
    let rx = supervisor.subscribe();
    let handle = must(supervisor.start().await);

    let state = wait_for_state(&rx, |state| matches!(state, SupervisorState::Ready)).await;
    assert!(matches!(state, SupervisorState::Ready));
    // Wait through one crash + backoff + restart to Ready again.
    let state = wait_for_state(&rx, |state| {
        matches!(state, SupervisorState::Restarting { attempt: 1 })
    })
    .await;
    assert!(matches!(state, SupervisorState::Restarting { attempt: 1 }));
    let state = wait_for_state(&rx, |state| matches!(state, SupervisorState::Ready)).await;
    assert!(matches!(state, SupervisorState::Ready));

    must(handle.stop(Duration::from_secs(3)).await);
    common::assert_no_orphans(&env);
}

/// Budget exhaustion: past `max_restarts` within the window the state is
/// `Failed("restart budget exhausted")`, and `reset()` allows starting again.
#[tokio::test]
async fn restart_budget_exhausted_then_reset() {
    let env = TestEnv::new();
    let mut config = env.config();
    config.restart_budget = RestartBudget {
        max_restarts: 2,
        window: Duration::from_secs(600),
    };
    // Crashes immediately after binding, so every generation dies fast.
    config
        .child_env
        .insert("FAKE_EXIT_AFTER_MS".to_string(), "20".to_string());
    config
        .child_env
        .insert("FAKE_LISTEN_DELAY_MS".to_string(), "10".to_string());
    let supervisor = supervisor_with(&env, config);
    let rx = supervisor.subscribe();
    let handle = must(supervisor.start().await);

    let state = wait_for_state(&rx, |state| {
        matches!(
            state,
            SupervisorState::Failed { .. } | SupervisorState::Stopped
        )
    })
    .await;
    match &state {
        SupervisorState::Failed { reason } => {
            assert!(
                reason.contains("restart budget exhausted"),
                "unexpected failure: {reason}"
            );
        }
        other => panic!("expected budget-exhausted Failed, got {other:?}"),
    }
    assert!(supervisor.is_budget_exhausted());

    // reset() clears it and start() works again.
    supervisor.reset().await;
    assert!(!supervisor.is_budget_exhausted());
    let handle2 = must(supervisor.start().await);
    let state = wait_for_state(&rx, |state| {
        matches!(
            state,
            SupervisorState::Ready | SupervisorState::Failed { .. }
        )
    })
    .await;
    // The fresh generation crashes again after 20 ms — the point is that start() was
    // allowed at all.
    assert!(
        matches!(
            state,
            SupervisorState::Ready | SupervisorState::Failed { .. }
        ),
        "{state:?}"
    );
    must(handle2.stop(Duration::from_millis(50)).await);
    let _ = handle;
    common::assert_no_orphans(&env);
}

/// Stale `postmaster.pid`: a dead pid is cleaned (with event); a live pid is refused
/// and the file and process stay untouched.
#[tokio::test]
async fn stale_lock_dead_pid_cleaned_live_pid_refused() {
    // Case 1: dead pid → cleaned.
    let env = TestEnv::new();
    let supervisor = supervisor(&env);
    // Subscribe BEFORE start: the stale-lock events are published during it.
    let mut events = supervisor.events();
    must(std::fs::create_dir_all(&env.data_dir));
    must(std::fs::write(
        env.data_dir.join("postmaster.pid"),
        "999999999\n/data\n1\n",
    ));
    let handle = must(supervisor.start().await);
    // The fake rewrites postmaster.pid when it starts; assert the event instead of
    // the file state.
    let mut cleaned = false;
    while let Ok(event) = events.try_recv() {
        if matches!(
            event,
            SupervisorEvent::StaleLockCleaned {
                pid: 999_999_999,
                ..
            }
        ) {
            cleaned = true;
        }
    }
    assert!(cleaned, "StaleLockCleaned(999999999) event missing");
    must(handle.stop(Duration::from_secs(3)).await);
    common::assert_no_orphans(&env);

    // Case 2: our own live pid → refused, file untouched.
    let env2 = TestEnv::new();
    let supervisor2 = supervisor_with(&env2, env2.config());
    must(std::fs::create_dir_all(&env2.data_dir));
    let my_pid = std::process::id();
    must(std::fs::write(
        env2.data_dir.join("postmaster.pid"),
        format!("{my_pid}\n/data\n5433\n"),
    ));
    match supervisor2.start().await {
        Err(SupervisorError::LivePostmaster { pid, path }) => {
            assert_eq!(pid, my_pid);
            assert!(path.ends_with("postmaster.pid"));
        }
        Ok(_) => panic!("expected LivePostmaster refusal, but start succeeded"),
        Err(error) => panic!("expected LivePostmaster refusal, got {error:?}"),
    }
    let contents = std::fs::read_to_string(env2.data_dir.join("postmaster.pid"))
        .unwrap_or_else(|error| panic!("pid file must be untouched: {error}"));
    assert!(contents.starts_with(&my_pid.to_string()));
    assert_eq!(std::process::id(), my_pid, "the live process was untouched");
}

/// A corrupt pid file is refused (never deleted blindly).
#[tokio::test]
async fn corrupt_pid_file_refused() {
    let env = TestEnv::new();
    let supervisor = supervisor(&env);
    must(std::fs::create_dir_all(&env.data_dir));
    must(std::fs::write(
        env.data_dir.join("postmaster.pid"),
        "garbage\n",
    ));
    match supervisor.start().await {
        Err(SupervisorError::CorruptPidFile { .. }) => {}
        Ok(_) => panic!("expected CorruptPidFile, but start succeeded"),
        Err(error) => panic!("expected CorruptPidFile, got {error:?}"),
    }
    assert!(
        env.data_dir.join("postmaster.pid").is_file(),
        "the corrupt file must not be deleted by the refusal"
    );
}

/// Starting twice is an InvalidState error.
#[tokio::test]
async fn double_start_is_invalid_state() {
    let env = TestEnv::new();
    let supervisor = supervisor(&env);
    let handle = must(supervisor.start().await);
    let second = supervisor.start().await;
    match second {
        Err(SupervisorError::InvalidState { .. }) => {}
        Ok(_) => panic!("expected InvalidState, but the second start succeeded"),
        Err(error) => panic!("expected InvalidState, got {error:?}"),
    }
    let rx = supervisor.subscribe();
    let _ = wait_for_state(&rx, |state| matches!(state, SupervisorState::Ready)).await;
    must(handle.stop(Duration::from_secs(3)).await);
    common::assert_no_orphans(&env);
}

/// A stop requested from a separate task ends the round gracefully (the service
/// wrapper shape: one task waits on ready, another stops).
#[tokio::test]
async fn stop_from_other_task() {
    let env = TestEnv::new();
    let supervisor = supervisor(&env);
    let rx = supervisor.subscribe();
    let handle = must(supervisor.start().await);
    let h2 = handle;
    let (tx, mut rxstop) = tokio::sync::mpsc::unbounded_channel::<()>();
    let join = tokio::spawn(async move {
        let _ = rxstop.recv().await;
        h2.stop(Duration::from_secs(3)).await
    });
    let _ = tx.send(());
    let stopped = join
        .await
        .unwrap_or_else(|error| panic!("stop task: {error}"));
    assert!(matches!(must(stopped), StoppedHow::Graceful));
    let final_state = wait_for_state(&rx, |state| {
        matches!(
            state,
            SupervisorState::Stopped | SupervisorState::Failed { .. }
        )
    })
    .await;
    assert!(
        matches!(final_state, SupervisorState::Stopped),
        "{final_state:?}"
    );
    common::assert_no_orphans(&env);
}

/// The fake bundle's environment is passed through `child_env`, never through process
/// globals: two supervisors can run with different fakes concurrently.
#[tokio::test]
async fn child_env_is_per_supervisor() {
    let env_one = TestEnv::new();
    let env_two = TestEnv::new();
    let mut config_one = env_one.config();
    let mut config_two = env_two.config();
    config_one
        .child_env
        .insert("FAKE_LISTEN_DELAY_MS".to_string(), "60".to_string());
    config_two
        .child_env
        .insert("FAKE_LISTEN_DELAY_MS".to_string(), "0".to_string());
    let supervisor_one = supervisor_with(&env_one, config_one);
    let supervisor_two = supervisor_with(&env_two, config_two);
    let handle_one = must(supervisor_one.start().await);
    let handle_two = must(supervisor_two.start().await);
    must(handle_two.ready().await);
    must(handle_one.ready().await);
    must(handle_one.stop(Duration::from_secs(3)).await);
    must(handle_two.stop(Duration::from_secs(3)).await);
    common::assert_no_orphans(&env_one);
    common::assert_no_orphans(&env_two);
}

/// Events carry timestamps from the injected clock.
#[tokio::test]
async fn events_carry_clock_timestamps() {
    let env = TestEnv::new();
    let clock = common::ManualClock::new();
    let bound: Arc<dyn spot_pg_supervisor::clock::Clock> = clock.clone();
    let supervisor = Supervisor::new(env.layout(), env.settings(), env.config()).with_clock(bound);
    // Subscribe BEFORE start so the pre-start events are captured too.
    let mut events = supervisor.events();
    clock.advance(42_000);
    let handle = must(supervisor.start().await);
    // Drain until a StateChanged arrives; its timestamp must reflect the clock.
    let mut stamped = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(200), events.recv()).await {
            Ok(Ok(SupervisorEvent::StateChanged { at_ms, .. })) => {
                assert!(at_ms >= 42_000, "timestamp {at_ms} predates the clock");
                stamped = true;
                break;
            }
            Ok(Ok(_)) => continue,
            Ok(Err(_)) | Err(_) => break,
        }
    }
    assert!(stamped, "no timestamped StateChanged event seen");
    must(handle.stop(Duration::from_secs(3)).await);
    common::assert_no_orphans(&env);
}
