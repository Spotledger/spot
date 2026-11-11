//! The owner check inside the document engine (WP-EDG-03.T2): the participant fires on every
//! write path, refuses a write this node does not own with nothing left behind, and decides
//! exactly as the `partition:owner_check` seam does.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use serde_json::{Value as Json, json};
use spot_app_api::context::{Actor, DocumentService, HookContext, Invocation, MethodContext};
use spot_app_api::controller::{MethodDecl, StepDecl, StepName};
use spot_app_api::prelude::{CommandId, Name, RecordId, SiteId, Version};
use spot_app_api::seams::SeamProvider;
use spot_app_api::{
    AppResult, BoxFuture, Controller, DocState, Document, DocumentPatch, ErrorClass,
};
use spot_core::ManualClock;
use spot_document::engine::{Engine, TxParticipant, V7Ids};
use spot_meta::{Definition, FieldSpec, FieldType, Registry};
use spot_partition::{
    AssignmentRequest, MemoryPartitionRegistry, OwnerCheckSeam, OwnerParticipant,
    PartitionRegistry, ScopeFields, ScopeKey,
};
use spot_seams::RegistryBuilder;
use spot_storage_contract::{ChangeFeed, FeedPosition};
use spot_storage_fake::FakeBackend;
use spot_sync_protocol::NodeId;
use time::OffsetDateTime;

const NOW: i64 = 1_790_000_000;

fn n(s: &str) -> Name {
    Name::new(s).unwrap()
}

fn at(seconds: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(seconds).unwrap()
}

fn node(id: &str) -> NodeId {
    NodeId::new(id).unwrap()
}

fn owner() -> NodeId {
    node("edge-khi#branch_07")
}

fn intruder() -> NodeId {
    node("edge-khi#branch_09")
}

fn text(name: &str) -> FieldSpec {
    FieldSpec::new(name, FieldType::Text)
}

fn definitions() -> Vec<Definition> {
    // `customer.name` is its warehouse.
    let mut customer = Definition::new("customer");
    let mut name = text("name");
    name.required = true;
    customer.fields = vec![name, text("note")];
    customer.title_field = Some("name".to_owned());

    // `sales_invoice.number` is its warehouse.
    let mut invoice = Definition::new("sales_invoice");
    invoice.is_posted = true;
    let mut number = text("number");
    number.required = true;
    let mut remarks = FieldSpec::new("remarks", FieldType::LongText);
    remarks.allow_on_posted = true;
    invoice.fields = vec![number, remarks];

    // `tagged.warehouse` is optional; `memo` has no partition scope at all.
    let mut tagged = Definition::new("tagged");
    tagged.fields = vec![text("warehouse"), text("title")];
    let mut memo = Definition::new("memo");
    memo.fields = vec![text("title")];
    vec![customer, invoice, tagged, memo]
}

fn fields() -> ScopeFields {
    ScopeFields::new()
        .with("customer", "warehouse", "name")
        .with("sales_invoice", "warehouse", "number")
        .with("tagged", "warehouse", "warehouse")
}

fn registry() -> Arc<MemoryPartitionRegistry> {
    let registry = MemoryPartitionRegistry::new();
    for (warehouse, to) in [("W7", None), ("W8", Some(NOW - 10))] {
        registry
            .assign(AssignmentRequest {
                scope: ScopeKey::new("warehouse", warehouse).unwrap(),
                node: owner(),
                effective_from: at(1_000),
                effective_to: to.map(at),
                actor: "admin".to_owned(),
                reason: "branch 7".to_owned(),
                at: at(1_000),
            })
            .unwrap();
    }
    Arc::new(registry)
}

fn inv() -> Invocation {
    Invocation {
        site: SiteId::new("partition.test").unwrap(),
        actor: Actor::System {
            component: n("partition_tests"),
        },
        command_id: CommandId::new("cmd-1").ok(),
        language: None,
        now: at(NOW),
    }
}

/// A controller of `customer` with a method that writes twice.
struct Probe;

impl Controller for Probe {
    fn doctype(&self) -> &str {
        "customer"
    }

    fn steps(&self) -> Vec<StepDecl> {
        Vec::new()
    }

    fn methods(&self) -> Vec<MethodDecl> {
        vec![MethodDecl::new(n("annotate"), true)]
    }

