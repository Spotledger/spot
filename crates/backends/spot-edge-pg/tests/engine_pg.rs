//! The edge participant through the real document engine on the real PostgreSQL backend
//! (WP-TST-14.T6; the fake-backend twin is `spot-edge-sync/tests/engine.rs`): the edge set equals
//! the link set after create, update, delete, post, cancel and correct; a failure after the edge
//! write rolls the edges back with the document; a write for another site is refused.
//!
//! Each test makes its own scratch database, so the suite runs in parallel. Without a server the
//! tests print `SKIPPED:` and return; `SPOT_REQUIRE_PG=1` turns that into a failure.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, RwLock};

use spot_app_api::context::{Actor, DocumentService, Invocation};
use spot_app_api::document::{ChildRow, Document};
use spot_app_api::prelude::{CommandId, Name, RecordId, SiteId, Value};
use spot_app_api::{AppError, AppResult, BoxFuture, DocumentPatch, ErrorClass};
use spot_core::ManualClock;
use spot_document::engine::{Engine, TxParticipant, V7Ids, WriteRecord};
use spot_edge_pg::{PgEdgeWriter, PgEdgeWriters};
use spot_edge_sync::{Edge, EdgeParticipant, EdgeSchema, MetaSource, check_invariant};
use spot_graph_model::RelationSpec;
use spot_meta::{Customisations, Definition, EffectiveMeta, FieldSpec, FieldType, Registry};
use spot_postgres::testing::TestDb;
use spot_postgres::{PgBackend, PgTransaction};
use spot_seams::RegistryBuilder;
use spot_storage_contract::{Isolation, NodeRef, StorageBackend, Transaction, TxOptions};
use time::OffsetDateTime;

// ---- fixtures (copied from spot-edge-sync/tests/common and engine.rs) ----

fn name(value: &str) -> Name {
    Name::new(value).unwrap()
}

fn link(field: &str, target: &str) -> FieldSpec {
    let mut spec = FieldSpec::new(field, FieldType::Link);
    spec.options = Some(target.to_owned());
    spec
}

fn text(field: &str) -> FieldSpec {
    FieldSpec::new(field, FieldType::Text)
}

fn doc(doctype: &str, id: RecordId) -> Document {
    Document::new(name(doctype), id)
}

fn set(doc: &mut Document, field: &str, value: Value) {
    doc.set(name(field), value);
}

fn row(id: RecordId, fields: &[(&str, Value)]) -> ChildRow {
    let mut map = std::collections::BTreeMap::new();
    for (field, value) in fields {
        map.insert(name(field), value.clone());
    }
    ChildRow { id, fields: map }
}

fn relation_spec() -> RelationSpec {
    RelationSpec {
        doctype: "task_link".to_owned(),
        from_field: "order".to_owned(),
        to_field: "project".to_owned(),
        edge_name: "task_link.relates".to_owned(),
        aggregate: None,
    }
}

/// The fixture DocTypes, with `order` a posted (cancellable, correctable) DocType.
fn engine_definitions() -> Vec<Definition> {
    let mut party = Definition::new("party");
    party.fields = vec![text("code")];

    let mut order = Definition::new("order");
    let mut kind = FieldSpec::new("ref_kind", FieldType::Select);
    kind.options = Some("party\nproject".to_owned());
    let mut ref_field = FieldSpec::new("ref", FieldType::DynamicLink);
    ref_field.options = Some("ref_kind".to_owned());
    let mut lines = FieldSpec::new("lines", FieldType::Table);
    lines.options = Some("order_line".to_owned());
    let mut watchers = FieldSpec::new("watchers", FieldType::TableMultiselect);
    watchers.options = Some("order_watcher".to_owned());
    order.fields = vec![
        text("number"),
        link("customer", "party"),
        kind,
        ref_field,
        lines,
        watchers,
    ];
    order.is_posted = true;
    order.title_field = Some("number".to_owned());

    let mut line = Definition::new("order_line");
    line.is_child = true;
    line.fields = vec![link("item", "party"), text("qty")];

    let mut watcher = Definition::new("order_watcher");
    watcher.is_child = true;
    watcher.fields = vec![link("party", "party")];

    let mut project = Definition::new("project");
    project.fields = vec![text("title"), link("customer", "party")];

    let mut task_link = Definition::new("task_link");
    task_link.fields = vec![
        link("order", "order"),
        link("project", "project"),
        text("role"),
    ];
    vec![party, order, line, watcher, project, task_link]
}

fn now() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_790_000_000).unwrap()
}

fn site() -> SiteId {
    SiteId::new("edges.pg").unwrap()
}

