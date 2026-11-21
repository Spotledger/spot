//! The edge participant through the real document engine on the fake backend (WP-DAG-04.T3):
//! the edge set equals the link set after create, update, post, cancel, correct and delete,
//! a failing edge write fails the document write, the schema follows the metadata version, and
//! a write for another site is refused.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::{Arc, RwLock};

use common::{doc, name, set};
use spot_app_api::context::{Actor, DocumentService, Invocation};
use spot_app_api::prelude::{CommandId, RecordId, SiteId, Value};
use spot_app_api::{Document, DocumentPatch};
use spot_core::ManualClock;
use spot_document::engine::{Engine, TxParticipant, V7Ids};
use spot_edge_sync::{
    EdgeParticipant, EdgeSchema, EdgeWriter, EdgeWriterFor, MemoryEdgeStore, MetaSource,
    check_invariant,
};
use spot_meta::{Customisations, Definition, EffectiveMeta, Registry};
use spot_seams::RegistryBuilder;
use spot_storage_fake::{FakeBackend, FakeTx};
use time::OffsetDateTime;

pub fn now() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_790_000_000).unwrap()
}

fn site() -> SiteId {
    SiteId::new("edges.test").unwrap()
}

fn inv_for(site: SiteId) -> Invocation {
    Invocation {
        site,
        actor: Actor::System {
            component: name("edge_tests"),
        },
        command_id: CommandId::new("cmd-edge").ok(),
        language: None,
        now: now(),
    }
}

fn inv() -> Invocation {
    inv_for(site())
}

/// The shared fixtures with `order` made a posted (cancellable, correctable) DocType.
pub fn engine_definitions() -> Vec<Definition> {
    let mut defs = common::definitions();
    for def in &mut defs {
        if def.name == "order" {
            def.is_posted = true;
            def.title_field = Some("number".to_owned());
        }
    }
    defs
}

fn build(customs: &Customisations) -> Arc<EffectiveMeta> {
    Arc::new(Registry::build(engine_definitions(), customs).unwrap())
}

/// The metadata the participant reads: the test swaps it, as a host does with `set_meta`.
#[derive(Clone)]
struct SharedMeta(Arc<RwLock<Arc<EffectiveMeta>>>);

impl MetaSource for SharedMeta {
    fn effective(&self) -> Arc<EffectiveMeta> {
        self.0.read().unwrap().clone()
    }
}

/// A factory whose writers refuse every change.
struct Failing;

impl EdgeWriterFor<FakeTx> for Failing {
    fn writer<'a>(&'a self, _tx: &'a mut FakeTx, _site: &'a SiteId) -> Box<dyn EdgeWriter + 'a> {
        Box::new(common::FailingWriter::default())
    }
}

struct Fx {
    engine: Engine<FakeBackend>,
    store: MemoryEdgeStore,
    meta: SharedMeta,
}

fn participant<F: EdgeWriterFor<FakeTx> + 'static>(
    meta: &SharedMeta,
    factory: F,
) -> EdgeParticipant<FakeTx, F> {
    EdgeParticipant::new(Arc::new(meta.clone()), factory)
        .with_relations(vec![common::relation_spec()])
}

async fn fx() -> Fx {
    let store = MemoryEdgeStore::new();
    fx_with(|meta| {
        vec![Arc::new(participant(meta, store.clone())) as Arc<dyn TxParticipant<FakeTx>>]
    })
    .await
    .with_store(store)
}

async fn fx_with(
    participants: impl FnOnce(&SharedMeta) -> Vec<Arc<dyn TxParticipant<FakeTx>>>,
) -> Fx {
    let effective = build(&Customisations::new());
    let meta = SharedMeta(Arc::new(RwLock::new(effective.clone())));
    let engine = Engine::new(
        FakeBackend::new(),
        effective,
        Arc::new(RegistryBuilder::new().build().unwrap()),
        Arc::new(ManualClock::new(now())),
        Arc::new(V7Ids),
    )
    .unwrap();
    engine.prepare().await.unwrap();
    engine.set_participants(participants(&meta));
    Fx {
        engine,
        store: MemoryEdgeStore::new(),
        meta,
    }
}

impl Fx {
    fn with_store(mut self, store: MemoryEdgeStore) -> Self {
        self.store = store;
        self
    }

