//! WP-AI-07.T7: the quarantine part (over an in-memory `QuarantineBackend` that records every
//! decision) and the edge part (over an in-memory `EdgeDesk`). The ops block on the runtime handle,
//! so each runs on a blocking thread, as the remedy host calls them.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use spot_cli::services::quarantine::{BackendError, Decision, Item, QuarantineBackend, State};
use spot_cli::services::world_access::edge::DeskEdgePart;
use spot_cli::services::world_access::edge::{
    DeskFuture, EdgeDesk, EdgeState, from_parts as edge_from_parts,
};
use spot_cli::services::world_access::parts::{EdgePart, QuarantinePart};
use spot_cli::services::world_access::quarantine::{
    BackendQuarantinePart, from_parts as quarantine_from_parts, state_of,
};
use spot_remedies::{EdgeRef, QuarantineRef, QuarantineState};
use tokio::runtime::Handle;

async fn blocking<R: Send + 'static>(work: impl FnOnce() -> R + Send + 'static) -> R {
    tokio::task::spawn_blocking(work).await.unwrap()
}

// ---------------------------------------------------------------------------------------------
// quarantine
// ---------------------------------------------------------------------------------------------

fn item(id: &str, state: State) -> Item {
    Item {
        id: id.to_owned(),
        doctype: "Invoice".to_owned(),
        reason: "apply_refused".to_owned(),
        node: "edge-1".to_owned(),
        record: "r".to_owned(),
        state,
        age_secs: 60,
    }
}

/// An in-memory store that records every transition as `actor|decision|id`.
struct Memory {
    items: Mutex<Vec<Item>>,
    log: Mutex<Vec<String>>,
    conflict: Mutex<bool>,
}

impl Memory {
    fn new(items: Vec<Item>) -> Arc<Self> {
        Arc::new(Self {
            items: Mutex::new(items),
            log: Mutex::new(Vec::new()),
            conflict: Mutex::new(false),
        })
    }

    fn log(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }

    fn state(&self, id: &str) -> State {
        self.items
            .lock()
            .unwrap()
            .iter()
            .find(|item| item.id == id)
            .unwrap()
            .state
    }
}