fn inv_for(site: SiteId) -> Invocation {
    Invocation {
        site,
        actor: Actor::System {
            component: name("edge_pg_tests"),
        },
        command_id: CommandId::new("cmd-edge-pg").ok(),
        language: None,
        now: now(),
    }
}

fn inv() -> Invocation {
    inv_for(site())
}

#[derive(Clone)]
struct FixedMeta(Arc<RwLock<Arc<EffectiveMeta>>>);

impl MetaSource for FixedMeta {
    fn effective(&self) -> Arc<EffectiveMeta> {
        self.0.read().unwrap().clone()
    }
}

/// A participant that refuses every write of an order, run after the edge participant.
struct RefuseAfter;

impl TxParticipant<PgTransaction> for RefuseAfter {
    fn after_write<'a>(
        &'a self,
        _tx: &'a mut PgTransaction,
        write: &'a WriteRecord<'a>,
    ) -> BoxFuture<'a, AppResult<()>> {
        // Only an order is refused: the parties it links to must be creatable first.
        let refuse = write.doctype == "order";
        Box::pin(async move {
            if refuse {
                return Err(AppError::new(
                    ErrorClass::Internal,
                    "test.refused_after_edges",
                    "refused",
                ));
            }
            Ok(())
        })
    }
}

// ---- the environment: a scratch database, the engine on it, the edge tables ----

struct Fx {
    db: TestDb,
    engine: Engine<PgBackend>,
    meta: FixedMeta,
    /// Every document ever created, by doctype, so the invariant also sees the edges of
    /// documents that were deleted (they must have none left).
    seen: std::sync::Mutex<Vec<(&'static str, RecordId)>>,
}

/// `None` without a server. `participants` is given the metadata source and returns the
/// participants the engine runs, in order.
async fn fx_with(
    participants: impl FnOnce(&FixedMeta) -> Vec<Arc<dyn TxParticipant<PgTransaction>>>,
) -> Option<Fx> {
    let db = TestDb::create().await?;
    let backend = PgBackend::connect(db.config()).await.expect("connect");
    let effective =
        Arc::new(Registry::build(engine_definitions(), &Customisations::new()).unwrap());
    let meta = FixedMeta(Arc::new(RwLock::new(effective.clone())));
    let engine = Engine::new(
        backend,
        effective,
        Arc::new(RegistryBuilder::new().build().unwrap()),
        Arc::new(ManualClock::new(now())),
        Arc::new(V7Ids),
    )
    .unwrap();
    engine.prepare().await.expect("prepare documents");
    {
        let mut client = db.raw_client().await;
        spot_edge_pg::schema::prepare(&mut client).await.unwrap();
    }
    engine.set_participants(participants(&meta));
    Some(Fx {
        db,
        engine,
        meta,
        seen: std::sync::Mutex::new(Vec::new()),
    })
}

fn participant(meta: &FixedMeta) -> EdgeParticipant<PgTransaction, PgEdgeWriters> {
    EdgeParticipant::new(Arc::new(meta.clone()), PgEdgeWriters)
        .with_relations(vec![relation_spec()])
}

async fn fx() -> Option<Fx> {
    fx_with(|meta| vec![Arc::new(participant(meta)) as Arc<dyn TxParticipant<PgTransaction>>]).await
}

impl Fx {
    fn schema(&self) -> EdgeSchema {
        EdgeSchema::derive(&self.meta.effective(), &[relation_spec()])
    }

    fn track(&self, doctype: &'static str, id: RecordId) {
        self.seen.lock().unwrap().push((doctype, id));
    }

    async fn count(&self, table: &str) -> i64 {
        let client = self.db.raw_client().await;
        client
            .query_one(&format!("SELECT count(*) FROM \"{table}\""), &[])
            .await
            .unwrap()
            .get(0)
    }

    /// Every stored edge: each tracked document's owned edges, read through a fresh transaction,
    /// and the total row count of the table must equal what was found (no row of an owner the
    /// test never created).
    async fn stored_edges(&self) -> Vec<Edge> {
        let options = TxOptions::new(name("edge_read"), Isolation::ReadCommitted);
        let mut tx = self
            .engine
            .backend()
            .begin(self.engine_catalog(), options)
            .await
            .unwrap();
        let mut edges = Vec::new();
        {
            let mut writer = PgEdgeWriter::new(&mut tx);
            let owners = self.seen.lock().unwrap().clone();
            for (doctype, id) in owners {
                let owner = NodeRef {
                    entity: name(doctype),
                    id,
                };
                edges.extend(writer.edges_owned_by(&owner).await.unwrap());
            }
        }
        tx.rollback().await.unwrap();
        edges.sort();
        edges.dedup();
        let rows = self.count("spot_graph_edge").await;
        assert_eq!(
            usize::try_from(rows).unwrap(),
            edges.len(),
            "the table holds only the edges of documents this test made"
        );
        edges
    }