    fn schema(&self) -> EdgeSchema {
        EdgeSchema::derive(&self.meta.effective(), &[common::relation_spec()])
    }

    async fn party(&self) -> RecordId {
        let mut party = doc("party", RecordId::new_v7());
        set(&mut party, "code", Value::Text("P".into()));
        self.engine.create(&inv(), party).await.unwrap().value.id()
    }

    async fn order(&self, customer: RecordId) -> Document {
        let mut order = doc("order", RecordId::new_v7());
        set(&mut order, "number", Value::Text("SO-1".into()));
        set(&mut order, "customer", Value::Id(customer));
        self.engine.create(&inv(), order).await.unwrap().value
    }

    async fn live(&self, ids: &[(&str, RecordId)]) -> Vec<Document> {
        let mut out = Vec::new();
        for (doctype, id) in ids {
            if let Some(doc) = self.engine.peek(doctype, *id).await.unwrap() {
                out.push(doc);
            }
        }
        out
    }

    /// The invariant over `ids`: the stored edges are exactly the live documents' links.
    async fn drift(&self, ids: &[(&str, RecordId)]) -> usize {
        let docs = self.live(ids).await;
        check_invariant(
            &self.schema(),
            docs.iter(),
            self.store.all(self.engine.backend()).await.unwrap(),
        )
        .len()
    }
}

#[tokio::test]
async fn create_update_and_delete_keep_the_edge_set_equal_to_the_link_set() {
    let f = fx().await;
    let (a, b) = (f.party().await, f.party().await);
    let order = f.order(a).await;
    let key = [("order", order.id())];
    assert_eq!(
        f.store.all(f.engine.backend()).await.unwrap().len(),
        1,
        "one link, one edge"
    );
    assert_eq!(f.drift(&key).await, 0);

    let mut patch = DocumentPatch::default();
    patch.fields.insert(name("customer"), Value::Id(b));
    f.engine
        .update(&inv(), "order", order.id(), patch, order.version().unwrap())
        .await
        .unwrap();
    assert_eq!(
        f.store.all(f.engine.backend()).await.unwrap().len(),
        1,
        "the edge moved, it did not duplicate"
    );
    assert_eq!(f.drift(&key).await, 0);
    let moved = f
        .store
        .all(f.engine.backend())
        .await
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    assert_eq!(moved.to.id, b);

    let current = f.engine.peek("order", order.id()).await.unwrap().unwrap();
    f.engine
        .delete(&inv(), "order", order.id(), current.version().unwrap())
        .await
        .unwrap();
    assert!(
        f.store.all(f.engine.backend()).await.unwrap().is_empty(),
        "delete removes the edges"
    );
    assert_eq!(f.drift(&key).await, 0);
}

#[tokio::test]
async fn table_rows_dynamic_links_and_multiselect_become_edges() {
    let f = fx().await;
    let (a, b, c) = (f.party().await, f.party().await, f.party().await);
    let mut order = doc("order", RecordId::new_v7());
    set(&mut order, "customer", Value::Id(a));
    set(&mut order, "ref_kind", Value::Text("party".into()));
    set(&mut order, "ref", Value::Id(b));
    order
        .rows_mut(name("lines"))
        .push(common::row(RecordId::new_v7(), &[("item", Value::Id(c))]));
    order
        .rows_mut(name("watchers"))
        .push(common::row(RecordId::new_v7(), &[("party", Value::Id(a))]));
    let order = f.engine.create(&inv(), order).await.unwrap().value;
    // customer, ref, line containment, the line's item, the watcher.
    assert_eq!(f.store.all(f.engine.backend()).await.unwrap().len(), 5);
    assert_eq!(f.drift(&[("order", order.id())]).await, 0);
}

#[tokio::test]
async fn post_and_cancel_keep_the_edges_of_the_document() {
    let f = fx().await;
    let a = f.party().await;
    let order = f.order(a).await;
    let key = [("order", order.id())];
    let posted = f
        .engine
        .post(&inv(), "order", order.id(), order.version().unwrap())
        .await
        .unwrap()
        .value;
    assert_eq!(f.store.all(f.engine.backend()).await.unwrap().len(), 1);
    assert_eq!(f.drift(&key).await, 0);
    f.engine
        .cancel(&inv(), "order", order.id(), posted.version().unwrap())
        .await
        .unwrap();
    // A cancelled document still holds its link, so it still has its edge.
    assert_eq!(
        f.store.all(f.engine.backend()).await.unwrap().len(),
        1,
        "cancel keeps the edges"
    );
    assert_eq!(f.drift(&key).await, 0);
}