    fn run_step<'a>(
        &'a self,
        _step: &'a StepName,
        _ctx: &'a mut dyn HookContext,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async { Ok(()) })
    }

    fn call_method<'a>(
        &'a self,
        _method: &'a str,
        _args: Json,
        ctx: &'a mut dyn MethodContext,
    ) -> BoxFuture<'a, AppResult<Json>> {
        Box::pin(async move {
            let id = ctx.doc().id();
            let version = ctx.doc().version().unwrap();
            let mut patch = DocumentPatch::default();
            patch.fields.insert(n("note"), "annotated".into());
            ctx.update("customer", id, patch, version).await?;
            Ok(json!({}))
        })
    }
}

struct Fx {
    engine: Engine<FakeBackend>,
    registry: Arc<MemoryPartitionRegistry>,
}

fn participant(node: NodeId, registry: &Arc<MemoryPartitionRegistry>) -> Arc<OwnerParticipant> {
    Arc::new(OwnerParticipant::new(node, registry.clone(), fields()))
}

async fn fx() -> Fx {
    let mut builder = RegistryBuilder::new();
    let probe = Arc::new(Probe);
    builder.add_controller("probe", probe.clone());
    let engine = Engine::new(
        FakeBackend::new(),
        Registry::new(definitions()).unwrap().effective(),
        Arc::new(builder.build().unwrap()),
        Arc::new(ManualClock::new(at(NOW))),
        Arc::new(V7Ids),
    )
    .unwrap()
    .with_controllers(vec![probe]);
    engine.prepare().await.unwrap();
    let registry = registry();
    let fx = Fx { engine, registry };
    fx.as_node(owner());
    fx
}

impl Fx {
    /// From now on writes are made by `who`.
    fn as_node(&self, who: NodeId) {
        let participants: Vec<Arc<dyn TxParticipant<_>>> =
            vec![participant(who, &self.registry) as Arc<dyn TxParticipant<_>>];
        self.engine.set_participants(participants);
    }

    async fn outbox_len(&self) -> usize {
        self.engine
            .backend()
            .read_outbox(FeedPosition::START, 10_000)
            .await
            .unwrap()
            .iter()
            // The journal's `doc_state` images (WP-JRN-02.T2) are not lifecycle events.
            .filter(|r| r.event.topic.as_str() != "doc_state")
            .count()
    }

    async fn create(&self, doctype: &str, field: &str, value: &str) -> AppResult<Document> {
        let mut doc = Document::new(n(doctype), RecordId::new_v7());
        doc.set(n(field), value);
        Ok(self.engine.create(&inv(), doc).await?.value)
    }

    async fn customer(&self, warehouse: &str) -> Document {
        self.create("customer", "name", warehouse).await.unwrap()
    }

    async fn posted(&self) -> Document {
        let draft = self.create("sales_invoice", "number", "W7").await.unwrap();
        self.engine
            .post(
                &inv(),
                "sales_invoice",
                draft.id(),
                draft.version().unwrap(),
            )
            .await
            .unwrap()
            .value
    }

    async fn stored(&self, doctype: &str, id: RecordId) -> Option<Document> {
        self.engine.peek(doctype, id).await.unwrap()
    }
}

fn assert_refused(error: &spot_app_api::AppError) {
    assert_eq!(error.code, "partition.not_owner", "{error:?}");
    assert_eq!(error.class, ErrorClass::PermissionDenied);
}

#[tokio::test]
async fn the_owner_writes_and_a_document_of_another_node_is_never_created() {
    let f = fx().await;
    f.customer("W7").await;
    let events = f.outbox_len().await;

    f.as_node(intruder());
    let mut doc = Document::new(n("customer"), RecordId::new_v7());
    doc.set(n("name"), "W7");
    let id = doc.id();
    let error = f.engine.create(&inv(), doc).await.unwrap_err();
    assert_refused(&error);
    assert!(error.message.contains("branch_07"), "names the owner");
    assert!(f.stored("customer", id).await.is_none(), "no document");
    assert_eq!(f.outbox_len().await, events, "no outbox event");
}

