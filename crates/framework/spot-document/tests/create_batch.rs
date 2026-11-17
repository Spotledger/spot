//! `Engine::create_batch`: many creates of one actor in one transaction, with each document's
//! own verdict, command id and lifecycle (fake backend).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::{n, now};
use spot_app_api::context::{Actor, DocumentService, Invocation};
use spot_app_api::prelude::{CommandId, RecordId, SiteId};
use spot_app_api::{AppResult, BoxFuture, Document};
use spot_core::ManualClock;
use spot_document::engine::{
    Engine, RetentionPolicy, TxParticipant, V7Ids, WriteIntent, WriteRecord,
};
use spot_seams::RegistryBuilder;
use spot_storage_fake::{FakeBackend, FakeTx};

#[derive(Default)]
struct Writes(AtomicUsize);

impl TxParticipant<FakeTx> for Writes {
    fn before_write<'a>(
        &'a self,
        _tx: &'a mut FakeTx,
        _write: &'a WriteIntent<'a>,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async move {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }

    fn after_write<'a>(
        &'a self,
        _tx: &'a mut FakeTx,
        _write: &'a WriteRecord<'a>,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async { Ok(()) })
    }
}

async fn fixture() -> (Engine<FakeBackend>, Arc<Writes>) {
    let policy = RetentionPolicy::new(
        std::time::Duration::from_secs(86_400),
        std::time::Duration::from_secs(86_400 * 30),
        std::collections::BTreeMap::new(),
    )
    .unwrap();
    let engine = Engine::new(
        FakeBackend::new(),
        common::meta(),
        Arc::new(RegistryBuilder::new().build().unwrap()),
        Arc::new(ManualClock::new(now())),
        Arc::new(V7Ids),
    )
    .unwrap()
    .with_command_ledger(policy);
    let writes = Arc::new(Writes::default());
    engine.set_participants(vec![writes.clone() as Arc<dyn TxParticipant<FakeTx>>]);
    engine.prepare().await.unwrap();
    (engine, writes)
}

fn inv(actor: &Actor, command: Option<&str>) -> Invocation {
    Invocation {
        site: SiteId::new("batch.test").unwrap(),
        actor: actor.clone(),
        command_id: command.map(|c| CommandId::new(c).unwrap()),
        language: None,
        now: now(),
    }
}

fn user() -> Actor {
    Actor::User {
        id: RecordId::new_v7(),
        via: None,
    }
}

fn customer(name: &str) -> Document {
    let mut doc = Document::new(n("customer"), RecordId::new_v7());
    doc.set(n("name"), name);
    doc
}

#[tokio::test]
async fn every_document_is_created_with_its_own_lifecycle_and_ledger_row() {
    let (engine, writes) = fixture().await;
    let actor = user();
    let items: Vec<_> = (0..5)
        .map(|k| {
            (
                inv(&actor, Some(&format!("cmd-{k}"))),
                customer(&format!("C{k}")),
            )
        })
        .collect();
    let results = engine.create_batch(items.clone()).await;
    assert_eq!(results.len(), 5);
    for ((_, sent), result) in items.iter().zip(&results) {
        let created = &result.as_ref().unwrap().value;
        assert_eq!(created.id(), sent.id());
        assert!(created.version().is_some());
        let stored = engine.load(&inv(&actor, None), "customer", sent.id()).await;
        assert!(stored.is_ok(), "the document is stored");
    }
    assert_eq!(writes.0.load(Ordering::SeqCst), 5, "a write per document");

    // The same command ids again: every document is answered from the ledger, nothing runs.
    let again = engine.create_batch(items.clone()).await;
    for (first, second) in results.iter().zip(&again) {
        assert_eq!(
            first.as_ref().unwrap().value.id(),
            second.as_ref().unwrap().value.id()
        );
    }
    assert_eq!(
        writes.0.load(Ordering::SeqCst),
        5,
        "a replay writes nothing"
    );
}

#[tokio::test]
async fn a_refused_document_does_not_stop_the_others_and_leaves_nothing() {
    let (engine, writes) = fixture().await;
    let actor = user();
    // The second has no name (required), the fourth a name over the 20 character cap.
    let mut nameless = Document::new(n("customer"), RecordId::new_v7());
    nameless.set(n("note"), "x");
    let long = customer("a name that is much too long for the field");
    let good = [customer("One"), customer("Two"), customer("Three")];
    let items = vec![
        (inv(&actor, Some("c-1")), good[0].clone()),
        (inv(&actor, Some("c-2")), nameless.clone()),
        (inv(&actor, Some("c-3")), good[1].clone()),
        (inv(&actor, Some("c-4")), long.clone()),
        (inv(&actor, Some("c-5")), good[2].clone()),
    ];
    let results = engine.create_batch(items).await;
    assert!(results[0].is_ok() && results[2].is_ok() && results[4].is_ok());
    assert!(results[1].is_err() && results[3].is_err());
    assert_eq!(
        writes.0.load(Ordering::SeqCst),
        3,
        "a refused document never reaches its write"
    );
    for refused in [&nameless, &long] {
        assert!(
            engine
                .load(&inv(&actor, None), "customer", refused.id())
                .await
                .is_err(),
            "the refused document was rolled back"
        );
    }
    // A refused document's command id was not recorded: it can be sent again, fixed.
    let fixed = customer("Fixed");
    let retry = engine
        .create_batch(vec![
            (inv(&actor, Some("c-2")), fixed.clone()),
            (inv(&actor, Some("c-6")), customer("Six")),
        ])
        .await;
    assert!(retry[0].is_ok() && retry[1].is_ok());
}

#[tokio::test]
async fn a_command_id_used_for_another_request_is_a_conflict_for_that_document_only() {
    let (engine, _) = fixture().await;
    let actor = user();
    engine
        .create_batch(vec![
            (inv(&actor, Some("same")), customer("First")),
            (inv(&actor, Some("other")), customer("Other")),
        ])
        .await;
    let results = engine
        .create_batch(vec![
            (inv(&actor, Some("same")), customer("Different")),
            (inv(&actor, Some("fresh")), customer("Fresh")),
        ])
        .await;
    assert_eq!(
        results[0].as_ref().err().map(|e| e.code.as_str()),
        Some("document.command_conflict")
    );
    assert!(results[1].is_ok());
}

#[tokio::test]
async fn a_batch_that_cannot_share_a_transaction_runs_document_by_document() {
    let (engine, _) = fixture().await;
    // Two actors, and a repeated command id: nothing here may be batched, all must still work.
    let (a, b) = (user(), user());
    let results = engine
        .create_batch(vec![
            (inv(&a, Some("x-1")), customer("A1")),
            (inv(&b, Some("x-2")), customer("B1")),
        ])
        .await;
    assert!(results.iter().all(Result::is_ok));
    let repeated = engine
        .create_batch(vec![
            (inv(&a, Some("dup")), customer("D1")),
            (inv(&a, Some("dup")), customer("D2")),
        ])
        .await;
    assert!(repeated[0].is_ok());
    assert_eq!(
        repeated[1].as_ref().err().map(|e| e.code.as_str()),
        Some("document.command_conflict"),
        "as two lone creates would answer"
    );
}
