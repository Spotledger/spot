//! Journal capture (WP-JRN-02.T2): one `doc_state` event per document per committed
//! transaction, net, at its final version, inside the transaction. No database: the in-memory
//! backend. The PostgreSQL run of the capture invariant is `capture_pg.rs`.
#![allow(
    missing_docs,
    dead_code,
    unused_imports,
    unused_macros,
    clippy::unwrap_used,
    clippy::expect_used
)]

mod common;
#[path = "secrets/support.rs"]
mod support;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{meta, n, now};
use serde_json::{Value as Json, json};
use spot_app_api::context::{Actor, DocumentService, HookContext, Invocation};
use spot_app_api::controller::{StepDecl, StepName};
use spot_app_api::prelude::{CommandId, RecordId, SiteId, Value, Version};
use spot_app_api::{
    AppError, AppResult, BoxFuture, Controller, Document, DocumentPatch, ErrorClass, HookPoint,
    Phase, Target,
};
use spot_core::ManualClock;
use spot_document::engine::{
    CaptureConfig, Engine, Exclusion, ExclusionKind, RetryPolicy, TOPIC_DOC_STATE, V7Ids,
};
use spot_events_contract::PayloadDocument;
use spot_events_contract::lifecycle::Operation;
use spot_events_contract::state;
use spot_seams::RegistryBuilder;
use spot_storage_contract::{
    Catalog, ChangeFeed, ChangeOp, DocumentData, DocumentStore, FeedPosition, GraphTraversal,
    HealthReport, LockKey, LockMode, Locks, Outbox, OutboxEvent, OutboxRecord, Query,
    QueryExecutor, ReadSource, RowLock, Rows, Savepoint, Sequences, SeriesKey, StorageBackend,
    StorageResult, Transaction, TxOptions, Wait,
};
use spot_storage_fake::FakeBackend;

type StepFn =
    Arc<dyn for<'a> Fn(&'a mut dyn HookContext) -> BoxFuture<'a, AppResult<()>> + Send + Sync>;

// The controller probe and the refusing backend are in a sibling file to keep this one short.
include!("capture/rigs.rs");

// ---------------------------------------------------------------------------------------------
// Rigs and readers
// ---------------------------------------------------------------------------------------------

fn no_wait() -> RetryPolicy {
    RetryPolicy {
        max_retries: 0,
        base_delay: Duration::ZERO,
        max_delay: Duration::ZERO,
    }
}

fn build<B: StorageBackend>(backend: B, probe: Probe) -> Engine<B> {
    let mut builder = RegistryBuilder::new();
    builder.add_controller("probe", Arc::new(probe));
    Engine::new(
        backend,
        meta(),
        Arc::new(builder.build().unwrap()),
        Arc::new(ManualClock::new(now())),
        Arc::new(V7Ids),
    )
    .unwrap()
    .with_retry_policy(no_wait())
}

async fn engine_with(probe: Probe) -> Engine<FakeBackend> {
    let engine = build(FakeBackend::new(), probe);
    engine.prepare().await.unwrap();
    engine
}

async fn engine() -> Engine<FakeBackend> {
    engine_with(Probe::new()).await
}

fn inv_at(site: &str) -> Invocation {
    Invocation {
        site: SiteId::new(site).unwrap(),
        actor: Actor::System {
            component: n("capture_tests"),
        },
        command_id: CommandId::new("cmd-9").ok(),
        language: None,
        now: now(),
    }
}

fn inv() -> Invocation {
    inv_at("capture.test")
}

fn customer(name: &str) -> Document {
    let mut doc = Document::new(n("customer"), RecordId::new_v7());
    doc.set(n("name"), name);
    doc
}

fn rename(name: &str) -> DocumentPatch {
    let mut patch = DocumentPatch::default();
    patch.fields.insert(n("name"), name.into());
    patch
}

fn version(doc: &Document) -> Version {
    doc.version().unwrap()
}