#[tokio::test]
async fn correct_keeps_the_cancelled_original_and_gives_the_amended_draft_its_own_edges() {
    let f = fx().await;
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
    assert_ne!(amended.id(), order.id());
    // The original's customer edge, the draft's customer edge and the draft's `amended_from`
    // link to the original (an engine-managed Link field is an edge like any other).
    assert_eq!(f.store.all(f.engine.backend()).await.unwrap().len(), 3);
    let ids = [("order", order.id()), ("order", amended.id())];
    assert_eq!(f.drift(&ids).await, 0);

    // Deleting the amended draft removes only its own edges.
    f.engine
        .delete(&inv(), "order", amended.id(), amended.version().unwrap())
        .await
        .unwrap();
    assert_eq!(f.store.all(f.engine.backend()).await.unwrap().len(), 1);
    assert_eq!(f.drift(&ids).await, 0);
}

#[tokio::test]
async fn a_failing_edge_write_rolls_back_the_document_write() {
    let f = fx_with(|meta| vec![Arc::new(participant(meta, Failing)) as Arc<dyn TxParticipant<_>>])
        .await;
    let mut order = doc("order", RecordId::new_v7());
    set(&mut order, "customer", Value::Id(f.party().await));
    let id = order.id();
    let error = f.engine.create(&inv(), order).await.unwrap_err();
    assert_eq!(error.code, "edge_sync.write_failed");
    assert!(
        f.engine.peek("order", id).await.unwrap().is_none(),
        "the document did not survive its failed edge write"
    );
    // A document with no links needs no edge write, so it is created.
    let plain = doc("order", RecordId::new_v7());
    let plain_id = plain.id();
    f.engine.create(&inv(), plain).await.unwrap();
    assert!(f.engine.peek("order", plain_id).await.unwrap().is_some());
}

