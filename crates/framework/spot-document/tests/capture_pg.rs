//! The capture invariant over the real engine and PostgreSQL (WP-JRN-02.T2, ADR-061 s3 and s8).
//!
//! 500 mixed operations run through the engine on a scratch database; then
//! `spot_capture_invariant::Checker` compares the real change feed with the real `doc_state`
//! events in the outbox: zero findings. A write that bypasses the engine (a transaction opened
//! on the backend directly) is then reported as `MissingStateEvent` naming that document.
//!
//! Needs `SPOT_TEST_PG_URL`; skips (printing why) without it, and fails under
//! `SPOT_REQUIRE_PG=1`.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{meta, n, now};
use spot_app_api::context::{Actor, DocumentService, HookContext, Invocation};
use spot_app_api::controller::{StepDecl, StepName};
use spot_app_api::prelude::{CommandId, RecordId, SiteId, Value, Version};
use spot_app_api::{
    AppResult, BoxFuture, Controller, Document, DocumentPatch, HookPoint, Phase, Target,
};
use spot_capture_invariant::{Checker, FeedEntry, FindingKind, Op, Position, StateEvent};
use spot_core::ManualClock;
use spot_document::engine::{Engine, RetryPolicy, TOPIC_DOC_STATE, V7Ids};
use spot_events_contract::lifecycle::Operation;
use spot_postgres::PgBackend;
use spot_postgres::testing::TestDb;
use spot_seams::RegistryBuilder;
use spot_storage_contract::{
    ChangeFeed, ChangeOp, Collation, DocumentStore, FeedPosition, Isolation, RowLock,
    StorageBackend, Transaction, TxOptions, Wait,
};

/// A controller of `customer`: a document named `Flash` creates and deletes another in its own
/// transaction (no feed entry, no event), and one named `Twice` creates another and updates it
/// (one insert at version 2).
struct Churn;

impl Controller for Churn {
    fn doctype(&self) -> &str {
        "customer"
    }

    fn steps(&self) -> Vec<StepDecl> {
        vec![StepDecl {
            name: StepName::new("churn:after_create").unwrap(),
            point: HookPoint::of(Phase::After, Target::Op(Operation::Create)),
            priority: 0,
            budget: Duration::from_secs(5),
        }]
    }

    fn run_step<'a>(
        &'a self,
        _step: &'a StepName,
        ctx: &'a mut dyn HookContext,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async move {
            match ctx.doc().text("name")? {
                Some("Flash") => {
                    let made = ctx.create(customer("Tmp")).await?;
                    ctx.delete("customer", made.id(), made.version().unwrap())
                        .await
                }
                Some("Twice") => {
                    let made = ctx.create(customer("Tmp")).await?;
                    ctx.update(
                        "customer",
                        made.id(),
                        rename("Tmp2"),
                        made.version().unwrap(),
                    )
                    .await?;
                    Ok(())
                }
                _ => Ok(()),
            }
        })
    }
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

fn inv() -> Invocation {
    Invocation {
        site: SiteId::new("capture.pg.test").unwrap(),
        actor: Actor::System {
            component: n("capture_pg_tests"),
        },
        command_id: CommandId::new("cmd-pg").ok(),
        language: None,
        now: now(),
    }
}

/// A small deterministic generator, so the run is the same every time.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self, bound: usize) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        usize::try_from((self.0 >> 33) % u64::try_from(bound).unwrap()).unwrap()
    }
}

async fn connect(db: &TestDb) -> PgBackend {
    // The product requires PostgreSQL 18; a developer machine with an older server may say so
    // with `SPOT_PG_UNCHECKED=1` (never set in CI).
    if std::env::var_os("SPOT_PG_UNCHECKED").is_some() {
        PgBackend::connect_unchecked(db.config()).unwrap()
    } else {
        PgBackend::connect(db.config()).await.unwrap()
    }
}

fn position(p: FeedPosition) -> Position {
    Position {
        generation: p.generation(),
        seq: p.seq(),
    }
}