/// One `doc_state` event, decoded.
#[derive(Debug, Clone)]
struct Captured {
    entity: String,
    id: RecordId,
    version: u64,
    op: String,
    entity_schema: i64,
    image: Json,
    event: OutboxEvent,
}

async fn records<B: ChangeFeed + StorageBackend>(engine: &Engine<B>) -> Vec<OutboxRecord> {
    engine
        .backend()
        .read_outbox(FeedPosition::START, 100_000)
        .await
        .unwrap()
}

async fn states<B: ChangeFeed + StorageBackend>(engine: &Engine<B>) -> Vec<Captured> {
    records(engine)
        .await
        .into_iter()
        .map(|r| r.event)
        .filter(|e| e.topic.as_str() == TOPIC_DOC_STATE)
        .map(|event| {
            let payload = PayloadDocument::from_outbox_event(&event).unwrap();
            state::validate(&payload.data).expect("a valid doc_state payload");
            Captured {
                entity: event.subject.entity.as_str().to_owned(),
                id: event.subject.id,
                version: event.subject_version.unwrap().0,
                op: payload.data["op"].as_str().unwrap().to_owned(),
                entity_schema: payload.data["entity_schema"].as_i64().unwrap(),
                image: payload.data["image"].clone(),
                event,
            }
        })
        .collect()
}

async fn topics<B: ChangeFeed + StorageBackend>(engine: &Engine<B>) -> Vec<String> {
    records(engine)
        .await
        .iter()
        .map(|r| r.event.topic.as_str().to_owned())
        .collect()
}

fn about(events: &[Captured], id: RecordId) -> Vec<&Captured> {
    events.iter().filter(|c| c.id == id).collect()
}

// ---------------------------------------------------------------------------------------------
// The net operation
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_create_writes_one_insert_with_the_whole_image() {
    let engine = engine().await;
    let saved = engine.create(&inv(), customer("Acme")).await.unwrap().value;
    let events = states(&engine).await;
    assert_eq!(events.len(), 1);
    let e = &events[0];
    assert_eq!(e.entity, "customer");
    assert_eq!(e.id, saved.id());
    assert_eq!(e.version, 1);
    assert_eq!(e.op, "insert");
    assert_eq!(e.entity_schema, i64::try_from(meta().version()).unwrap());
    assert_eq!(e.image["id"], json!(saved.id().to_string()));
    assert_eq!(e.image["version"], json!(1));
    assert_eq!(e.image["name"], json!("Acme"));
    // The lifecycle event is still emitted, and the state event is the last of the transaction.
    assert_eq!(topics(&engine).await, ["doc_created", "doc_state"]);
}

#[tokio::test]
async fn each_update_writes_one_update_at_its_version() {
    let engine = engine().await;
    let saved = engine.create(&inv(), customer("A")).await.unwrap().value;
    let v2 = engine
        .update(&inv(), "customer", saved.id(), rename("B"), version(&saved))
        .await
        .unwrap()
        .value;
    let v3 = engine
        .update(&inv(), "customer", saved.id(), rename("C"), version(&v2))
        .await
        .unwrap()
        .value;
    assert_eq!(version(&v3).0, 3);
    let events = states(&engine).await;
    let seen: Vec<_> = events
        .iter()
        .map(|c| (c.op.as_str(), c.version, c.image["name"].clone()))
        .collect();
    assert_eq!(
        seen,
        [
            ("insert", 1, json!("A")),
            ("update", 2, json!("B")),
            ("update", 3, json!("C")),
        ]
    );
}

#[tokio::test]
async fn a_delete_keeps_the_last_image_at_the_last_version() {
    let engine = engine().await;
    let saved = engine.create(&inv(), customer("Gone")).await.unwrap().value;
    let v2 = engine
        .update(
            &inv(),
            "customer",
            saved.id(),
            rename("Gone2"),
            version(&saved),
        )
        .await
        .unwrap()
        .value;
    engine
        .delete(&inv(), "customer", saved.id(), version(&v2))
        .await
        .unwrap();
    let events = states(&engine).await;
    let last = events.last().unwrap();
    assert_eq!(last.op, "delete");
    assert_eq!(last.version, 2, "the last version before the deletion");
    assert_eq!(last.image["name"], json!("Gone2"));
    assert_eq!(last.image["version"], json!(2));
    assert_eq!(events.len(), 3, "insert, update, delete: one each");
}

