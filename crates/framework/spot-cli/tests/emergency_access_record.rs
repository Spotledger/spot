//! WP-IAM-08.T16: actions taken under an emergency grant are recorded once, only for a user with an
//! active grant, retried when the first try fails, and carry a type and an id, never a payload.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use spot_cli::services::emergency_access::actions::{
    ActionRecorder, EmergencyActions, GrantActions, QUEUE_MAX,
};
use spot_cli::services::emergency_access::model::EmergencyError;
use spot_http::api::service::BoxFuture;

#[derive(Default)]
struct Fake {
    active: Mutex<Vec<(String, String)>>,
    recorded: Mutex<Vec<(String, String, String, String)>>,
    failing: AtomicBool,
    unreadable: AtomicBool,
}

impl GrantActions for Fake {
    fn active_grants<'a>(
        &'a self,
        user: &'a str,
    ) -> BoxFuture<'a, Result<Vec<String>, EmergencyError>> {
        Box::pin(async move {
            if self.unreadable.load(Ordering::SeqCst) {
                return Err(EmergencyError::new("emergency_access_host.unavailable"));
            }
            Ok(self
                .active
                .lock()
                .unwrap()
                .iter()
                .filter(|(u, _)| u == user)
                .map(|(_, id)| id.clone())
                .collect())
        })
    }
    fn record<'a>(
        &'a self,
        id: &'a str,
        user: &'a str,
        action: &'a str,
        detail: &'a str,
    ) -> BoxFuture<'a, Result<(), EmergencyError>> {
        Box::pin(async move {
            if self.failing.load(Ordering::SeqCst) {
                return Err(EmergencyError::new("emergency_access_host.unavailable"));
            }
            self.recorded.lock().unwrap().push((
                id.to_owned(),
                user.to_owned(),
                action.to_owned(),
                detail.to_owned(),
            ));
            Ok(())
        })
    }
}

trait Recorded {
    fn grants_recorded(&self) -> Vec<(String, String, String, String)>;
}

impl Recorded for ActionRecorder<Fake> {
    fn grants_recorded(&self) -> Vec<(String, String, String, String)> {
        self.grants().recorded.lock().unwrap().clone()
    }
}

fn rig() -> ActionRecorder<Fake> {
    let fake = Fake::default();
    fake.active
        .lock()
        .unwrap()
        .push(("alice".into(), "g1".into()));
    ActionRecorder::new(fake)
}

#[tokio::test]
async fn an_action_under_an_active_grant_is_recorded_once_with_type_and_id_only() {
    let rec = rig();
    EmergencyActions::record(&rec, "alice", "update", "sales_invoice", "INV-1").await;
    let out = rec.grants_recorded();
    assert_eq!(out.len(), 1);
    assert_eq!(
        out[0],
        (
            "g1".into(),
            "alice".into(),
            "update".into(),
            "sales_invoice:INV-1".into()
        )
    );
    assert_eq!(rec.pending(), 0);
}

#[tokio::test]
async fn none_without_an_active_grant() {
    let rec = rig();
    EmergencyActions::record(&rec, "bob", "update", "sales_invoice", "INV-1").await;
    assert!(rec.grants_recorded().is_empty());
    assert_eq!(rec.pending(), 0);
}

#[tokio::test]
async fn a_failed_recording_is_queued_and_retried_once() {
    let rec = rig();
    rec.grants().failing.store(true, Ordering::SeqCst);
    EmergencyActions::record(&rec, "alice", "delete", "sales_invoice", "INV-2").await;
    assert_eq!(rec.pending(), 1);
    assert_eq!(rec.retry().await, 0);
    assert_eq!(rec.pending(), 1);
    rec.grants().failing.store(false, Ordering::SeqCst);
    assert_eq!(rec.retry().await, 1);
    assert_eq!(rec.pending(), 0);
    assert_eq!(rec.grants_recorded().len(), 1);
    assert_eq!(rec.retry().await, 0);
    assert_eq!(rec.grants_recorded().len(), 1, "recorded once, not again");
}

#[tokio::test]
async fn unreadable_grants_queue_the_action_and_the_queue_is_bounded() {
    let rec = rig();
    rec.grants().unreadable.store(true, Ordering::SeqCst);
    for i in 0..(QUEUE_MAX + 5) {
        EmergencyActions::record(&rec, "alice", "read", "doc", &i.to_string()).await;
    }
    assert_eq!(rec.pending(), QUEUE_MAX);
    assert_eq!(rec.dropped(), 5);
    rec.grants().unreadable.store(false, Ordering::SeqCst);
    assert_eq!(rec.retry().await, QUEUE_MAX);
}
