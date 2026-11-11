//! The fixtures every test shares: a small DocType set, an in-memory document engine and a way
//! to run one Document node.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

pub mod replay;

use std::future::Future;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use serde_json::Value as Json;
use spot_app_api::prelude::{Name, OffsetDateTime, SiteId};
use spot_bpm_documents::DocumentNodeHandler;
use spot_bpm_engine::{
    CaseId, IdempotencyKey, NodeCtx, NodeHandler, NodeKind, NodeOutcome, OutgoingEdge,
};
use spot_core::ManualClock;
use spot_dag::NodeId;
use spot_document::engine::{Engine, V7Ids};
use spot_meta::{Definition, EffectiveMeta, FieldSpec, FieldType, Registry};
use spot_seams::RegistryBuilder;
use spot_storage_fake::FakeBackend;

/// Runs a future to completion on the current thread (every in-memory future is ready).
pub fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let waker = Waker::noop();
    let mut context = Context::from_waker(waker);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => return value,
            Poll::Pending => std::thread::yield_now(),
        }
    }
}

/// A valid storage name.
pub fn n(name: &str) -> Name {
    Name::new(name).unwrap()
}

/// The fixed instant every test runs at.
pub fn now() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_790_000_000).unwrap()
}

/// The test site.
pub fn site() -> SiteId {
    SiteId::new("bpm.test").unwrap()
}

fn link(name: &str, doctype: &str) -> FieldSpec {
    let mut field = FieldSpec::new(name, FieldType::Link);
    field.options = Some(doctype.to_owned());
    field
}

fn text(name: &str) -> FieldSpec {
    FieldSpec::new(name, FieldType::Text)
}

/// The DocType set: a posted order that links a customer, a delivery that links the order, an
/// invoice that links the delivery, and a bare note.
pub fn definitions() -> Vec<Definition> {
    let mut customer = Definition::new("customer");
    customer.fields = vec![text("name")];

    let mut order = Definition::new("sales_order");
    order.is_posted = true;
    let mut title = text("title");
    title.required = true;
    order.fields = vec![title, link("customer", "customer"), text("note")];

    let mut delivery = Definition::new("delivery_note");
    delivery.fields = vec![link("sales_order", "sales_order"), text("note")];

    let mut invoice = Definition::new("sales_invoice");
    invoice.is_posted = true;
    let mut title = text("title");
    title.required = true;
    invoice.fields = vec![title, link("delivery_note", "delivery_note"), text("note")];

    let note = Definition::new("note");

    vec![customer, order, delivery, invoice, note]
}

/// The effective metadata.
pub fn meta() -> Arc<EffectiveMeta> {
    Registry::new(definitions()).unwrap().effective()
}

/// An in-memory document engine over the test metadata.
pub fn engine() -> Arc<Engine<FakeBackend>> {
    let seams = Arc::new(RegistryBuilder::new().build().unwrap());
    let engine = Engine::new(
        FakeBackend::new(),
        meta(),
        seams,
        Arc::new(ManualClock::new(now())),
        Arc::new(V7Ids),
    )
    .unwrap();
    block_on(engine.prepare()).unwrap();
    Arc::new(engine)
}

/// Runs one Document node attempt against `variables`, returning its outcome.
pub fn run_node(
    handler: &DocumentNodeHandler,
    case: &CaseId,
    node: &str,
    attempt: u32,
    config: &Json,
    principal: Option<&str>,
    variables: &mut Json,
) -> NodeOutcome {
    run_node_with_edges(
        handler,
        case,
        node,
        attempt,
        config,
        principal,
        variables,
        &[],
    )
}

/// Runs one Document node attempt with outgoing edges, returning its outcome.
#[allow(clippy::too_many_arguments)]
pub fn run_node_with_edges(
    handler: &DocumentNodeHandler,
    case: &CaseId,
    node: &str,
    attempt: u32,
    config: &Json,
    principal: Option<&str>,
    variables: &mut Json,
    outgoing: &[OutgoingEdge],
) -> NodeOutcome {
    let node_id = NodeId::new(node).unwrap();
    let mut ctx = NodeCtx {
        case,
        node: &node_id,
        kind: NodeKind::Document,
        attempt,
        key: IdempotencyKey::new(case.clone(), node_id.clone(), attempt),
        is_retry: false,
        now: now(),
        deadline: None,
        principal,
        config,
        outgoing,
        variables,
        cancelled: false,
    };
    handler.run(&mut ctx)
}