#[tokio::test]
async fn update_is_gated_and_leaves_the_stored_document_alone() {
    let f = fx().await;
    let created = f.customer("W7").await;
    let events = f.outbox_len().await;
    f.as_node(intruder());
    let mut patch = DocumentPatch::default();
    patch.fields.insert(n("note"), "hijack".into());
    let error = f
        .engine
        .update(&inv(), "customer", created.id(), patch, Version::FIRST)
        .await
        .unwrap_err();
    assert_refused(&error);
    let stored = f.stored("customer", created.id()).await.unwrap();
    assert_eq!(stored.version(), Some(Version::FIRST));
    assert_eq!(f.outbox_len().await, events);
}

#[tokio::test]
async fn an_update_that_moves_a_document_into_a_partition_the_node_does_not_own_is_refused() {
    let f = fx().await;
    let created = f.customer("W7").await;
    // W7 is owned; W9 has no assignment. Changing the scope field moves the record.
    let mut patch = DocumentPatch::default();
    patch.fields.insert(n("name"), "W9".into());
    let error = f
        .engine
        .update(&inv(), "customer", created.id(), patch, Version::FIRST)
        .await
        .unwrap_err();
    assert_refused(&error);
    let stored = f.stored("customer", created.id()).await.unwrap();
    assert_eq!(stored.get("name"), &"W7".into());
}

#[tokio::test]
async fn post_update_posted_and_cancel_are_gated() {
    let f = fx().await;
    let draft = f.create("sales_invoice", "number", "W7").await.unwrap();
    f.as_node(intruder());
    let error = f
        .engine
        .post(
            &inv(),
            "sales_invoice",
            draft.id(),
            draft.version().unwrap(),
        )
        .await
        .unwrap_err();
    assert_refused(&error);
    let still = f.stored("sales_invoice", draft.id()).await.unwrap();
    assert_eq!(still.state(), Some(DocState::Draft));

    f.as_node(owner());
    let posted = f
        .engine
        .post(
            &inv(),
            "sales_invoice",
            draft.id(),
            draft.version().unwrap(),
        )
        .await
        .unwrap()
        .value;
    let events = f.outbox_len().await;

    f.as_node(intruder());
    let mut patch = DocumentPatch::default();
    patch.fields.insert(n("remarks"), "late note".into());
    let error = f
        .engine
        .update(
            &inv(),
            "sales_invoice",
            posted.id(),
            patch,
            posted.version().unwrap(),
        )
        .await
        .unwrap_err();
    assert_refused(&error);
    let error = f
        .engine
        .cancel(
            &inv(),
            "sales_invoice",
            posted.id(),
            posted.version().unwrap(),
        )
        .await
        .unwrap_err();
    assert_refused(&error);
    let still = f.stored("sales_invoice", posted.id()).await.unwrap();
    assert_eq!(still.state(), Some(DocState::Posted));
    assert_eq!(still.version(), posted.version());
    assert_eq!(f.outbox_len().await, events);
}

#[tokio::test]
async fn delete_and_rename_are_gated() {
    let f = fx().await;
    let created = f.customer("W7").await;
    let events = f.outbox_len().await;
    f.as_node(intruder());
    let error = f
        .engine
        .delete(&inv(), "customer", created.id(), Version::FIRST)
        .await
        .unwrap_err();
    assert_refused(&error);
    let error = f
        .engine
        .rename(&inv(), "customer", created.id(), "Other", Version::FIRST)
        .await
        .unwrap_err();
    assert_refused(&error);
    assert!(f.stored("customer", created.id()).await.is_some());
    assert_eq!(f.outbox_len().await, events);
}

#[tokio::test]
async fn correct_is_gated_on_the_cancel_half_and_nothing_remains() {
    let f = fx().await;
    let posted = f.posted().await;
    let events = f.outbox_len().await;
    f.as_node(intruder());
    let error = f
        .engine
        .correct(
            &inv(),
            "sales_invoice",
            posted.id(),
            posted.version().unwrap(),
        )
        .await
        .unwrap_err();
    assert_refused(&error);
    let still = f.stored("sales_invoice", posted.id()).await.unwrap();
    assert_eq!(still.state(), Some(DocState::Posted));
    assert_eq!(f.outbox_len().await, events);
}