#[tokio::test]
async fn a_document_created_and_deleted_in_one_transaction_has_no_event() {
    let engine =
        engine_with(
            Probe::new().step("probe:flash", Phase::After, Operation::Create, |ctx| {
                Box::pin(async move {
                    if ctx.doc().text("name")? != Some("Parent") {
                        return Ok(());
                    }
                    let made = ctx.create(spot_doc("Flash")).await?;
                    ctx.delete("customer", made.id(), made.version().unwrap())
                        .await
                })
            }),
        )
        .await;
    engine.create(&inv(), customer("Parent")).await.unwrap();
    let events = states(&engine).await;
    assert_eq!(events.len(), 1, "only the parent");
    assert_eq!(events[0].image["name"], json!("Parent"));
    // The change feed agrees: the storage's own witness has no entry for the flash document.
    let changes = engine
        .backend()
        .read_changes(FeedPosition::START, 1000)
        .await
        .unwrap();
    assert_eq!(changes.len(), 1);
}

fn spot_doc(name: &str) -> Document {
    customer(name)
}

#[tokio::test]
async fn a_create_and_an_update_in_one_transaction_is_one_insert_at_the_final_version() {
    let made = Arc::new(Mutex::new(None));
    let slot = made.clone();
    let engine = engine_with(Probe::new().step(
        "probe:twice",
        Phase::After,
        Operation::Create,
        move |ctx| {
            let slot = slot.clone();
            Box::pin(async move {
                if ctx.doc().text("name")? != Some("Parent") {
                    return Ok(());
                }
                let first = ctx.create(spot_doc("Draft")).await?;
                let second = ctx
                    .update(
                        "customer",
                        first.id(),
                        rename("Final"),
                        first.version().unwrap(),
                    )
                    .await?;
                *slot.lock().unwrap() = Some((second.id(), second.version().unwrap()));
                Ok(())
            })
        },
    ))
    .await;
    engine.create(&inv(), customer("Parent")).await.unwrap();
    let (id, v) = made.lock().unwrap().unwrap();
    assert_eq!(v.0, 2);
    let events = states(&engine).await;
    let mine = about(&events, id);
    assert_eq!(mine.len(), 1);
    assert_eq!(mine[0].op, "insert");
    assert_eq!(mine[0].version, 2);
    assert_eq!(mine[0].image["name"], json!("Final"));
}

#[tokio::test]
async fn repeated_updates_in_one_transaction_are_one_update_at_the_final_version() {
    let target = Arc::new(Mutex::new(None));
    let aim = target.clone();
    let engine =
        engine_with(
            Probe::new().step("probe:bump", Phase::After, Operation::Create, move |ctx| {
                let aim = aim.clone();
                Box::pin(async move {
                    if ctx.doc().text("name")? != Some("Bump") {
                        return Ok(());
                    }
                    let id = aim.lock().unwrap().unwrap();
                    let mut current = ctx.load("customer", id).await?;
                    for round in 0..3 {
                        current = ctx
                            .update(
                                "customer",
                                id,
                                rename(&format!("v{round}")),
                                current.version().unwrap(),
                            )
                            .await?;
                    }
                    Ok(())
                })
            }),
        )
        .await;
    let saved = engine.create(&inv(), customer("T")).await.unwrap().value;
    *target.lock().unwrap() = Some(saved.id());
    engine.create(&inv(), customer("Bump")).await.unwrap();
    let events = states(&engine).await;
    let mine = about(&events, saved.id());
    assert_eq!(
        mine.len(),
        2,
        "its insert, then one update for the whole bump"
    );
    assert_eq!(mine[1].op, "update");
    assert_eq!(mine[1].version, 4);
    assert_eq!(mine[1].image["name"], json!("v2"));
}

