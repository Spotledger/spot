//! The quarantine admin's rules (WP-RTB-03.T3, T9), over an in-memory backend; no database. The
//! rules that depend on the library's store, the real permission engine and the conditions are
//! tested in `quarantine_pg.rs` and below with the library's in-memory store.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::sync::Mutex;

use async_trait::async_trait;
use spot_cli::services::quarantine::condition::{ConditionPort, QuarantinedSync};
use spot_cli::services::quarantine::health::{self, DriftSource, LagSource};
use spot_cli::services::quarantine::{
    BackendError, CloseGuard, DISCARD_PERMISSION, Decision, GuardedAdmin, Item, MAX_PAGE,
    QuarantineBackend, ReadScope, Refusal, State,
};

struct Backend {
    items: Mutex<Vec<Item>>,
    log: Mutex<Vec<String>>,
    down: bool,
}

impl Backend {
    fn new(items: Vec<Item>) -> Self {
        Self {
            items: Mutex::new(items),
            log: Mutex::new(Vec::new()),
            down: false,
        }
    }

    fn log(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }
}

#[async_trait]
impl QuarantineBackend for Backend {
    async fn list(
        &self,
        tenant: &str,
        _after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Item>, BackendError> {
        assert_eq!(tenant, "acme");
        if self.down {
            return Err(BackendError::Unavailable);
        }
        Ok(self
            .items
            .lock()
            .unwrap()
            .iter()
            .take(limit)
            .cloned()
            .collect())
    }

    async fn get(&self, tenant: &str, id: &str) -> Result<Option<Item>, BackendError> {
        if self.down {
            return Err(BackendError::Unavailable);
        }
        Ok((tenant == "acme")
            .then(|| {
                self.items
                    .lock()
                    .unwrap()
                    .iter()
                    .find(|i| i.id == id)
                    .cloned()
            })
            .flatten())
    }

    async fn image(&self, _t: &str, _id: &str) -> Result<Option<String>, BackendError> {
        Ok(Some(r#"{"name":"A","cost":12}"#.to_owned()))
    }

    async fn transition(
        &self,
        _t: &str,
        id: &str,
        to: State,
        decision: Decision,
        actor: &str,
        reason: Option<&str>,
    ) -> Result<(), BackendError> {
        self.log.lock().unwrap().push(format!(
            "{} {id} {actor} {}",
            decision.kind(),
            reason.unwrap_or("-")
        ));
        if let Some(i) = self.items.lock().unwrap().iter_mut().find(|i| i.id == id) {
            i.state = to;
        }
        Ok(())
    }

    async fn summary(&self, _t: &str) -> Result<Vec<Item>, BackendError> {
        if self.down {
            return Err(BackendError::Unavailable);
        }
        Ok(self.items.lock().unwrap().clone())
    }
}

struct Guard(Result<bool, String>);
impl CloseGuard for Guard {
    fn allows(&self, _a: &str, permission: &str) -> Result<bool, String> {
        assert_eq!(permission, DISCARD_PERMISSION);
        self.0.clone()
    }
}

struct Scope;
impl ReadScope for Scope {
    fn may_read(&self, _a: &str, doctype: &str) -> bool {
        doctype != "Payroll"
    }
    fn may_read_field(&self, _a: &str, _d: &str, field: &str) -> bool {
        field != "cost"
    }
}

fn item(id: &str, doctype: &str, state: State, age: i64) -> Item {
    Item {
        id: id.into(),
        doctype: doctype.into(),
        reason: "shape".into(),
        node: "edge-1#b1".into(),
        record: format!("{doctype}:{id}"),
        state,
        age_secs: age,
    }
}

fn items() -> Vec<Item> {
    vec![
        item("a", "Invoice", State::Held, 2 * 86_400 + 100),
        item("b", "Payroll", State::Held, 10),
        item("c", "Invoice", State::Parked, 60),
    ]
}

fn admin(allow: Result<bool, String>) -> GuardedAdmin<Backend, Guard, Scope> {
    GuardedAdmin::new(Backend::new(items()), Guard(allow), Scope, "acme")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_listing_counts_but_does_not_show_what_the_principal_cannot_read() {
    let page = admin(Ok(true)).list("u", None, 10_000).await.unwrap();
    assert_eq!(page.items.len(), 2);
    assert_eq!(page.restricted, 1);
    const { assert!(MAX_PAGE == 100) };
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inspect_hides_unreadable_fields_and_does_not_reveal_restricted_items() {
    let admin = admin(Ok(true));
    let (_, view) = admin.inspect("u", "a").await.unwrap();
    let view = view.unwrap();
    assert!(view.contains("\"cost\":\"hidden\"") && view.contains("\"name\":\"A\""));
    assert_eq!(
        admin.inspect("u", "b").await.unwrap_err(),
        Refusal::NotFound
    );
    assert_eq!(
        admin.inspect("u", "missing").await.unwrap_err(),
        Refusal::NotFound
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn discard_needs_the_permission_and_a_reason_and_fails_closed_on_an_engine_error() {
    let denied = admin(Ok(false));
    assert_eq!(
        denied.discard("u", "a", "dup").await.unwrap_err(),
        Refusal::Forbidden
    );
    let broken = admin(Err("engine down".into()));
    assert_eq!(
        broken.discard("u", "a", "dup").await.unwrap_err(),
        Refusal::Forbidden
    );
    let ok = admin(Ok(true));
    assert_eq!(
        ok.discard("u", "a", " ").await.unwrap_err(),
        Refusal::ReasonRequired
    );
    ok.discard("u", "a", "duplicate").await.unwrap();
    assert_eq!(
        ok.backend().log().as_slice(),
        ["quarantine_discarded a u duplicate"]
    );
    assert_eq!(
        ok.discard("u", "a", "again").await.unwrap_err(),
        Refusal::WrongState
    );
    assert!(denied.backend().log().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn release_applies_to_parked_items_only_and_retry_is_audited() {
    let admin = admin(Ok(true));
    assert_eq!(
        admin.release("u", "a").await.unwrap_err(),
        Refusal::WrongState
    );
    admin.release("u", "c").await.unwrap();
    admin.retry("u", "a").await.unwrap();
    assert_eq!(
        admin.backend().log().as_slice(),
        ["quarantine_released c u -", "quarantine_retried a u -"]
    );
    assert_eq!(
        admin.release("u", "b").await.unwrap_err(),
        Refusal::NotFound
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_readiness_line_counts_open_items_and_the_oldest_age() {
    assert_eq!(
        admin(Ok(true)).readiness().await,
        "quarantine: 3 items (oldest 2d)"
    );
    let quiet = GuardedAdmin::new(Backend::new(vec![]), Guard(Ok(true)), Scope, "acme");
    assert_eq!(quiet.readiness().await, "quarantine: 0 items");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_store_that_cannot_answer_is_a_refusal_never_an_empty_queue() {
    let mut backend = Backend::new(items());
    backend.down = true;
    let down = GuardedAdmin::new(backend, Guard(Ok(true)), Scope, "acme");
    assert_eq!(
        down.list("u", None, 10).await.unwrap_err(),
        Refusal::Unavailable
    );
    assert_eq!(
        down.inspect("u", "a").await.unwrap_err(),
        Refusal::Unavailable
    );
    assert_eq!(
        down.discard("u", "a", "dup").await.unwrap_err(),
        Refusal::Unavailable
    );
    assert_eq!(down.readiness().await, "quarantine: unavailable");
    assert_eq!(Refusal::Unavailable.code(), "quarantine_host.unavailable");
}

// ---- the QUARANTINED condition ----------------------------------------------------------

#[derive(Default)]
struct Port {
    raised: Mutex<BTreeSet<String>>,
    refuse: Mutex<bool>,
}

#[async_trait]
impl ConditionPort for Port {
    async fn raise(
        &self,
        subject: &str,
        node: &str,
        record: &str,
        cause: &str,
    ) -> Result<(), String> {
        assert_eq!(node, "edge-1#b1");
        assert!(record.contains(subject) || !record.is_empty());
        assert_eq!(cause, "shape");
        if *self.refuse.lock().unwrap() {
            return Err("engine stopped".into());
        }
        self.raised.lock().unwrap().insert(subject.to_owned());
        Ok(())
    }

    async fn clear(&self, subject: &str) -> Result<(), String> {
        self.raised.lock().unwrap().remove(subject);
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_condition_is_raised_for_held_items_and_cleared_when_they_leave() {
    let port = Port::default();
    let sync = QuarantinedSync::new();
    let mut all = items();
    let first = sync.reconcile(&all, &port).await;
    // Only held items: the parked dependent waits behind its parent.
    assert_eq!(first.raised, vec!["a".to_owned(), "b".to_owned()]);
    assert_eq!(sync.raised(), 2);
    // Nothing changed: nothing is sent again.
    let quiet = sync.reconcile(&all, &port).await;
    assert!(quiet.raised.is_empty() && quiet.cleared.is_empty());
    // Item a is discarded: its condition clears; the parked item becomes held: raised.
    all[0].state = State::Discarded;
    all[2].state = State::Held;
    let moved = sync.reconcile(&all, &port).await;
    assert_eq!(moved.raised, vec!["c".to_owned()]);
    assert_eq!(moved.cleared, vec!["a".to_owned()]);
    assert_eq!(
        port.raised
            .lock()
            .unwrap()
            .iter()
            .cloned()
            .collect::<Vec<_>>(),
        vec!["b".to_owned(), "c".to_owned()]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_raise_is_tried_again_next_time() {
    let port = Port::default();
    let sync = QuarantinedSync::new();
    *port.refuse.lock().unwrap() = true;
    let failed = sync.reconcile(&items(), &port).await;
    assert_eq!(failed.failed.len(), 2);
    assert_eq!(sync.raised(), 0);
    *port.refuse.lock().unwrap() = false;
    let retried = sync.reconcile(&items(), &port).await;
    assert_eq!(retried.raised.len(), 2);
}

// ---- the readiness line with replication health ------------------------------------------

struct Lag(Option<u64>);
impl LagSource for Lag {
    fn lag_millis(&self) -> Option<u64> {
        self.0
    }
}

struct Drift(Option<u64>);
impl DriftSource for Drift {
    fn open_drift(&self) -> Option<u64> {
        self.0
    }
}

#[test]
fn lag_and_drift_are_appended_when_their_ports_answer() {
    let base = "quarantine: 3 items (oldest 2d)";
    assert_eq!(health::line(base, None, None), base);
    assert_eq!(
        health::line(base, Some(&Lag(Some(200))), Some(&Drift(Some(0)))),
        "quarantine: 3 items (oldest 2d), lag 0.2s, drift 0"
    );
    assert_eq!(
        health::line(base, Some(&Lag(None)), Some(&Drift(Some(4)))),
        "quarantine: 3 items (oldest 2d), drift 4"
    );
    assert_eq!(health::seconds(12_049), "12.0s");
}
