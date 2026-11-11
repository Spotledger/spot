//! `TokenVerifier::verify` is async (WP-PLT-08.T4): a verifier over a durable session store
//! awaits its lookup inside the connection task. A slow verifier is bounded by the same
//! 10 s auth window as a missing `auth` (4001), a gated one authenticates once released, and
//! the expiry re-check awaits a verifier that now rejects (4009).

#[allow(dead_code)]
mod support;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use spot_core::SiteId;
use spot_realtime::ws::{ExpiryWatch, Rejected, TokenVerifier};
use spot_realtime::{AllowAll, Principal};
use support::{ClientMessage, Harness, TEST_TOKEN, TEST_USER, expect_kind};
use tokio::sync::Notify;

type Verdict<'a> = Pin<Box<dyn Future<Output = Result<Principal, Rejected>> + Send + 'a>>;

/// Waits (optionally for ever) before accepting; tells the test when `verify` was entered.
struct DelayedVerifier {
    entered: Arc<Notify>,
    gate: Option<Arc<Notify>>,
}

impl TokenVerifier for DelayedVerifier {
    fn verify<'a>(&'a self, _site: &'a SiteId, _token: &'a str) -> Verdict<'a> {
        Box::pin(async move {
            self.entered.notify_one();
            match &self.gate {
                Some(gate) => gate.notified().await,
                None => std::future::pending::<()>().await,
            }
            Ok(Principal::user(TEST_USER))
        })
    }
}

/// Accepts the first call; later calls await a short sleep and then reject (a session that
/// was revoked in the durable store). Its watch fires when the test says so.
struct RevokedLaterVerifier {
    calls: AtomicUsize,
    fire: Arc<Notify>,
}

struct FireWatch(Arc<Notify>);

impl ExpiryWatch for FireWatch {
    fn wait(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move { self.0.notified().await })
    }
}

impl TokenVerifier for RevokedLaterVerifier {
    fn verify<'a>(&'a self, _site: &'a SiteId, _token: &'a str) -> Verdict<'a> {
        Box::pin(async move {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                return Ok(Principal::user(TEST_USER));
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
            Err(Rejected::new("revoked in the store"))
        })
    }

    fn expiry_watch(&self, _token: &str) -> Option<Arc<dyn ExpiryWatch>> {
        Some(Arc::new(FireWatch(Arc::clone(&self.fire))))
    }
}

async fn serve(verifier: Arc<dyn TokenVerifier>) -> Harness {
    support::start_with_verifier(verifier, Arc::default(), 25, Arc::new(AllowAll), 256).await
}

fn auth() -> ClientMessage {
    ClientMessage::Auth {
        token: TEST_TOKEN.to_owned(),
    }
}

#[tokio::test(start_paused = true)]
async fn a_verifier_slower_than_the_auth_window_closes_4001() {
    let entered = Arc::new(Notify::new());
    let harness = serve(Arc::new(DelayedVerifier {
        entered: Arc::clone(&entered),
        gate: None,
    }))
    .await;
    let client = support::connect(harness.addr).await;
    client.send_message(&auth()).await;
    // The verifier is running (and never finishes): only the auth window can end this.
    entered.notified().await;
    tokio::time::advance(Duration::from_secs(10)).await;
    assert_eq!(client.read_until_close().await, 4001);
}

#[tokio::test]
async fn a_verifier_that_awaits_a_gate_authenticates_once_released() {
    let entered = Arc::new(Notify::new());
    let gate = Arc::new(Notify::new());
    let harness = serve(Arc::new(DelayedVerifier {
        entered: Arc::clone(&entered),
        gate: Some(Arc::clone(&gate)),
    }))
    .await;
    let client = support::connect(harness.addr).await;
    client.send_message(&auth()).await;
    entered.notified().await;
    gate.notify_one();
    let ready = expect_kind(&client, "ready").await;
    assert_eq!(ready["user"], TEST_USER);
}

#[tokio::test]
async fn the_expiry_recheck_awaits_a_verifier_that_now_rejects_and_closes_4009() {
    let fire = Arc::new(Notify::new());
    let harness = serve(Arc::new(RevokedLaterVerifier {
        calls: AtomicUsize::new(0),
        fire: Arc::clone(&fire),
    }))
    .await;
    let (client, _ready) = support::connect_and_auth(&harness, TEST_TOKEN).await;
    fire.notify_one();
    assert_eq!(client.read_until_close().await, 4009);
}