#[tokio::test]
async fn a_failing_edge_write_on_update_leaves_the_stored_document_unchanged() {
    // Create succeeds (no links), then an update that adds a link must fail whole.
    let f = fx_with(|meta| vec![Arc::new(participant(meta, Failing)) as Arc<dyn TxParticipant<_>>])
        .await;
    let plain = doc("order", RecordId::new_v7());
    let created = f.engine.create(&inv(), plain).await.unwrap().value;
    let mut patch = DocumentPatch::default();
    patch
        .fields
        .insert(name("customer"), Value::Id(f.party().await));
    let error = f
        .engine
        .update(
            &inv(),
            "order",
            created.id(),
            patch,
            created.version().unwrap(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, "edge_sync.write_failed");
    let stored = f.engine.peek("order", created.id()).await.unwrap().unwrap();
    assert_eq!(stored.version(), created.version());
    assert_eq!(stored.get("customer"), &Value::Null);
}

#[tokio::test]
async fn the_schema_follows_the_metadata_version() {
    let f = fx().await;
    let party = f.party().await;
    let before = f.order(party).await;
    assert_eq!(f.store.all(f.engine.backend()).await.unwrap().len(), 1);

    // A custom Link field is added at runtime: new metadata, same participant, no code change.
    let mut customs = Customisations::new();
    customs.add_field("order", common::link("custom_ref", "party"), None);
    let next = build(&customs);
    assert!(next.version() >= f.meta.effective().version());
    f.engine.set_meta(next.clone()).await.unwrap();
    *f.meta.0.write().unwrap() = next;

    let mut order = doc("order", RecordId::new_v7());
    set(&mut order, "custom_ref", Value::Id(party));
    let order = f.engine.create(&inv(), order).await.unwrap().value;
    assert_eq!(
        f.store.all(f.engine.backend()).await.unwrap().len(),
        2,
        "the runtime link field has its edge"
    );
    let ids = [("order", before.id()), ("order", order.id())];
    assert_eq!(f.drift(&ids).await, 0);
}

/// Metadata whose `order` DocType also has a Link field `extra_ref`, built as a definition (not
/// a customisation), so its `version()` equals the base metadata's.
fn build_with_extra_link() -> Arc<EffectiveMeta> {
    let mut defs = engine_definitions();
    for def in &mut defs {
        if def.name == "order" {
            def.fields.push(common::link("extra_ref", "party"));
        }
    }
    Arc::new(Registry::build(defs, &Customisations::new()).unwrap())
}

#[tokio::test]
async fn a_new_generation_with_the_same_version_is_rederived() {
    let f = fx().await;
    let party = f.party().await;
    let next = build_with_extra_link();
    assert_eq!(
        next.version(),
        f.meta.effective().version(),
        "the trap: same version, different content"
    );
    // Warm the cache on the first generation.
    f.order(party).await;
    f.engine.set_meta(next.clone()).await.unwrap();
    *f.meta.0.write().unwrap() = next;

    let mut order = doc("order", RecordId::new_v7());
    set(&mut order, "extra_ref", Value::Id(party));
    f.engine.create(&inv(), order).await.unwrap();
    assert_eq!(
        f.store.all(f.engine.backend()).await.unwrap().len(),
        2,
        "the new Link field has its edge although version() did not change"
    );
}

#[test]
fn an_unchanged_generation_is_derived_once_and_a_swap_is_seen_whole_by_concurrent_calls() {
    let current = Arc::new(RwLock::new(build(&Customisations::new())));
    let cur = current.clone();
    let source = move || cur.read().unwrap().clone();
    let p = Arc::new(
        EdgeParticipant::<FakeTx, _>::new(Arc::new(source), MemoryEdgeStore::new())
            .with_relations(vec![common::relation_spec()]),
    );
    let a = p.current_schema();
    assert!(
        Arc::ptr_eq(&a, &p.current_schema()),
        "an unchanged generation reuses the schema"
    );
    let mut probe = doc("order", RecordId::new_v7());
    set(&mut probe, "extra_ref", Value::Id(RecordId::new_v7()));
    assert_eq!(
        a.edges_of(&probe).len(),
        0,
        "the first generation has no such edge"
    );

    *current.write().unwrap() = build_with_extra_link();
    let handles: Vec<_> = (0..16)
        .map(|_| {
            let p = p.clone();
            std::thread::spawn(move || p.current_schema())
        })
        .collect();
    for h in handles {
        assert_eq!(
            h.join().unwrap().edges_of(&probe).len(),
            1,
            "a call after the swap sees the new generation whole"
        );
    }
    assert!(Arc::ptr_eq(&p.current_schema(), &p.current_schema()));
}

#[tokio::test]
async fn a_participant_serves_one_site_and_refuses_a_write_for_another() {
    let store = MemoryEdgeStore::new();
    let f = fx_with(|meta| {
        vec![Arc::new(participant(meta, store.clone()).for_site(site()))
            as Arc<dyn TxParticipant<_>>]
    })
    .await;
    let other = inv_for(SiteId::new("other.test").unwrap());
    let mut order = doc("order", RecordId::new_v7());
    set(&mut order, "customer", Value::Id(f.party().await));
    let id = order.id();
    let error = f.engine.create(&other, order).await.unwrap_err();
    assert_eq!(error.code, "edge_sync.write_failed");
    assert!(error.message.contains("another site"), "{}", error.message);
    assert!(f.engine.peek("order", id).await.unwrap().is_none());
    assert!(
        store.all(f.engine.backend()).await.unwrap().is_empty(),
        "nothing was written for the wrong site"
    );
}

#[tokio::test]
async fn a_doctype_the_schema_does_not_know_fails_closed() {
    // The participant reads older metadata that lacks `order`, while the engine serves it.
    let old = Arc::new(Registry::build(vec![common::party()], &Customisations::new()).unwrap());
    let store = MemoryEdgeStore::new();
    let f = fx_with(|_| {
        let meta = SharedMeta(Arc::new(RwLock::new(old.clone())));
        vec![Arc::new(participant(&meta, store.clone())) as Arc<dyn TxParticipant<_>>]
    })
    .await;
    let order = doc("order", RecordId::new_v7());
    let id = order.id();
    let error = f.engine.create(&inv(), order).await.unwrap_err();
    assert_eq!(error.code, "edge_sync.unknown_doctype");
    assert!(f.engine.peek("order", id).await.unwrap().is_none());
}

#[tokio::test]
async fn a_rename_changes_no_link_and_so_no_edge() {
    let f = fx().await;
    let a = f.party().await;
    let order = f.order(a).await;
    let before = f.store.all(f.engine.backend()).await.unwrap();
    f.engine
        .rename(
            &inv(),
            "order",
            order.id(),
            "SO-9",
            order.version().unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(f.store.all(f.engine.backend()).await.unwrap(), before);
    assert_eq!(f.drift(&[("order", order.id())]).await, 0);
}

/// A participant that refuses every write after the edge participant has run.
struct RefuseAfter;

impl<Tx: spot_storage_contract::Transaction> TxParticipant<Tx> for RefuseAfter {
    fn after_write<'a>(
        &'a self,
        _tx: &'a mut Tx,
        write: &'a spot_document::engine::WriteRecord<'a>,
    ) -> spot_app_api::BoxFuture<'a, spot_app_api::AppResult<()>> {
        // Only an order is refused: the party it links to must be creatable first.
        let refuse = write.doctype == "order";
        Box::pin(async move {
            if !refuse {
                return Ok(());
            }
            Err(spot_app_api::AppError::new(
                spot_app_api::ErrorClass::Internal,
                "test.refused_after_edges",
                "refused",
            ))
        })
    }
}