#[async_trait]
impl QuarantineBackend for Memory {
    async fn list(
        &self,
        tenant: &str,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Item>, BackendError> {
        if tenant != "site-1" {
            return Ok(Vec::new());
        }
        let items = self.items.lock().unwrap();
        let open: Vec<Item> = items
            .iter()
            .filter(|item| !matches!(item.state, State::Closed | State::Discarded))
            .cloned()
            .collect();
        let start = match after {
            Some(id) => open
                .iter()
                .position(|item| item.id == id)
                .map_or(0, |at| at + 1),
            None => 0,
        };
        Ok(open.into_iter().skip(start).take(limit).collect())
    }

    async fn get(&self, tenant: &str, id: &str) -> Result<Option<Item>, BackendError> {
        if tenant != "site-1" {
            return Ok(None);
        }
        Ok(self
            .items
            .lock()
            .unwrap()
            .iter()
            .find(|item| item.id == id)
            .cloned())
    }

    async fn image(&self, _tenant: &str, _id: &str) -> Result<Option<String>, BackendError> {
        Ok(None)
    }

    async fn transition(
        &self,
        _tenant: &str,
        id: &str,
        to: State,
        decision: Decision,
        actor: &str,
        _reason: Option<&str>,
    ) -> Result<(), BackendError> {
        if *self.conflict.lock().unwrap() {
            return Err(BackendError::Conflict);
        }
        let mut items = self.items.lock().unwrap();
        let found = items.iter_mut().find(|item| item.id == id).unwrap();
        found.state = to;
        self.log
            .lock()
            .unwrap()
            .push(format!("{actor}|{}|{id}", decision.kind()));
        Ok(())
    }

    async fn summary(&self, _tenant: &str) -> Result<Vec<Item>, BackendError> {
        Ok(self.items.lock().unwrap().clone())
    }
}

fn quarantine_world() -> Arc<Memory> {
    Memory::new(vec![
        item("q1", State::Held),
        item("q2", State::Parked),
        item("q3", State::Retrying),
        item("q4", State::Closed),
    ])
}

fn quarantine_part(memory: &Arc<Memory>) -> Arc<BackendQuarantinePart> {
    Arc::new(BackendQuarantinePart::new(
        Arc::clone(memory) as Arc<dyn QuarantineBackend>,
        "site-1",
        Handle::current(),
    ))
}

fn q(id: &str) -> QuarantineRef {
    QuarantineRef::new(id)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn held_lists_held_and_parked_items_and_states_map() {
    let memory = quarantine_world();
    let part = quarantine_part(&memory);
    let (held, q3, q4, none) = blocking(move || {
        let ops = part.live();
        (
            ops.held(),
            ops.item(&q("q3")),
            ops.item(&q("q4")),
            ops.item(&q("zz")),
        )
    })
    .await;
    assert_eq!(held, vec![q("q1"), q("q2")]);
    assert_eq!(q3.unwrap().state, QuarantineState::Retrying);
    assert_eq!(q4.unwrap().state, QuarantineState::Released);
    assert!(none.is_none());
    assert_eq!(state_of(State::Parked), QuarantineState::Held);
    assert_eq!(state_of(State::Discarded), QuarantineState::Discarded);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_retry_runs_as_the_remedys_actor_and_records_the_decision() {
    let memory = quarantine_world();
    let part = quarantine_part(&memory);
    let change = blocking(move || part.live_as("agent:agent-t2").retry(&q("q1")).unwrap()).await;
    assert_eq!(change.before, QuarantineState::Held);
    assert_eq!(change.after, QuarantineState::Retrying);
    assert_eq!(memory.state("q1"), State::Retrying);
    assert_eq!(
        memory.log(),
        vec!["agent:agent-t2|quarantine_retried|q1".to_owned()]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_an_actor_every_change_is_refused() {
    let memory = quarantine_world();
    let part = quarantine_part(&memory);
    let refused = blocking(move || part.live().retry(&q("q1")))
        .await
        .unwrap_err();
    assert!(refused.detail.contains("acting identity"), "{refused}");
    assert!(memory.log().is_empty());
    assert_eq!(memory.state("q1"), State::Held);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_item_that_is_not_held_or_changed_meanwhile_is_refused() {
    let memory = quarantine_world();
    let part = quarantine_part(&memory);
    let (retrying, closed, missing) = {
        let part = Arc::clone(&part);
        blocking(move || {
            let mut ops = part.live_as("agent:a");
            (
                ops.retry(&q("q3")),
                ops.retry(&q("q4")),
                ops.retry(&q("zz")),
            )
        })
        .await
    };
    assert!(retrying.unwrap_err().detail.contains("not held"));
    assert!(closed.unwrap_err().detail.contains("not held"));
    assert!(missing.unwrap_err().detail.contains("no quarantined item"));
    assert!(memory.log().is_empty());

    *memory.conflict.lock().unwrap() = true;
    let conflict = blocking(move || part.live_as("agent:a").retry(&q("q1"))).await;
    assert!(conflict.unwrap_err().detail.contains("changed meanwhile"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restore_is_not_supported_except_to_the_state_the_item_has() {
    let memory = quarantine_world();
    let part = quarantine_part(&memory);
    let outcome = blocking(move || {
        let mut ops = part.live_as("agent:a");
        let change = ops.retry(&q("q1")).unwrap();
        let undone = ops.restore(&change);
        // The item already has the state a no-op restore names.
        let mut same = change.clone();
        same.before = QuarantineState::Retrying;
        (undone, ops.restore(&same))
    })
    .await;
    assert_eq!(
        outcome.0.unwrap_err().detail,
        "not supported by the quarantine port"
    );
    assert!(outcome.1.is_ok());
    assert_eq!(memory.log().len(), 1, "restore wrote nothing");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rehearsal_retry_never_writes_to_the_store() {
    let memory = quarantine_world();
    let part = quarantine_part(&memory);
    let after = blocking(move || {
        let mut ops = part.rehearsal().unwrap();
        assert_eq!(ops.held(), vec![q("q1"), q("q2")]);
        let change = ops.retry(&q("q1")).unwrap();
        assert_eq!(change.after, QuarantineState::Retrying);
        (ops.held(), part.live().held())
    })
    .await;
    assert_eq!(after.0, vec![q("q2")]);
    assert_eq!(
        after.1,
        vec![q("q1"), q("q2")],
        "the live store is unchanged"
    );
    assert!(memory.log().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_quarantine_part_with_no_backend_is_none() {
    let memory = quarantine_world();
    let backend = Arc::clone(&memory) as Arc<dyn QuarantineBackend>;
    assert!(quarantine_from_parts(None, "site-1", Some(Handle::current())).is_none());
    assert!(quarantine_from_parts(Some(Arc::clone(&backend)), "site-1", None).is_none());
    assert!(
        quarantine_from_parts(Some(Arc::clone(&backend)), "", Some(Handle::current())).is_none()
    );
    assert!(quarantine_from_parts(Some(backend), "site-1", Some(Handle::current())).is_some());
}

// ---------------------------------------------------------------------------------------------
// edge
// ---------------------------------------------------------------------------------------------

/// An in-memory desk: every resync brings the edge to its expected position and is counted.
struct Desk {
    edges: Mutex<Vec<EdgeState>>,
    resyncs: Mutex<usize>,
}

impl Desk {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            edges: Mutex::new(vec![
                EdgeState {
                    edge: "customer-1".to_owned(),
                    expected: 10,
                    actual: 7,
                },
                EdgeState {
                    edge: "customer-2".to_owned(),
                    expected: 4,
                    actual: 4,
                },
            ]),
            resyncs: Mutex::new(0),
        })
    }

    fn resyncs(&self) -> usize {
        *self.resyncs.lock().unwrap()
    }
}

impl EdgeDesk for Desk {
    fn edges(&self) -> DeskFuture<'_, Vec<EdgeState>> {
        Box::pin(
            async move { Ok::<Vec<EdgeState>, &'static str>(self.edges.lock().unwrap().clone()) },
        )
    }

    fn resync<'a>(&'a self, edge: &'a str) -> DeskFuture<'a, EdgeState> {
        Box::pin(async move {
            *self.resyncs.lock().unwrap() += 1;
            let mut edges = self.edges.lock().unwrap();
            match edges.iter_mut().find(|state| state.edge == edge) {
                Some(found) => {
                    found.actual = found.expected;
                    Ok::<EdgeState, &'static str>(found.clone())
                }
                None => Err("no such edge"),
            }
        })
    }
}

fn e(name: &str) -> EdgeRef {
    EdgeRef::new(name)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn drift_is_read_and_a_repair_reports_the_positions() {
    let desk = Desk::new();
    let part = Arc::new(DeskEdgePart::new(
        Arc::clone(&desk) as Arc<dyn EdgeDesk>,
        Handle::current(),
    ));
    let (drifted, view, change) = blocking(move || {
        let mut ops = part.live();
        let drifted = ops.drifted();
        let view = ops.edge(&e("customer-1")).unwrap();
        (drifted, view, ops.repair(&e("customer-1")).unwrap())
    })
    .await;
    assert_eq!(drifted, vec![e("customer-1")]);
    assert_eq!((view.expected, view.actual), (10, 7));
    assert_eq!((change.before, change.after), (7, 10));
    assert_eq!(desk.resyncs(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_edge_is_refused_and_a_restore_is_not_supported() {
    let desk = Desk::new();
    let part = Arc::new(DeskEdgePart::new(
        Arc::clone(&desk) as Arc<dyn EdgeDesk>,
        Handle::current(),
    ));
    let (unknown, restore, same) = blocking(move || {
        let mut ops = part.live();
        let unknown = ops.repair(&e("zz"));
        let change = ops.repair(&e("customer-1")).unwrap();
        let mut same = change.clone();
        same.before = change.after;
        (unknown, ops.restore(&change), ops.restore(&same))
    })
    .await;
    assert!(unknown.unwrap_err().detail.contains("no edge"));
    assert_eq!(
        restore.unwrap_err().detail,
        "not supported by the edge port"
    );
    assert!(same.is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rehearsal_resync_leaves_the_real_edge() {
    let desk = Desk::new();
    let part = Arc::new(DeskEdgePart::new(
        Arc::clone(&desk) as Arc<dyn EdgeDesk>,
        Handle::current(),
    ));
    let (rehearsed, live_actual) = blocking(move || {
        let mut ops = part.rehearsal().unwrap();
        let change = ops.repair(&e("customer-1")).unwrap();
        (
            change.after,
            part.live().edge(&e("customer-1")).unwrap().actual,
        )
    })
    .await;
    assert_eq!(rehearsed, 10);
    assert_eq!(live_actual, 7);
    assert_eq!(desk.resyncs(), 0, "a rehearsal never reaches the desk");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_site_with_no_edge_desk_has_no_edge_part() {
    let desk = Desk::new();
    assert!(edge_from_parts(None, Some(Handle::current())).is_none());
    assert!(edge_from_parts(Some(Arc::clone(&desk) as Arc<dyn EdgeDesk>), None).is_none());
    assert!(edge_from_parts(Some(desk as Arc<dyn EdgeDesk>), Some(Handle::current())).is_some());
}