#[tokio::test]
async fn events_follow_first_touch_order() {
    let engine =
        engine_with(
            Probe::new().step("probe:order", Phase::After, Operation::Create, |ctx| {
                Box::pin(async move {
                    if ctx.doc().text("name")? != Some("Parent") {
                        return Ok(());
                    }
                    ctx.create(spot_doc("First")).await?;
                    ctx.create(spot_doc("Second")).await?;
                    Ok(())
                })
            }),
        )
        .await;
    engine.create(&inv(), customer("Parent")).await.unwrap();
    let names: Vec<_> = states(&engine)
        .await
        .iter()
        .map(|c| c.image["name"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(names, ["Parent", "First", "Second"]);
}

#[tokio::test]
async fn a_write_rolled_back_to_a_savepoint_leaves_no_event() {
    // `Victim` is updated by a nested write whose own `after_update` step fails: the step that
    // made the write handles the error, so the transaction commits without that update.
    let target = Arc::new(Mutex::new(None));
    let aim = target.clone();
    let engine = engine_with(
        Probe::new()
            .step("probe:fail", Phase::After, Operation::Update, |ctx| {
                Box::pin(async move {
                    if ctx.doc().text("name")? == Some("Bad") {
                        return Err(AppError::new(
                            ErrorClass::Validation,
                            "probe.refused",
                            "refused",
                        ));
                    }
                    Ok(())
                })
            })
            .step("probe:try", Phase::After, Operation::Create, move |ctx| {
                let aim = aim.clone();
                Box::pin(async move {
                    if ctx.doc().text("name")? == Some("Try") {
                        let id = aim.lock().unwrap().unwrap();
                        let current = ctx.load("customer", id).await?;
                        let refused = ctx
                            .update("customer", id, rename("Bad"), current.version().unwrap())
                            .await;
                        assert!(refused.is_err());
                    }
                    Ok(())
                })
            }),
    )
    .await;
    let victim = engine
        .create(&inv(), customer("Victim"))
        .await
        .unwrap()
        .value;
    *target.lock().unwrap() = Some(victim.id());
    engine.create(&inv(), customer("Try")).await.unwrap();
    let events = states(&engine).await;
    let mine = about(&events, victim.id());
    assert_eq!(
        mine.len(),
        1,
        "only its insert: the rolled-back update left nothing"
    );
    let stored = engine.peek("customer", victim.id()).await.unwrap().unwrap();
    assert_eq!(stored.text("name").unwrap(), Some("Victim"));
}

#[tokio::test]
async fn a_bulk_load_through_the_engine_is_captured_document_by_document() {
    // The engine offers no separate bulk entry: a loader writes through the repository of one
    // transaction (a step), and every document it writes is captured by the same code.
    let engine =
        engine_with(
            Probe::new().step("probe:bulk", Phase::After, Operation::Create, |ctx| {
                Box::pin(async move {
                    if ctx.doc().text("name")? != Some("Loader") {
                        return Ok(());
                    }
                    for i in 0..40 {
                        ctx.create(spot_doc(&format!("row{i}"))).await?;
                    }
                    Ok(())
                })
            }),
        )
        .await;
    engine.create(&inv(), customer("Loader")).await.unwrap();
    let events = states(&engine).await;
    assert_eq!(events.len(), 41);
    assert!(events.iter().all(|c| c.op == "insert" && c.version == 1));
}

// ---------------------------------------------------------------------------------------------
// The image
// ---------------------------------------------------------------------------------------------

fn invoice_with_rows() -> Document {
    let mut doc = common::valid_invoice();
    doc.set(n("total"), common::money(25_000));
    doc
}

#[tokio::test]
async fn child_rows_are_in_the_image_and_follow_every_change() {
    let engine = engine().await;
    let customer_doc = engine.create(&inv(), customer("Acme")).await.unwrap().value;
    let mut doc = invoice_with_rows();
    doc.set(n("customer"), Value::Id(customer_doc.id()));
    let saved = engine.create(&inv(), doc).await.unwrap().value;
    let rows: Vec<_> = saved.rows("items").to_vec();
    assert_eq!(rows.len(), 2);

    let events = states(&engine).await;
    let insert = about(&events, saved.id())[0];
    let items = insert.image["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["id"], json!(rows[0].id.to_string()));
    assert_eq!(items[0]["idx"], json!(1));
    assert_eq!(items[0]["item"], json!("bolt"));
    assert_eq!(items[0]["qty"], json!("2.000"), "a decimal is a string");
    assert_eq!(items[1]["idx"], json!(2));
    assert_eq!(insert.image["total"], json!("250.00"));
    assert!(
        insert.image["remarks"].is_null() && insert.image.get("remarks").is_some(),
        "every declared field is present, null when unset"
    );

    // Remove the first row and add one: the image is the whole table as it now stands.
    let mut patch = DocumentPatch::default();
    let keep = spot_app_api::document::RowPatch {
        id: Some(rows[1].id),
        fields: Default::default(),
    };
    let fresh = spot_app_api::document::RowPatch {
        id: None,
        fields: [
            (n("item"), Value::from("washer")),
            (n("qty"), common::qty(7_000)),
        ]
        .into_iter()
        .collect(),
    };
    patch.tables.insert(n("items"), vec![keep, fresh]);
    let updated = engine
        .update(&inv(), "sales_invoice", saved.id(), patch, version(&saved))
        .await
        .unwrap()
        .value;
    let events = states(&engine).await;
    let mine = about(&events, saved.id());
    assert_eq!(mine.len(), 2);
    let items = mine[1].image["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["item"], json!("nut"));
    assert_eq!(items[0]["idx"], json!(1));
    assert_eq!(items[1]["item"], json!("washer"));
    assert_eq!(mine[1].version, version(&updated).0);

    // A delete of a draft keeps the rows of the last version.
    engine
        .delete(&inv(), "sales_invoice", saved.id(), version(&updated))
        .await
        .unwrap();
    let events = states(&engine).await;
    let last = about(&events, saved.id()).pop().unwrap();
    assert_eq!(last.op, "delete");
    assert_eq!(last.image["items"].as_array().unwrap().len(), 2);
    assert_eq!(last.image["_state"], json!("draft"));
}

#[tokio::test]
async fn a_posted_documents_state_is_in_the_image() {
    let engine = engine().await;
    let customer_doc = engine.create(&inv(), customer("Acme")).await.unwrap().value;
    let mut doc = invoice_with_rows();
    doc.set(n("customer"), Value::Id(customer_doc.id()));
    let saved = engine.create(&inv(), doc).await.unwrap().value;
    let posted = engine
        .post(&inv(), "sales_invoice", saved.id(), version(&saved))
        .await
        .unwrap()
        .value;
    let events = states(&engine).await;
    let last = about(&events, saved.id()).pop().unwrap();
    assert_eq!(last.op, "update");
    assert_eq!(last.version, version(&posted).0);
    assert_eq!(last.image["_state"], json!("posted"));
}

// ---------------------------------------------------------------------------------------------
// Atomicity
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_failing_flush_rolls_the_whole_operation_back() {
    let armed = Arc::new(AtomicBool::new(true));
    let backend = RefusesState {
        inner: FakeBackend::new(),
        armed: armed.clone(),
    };
    let engine = build(backend, Probe::new());
    engine.prepare().await.unwrap();
    let doc = customer("Never");
    let id = doc.id();
    let refused = engine.create(&inv(), doc).await;
    assert!(
        refused.is_err(),
        "a write without its state event must not commit"
    );
    assert!(engine.peek("customer", id).await.unwrap().is_none());
    assert!(
        records(&engine).await.is_empty(),
        "no lifecycle event either"
    );
    assert!(
        engine
            .backend()
            .read_changes(FeedPosition::START, 100)
            .await
            .unwrap()
            .is_empty()
    );
    // Disarmed, the same engine commits normally.
    armed.store(false, Ordering::SeqCst);
    engine.create(&inv(), customer("Fine")).await.unwrap();
    assert_eq!(states(&engine).await.len(), 1);
}

#[tokio::test]
async fn a_failed_operation_leaves_no_state_event() {
    let engine =
        engine_with(
            Probe::new().step("probe:boom", Phase::After, Operation::Create, |ctx| {
                Box::pin(async move {
                    if ctx.doc().text("name")? == Some("Boom") {
                        ctx.create(spot_doc("Child")).await?;
                        return Err(AppError::new(ErrorClass::Validation, "probe.boom", "boom"));
                    }
                    Ok(())
                })
            }),
        )
        .await;
    assert!(engine.create(&inv(), customer("Boom")).await.is_err());
    assert!(states(&engine).await.is_empty());
    assert!(records(&engine).await.is_empty());
}

// ---------------------------------------------------------------------------------------------
// Exclusions
// ---------------------------------------------------------------------------------------------

fn exclusion(doctype: &str, owner: &str, reason: &str) -> Exclusion {
    Exclusion {
        doctype: doctype.to_owned(),
        owner: owner.to_owned(),
        reason_code: reason.to_owned(),
        kind: ExclusionKind::Derived,
    }
}

#[tokio::test]
async fn an_excluded_doctype_emits_nothing_and_is_listed() {
    let engine = engine()
        .await
        .with_capture(CaptureConfig {
            exclude: vec![exclusion("customer", "sales_team", "rebuilt_by_projector")],
        })
        .unwrap();
    engine.create(&inv(), customer("Quiet")).await.unwrap();
    assert!(states(&engine).await.is_empty());
    assert_eq!(topics(&engine).await, ["doc_created"]);
    let listed = engine.capture_exclusions();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].doctype, "customer");
    assert_eq!(listed[0].owner, "sales_team");
    assert_eq!(listed[0].reason_code, "rebuilt_by_projector");
    assert_eq!(listed[0].kind, ExclusionKind::Derived);
    // A DocType that is not excluded is still captured.
    let mut doc = invoice_with_rows();
    let c = engine
        .create(&inv(), customer("Other"))
        .await
        .unwrap()
        .value;
    doc.set(n("customer"), Value::Id(c.id()));
    engine.create(&inv(), doc).await.unwrap();
    let events = states(&engine).await;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].entity, "sales_invoice");
}

