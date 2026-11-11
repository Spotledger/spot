//! The first-start extension gate (ADR-087 sections 4.3, 4.7 and 11.3 S2): `postgis` is
//! provisioned once the cluster is up, before `Ready` is published; the second start takes
//! the idempotent path; a failing provisioner leaves the supervisor `Failed` with `Ready`
//! never published.
//!
//! CI machines have no PostgreSQL: the bundle is the crate's fake one and the
//! `extension.install` adapter is [`common::FakeProvisioner`].

mod common;

use common::{FakeProvisioner, TestEnv, must};
use spot_pg_supervisor::Supervisor;
use spot_pg_supervisor::events::SupervisorEvent;
use spot_pg_supervisor::extensions::ExtensionProvisioner;
use spot_pg_supervisor::supervisor::{StoppedHow, SupervisorState};
use std::sync::Arc;
use std::time::Duration;

fn supervisor_with(env: &TestEnv, provisioner: &Arc<FakeProvisioner>) -> Supervisor {
    let shared: Arc<FakeProvisioner> = Arc::clone(provisioner);
    let adapter: Arc<dyn ExtensionProvisioner> = shared;
    Supervisor::new(env.layout(), env.settings(), env.config()).with_extension_provisioner(adapter)
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
            continue;
        }
    }
}

/// Provisioning happens once, after the cluster accepts connections and before `Ready`:
/// the provisioner observes `Starting`, and `ExtensionProvisioned` precedes `Ready` on the
/// event stream.
#[tokio::test]
async fn provision_runs_once_after_readiness_and_before_ready() {
    let env = TestEnv::new();
    let provisioner = FakeProvisioner::new();
    let supervisor = supervisor_with(&env, &provisioner);
    provisioner.watch(supervisor.subscribe());
    let mut events = supervisor.events();
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

    let calls = provisioner.calls();
    assert_eq!(calls, vec!["postgis".to_string()], "one call, for postgis");
    let at_call = provisioner.states_at_calls();
    assert_eq!(at_call.len(), 1, "the provisioner was called once");
    assert!(
        matches!(&at_call[0], Some(SupervisorState::Starting)),
        "the call must happen while starting (after the port accepts), not {:?}",
        at_call[0]
    );

    // Program order: the provisioning event precedes the Ready state change, so a client
    // that sees Ready also sees the extension event before it.
    let mut order = Vec::new();
    while let Ok(event) = events.try_recv() {
        match event {
            SupervisorEvent::ExtensionProvisioned {
                extension,
                already_present,
                ..
            } => {
                assert_eq!(extension, "postgis");
                assert!(!already_present, "the first call installs");
                order.push("provisioned");
            }
            SupervisorEvent::StateChanged { state, .. } => {
                if matches!(state, SupervisorState::Ready) {
                    order.push("ready");
                }
            }
            _ => {}
        }
    }
    assert_eq!(
        order,
        vec!["provisioned", "ready"],
        "event order: {order:?}"
    );

    must(handle.stop(Duration::from_secs(5)).await);
    common::assert_no_orphans(&env);
}

/// The second start takes the idempotent path: the call is made again (the platform
/// operation is `IF NOT EXISTS`-shaped) and answers "already present", and the supervisor
/// still reaches `Ready`.
#[tokio::test]
async fn second_start_takes_the_idempotent_path() {
    let env = TestEnv::new();
    let provisioner = FakeProvisioner::new();
    let supervisor = supervisor_with(&env, &provisioner);
    let rx = supervisor.subscribe();

    let handle = must(supervisor.start().await);
    let state = wait_for_state(&rx, |state| {
        matches!(
            state,
            SupervisorState::Ready | SupervisorState::Failed { .. }
        )
    })
    .await;
    assert!(
        matches!(state, SupervisorState::Ready),
        "first start: {state:?}"
    );
    must(handle.stop(Duration::from_secs(5)).await);
    let state = wait_for_state(&rx, |state| matches!(state, SupervisorState::Stopped)).await;
    assert!(matches!(state, SupervisorState::Stopped), "{state:?}");
    assert_eq!(provisioner.calls().len(), 1);

    // Second start: one more call, which reports the extension already present.
    let mut events = supervisor.events();
    let handle = must(supervisor.start().await);
    let state = wait_for_state(&rx, |state| {
        matches!(
            state,
            SupervisorState::Ready | SupervisorState::Failed { .. }
        )
    })
    .await;
    assert!(
        matches!(state, SupervisorState::Ready),
        "second start: {state:?}"
    );
    assert_eq!(
        provisioner.calls(),
        vec!["postgis".to_string(), "postgis".to_string()],
        "the second start provisions again, idempotently"
    );

    let mut second_receipt_already_present = None;
    while let Ok(event) = events.try_recv() {
        if let SupervisorEvent::ExtensionProvisioned {
            already_present,
            version,
            ..
        } = event
        {
            second_receipt_already_present = Some(already_present);
            assert_eq!(version, "3.6.2");
        }
    }
    assert_eq!(
        second_receipt_already_present,
        Some(true),
        "the second start must report the idempotent no-op"
    );

    must(handle.stop(Duration::from_secs(5)).await);
    common::assert_no_orphans(&env);
}