    fn engine_catalog(&self) -> Arc<spot_storage_contract::Catalog> {
        let meta = self.meta.effective();
        Arc::new(
            meta.catalog(spot_storage_contract::Collation::new("und").unwrap())
                .unwrap(),
        )
    }

    async fn party(&self) -> RecordId {
        let mut party = doc("party", RecordId::new_v7());
        set(&mut party, "code", Value::Text("P".into()));
        let id = self.engine.create(&inv(), party).await.unwrap().value.id();
        self.track("party", id);
        id
    }

    async fn order(&self, customer: RecordId) -> Document {
        let mut order = doc("order", RecordId::new_v7());
        set(&mut order, "number", Value::Text("SO-1".into()));
        set(&mut order, "customer", Value::Id(customer));
        let order = self.engine.create(&inv(), order).await.unwrap().value;
        self.track("order", order.id());
        order
    }

    /// The invariant over the live documents among `ids`: the stored edges are exactly their links.
    async fn drift(&self, ids: &[(&str, RecordId)]) -> usize {
        let mut docs = Vec::new();
        for (doctype, id) in ids {
            if let Some(doc) = self.engine.peek(doctype, *id).await.unwrap() {
                docs.push(doc);
            }
        }
        check_invariant(&self.schema(), docs.iter(), self.stored_edges().await).len()
    }
}

// ---- (a) create, update, delete ----

#[tokio::test]
async fn create_update_and_delete_keep_the_edge_set_equal_to_the_link_set() {
    let Some(f) = fx().await else {
        return;
    };
    let (a, b) = (f.party().await, f.party().await);
    let order = f.order(a).await;
    let key = [("order", order.id())];
    assert_eq!(f.stored_edges().await.len(), 1, "one link, one edge");
    assert_eq!(f.drift(&key).await, 0);

    let mut patch = DocumentPatch::default();
    patch.fields.insert(name("customer"), Value::Id(b));
    f.engine
        .update(&inv(), "order", order.id(), patch, order.version().unwrap())
        .await
        .unwrap();
    let edges = f.stored_edges().await;
    assert_eq!(edges.len(), 1, "the edge moved, it did not duplicate");
    assert_eq!(edges[0].to.id, b);
    assert_eq!(f.drift(&key).await, 0);

    let current = f.engine.peek("order", order.id()).await.unwrap().unwrap();
    f.engine
        .delete(&inv(), "order", order.id(), current.version().unwrap())
        .await
        .unwrap();
    assert!(
        f.stored_edges().await.is_empty(),
        "delete removes the edges"
    );
    assert_eq!(f.count("spot_graph_edge").await, 0);
    assert_eq!(f.drift(&key).await, 0);
}

#[tokio::test]
async fn table_rows_dynamic_links_and_multiselect_become_edges() {
    let Some(f) = fx().await else {
        return;
    };
    let (a, b, c) = (f.party().await, f.party().await, f.party().await);
    let mut order = doc("order", RecordId::new_v7());
    set(&mut order, "customer", Value::Id(a));
    set(&mut order, "ref_kind", Value::Text("party".into()));
    set(&mut order, "ref", Value::Id(b));
    order
        .rows_mut(name("lines"))
        .push(row(RecordId::new_v7(), &[("item", Value::Id(c))]));
    order
        .rows_mut(name("watchers"))
        .push(row(RecordId::new_v7(), &[("party", Value::Id(a))]));
    let order = f.engine.create(&inv(), order).await.unwrap().value;
    f.track("order", order.id());
    // customer, ref, line containment, the line's item, the watcher.
    assert_eq!(f.stored_edges().await.len(), 5);
    assert_eq!(f.drift(&[("order", order.id())]).await, 0);
}

// ---- (b) post, cancel, correct ----

#[tokio::test]
async fn post_and_cancel_keep_the_edges_of_the_document() {
    let Some(f) = fx().await else {
        return;
    };
    let a = f.party().await;
    let order = f.order(a).await;
    let key = [("order", order.id())];
    let posted = f
        .engine
        .post(&inv(), "order", order.id(), order.version().unwrap())
        .await
        .unwrap()
        .value;
    assert_eq!(f.stored_edges().await.len(), 1);
    assert_eq!(f.drift(&key).await, 0);
    f.engine
        .cancel(&inv(), "order", order.id(), posted.version().unwrap())
        .await
        .unwrap();
    // A cancelled document still holds its link, so it still has its edge.
    assert_eq!(f.stored_edges().await.len(), 1, "cancel keeps the edges");
    assert_eq!(f.drift(&key).await, 0);
}

#[tokio::test]
async fn correct_keeps_the_cancelled_original_and_gives_the_amended_draft_its_own_edges() {
    let Some(f) = fx().await else {
        return;
    };
    let a = f.party().await;
    let order = f.order(a).await;
    let posted = f
        .engine
        .post(&inv(), "order", order.id(), order.version().unwrap())
        .await
        .unwrap()
        .value;
    let amended = f
        .engine
        .correct(&inv(), "order", order.id(), posted.version().unwrap())
        .await
        .unwrap()
        .value;
    f.track("order", amended.id());
    assert_ne!(amended.id(), order.id());
    // The original's customer edge, the draft's customer edge and the draft's `amended_from`
    // link to the original.
    assert_eq!(f.stored_edges().await.len(), 3);
    let ids = [("order", order.id()), ("order", amended.id())];
    assert_eq!(f.drift(&ids).await, 0);

    // Deleting the amended draft removes only its own edges.
    f.engine
        .delete(&inv(), "order", amended.id(), amended.version().unwrap())
        .await
        .unwrap();
    assert_eq!(f.stored_edges().await.len(), 1);
    assert_eq!(f.drift(&ids).await, 0);
}

// ---- (c) atomicity ----

#[tokio::test]
async fn edges_roll_back_with_the_document_when_a_later_participant_fails() {
    let Some(f) = fx_with(|meta| {
        vec![
            Arc::new(participant(meta)) as Arc<dyn TxParticipant<PgTransaction>>,
            Arc::new(RefuseAfter),
        ]
    })
    .await
    else {
        return;
    };
    let mut order = doc("order", RecordId::new_v7());
    set(&mut order, "customer", Value::Id(f.party().await));
    let id = order.id();
    f.track("order", id);
    let error = f.engine.create(&inv(), order).await.unwrap_err();
    assert_eq!(error.code, "test.refused_after_edges");
    assert!(f.engine.peek("order", id).await.unwrap().is_none());
    assert_eq!(
        f.count("order").await,
        0,
        "no document row, counted on a second connection"
    );
    assert_eq!(
        f.count("spot_graph_edge").await,
        0,
        "the edge written before the failure rolled back with the document"
    );
}

#[tokio::test]
async fn a_failing_update_after_the_edge_write_leaves_document_and_edges_unchanged() {
    // The refusing participant is installed after the create, so the update is what fails.
    let Some(f) = fx().await else {
        return;
    };
    let (a, b) = (f.party().await, f.party().await);
    let order = f.order(a).await;
    f.engine.set_participants(vec![
        Arc::new(participant(&f.meta)) as Arc<dyn TxParticipant<PgTransaction>>,
        Arc::new(RefuseAfter),
    ]);
    let mut patch = DocumentPatch::default();
    patch.fields.insert(name("customer"), Value::Id(b));
    let error = f
        .engine
        .update(&inv(), "order", order.id(), patch, order.version().unwrap())
        .await
        .unwrap_err();
    assert_eq!(error.code, "test.refused_after_edges");
    let stored = f.engine.peek("order", order.id()).await.unwrap().unwrap();
    assert_eq!(stored.version(), order.version());
    assert_eq!(stored.get("customer"), &Value::Id(a));
    let edges = f.stored_edges().await;
    assert_eq!(edges.len(), 1);
    assert_eq!(
        edges[0].to.id, a,
        "the edge still points at the old customer"
    );
    assert_eq!(f.drift(&[("order", order.id())]).await, 0);
}

// ---- (d) another site ----

#[tokio::test]
async fn a_participant_serves_one_site_and_refuses_a_write_for_another() {
    let Some(f) = fx_with(|meta| {
        vec![Arc::new(participant(meta).for_site(site())) as Arc<dyn TxParticipant<PgTransaction>>]
    })
    .await
    else {
        return;
    };
    let other = inv_for(SiteId::new("other.test").unwrap());
    let mut order = doc("order", RecordId::new_v7());
    set(&mut order, "customer", Value::Id(f.party().await));
    let id = order.id();
    f.track("order", id);
    let error = f.engine.create(&other, order).await.unwrap_err();
    assert_eq!(error.code, "edge_sync.write_failed");
    assert!(error.message.contains("another site"), "{}", error.message);
    assert!(f.engine.peek("order", id).await.unwrap().is_none());
    assert_eq!(f.count("order").await, 0);
    assert_eq!(
        f.count("spot_graph_edge").await,
        0,
        "nothing was written for the wrong site"
    );
}