#[tokio::test]
async fn an_exclusion_without_an_owner_or_a_reason_is_refused() {
    for (owner, reason) in [("", "r"), ("  ", "r"), ("o", ""), ("o", " ")] {
        let refused = engine()
            .await
            .with_capture(CaptureConfig {
                exclude: vec![exclusion("customer", owner, reason)],
            })
            .err()
            .unwrap();
        assert_eq!(refused.code, "capture.exclusion_unowned");
    }
}

#[tokio::test]
async fn an_exclusion_naming_an_unknown_doctype_is_a_startup_error() {
    for name in ["no_such_doctype", "sales_invoice_item"] {
        let refused = engine()
            .await
            .with_capture(CaptureConfig {
                exclude: vec![exclusion(name, "owner", "reason")],
            })
            .err()
            .unwrap();
        assert_eq!(refused.code, "capture.exclusion_unknown", "{name}");
    }
}

#[tokio::test]
async fn the_same_doctype_excluded_twice_is_refused() {
    let refused = engine()
        .await
        .with_capture(CaptureConfig {
            exclude: vec![
                exclusion("customer", "owner", "reason"),
                exclusion("customer", "owner", "reason"),
            ],
        })
        .err()
        .unwrap();
    assert_eq!(refused.code, "capture.exclusion_duplicate");
}