#[tokio::test]
async fn a_refused_correction_leaves_the_original_posted_and_correctable_by_the_owner() {
    let f = fx().await;
    let posted = f.posted().await;
    let events = f.outbox_len().await;
    f.as_node(intruder());
    let error = f
        .engine
        .correct(
            &inv(),
            "sales_invoice",
            posted.id(),
            posted.version().unwrap(),
        )
        .await
        .unwrap_err();
    assert_refused(&error);
    assert_eq!(f.outbox_len().await, events);
    let original = f.stored("sales_invoice", posted.id()).await.unwrap();
    assert_eq!(original.version(), posted.version());
    f.as_node(owner());
    f.engine
        .correct(
            &inv(),
            "sales_invoice",
            posted.id(),
            posted.version().unwrap(),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn a_write_a_method_makes_is_gated() {
    let f = fx().await;
    let created = f.customer("W7").await;
    let events = f.outbox_len().await;
    f.as_node(intruder());
    let error = f
        .engine
        .call_method(
            &inv(),
            "customer",
            created.id(),
            "annotate",
            json!({}),
            None,
        )
        .await
        .unwrap_err();
    assert_refused(&error);
    let stored = f.stored("customer", created.id()).await.unwrap();
    assert_eq!(stored.get("note"), &spot_app_api::prelude::Value::Null);
    assert_eq!(f.outbox_len().await, events);

    f.as_node(owner());
    f.engine
        .call_method(
            &inv(),
            "customer",
            created.id(),
            "annotate",
            json!({}),
            None,
        )
        .await
        .unwrap();
    let stored = f.stored("customer", created.id()).await.unwrap();
    assert_eq!(stored.get("note"), &"annotated".into());
}

#[tokio::test]
async fn fail_closed_unknown_partition_expired_owner_and_missing_scope() {
    let f = fx().await;
    // No assignment for W9 at all.
    let error = f.create("customer", "name", "W9").await.unwrap_err();
    assert_refused(&error);
    assert!(!error.message.contains("owned by"), "no owner to name");
    // W8 was owned until just before the invocation's instant.
    let error = f.create("customer", "name", "W8").await.unwrap_err();
    assert_refused(&error);
    // A scoped DocType whose scope field is unset cannot be placed: refused, not allowed.
    let error = f.create("tagged", "title", "x").await.unwrap_err();
    assert_eq!(error.code, "partition.invalid_scope", "{error:?}");
    assert_eq!(f.outbox_len().await, 0, "nothing was written");
    // With the field set to an owned warehouse it is allowed.
    let mut doc = Document::new(n("tagged"), RecordId::new_v7());
    doc.set(n("warehouse"), "W7");
    f.engine.create(&inv(), doc).await.unwrap();
}

#[tokio::test]
async fn a_node_with_no_assignments_at_all_refuses_everything_scoped() {
    let f = fx().await;
    f.as_node(node("edge-lhe#branch_01"));
    let error = f.create("customer", "name", "W7").await.unwrap_err();
    assert_refused(&error);
}

#[tokio::test]
async fn a_doctype_without_a_partition_scope_is_allowed_untouched() {
    let f = fx().await;
    // Even a node that owns nothing may write a local DocType: `memo` has no scope.
    f.as_node(node("edge-lhe#branch_01"));
    let memo = f.create("memo", "title", "hello").await.unwrap();
    assert!(f.stored("memo", memo.id()).await.is_some());
    assert!(f.outbox_len().await > 0, "the lifecycle event was written");
}

#[tokio::test]
async fn the_participant_and_the_seam_decide_alike() {
    let f = fx().await;
    let seam = OwnerCheckSeam::new(f.registry.clone());
    for (who, warehouse) in [
        (owner(), "W7"),
        (intruder(), "W7"),
        (owner(), "W9"),
        (owner(), "W8"),
    ] {
        f.as_node(who.clone());
        let by_participant = f.create("customer", "name", warehouse).await;
        let by_seam = seam
            .call(json!({
                "node": who.as_str(),
                "doctype": "customer",
                "scope_values": [{ "key_type": "warehouse", "value": warehouse }],
                "at_unix_seconds": NOW,
                "at_nanos": 0
            }))
            .await;
        assert_eq!(
            by_participant.is_ok(),
            by_seam.is_ok(),
            "{who:?} {warehouse}"
        );
        if let (Err(a), Err(b)) = (by_participant, by_seam) {
            assert_eq!(a.code, b.code);
            assert_eq!(a.message, b.message);
        }
    }
}

#[tokio::test]
async fn two_branches_with_leased_ranges_name_documents_without_collision() {
    use spot_partition::RangeAllocator;
    use spot_partition::{BranchSeries, LowWater, MemoryRangeAllocator, MemorySeriesStore};
    use spot_storage_contract::SeriesKey;

    let f = fx().await;
    let key = SeriesKey::new("MEMO-").unwrap();
    let mut hub = MemoryRangeAllocator::new();
    let mut series = Vec::new();
    for who in [owner(), intruder()] {
        let lease = hub.lease(&key, &who, 4, at(1)).unwrap();
        let alloc =
            BranchSeries::install(lease, LowWater::half(), Arc::new(MemorySeriesStore::new()))
                .unwrap();
        series.push(alloc);
    }
    let mut titles = Vec::new();
    let mut low = [false, false];
    for round in 0..4 {
        for (i, alloc) in series.iter_mut().enumerate() {
            let name = alloc.allocate_name("MEMO-").unwrap();
            // `memo` has no partition scope: any node writes it, so both branches create.
            f.as_node(if i == 0 { owner() } else { intruder() });
            f.create("memo", "title", name.as_str()).await.unwrap();
            titles.push(name.as_str().to_owned());
            low[i] |= alloc.running_low();
            assert_eq!(alloc.remaining(), 3 - round);
        }
    }
    let mut unique = titles.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), 8, "no collision across branches: {titles:?}");
    assert_eq!(low, [true, true], "each branch saw the running-low signal");
    assert_eq!(f.outbox_len().await, 8, "one lifecycle event per document");
}

