//! Every policy and every answer of the gate, over a fake outbox head and a fake journal.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use axum::middleware::from_fn_with_state;
use axum::routing::{get, post};
use http_body_util::BodyExt;
use spot_durability_gate::{
    GateConfig, GateError, GateFuture, GateState, HeadSource, Journal, Kind, OnUnreachable, Policy,
    PolicyCell, Stats, gate,
};
use tower::ServiceExt;

struct Head(AtomicU64);

impl HeadSource for Head {
    fn head(&self) -> GateFuture<'_, Option<(u64, u64)>> {
        Box::pin(async move {
            match self.0.load(Ordering::SeqCst) {
                u64::MAX => None,
                seq => Some((1, seq)),
            }
        })
    }
}

struct FakeJournal {
    durable: AtomicU64,
    unreachable: AtomicBool,
    /// Becomes durable at this point when a wait starts (a store that answers late).
    arrives: AtomicU64,
}

impl Journal for FakeJournal {
    fn durable(&self) -> (u64, u64) {
        (1, self.durable.load(Ordering::SeqCst))
    }

    fn wait_durable(&self, position: (u64, u64), timeout: Duration) -> GateFuture<'_, bool> {
        Box::pin(async move {
            let arrives = self.arrives.load(Ordering::SeqCst);
            if arrives >= position.1 {
                self.durable.store(arrives, Ordering::SeqCst);
                return true;
            }
            tokio::time::sleep(timeout).await;
            false
        })
    }

    fn unreachable(&self) -> bool {
        self.unreachable.load(Ordering::SeqCst)
    }
}

struct Rig {
    app: Router,
    head: Arc<Head>,
    journal: Arc<FakeJournal>,
    handler_runs: Arc<AtomicU64>,
    state: Arc<GateState>,
    policy: Arc<PolicyCell>,
}

fn rig(kind: Kind, on_unreachable: OnUnreachable, journal_on: bool) -> Rig {
    let head = Arc::new(Head(AtomicU64::new(10)));
    let journal = Arc::new(FakeJournal {
        durable: AtomicU64::new(5),
        unreachable: AtomicBool::new(false),
        arrives: AtomicU64::new(0),
    });
    let policy = Arc::new(PolicyCell::new(Policy {
        kind,
        on_unreachable,
        journal_on,
    }));
    let state = Arc::new(GateState {
        policy: Arc::clone(&policy),
        head: head.clone(),
        journal: journal.clone(),
        standby: None,
        config: GateConfig {
            wait: Duration::from_millis(80),
            retry_after_secs: 7,
        },
        stats: Arc::new(Stats::default()),
    });
    let handler_runs = Arc::new(AtomicU64::new(0));
    let runs = Arc::clone(&handler_runs);
    let app = Router::new()
        .route(
            "/w",
            post(move || {
                let runs = Arc::clone(&runs);
                async move {
                    runs.fetch_add(1, Ordering::SeqCst);
                    "written"
                }
            })
            .put(|| async { "put" })
            .delete(|| async { "deleted" }),
        )
        .route("/r", get(|| async { "read" }))
        .route(
            "/bad",
            post(|| async { (StatusCode::UNPROCESSABLE_ENTITY, "no") }),
        )
        .route_layer(from_fn_with_state(Arc::clone(&state), gate));
    Rig {
        app,
        head,
        journal,
        handler_runs,
        state,
        policy,
    }
}