/// Runs the checker over everything the feed and the outbox hold now.
///
/// A delete is named by the last version before it, which is also the version of the entry the
/// document's previous write left in the feed, so the checker (which keys by document and
/// version) reports the pair as a `feed_duplicate` error (an open question of the PR). Deletes
/// are therefore compared here, one to one, and the checker sees every other entry.
async fn check(engine: &Engine<PgBackend>) -> spot_capture_invariant::Report {
    let mut feed = Vec::new();
    let mut after = FeedPosition::START;
    loop {
        let page = engine.backend().read_changes(after, 500).await.unwrap();
        let Some(last) = page.last().map(|c| c.position) else {
            break;
        };
        for change in page {
            feed.push(FeedEntry {
                position: position(change.position),
                entity: change.document.entity.as_str().to_owned(),
                id: change.document.id.to_string(),
                version: change.version.0,
                op: match change.op {
                    ChangeOp::Insert => Op::Insert,
                    ChangeOp::Update => Op::Update,
                    ChangeOp::Delete => Op::Delete,
                },
            });
        }
        after = last;
    }
    let mut events = Vec::new();
    let mut after = FeedPosition::START;
    loop {
        let page = engine.backend().read_outbox(after, 500).await.unwrap();
        let Some(last) = page.last().map(|r| r.position) else {
            break;
        };
        for record in page {
            if record.event.topic.as_str() != TOPIC_DOC_STATE {
                continue;
            }
            let payload = spot_events_contract::PayloadDocument::from_outbox_event(&record.event)
                .unwrap()
                .data;
            let op = match payload["op"].as_str().unwrap() {
                "insert" => Op::Insert,
                "update" => Op::Update,
                _ => Op::Delete,
            };
            if op == Op::Delete {
                // The image of a delete is the last committed version.
                assert_eq!(
                    payload["image"]["version"],
                    record.event.subject_version.unwrap().0
                );
            }
            events.push(StateEvent {
                position: position(record.position),
                entity: record.event.subject.entity.as_str().to_owned(),
                id: record.event.subject.id.to_string(),
                version: record.event.subject_version.unwrap().0,
                op,
            });
        }
        after = last;
    }
    assert!(feed.iter().any(|e| e.op == Op::Delete), "the run deletes");
    let mut feed_deletes: Vec<_> = feed
        .iter()
        .filter(|e| e.op == Op::Delete)
        .map(|e| (e.entity.clone(), e.id.clone(), e.version))
        .collect();
    feed_deletes.sort();
    let mut state_deletes: Vec<_> = events
        .iter()
        .filter(|e| e.op == Op::Delete)
        .map(|e| (e.entity.clone(), e.id.clone(), e.version))
        .collect();
    state_deletes.sort();
    assert_eq!(
        feed_deletes, state_deletes,
        "every delete has exactly one state event of the same version"
    );

    let mut checker = Checker::with_defaults();
    for entry in feed.into_iter().filter(|e| e.op != Op::Delete) {
        checker.push_feed(entry).unwrap();
    }
    for event in events.into_iter().filter(|e| e.op != Op::Delete) {
        checker.push_event(event).unwrap();
    }
    checker.finish()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn five_hundred_operations_leave_zero_findings_and_a_bypass_is_caught() {
    let Some(db) = TestDb::create().await else {
        assert!(
            std::env::var_os("SPOT_REQUIRE_PG").is_none(),
            "SPOT_REQUIRE_PG is set but SPOT_TEST_PG_URL gives no database"
        );
        return;
    };
    let mut builder = RegistryBuilder::new();
    builder.add_controller("churn", Arc::new(Churn));
    let engine = Engine::new(
        connect(&db).await,
        meta(),
        Arc::new(builder.build().unwrap()),
        Arc::new(ManualClock::new(now())),
        Arc::new(V7Ids),
    )
    .unwrap()
    .with_retry_policy(RetryPolicy {
        max_retries: 3,
        base_delay: Duration::ZERO,
        max_delay: Duration::ZERO,
    });
    engine.prepare().await.unwrap();

    // The customer every invoice links to is never deleted.
    let root = engine.create(&inv(), customer("Root")).await.unwrap().value;
    let mut loose: Vec<(RecordId, Version)> = Vec::new();
    let mut drafts: Vec<(RecordId, Version)> = Vec::new();
    let mut posted: Vec<(RecordId, Version)> = Vec::new();
    let mut rng = Lcg(0x5107_5107);
    let mut done = 1usize;
    while done < 500 {
        match rng.next(100) {
            // A plain customer.
            0..=24 => {
                let doc = engine
                    .create(&inv(), customer(&format!("C{done}")))
                    .await
                    .unwrap()
                    .value;
                loose.push((doc.id(), doc.version().unwrap()));
            }
            // Rename a customer.
            25..=44 if !loose.is_empty() => {
                let i = rng.next(loose.len());
                let (id, v) = loose[i];
                let doc = engine
                    .update(&inv(), "customer", id, rename(&format!("R{done}")), v)
                    .await
                    .unwrap()
                    .value;
                loose[i] = (id, doc.version().unwrap());
            }
            // Delete a customer nothing links to.
            45..=54 if !loose.is_empty() => {
                let i = rng.next(loose.len());
                let (id, v) = loose.swap_remove(i);
                engine.delete(&inv(), "customer", id, v).await.unwrap();
            }
            // An invoice with rows.
            55..=69 => {
                let mut doc = common::valid_invoice();
                doc.set(n("customer"), Value::Id(root.id()));
                doc.set(n("number"), format!("SI-{done}"));
                doc.set(
                    n("total"),
                    common::money(1_000 + i64::try_from(done).unwrap()),
                );
                let saved = engine.create(&inv(), doc).await.unwrap().value;
                drafts.push((saved.id(), saved.version().unwrap()));
            }
            // Replace an invoice's rows.
            70..=79 if !drafts.is_empty() => {
                let i = rng.next(drafts.len());
                let (id, v) = drafts[i];
                let mut patch = DocumentPatch::default();
                patch.tables.insert(
                    n("items"),
                    vec![spot_app_api::document::RowPatch {
                        id: None,
                        fields: [
                            (n("item"), Value::from(format!("part{done}"))),
                            (n("qty"), common::qty(1_000)),
                        ]
                        .into_iter()
                        .collect(),
                    }],
                );
                let doc = engine
                    .update(&inv(), "sales_invoice", id, patch, v)
                    .await
                    .unwrap()
                    .value;
                drafts[i] = (id, doc.version().unwrap());
            }
            // Post a draft.
            80..=84 if !drafts.is_empty() => {
                let i = rng.next(drafts.len());
                let (id, v) = drafts.swap_remove(i);
                let doc = engine
                    .post(&inv(), "sales_invoice", id, v)
                    .await
                    .unwrap()
                    .value;
                posted.push((id, doc.version().unwrap()));
            }
            // Cancel a posted invoice.
            85..=87 if !posted.is_empty() => {
                let i = rng.next(posted.len());
                let (id, v) = posted.swap_remove(i);
                engine.cancel(&inv(), "sales_invoice", id, v).await.unwrap();
            }
            // Delete a draft invoice.
            88..=91 if !drafts.is_empty() => {
                let i = rng.next(drafts.len());
                let (id, v) = drafts.swap_remove(i);
                engine.delete(&inv(), "sales_invoice", id, v).await.unwrap();
            }
            // A transaction that creates and deletes a document: neither entry nor event.
            92..=95 => {
                engine.create(&inv(), customer("Flash")).await.unwrap();
            }
            // A transaction that creates and updates a document: one insert at version 2.
            96..=99 => {
                engine.create(&inv(), customer("Twice")).await.unwrap();
            }
            _ => continue,
        }
        done += 1;
    }

    // The real change feed against the real outbox: nothing missing, nothing extra.
    let report = check(&engine).await;
    assert!(
        report.is_clean(),
        "capture invariant findings: {:?}",
        report.findings
    );
    assert!(report.feed_entries > 300, "{}", report.feed_entries);
    assert_eq!(report.feed_entries, report.state_events);
    assert_eq!(report.matched, report.feed_entries);

    // A write behind the engine's back: a transaction on the backend itself.
    let victim = loose
        .first()
        .copied()
        .unwrap_or((root.id(), root.version().unwrap()));
    let catalog = Arc::new(meta().catalog(Collation::new("und").unwrap()).unwrap());
    let mut tx = engine
        .backend()
        .begin(
            catalog,
            TxOptions::new(n("bypass"), Isolation::ReadCommitted),
        )
        .await
        .unwrap();
    let mut row = tx
        .load(&n("customer"), victim.0, RowLock::Update(Wait::Block))
        .await
        .unwrap()
        .unwrap();
    row.data
        .fields
        .insert(n("name"), Value::Text("Sneaked".to_owned()));
    tx.update(&row.data, row.version).await.unwrap();
    tx.commit().await.unwrap();

    let report = check(&engine).await;
    assert_eq!(report.findings.len(), 1, "{:?}", report.findings);
    let finding = &report.findings[0];
    assert_eq!(finding.kind, FindingKind::MissingStateEvent);
    assert_eq!(finding.entity, "customer");
    assert_eq!(finding.id, victim.0.to_string());
}