#[tokio::test]
async fn edges_roll_back_with_the_document_when_a_later_participant_fails() {
    let store = MemoryEdgeStore::new();
    let f = fx_with(|meta| {
        vec![
            Arc::new(participant(meta, store.clone())) as Arc<dyn TxParticipant<FakeTx>>,
            Arc::new(RefuseAfter),
        ]
    })
    .await;
    let mut order = doc("order", RecordId::new_v7());
    set(&mut order, "customer", Value::Id(f.party().await));
    let id = order.id();
    let error = f.engine.create(&inv(), order).await.unwrap_err();
    assert_eq!(error.code, "test.refused_after_edges");
    assert!(f.engine.peek("order", id).await.unwrap().is_none());
    assert!(
        store.all(f.engine.backend()).await.unwrap().is_empty(),
        "the edge written before the failure rolled back with the document"
    );
}

#[test]
fn a_relation_edge_never_carries_a_secret_masked_or_raw_bytes_value() {
    use spot_edge_sync::{PropValue, edges_of};
    use spot_meta::{FieldSpec, FieldType, MaskSpec};

    let mut defs = common::definitions();
    for def in &mut defs {
        if def.name == "task_link" {
            def.protection_groups.push(spot_meta::ProtectionGroupSpec {
                name: "pii".to_owned(),
                label: None,
                trace: false,
                sync_class_hint: None,
            });
            let mut note = FieldSpec::new("note", FieldType::Text);
            note.masking = Some(MaskSpec::default());
            let mut classified = FieldSpec::new("tax_id", FieldType::Text);
            classified.protection_group = Some("pii".to_owned());
            def.fields.extend([
                note,
                classified,
                FieldSpec::new("token", FieldType::Secret),
                FieldSpec::new("blob", FieldType::Bytes),
            ]);
        }
    }
    let meta = Registry::build(defs, &Customisations::new()).unwrap();
    let schema = EdgeSchema::derive(&meta, &[common::relation_spec()]);

    let mut link = doc("task_link", RecordId::new_v7());
    set(&mut link, "order", Value::Id(RecordId::new_v7()));
    set(&mut link, "project", Value::Id(RecordId::new_v7()));
    set(&mut link, "role", Value::Text("lead".into()));
    set(&mut link, "note", Value::Text("card 4111".into()));
    set(&mut link, "tax_id", Value::Text("12-345".into()));
    set(&mut link, "token", Value::Text("hunter2".into()));
    set(&mut link, "blob", Value::Bytes(vec![1, 2, 3]));
    let edge = edges_of(&schema, &link).into_iter().next().unwrap();
    let keys: Vec<&str> = edge.properties.keys().map(String::as_str).collect();
    assert_eq!(keys, ["role"], "only the plain field is copied");
    assert_eq!(edge.properties["role"], PropValue::Text("lead".into()));
}