async fn call(rig: &Rig, method: Method, path: &str) -> (StatusCode, Option<String>, String) {
    let response = rig
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("x-request-id", "req-1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let retry = response
        .headers()
        .get("retry-after")
        .map(|v| v.to_str().unwrap().to_owned());
    let body = String::from_utf8(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    (status, retry, body)
}

#[tokio::test]
async fn local_and_no_journal_answer_at_once() {
    for (kind, on) in [(Kind::Local, true), (Kind::RemoteSync, false)] {
        let rig = rig(kind, OnUnreachable::Halt, on);
        rig.journal.unreachable.store(true, Ordering::SeqCst);
        let (status, _, body) = call(&rig, Method::POST, "/w").await;
        assert_eq!(
            (status, body.as_str()),
            (StatusCode::OK, "written"),
            "{kind:?}"
        );
    }
    let rig = rig(Kind::Local, OnUnreachable::Halt, true);
    call(&rig, Method::POST, "/w").await;
    assert_eq!(rig.state.stats.local_commit_durable_count(), 1);
}

#[tokio::test]
async fn remote_sync_waits_for_the_journal_and_answers_once_it_is_durable() {
    let rig = rig(Kind::RemoteSync, OnUnreachable::Degrade, true);
    rig.journal.arrives.store(10, Ordering::SeqCst);
    let (status, _, _) = call(&rig, Method::POST, "/w").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        rig.journal.durable(),
        (1, 10),
        "the response waited for the head"
    );
    // Already durable: no wait at all.
    rig.head.0.store(9, Ordering::SeqCst);
    let (status, _, _) = call(&rig, Method::PUT, "/w").await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn degrade_acknowledges_on_the_local_commit_after_the_grace_or_at_once_when_the_store_is_known_down()
 {
    let rig = rig(Kind::RemoteSync, OnUnreachable::Degrade, true);
    // The journal never reaches the head: after the wait the write is acknowledged locally.
    let (status, _, body) = call(&rig, Method::POST, "/w").await;
    assert_eq!((status, body.as_str()), (StatusCode::OK, "written"));
    assert_eq!(rig.state.stats.acked_on_local(), 1);
    // The shipper says the store is down: no wait.
    rig.journal.unreachable.store(true, Ordering::SeqCst);
    let started = std::time::Instant::now();
    let (status, _, _) = call(&rig, Method::DELETE, "/w").await;
    assert_eq!(status, StatusCode::OK);
    assert!(started.elapsed() < Duration::from_millis(60));
    assert_eq!(rig.state.stats.acked_on_local(), 2);
}

#[tokio::test]
async fn halt_refuses_before_the_handler_runs_and_resumes_when_the_store_returns() {
    let rig = rig(Kind::RemoteSync, OnUnreachable::Halt, true);
    rig.journal.unreachable.store(true, Ordering::SeqCst);
    let (status, retry, body) = call(&rig, Method::POST, "/w").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(retry.as_deref(), Some("7"));
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["error"]["code"], "server.unavailable");
    assert_eq!(json["error"]["details"]["code"], "journal.halted");
    assert_eq!(json["error"]["retryable"], true);
    assert_eq!(
        rig.handler_runs.load(Ordering::SeqCst),
        0,
        "the handler did not run"
    );
    assert_eq!(rig.state.stats.refused_before_handler(), 1);
    // Reads are never gated: a halted tenant can still be read.
    assert_eq!(call(&rig, Method::GET, "/r").await.0, StatusCode::OK);
    // The store returns: the gate resumes by itself.
    rig.journal.unreachable.store(false, Ordering::SeqCst);
    rig.journal.arrives.store(10, Ordering::SeqCst);
    assert_eq!(call(&rig, Method::POST, "/w").await.0, StatusCode::OK);
    assert_eq!(rig.handler_runs.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn an_unconfirmed_commit_answers_commit_outcome_unknown_without_leaking_a_position() {
    let rig = rig(Kind::RemoteSync, OnUnreachable::Halt, true);
    let (status, retry, body) = call(&rig, Method::POST, "/w").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(retry.as_deref(), Some("7"));
    assert_eq!(
        rig.handler_runs.load(Ordering::SeqCst),
        1,
        "the write is committed"
    );
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        json["error"]["details"]["code"],
        "journal.commit_outcome_unknown"
    );
    assert!(
        !body.contains("\"seq\"") && !body.contains("generation"),
        "{body}"
    );
    assert_eq!(rig.state.stats.commit_outcome_unknown(), 1);
}

#[tokio::test]
async fn a_head_that_cannot_be_read_is_an_unconfirmed_commit() {
    let rig = rig(Kind::RemoteSync, OnUnreachable::Halt, true);
    rig.head.0.store(u64::MAX, Ordering::SeqCst);
    let (status, _, body) = call(&rig, Method::POST, "/w").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(body.contains("journal.commit_outcome_unknown"));
}

#[tokio::test]
async fn reads_and_failed_writes_pass_untouched() {
    let rig = rig(Kind::RemoteSync, OnUnreachable::Halt, true);
    rig.journal.unreachable.store(false, Ordering::SeqCst);
    let (status, _, body) = call(&rig, Method::POST, "/bad").await;
    assert_eq!(
        (status, body.as_str()),
        (StatusCode::UNPROCESSABLE_ENTITY, "no")
    );
    assert_eq!(call(&rig, Method::GET, "/r").await.0, StatusCode::OK);
}

#[tokio::test]
async fn a_policy_change_takes_effect_on_the_next_request_with_its_rules() {
    let rig = rig(Kind::RemoteSync, OnUnreachable::Halt, true);
    rig.journal.unreachable.store(true, Ordering::SeqCst);
    assert_eq!(
        call(&rig, Method::POST, "/w").await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    let local = Policy {
        kind: Kind::Local,
        on_unreachable: OnUnreachable::Halt,
        journal_on: true,
    };
    // A downgrade is for the tenant administrator only, and is always flagged for the audit.
    assert_eq!(
        rig.policy.change(local, false, true, false).unwrap_err(),
        GateError::PermissionDenied
    );
    let change = rig.policy.change(local, true, true, false).unwrap();
    assert!(change.downgrade);
    assert_eq!(call(&rig, Method::POST, "/w").await.0, StatusCode::OK);
    // An upgrade needs a healthy shipper; replica-sync needs a standby.
    let remote = Policy {
        kind: Kind::RemoteSync,
        ..local
    };
    assert_eq!(
        rig.policy.change(remote, true, false, false).unwrap_err(),
        GateError::ShipperUnhealthy
    );
    let replica = Policy {
        kind: Kind::ReplicaSync,
        ..local
    };
    assert_eq!(
        rig.policy.change(replica, true, true, false).unwrap_err(),
        GateError::NoStandby
    );
    assert!(
        !rig.policy
            .change(remote, true, true, false)
            .unwrap()
            .downgrade
    );
}

#[tokio::test]
async fn the_p99_is_reported() {
    let rig = rig(Kind::RemoteSync, OnUnreachable::Degrade, true);
    rig.journal.arrives.store(10, Ordering::SeqCst);
    for _ in 0..5 {
        call(&rig, Method::POST, "/w").await;
        rig.journal.durable.store(5, Ordering::SeqCst);
    }
    assert!(rig.state.stats.p99_millis() < 1_000);
}