// ------------------------------------------------------------ coverage (WP-EDG-03.T7)

fn assert_uncovered(error: &spot_app_api::AppError) {
    assert_eq!(error.code, "partition.scope_uncovered", "{error:?}");
    assert_eq!(error.class, ErrorClass::PermissionDenied);
}

#[tokio::test]
async fn a_listed_but_uncovered_doctype_refuses_writes_and_an_unlisted_one_is_allowed() {
    let f = fx().await;
    let participants: Vec<Arc<dyn TxParticipant<_>>> = vec![Arc::new(
        OwnerParticipant::new(owner(), f.registry.clone(), fields()).with_partitioned(&["memo"]),
    ) as Arc<dyn TxParticipant<_>>];
    f.engine.set_participants(participants);
    let events = f.outbox_len().await;

    // `memo` is partitioned in the deployment but has no scope field: refused, nothing left.
    let mut doc = Document::new(n("memo"), RecordId::new_v7());
    doc.set(n("title"), "hello");
    let id = doc.id();
    let error = f.engine.create(&inv(), doc).await.unwrap_err();
    assert_uncovered(&error);
    assert!(error.message.contains("memo"), "names the DocType");
    assert!(f.stored("memo", id).await.is_none(), "no document");
    assert_eq!(f.outbox_len().await, events, "no outbox event");

    // The same engine still writes a DocType nobody listed.
    let mut doc = Document::new(n("tagged"), RecordId::new_v7());
    doc.set(n("warehouse"), "W7");
    f.engine.create(&inv(), doc).await.unwrap();
}

#[tokio::test]
async fn a_participant_built_without_the_list_has_no_coverage_guarantee() {
    let f = fx().await;
    // `fx` builds with `new`: `memo` is not known to be partitioned, so it is allowed.
    let mut doc = Document::new(n("memo"), RecordId::new_v7());
    doc.set(n("title"), "hello");
    f.engine.create(&inv(), doc).await.unwrap();
}

#[tokio::test]
async fn a_checked_participant_is_built_only_when_covered_and_still_owner_checks() {
    let f = fx().await;
    let uncovered =
        OwnerParticipant::checked(owner(), f.registry.clone(), fields(), &["customer", "memo"])
            .err()
            .expect("memo has no scope field");
    assert_eq!(uncovered.code(), "partition.scope_uncovered");

    let checked = OwnerParticipant::checked(
        intruder(),
        f.registry.clone(),
        fields(),
        &["customer", "sales_invoice"],
    )
    .unwrap();
    f.engine
        .set_participants(vec![Arc::new(checked) as Arc<dyn TxParticipant<_>>]);
    let mut doc = Document::new(n("customer"), RecordId::new_v7());
    doc.set(n("name"), "W7");
    let error = f.engine.create(&inv(), doc).await.unwrap_err();
    assert_refused(&error);
}