/// A failing provisioner is fatal: the state is `Failed` with a PostGIS-naming reason, an
/// `ExtensionProvisionFailed` event is published, and `Ready` is never published — there is
/// no switch, environment variable or fallback that softens it (ADR-087 principle 6).
#[tokio::test]
async fn provisioner_failure_fails_loudly_and_never_publishes_ready() {
    let env = TestEnv::new();
    // The host's typed refusal, as `extension.install` words a version failure.
    let provisioner = FakeProvisioner::failing(
        "migrate.extension_version: postgis 3.0 is below the required 3.6.2",
    );
    let supervisor = supervisor_with(&env, &provisioner);
    let rx = supervisor.subscribe();
    let handle = must(supervisor.start().await);
    let mut events = handle.events();

    // Nothing may ever be published as Ready, so watch every published state.
    let mut published = Vec::new();
    let mut waitee = rx.clone();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let final_state = loop {
        let state = waitee.borrow().clone();
        if published.last() != Some(&state) {
            published.push(state.clone());
        }
        if matches!(
            state,
            SupervisorState::Failed { .. } | SupervisorState::Stopped
        ) {
            break state;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the supervisor never reached a terminal state; published {published:?}"
        );
        let _ = tokio::time::timeout(Duration::from_millis(200), waitee.changed()).await;
    };

    assert!(
        !published.contains(&SupervisorState::Ready),
        "Ready was published despite the failure: {published:?}"
    );
    match &final_state {
        SupervisorState::Failed { reason } => {
            assert!(
                reason.contains("postgis"),
                "the reason must name PostGIS: {reason}"
            );
            assert!(
                reason.contains("below the required"),
                "the host's reason must survive: {reason}"
            );
        }
        other => panic!("expected Failed, got {other:?}"),
    }

    // The event stream carries the failure, naming the extension, and no Ready event.
    let mut failure_seen = false;
    while let Ok(event) = events.try_recv() {
        if let SupervisorEvent::StateChanged {
            state: SupervisorState::Ready,
            ..
        } = &event
        {
            panic!("a Ready state event was published despite the failure");
        }
        if let SupervisorEvent::ExtensionProvisionFailed {
            extension, reason, ..
        } = &event
        {
            assert_eq!(extension, "postgis");
            assert!(reason.contains("migrate.extension_version"), "{reason}");
            failure_seen = true;
        }
    }
    assert!(failure_seen, "ExtensionProvisionFailed event missing");

    // `ready()` reports the typed failure instead of succeeding, and the child is gone.
    match handle.ready().await {
        Err(spot_pg_supervisor::SupervisorError::FailedState { reason }) => {
            assert!(reason.contains("postgis"), "{reason}");
        }
        other => panic!("expected a failed ready(), got {other:?}"),
    }
    assert_eq!(provisioner.calls().len(), 1, "no retry loop on the gate");
    common::assert_no_orphans(&env);
}

/// Without an adapter the supervisor supervises PostgreSQL alone and still becomes ready:
/// that is the shape the snapshot and fault-harness supervisors use. It is not a desktop
/// path — the desktop service wrapper always installs the adapter (WP-PRD-03).
#[tokio::test]
async fn no_provisioner_supervises_without_the_gate() {
    let env = TestEnv::new();
    let supervisor = Supervisor::new(env.layout(), env.settings(), env.config());
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
    let stopped = must(handle.stop(Duration::from_secs(5)).await);
    assert!(matches!(stopped, StoppedHow::Graceful));
    common::assert_no_orphans(&env);
}

/// A crash restart does not provision twice: the gate belongs to the first `Ready` of the
/// round, and the restarted generation brings the same cluster back.
#[tokio::test]
async fn crash_restart_does_not_provision_again() {
    let env = TestEnv::new();
    let provisioner = FakeProvisioner::new();
    let mut config = env.config();
    config
        .child_env
        .insert("FAKE_EXIT_AFTER_MS".to_string(), "250".to_string());
    config
        .child_env
        .insert("FAKE_EXIT_CODE".to_string(), "7".to_string());
    let shared: Arc<FakeProvisioner> = Arc::clone(&provisioner);
    let adapter: Arc<dyn ExtensionProvisioner> = shared;
    let supervisor =
        Supervisor::new(env.layout(), env.settings(), config).with_extension_provisioner(adapter);
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
    // Through one crash, the backoff and the respawn to Ready again.
    let state = wait_for_state(&rx, |state| {
        matches!(state, SupervisorState::Restarting { .. })
    })
    .await;
    assert!(
        matches!(state, SupervisorState::Restarting { .. }),
        "{state:?}"
    );
    let state = wait_for_state(&rx, |state| {
        matches!(
            state,
            SupervisorState::Ready | SupervisorState::Failed { .. }
        )
    })
    .await;
    assert!(
        matches!(state, SupervisorState::Ready),
        "after restart: {state:?}"
    );

    assert_eq!(
        provisioner.calls(),
        vec!["postgis".to_string()],
        "a restart generation must not provision again"
    );

    must(handle.stop(Duration::from_secs(3)).await);
    common::assert_no_orphans(&env);
}