// ---------------------------------------------------------------------------------------------
// Secrets, tenancy and the topic
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_image_of_a_record_with_a_secret_field_carries_no_plaintext() {
    let rig = support::rig_fake().await.unwrap();
    let (id, v1) = rig.account("cy").await;
    let access = rig.access();
    let v2 = rig
        .engine
        .set_secret(
            &access,
            "account",
            id,
            "password",
            support::s(support::PASSWORD),
            v1,
        )
        .await
        .unwrap();
    rig.engine
        .set_secret(
            &access,
            "account",
            id,
            "token",
            support::s(support::TOKEN),
            v2,
        )
        .await
        .unwrap();
    let sealed = rig.sealed(id, "password").await.unwrap();
    let events: Vec<_> = rig
        .engine
        .backend()
        .read_outbox(FeedPosition::START, 1000)
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.event.topic.as_str() == TOPIC_DOC_STATE)
        .collect();
    assert_eq!(
        events.len(),
        3,
        "the create and the two secret writes: one each"
    );
    for record in &events {
        let text = &record.event.payload;
        for needle in support::forms_of_plain(support::PASSWORD)
            .into_iter()
            .chain(support::forms_of_plain(support::TOKEN))
            .chain(support::forms_of_sealed(&sealed))
        {
            assert!(
                !contains(text.as_bytes(), &needle),
                "a secret, in some form, is in a doc_state payload"
            );
        }
        let data = PayloadDocument::from_outbox_event(&record.event)
            .unwrap()
            .data;
        let image = data["image"].as_object().unwrap();
        assert!(!image.contains_key("password") && !image.contains_key("token"));
        assert!(
            !image
                .keys()
                .any(|k| k.contains("sealed") || k.contains("bidx"))
        );
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|w| w == needle)
}

#[tokio::test]
async fn one_sites_image_never_reaches_the_other_sites_outbox() {
    // A site is a tenant and has a database of its own: two engines, two backends.
    let site_a = engine().await;
    let site_b = engine().await;
    let a = site_a
        .create(&inv_at("a.test"), customer("Secret of A"))
        .await
        .unwrap()
        .value;
    let b = site_b
        .create(&inv_at("b.test"), customer("Plain B"))
        .await
        .unwrap()
        .value;
    let in_b = records(&site_b).await;
    assert!(!in_b.is_empty());
    for record in &in_b {
        assert!(!record.event.payload.contains("Secret of A"));
        assert_ne!(record.event.subject.id, a.id());
    }
    let a_states = states(&site_a).await;
    assert_eq!(a_states.len(), 1);
    assert_eq!(a_states[0].id, a.id());
    let b_states = states(&site_b).await;
    assert_eq!(b_states.len(), 1);
    assert_eq!(b_states[0].id, b.id());
}

#[tokio::test]
async fn the_state_topic_is_in_the_reserved_doc_class() {
    // `spot-event-stream` lets only an owner holding the explicit `doc_` stream right select a
    // `doc_` topic (`DenyStreamRights` is its default), so the journal's images are audit-grade.
    assert!(TOPIC_DOC_STATE.starts_with("doc_"));
    let engine = engine().await;
    engine.create(&inv(), customer("Acme")).await.unwrap();
    let events = states(&engine).await;
    assert!(events[0].event.topic.as_str().starts_with("doc_"));
}

#[tokio::test]
async fn the_change_feed_and_the_state_events_agree_in_a_small_run() {
    let engine = engine().await;
    let a = engine.create(&inv(), customer("A")).await.unwrap().value;
    let a2 = engine
        .update(&inv(), "customer", a.id(), rename("A2"), version(&a))
        .await
        .unwrap()
        .value;
    engine
        .delete(&inv(), "customer", a.id(), version(&a2))
        .await
        .unwrap();
    engine.create(&inv(), customer("B")).await.unwrap();
    let changes = engine
        .backend()
        .read_changes(FeedPosition::START, 1000)
        .await
        .unwrap();
    let events = states(&engine).await;
    assert_eq!(changes.len(), events.len());
    for (change, event) in changes.iter().zip(&events) {
        assert_eq!(change.document.id, event.id);
        assert_eq!(change.version.0, event.version);
        let op = match change.op {
            ChangeOp::Insert => "insert",
            ChangeOp::Update => "update",
            ChangeOp::Delete => "delete",
        };
        assert_eq!(op, event.op);
    }
}
