//! The audit participant on a real document engine over the fake backend (WP-SEC-06.T2): one
//! event per stored version on every write path, written in the document's own transaction,
//! failing closed, masked, and chained and verified end to end.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::{Arc, Mutex};

use serde_json::{Value as Json, json};
use spot_app_api::context::{Actor, DocumentService, HookContext, Invocation, MethodContext};
use spot_app_api::controller::{MethodDecl, StepDecl, StepName};
use spot_app_api::document::ChildRow;
use spot_app_api::prelude::{
    CommandId, Decimal, Name, OffsetDateTime, Query, RecordId, SiteId, Value, Version,
};
use spot_app_api::{
    AppResult, BoxFuture, Controller, DocState, Document, DocumentPatch, Operation,
};
use spot_audit_chain::{
    AppendOutcome, AuditStore, ChainEntry, Checkpoint, CheckpointSigner, InMemoryStore, KeyRing,
    LocalSigner, StoreFuture,
};
use spot_core::ManualClock;
use spot_doc_audit::{
    AUDIT_TOPIC, AuditDenialSink, AuditParticipant, AuditWriter, DenialStats, DocAudit,
    DocAuditError, DocAuditEvent, DocOperation, FailingWriter, GuardMasker, InMemoryAppenderStore,
    LaneConfig, Placed, Role, checked_participants, decode_outbox, enqueue_audit,
    is_recordable_method, lane_of, operation_for, record_of, with_audit_participant,
};
use spot_document::engine::{
    AllowAllAuthorizer, Authorizer, Engine, NodeId, TxParticipant, V7Ids, WriteCause, WriteRecord,
};
use spot_events_contract::system;
use spot_field_guard::source::{DocProtection, StaticProtection};
use spot_meta::{Definition, FieldSpec, FieldType, MaskSpec, ProtectionGroupSpec, Registry};
use spot_permissions::{Action, DocFacts, FieldFacts, GroupName, MaskFormat, Principal};
use spot_seams::RegistryBuilder;
use spot_storage_contract::feed::Outbox;
use spot_storage_contract::{
    ChangeFeed, ErrorKind, FeedPosition, Isolation, OutboxEvent, OutboxRecord, StorageBackend,
    StorageError, StorageResult, Transaction, TxOptions, contract_tests,
};
use spot_storage_fake::FakeBackend;

const SITE: &str = "engine.spotledger.app";
const SECRET_VALUE: &str = "TAX-9981-7765-CONFIDENTIAL";
const SECRET_UPDATED: &str = "TAX-1234-0000-ROTATED";

fn n(s: &str) -> Name {
    Name::new(s).unwrap()
}

fn definitions() -> Vec<Definition> {
    let mut customer = Definition::new("customer");
    let mut name = FieldSpec::new("name", FieldType::Text);
    name.required = true;
    let mut tax_id = FieldSpec::new("tax_id", FieldType::Text);
    tax_id.protection_group = Some("confidential".to_owned());
    tax_id.masking = Some(MaskSpec::placeholder());
    customer.fields = vec![name, tax_id];
    customer.title_field = Some("name".to_owned());
    customer.protection_groups = vec![ProtectionGroupSpec::new("confidential", "Confidential")];

    let mut invoice = Definition::new("sales_invoice");
    invoice.is_posted = true;
    let mut number = FieldSpec::new("number", FieldType::Text);
    number.required = true;
    let mut link = FieldSpec::new("customer", FieldType::Link);
    link.required = true;
    link.options = Some("customer".into());
    let mut remarks = FieldSpec::new("remarks", FieldType::LongText);
    remarks.allow_on_posted = true;
    let mut items = FieldSpec::new("items", FieldType::Table);
    items.options = Some("sales_invoice_item".into());
    invoice.fields = vec![number, link, remarks, items];

    let mut item = Definition::new("sales_invoice_item");
    item.is_child = true;
    let mut name = FieldSpec::new("item", FieldType::Text);
    name.required = true;
    let mut qty = FieldSpec::new("qty", FieldType::Decimal);
    qty.scale = Some(3);
    qty.required = true;
    item.fields = vec![name, qty];
    let mut chain = Definition::new("audit_chain");
    let mut label = FieldSpec::new("label", FieldType::Text);
    label.required = true;
    chain.fields = vec![label];
    vec![customer, invoice, item, chain]
}

fn protection(with_customer: bool) -> Arc<StaticProtection> {
    let mut docs = vec![
        DocProtection {
            name: "sales_invoice".to_owned(),
            tables: BTreeMap::from([("items".to_owned(), "sales_invoice_item".to_owned())]),
            ..DocProtection::default()
        },
        DocProtection {
            name: "sales_invoice_item".to_owned(),
            ..DocProtection::default()
        },
    ];
    if with_customer {
        docs.push(DocProtection {
            name: "customer".to_owned(),
            fields: vec![FieldFacts {
                path: "tax_id".to_owned(),
                groups: vec![GroupName("confidential".to_owned())],
                tags: vec![],
                mask: Some(MaskFormat::Placeholder),
            }],
            ..DocProtection::default()
        });
    }
    Arc::new(StaticProtection::new(docs))
}

fn masker(with_customer: bool) -> Arc<GuardMasker> {
    let guard = common::fixture().guard;
    Arc::new(GuardMasker::new(guard, protection(with_customer)))
}

fn now() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_790_000_000).unwrap()
}

fn user() -> RecordId {
    RecordId::new_v7()
}

fn inv(actor: RecordId) -> Invocation {
    Invocation {
        site: SiteId::new(SITE).unwrap(),
        actor: Actor::User {
            id: actor,
            via: None,
        },
        command_id: CommandId::new("cmd-audit-1").ok(),
        language: None,
        now: now(),
    }
}

/// A customer controller with a method that writes twice.
struct Probe;

impl Controller for Probe {
    fn doctype(&self) -> &str {
        "customer"
    }

    fn steps(&self) -> Vec<StepDecl> {
        Vec::new()
    }

    fn run_step<'a>(
        &'a self,
        _step: &'a StepName,
        _ctx: &'a mut dyn HookContext,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async { Ok(()) })
    }

    fn methods(&self) -> Vec<MethodDecl> {
        vec![MethodDecl::new(n("rename_twice"), true)]
    }

    fn call_method<'a>(
        &'a self,
        _method: &'a str,
        _args: Json,
        ctx: &'a mut dyn MethodContext,
    ) -> BoxFuture<'a, AppResult<Json>> {
        Box::pin(async move {
            let id = ctx.doc().id();
            let mut version = ctx.doc().version().unwrap();
            for name in ["First", "Second"] {
                let mut patch = DocumentPatch::default();
                patch.fields.insert(n("name"), name.into());
                version = ctx
                    .update("customer", id, patch, version)
                    .await?
                    .version()
                    .unwrap();
            }
            Ok(json!({"version": version.0}))
        })
    }
}

/// An outbox that always refuses.
struct FailingOutbox;

impl Outbox for FailingOutbox {
    #[allow(clippy::manual_async_fn)]
    fn enqueue(&mut self, _event: &OutboxEvent) -> impl Future<Output = StorageResult<()>> + Send {
        async { Err(StorageError::new(ErrorKind::Internal, "the outbox is down")) }
    }
}

/// The real participant, except that it writes to a failing outbox for one operation.
struct FlakyAudit {
    inner: AuditParticipant,
    fail_on: Mutex<Option<Operation>>,
}

impl<Tx: Transaction> TxParticipant<Tx> for FlakyAudit {
    fn after_write<'a>(
        &'a self,
        tx: &'a mut Tx,
        write: &'a WriteRecord<'a>,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async move {
            let failing = *self.fail_on.lock().unwrap() == Some(write.operation);
            if failing {
                self.inner.write_to(&mut FailingOutbox, write).await
            } else {
                self.inner.write_to(tx, write).await
            }
        })
    }
}

/// A participant that refuses a write after the audit participant ran.
struct RefuseAfter(Operation);

impl<Tx: Transaction> TxParticipant<Tx> for RefuseAfter {
    fn after_write<'a>(
        &'a self,
        _tx: &'a mut Tx,
        write: &'a WriteRecord<'a>,
    ) -> BoxFuture<'a, AppResult<()>> {
        let refuse = write.operation == self.0;
        Box::pin(async move {
            if refuse {
                Err(spot_app_api::AppError::new(
                    spot_app_api::ErrorClass::PermissionDenied,
                    "test.later_participant_refused",
                    "refused",
                ))
            } else {
                Ok(())
            }
        })
    }
}

struct Fx {
    engine: Engine<FakeBackend>,
}

type Dyn = Arc<dyn TxParticipant<<FakeBackend as spot_storage_contract::StorageBackend>::Tx>>;

async fn engine_with(participants: Vec<Dyn>) -> Fx {
    engine_on(participants, FakeBackend::new(), None, None).await
}

async fn engine_on(
    participants: Vec<Dyn>,
    backend: FakeBackend,
    node: Option<&str>,
    authorizer: Option<Arc<dyn Authorizer>>,
) -> Fx {
    let probe = Arc::new(Probe);
    let mut builder = RegistryBuilder::new();
    builder.add_controller("probe", probe.clone());
    let mut engine = Engine::new(
        backend,
        Registry::new(definitions()).unwrap().effective(),
        Arc::new(builder.build().unwrap()),
        Arc::new(ManualClock::new(now())),
        Arc::new(V7Ids),
    )
    .unwrap()
    .with_controllers(vec![probe]);
    if let Some(node) = node {
        engine = engine.with_node(NodeId::new(node));
    }
    if let Some(authorizer) = authorizer {
        engine = engine.with_authorizer(authorizer);
    }
    engine.prepare().await.unwrap();
    engine.set_participants(participants);
    Fx { engine }
}

async fn fx() -> Fx {
    engine_with(with_audit_participant(
        AuditParticipant::new(masker(true)),
        Vec::new(),
    ))
    .await
}

impl Fx {
    async fn outbox(&self) -> Vec<OutboxRecord> {
        self.engine
            .backend()
            .read_outbox(FeedPosition::START, 10_000)
            .await
            .unwrap()
            .into_iter()
            // The journal's `doc_state` images (WP-JRN-02.T2) hold the whole record, values
            // included, by design; these tests inspect the audit and lifecycle events.
            .filter(|r| r.event.topic.as_str() != "doc_state")
            .collect()
    }

    /// The audit records in the outbox, in commit order, with their decoded events.
    async fn audits(&self) -> Vec<(OutboxRecord, DocAuditEvent)> {
        self.outbox()
            .await
            .into_iter()
            .filter(|r| r.event.topic.as_str() == AUDIT_TOPIC)
            .map(|r| {
                let event = decode_outbox(&r).unwrap();
                (r, event)
            })
            .collect()
    }

    async fn customer(&self, actor: RecordId, name: &str) -> Document {
        let mut doc = Document::new(n("customer"), RecordId::new_v7());
        doc.set(n("name"), name);
        doc.set(n("tax_id"), SECRET_VALUE);
        self.engine.create(&inv(actor), doc).await.unwrap().value
    }

    async fn invoice(&self, actor: RecordId) -> Document {
        let customer = self.customer(actor, "Acme").await;
        let mut doc = Document::new(n("sales_invoice"), RecordId::new_v7());
        doc.set(n("number"), "SI-0001");
        doc.set(n("customer"), customer.id());
        for (item, milli) in [("bolt", 2_000), ("nut", 1_500)] {
            let mut row = ChildRow {
                id: RecordId::new_v7(),
                fields: Default::default(),
            };
            row.fields.insert(n("item"), item.into());
            row.fields
                .insert(n("qty"), Value::Decimal(Decimal::new(milli, 3)));
            doc.rows_mut(n("items")).push(row);
        }
        self.engine.create(&inv(actor), doc).await.unwrap().value
    }

    async fn posted(&self, actor: RecordId) -> Document {
        let draft = self.invoice(actor).await;
        self.engine
            .post(
                &inv(actor),
                "sales_invoice",
                draft.id(),
                draft.version().unwrap(),
            )
            .await
            .unwrap()
            .value
    }
}

fn ops(audits: &[(OutboxRecord, DocAuditEvent)]) -> Vec<(String, String, u64)> {
    audits
        .iter()
        .map(|(_, e)| {
            (
                e.doctype.clone(),
                e.operation.as_str().to_owned(),
                e.version,
            )
        })
        .collect()
}

fn tail<T: Clone>(all: &[T], from: usize) -> Vec<T> {
    all[from..].to_vec()
}

#[tokio::test]
async fn create_writes_one_audit_event_in_the_same_transaction_as_the_document() {
    let f = fx().await;
    let actor = user();
    let created = f.customer(actor, "Acme").await;
    let all = f.outbox().await;
    let audits = f.audits().await;
    assert_eq!(audits.len(), 1, "one event per stored version");
    let (record, event) = &audits[0];
    assert_eq!(event.operation, DocOperation::Create);
    assert_eq!(event.version, 1);
    assert_eq!(event.id, created.id().to_string());
    assert_eq!(event.site, SITE);
    assert_eq!(event.actor.actor, format!("user:{actor}"));
    assert_eq!(event.command_id.as_deref(), Some("cmd-audit-1"));
    assert_eq!(event.at, 1_790_000_000_000_000);
    // The subject and its revision are the document's.
    assert_eq!(record.event.subject.id, created.id());
    assert_eq!(record.event.subject_version, Some(Version::FIRST));
    // Same business transaction as the document's own lifecycle event.
    let lifecycle: Vec<_> = all
        .iter()
        .filter(|r| r.event.topic.as_str() != AUDIT_TOPIC)
        .collect();
    assert!(!lifecycle.is_empty());
    assert!(
        lifecycle
            .iter()
            .all(|r| r.transaction == record.transaction),
        "the audit event shares the document's transaction"
    );
    // The lane is the document's lane.
    let lane = lane_of(&event.doctype, &event.id, 4);
    assert!(lane < 4);
}

#[tokio::test]
async fn the_payload_is_the_registered_sys_doc_audit_payload() {
    let f = fx().await;
    f.invoice(user()).await;
    let audits = f.audits().await;
    assert_eq!(audits.len(), 2);
    for (record, _) in &audits {
        assert_eq!(record.event.topic.as_str(), "sys_doc_audit");
        let payload: Json = serde_json::from_str(&record.event.payload).unwrap();
        let data = payload.get("data").unwrap();
        assert_eq!(system::validate("sys_doc_audit", data), Ok(()));
    }
}

#[tokio::test]
async fn every_write_path_yields_exactly_one_event_per_stored_version_in_order() {
    let f = fx().await;
    let actor = user();
    // create (customer), create (invoice), post.
    let posted = f.posted(actor).await;
    let mut seen = f.audits().await;
    assert_eq!(
        ops(&seen),
        [
            ("customer".into(), "create".into(), 1),
            ("sales_invoice".into(), "create".into(), 1),
            ("sales_invoice".into(), "post".into(), 2),
        ]
    );
    let mut count = seen.len();

    // update-posted.
    let mut patch = DocumentPatch::default();
    patch
        .fields
        .insert(n("remarks"), Value::Text("late note".into()));
    let edited = f
        .engine
        .update(
            &inv(actor),
            "sales_invoice",
            posted.id(),
            patch,
            posted.version().unwrap(),
        )
        .await
        .unwrap()
        .value;
    seen = f.audits().await;
    assert_eq!(
        ops(&tail(&seen, count)),
        [("sales_invoice".into(), "update".into(), 3)]
    );
    count = seen.len();

    // cancel.
    f.engine
        .cancel(
            &inv(actor),
            "sales_invoice",
            posted.id(),
            edited.version().unwrap(),
        )
        .await
        .unwrap();
    seen = f.audits().await;
    assert_eq!(
        ops(&tail(&seen, count)),
        [("sales_invoice".into(), "cancel".into(), 4)]
    );
    count = seen.len();

    // update (draft customer).
    let customer = f.customer(actor, "Beta").await;
    count += 1;
    let mut patch = DocumentPatch::default();
    patch.fields.insert(n("name"), "Beta Ltd".into());
    f.engine
        .update(
            &inv(actor),
            "customer",
            customer.id(),
            patch,
            customer.version().unwrap(),
        )
        .await
        .unwrap();
    seen = f.audits().await;
    assert_eq!(
        ops(&tail(&seen, count)),
        [("customer".into(), "update".into(), 2)]
    );
    count = seen.len();

    // rename.
    f.engine
        .rename(
            &inv(actor),
            "customer",
            customer.id(),
            "Beta Group",
            Version(2),
        )
        .await
        .unwrap();
    seen = f.audits().await;
    let renamed = tail(&seen, count);
    assert_eq!(renamed.len(), 1, "{:?}", ops(&renamed));
    assert_eq!(renamed[0].1.operation, DocOperation::Rename);
    let renamed_version = renamed[0].1.version;
    assert!(renamed_version > 2);
    count = seen.len();

    // delete.
    f.engine
        .delete(
            &inv(actor),
            "customer",
            customer.id(),
            Version(renamed_version),
        )
        .await
        .unwrap();
    seen = f.audits().await;
    let deleted = tail(&seen, count);
    assert_eq!(deleted.len(), 1);
    assert_eq!(deleted[0].1.operation, DocOperation::Delete);
    assert_eq!(
        deleted[0].1.version, renamed_version,
        "the last stored version"
    );
    assert!(
        !deleted[0].1.changed_fields.is_empty(),
        "a delete records every field as removed"
    );
}

#[tokio::test]
async fn correct_writes_the_cancel_then_the_amended_create_in_order() {
    let f = fx().await;
    let actor = user();
    let posted = f.posted(actor).await;
    let before = f.audits().await.len();
    let amended = f
        .engine
        .correct(
            &inv(actor),
            "sales_invoice",
            posted.id(),
            posted.version().unwrap(),
        )
        .await
        .unwrap()
        .value;
    assert_eq!(amended.state(), Some(DocState::Draft));
    let seen = f.audits().await;
    let new = tail(&seen, before);
    assert_eq!(new.len(), 2, "{:?}", ops(&new));
    assert_eq!(new[0].1.operation, DocOperation::Correct);
    assert_eq!(new[0].1.correction_half.as_deref(), Some("cancel"));
    assert_eq!(new[0].1.id, posted.id().to_string());
    assert_eq!(new[1].1.operation, DocOperation::Correct);
    assert_eq!(new[1].1.correction_half.as_deref(), Some("amend"));
    assert_eq!(new[1].1.id, amended.id().to_string());
    assert!(new[0].0.position < new[1].0.position, "in order");
    assert_eq!(
        new[0].0.transaction, new[1].0.transaction,
        "one transaction for both halves"
    );
}

#[tokio::test]
async fn a_method_that_writes_twice_yields_two_events_one_per_version() {
    let f = fx().await;
    let actor = user();
    let created = f.customer(actor, "Acme").await;
    let before = f.audits().await.len();
    f.engine
        .call_method(
            &inv(actor),
            "customer",
            created.id(),
            "rename_twice",
            json!({}),
            None,
        )
        .await
        .unwrap();
    let seen = f.audits().await;
    assert_eq!(
        ops(&tail(&seen, before)),
        [
            ("customer".into(), "rename_twice".into(), 2),
            ("customer".into(), "rename_twice".into(), 3),
        ]
    );
    for (_, event) in tail(&seen, before) {
        assert_eq!(event.operation, DocOperation::Method("rename_twice".into()));
        assert_eq!(event.method.as_deref(), Some("rename_twice"));
        assert_eq!(event.operation.kind_segment(), "method");
    }
}

#[tokio::test]
async fn no_secret_value_reaches_an_audit_row() {
    let f = fx().await;
    let actor = user();
    let created = f.customer(actor, "Acme").await;
    let mut patch = DocumentPatch::default();
    patch.fields.insert(n("tax_id"), SECRET_UPDATED.into());
    f.engine
        .update(
            &inv(actor),
            "customer",
            created.id(),
            patch,
            created.version().unwrap(),
        )
        .await
        .unwrap();
    f.engine
        .delete(&inv(actor), "customer", created.id(), Version(2))
        .await
        .unwrap();
    let audits = f.audits().await;
    assert_eq!(audits.len(), 3);
    for (record, event) in &audits {
        for secret in [SECRET_VALUE, SECRET_UPDATED] {
            assert!(
                !record.event.payload.contains(secret),
                "a secret value is in the stored audit row"
            );
            assert!(!record.event.payload.contains(&secret.to_lowercase()));
        }
        let _ = event;
    }
    // The change to the protected field is recorded, in its masked form.
    let update = &audits[1].1;
    assert!(update.changed_fields.contains(&"tax_id".to_owned()));
    let change = update.diff.iter().find(|d| d.path == "tax_id").unwrap();
    assert!(change.before.is_some() && change.after.is_some());
}

#[tokio::test]
async fn an_audit_off_doctype_gets_no_event_and_no_recursion() {
    let participant = AuditParticipant::new(masker(true));
    let doc = Document::new(n("audit_chain"), RecordId::new_v7());
    let invocation = inv(user());
    let write = WriteRecord {
        doctype: "audit_chain",
        operation: Operation::Create,
        cause: spot_document::engine::WriteCause::Direct,
        node: None,
        old: None,
        new: Some(&doc),
        version: Some(Version::FIRST),
        invocation: &invocation,
    };
    assert_eq!(participant.build(&write).unwrap(), None);
    for off in spot_doc_audit::AUDIT_OFF_DOCTYPES {
        let write = WriteRecord {
            doctype: off,
            ..write
        };
        assert_eq!(participant.build(&write).unwrap(), None, "{off}");
    }
    // A business DocType is audited.
    let mut customer = Document::new(n("customer"), RecordId::new_v7());
    customer.set(n("name"), "Acme");
    let write = WriteRecord {
        doctype: "customer",
        new: Some(&customer),
        ..write
    };
    assert!(participant.build(&write).unwrap().is_some());
}

#[tokio::test]
async fn a_failing_outbox_fails_closed_and_nothing_is_stored() {
    let flaky = Arc::new(FlakyAudit {
        inner: AuditParticipant::new(masker(true)),
        fail_on: Mutex::new(None),
    });
    let f = engine_with(vec![flaky.clone() as Dyn]).await;
    let actor = user();
    let posted = f.posted(actor).await;
    let before = f.outbox().await.len();

    // Create fails: no document, no event of any topic.
    *flaky.fail_on.lock().unwrap() = Some(Operation::Create);
    let mut doc = Document::new(n("customer"), RecordId::new_v7());
    doc.set(n("name"), "Ghost");
    let ghost = doc.id();
    let error = f.engine.create(&inv(actor), doc).await.unwrap_err();
    assert_eq!(error.code, "docaudit.outbox_write");
    assert!(f.engine.peek("customer", ghost).await.unwrap().is_none());

    // Cancel fails: the invoice stays posted at its version.
    *flaky.fail_on.lock().unwrap() = Some(Operation::Cancel);
    let error = f
        .engine
        .cancel(
            &inv(actor),
            "sales_invoice",
            posted.id(),
            posted.version().unwrap(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, "docaudit.outbox_write");
    let still = f
        .engine
        .peek("sales_invoice", posted.id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(still.state(), Some(DocState::Posted));
    assert_eq!(still.version(), posted.version());
    assert_eq!(f.outbox().await.len(), before, "nothing was written");

    // Correct: the cancel's audit event is enqueued, then the amended create's fails; the
    // whole correction rolls back, the cancel's event with it.
    *flaky.fail_on.lock().unwrap() = Some(Operation::Create);
    let error = f
        .engine
        .correct(
            &inv(actor),
            "sales_invoice",
            posted.id(),
            posted.version().unwrap(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, "docaudit.outbox_write");
    let still = f
        .engine
        .peek("sales_invoice", posted.id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(still.state(), Some(DocState::Posted));
    assert_eq!(f.outbox().await.len(), before);
}

#[tokio::test]
async fn an_event_that_cannot_be_built_fails_closed() {
    // The masker has no protection facts for `customer`: the audit cannot be built, so the
    // write is refused rather than stored unaudited.
    let f = engine_with(with_audit_participant(
        AuditParticipant::new(masker(false)),
        Vec::new(),
    ))
    .await;
    let mut doc = Document::new(n("customer"), RecordId::new_v7());
    doc.set(n("name"), "Acme");
    let id = doc.id();
    let error = f.engine.create(&inv(user()), doc).await.unwrap_err();
    assert_eq!(error.code, "docaudit.unknown_doctype");
    assert!(f.engine.peek("customer", id).await.unwrap().is_none());
    assert!(f.outbox().await.is_empty());
}

#[tokio::test]
async fn a_later_participant_refusing_leaves_no_audit_event_behind() {
    let f = engine_with(with_audit_participant(
        AuditParticipant::new(masker(true)),
        vec![Arc::new(RefuseAfter(Operation::Create)) as Dyn],
    ))
    .await;
    let mut doc = Document::new(n("customer"), RecordId::new_v7());
    doc.set(n("name"), "Acme");
    let id = doc.id();
    let error = f.engine.create(&inv(user()), doc).await.unwrap_err();
    assert_eq!(error.code, "test.later_participant_refused");
    assert!(f.engine.peek("customer", id).await.unwrap().is_none());
    assert!(
        f.outbox().await.is_empty(),
        "no audit event without the change"
    );
}

#[tokio::test]
async fn a_failed_validation_writes_no_audit_event() {
    let f = fx().await;
    let doc = Document::new(n("customer"), RecordId::new_v7());
    assert!(f.engine.create(&inv(user()), doc).await.is_err());
    assert!(f.audits().await.is_empty());
}

// ------------------------------------------------------------- the chain, end to end

/// A store whose entries can be altered on read.
struct TamperStore {
    inner: InMemoryStore,
    tampered: Mutex<Vec<(String, u64)>>,
}

impl TamperStore {
    fn visible(&self, site: &str, entries: Vec<ChainEntry>) -> Vec<ChainEntry> {
        let tampered = self.tampered.lock().unwrap();
        entries
            .into_iter()
            .map(|mut e| {
                if tampered.contains(&(site.to_owned(), e.seq)) {
                    e.record.at = e.record.at.saturating_add(1);
                }
                e
            })
            .collect()
    }
}

impl AuditStore for TamperStore {
    fn head<'a>(&'a self, site: &'a str) -> StoreFuture<'a, Option<ChainEntry>> {
        Box::pin(async move {
            let entries = self.inner.range(site, 1, u64::MAX).await?;
            Ok(self.visible(site, entries).into_iter().last())
        })
    }
    fn append_if_head<'a>(
        &'a self,
        site: &'a str,
        expected_seq: u64,
        entry: ChainEntry,
    ) -> StoreFuture<'a, AppendOutcome> {
        self.inner.append_if_head(site, expected_seq, entry)
    }
    fn range<'a>(&'a self, site: &'a str, from: u64, to: u64) -> StoreFuture<'a, Vec<ChainEntry>> {
        Box::pin(async move {
            let entries = self.inner.range(site, from, to).await?;
            Ok(self.visible(site, entries))
        })
    }
    fn put_checkpoint<'a>(&'a self, checkpoint: Checkpoint) -> StoreFuture<'a, ()> {
        self.inner.put_checkpoint(checkpoint)
    }
    fn latest_checkpoint<'a>(&'a self, site: &'a str) -> StoreFuture<'a, Option<Checkpoint>> {
        self.inner.latest_checkpoint(site)
    }
}

#[tokio::test]
async fn a_sequence_driven_through_the_engine_chains_and_verifies_and_one_altered_entry_fails() {
    let f = fx().await;
    let actor = user();
    let posted = f.posted(actor).await;
    let mut patch = DocumentPatch::default();
    patch.fields.insert(n("remarks"), "note".into());
    f.engine
        .update(
            &inv(actor),
            "sales_invoice",
            posted.id(),
            patch,
            posted.version().unwrap(),
        )
        .await
        .unwrap();
    let customer = f.customer(actor, "Beta").await;
    f.engine
        .delete(&inv(actor), "customer", customer.id(), Version::FIRST)
        .await
        .unwrap();
    let audits = f.audits().await;
    assert_eq!(audits.len(), 6);

    let config = LaneConfig::new(1, 1);
    let store = Arc::new(TamperStore {
        inner: InMemoryStore::new(),
        tampered: Mutex::new(Vec::new()),
    });
    let local = LocalSigner::from_seed([9u8; 32]);
    let mut keys = KeyRing::new();
    keys.add(&local.public_key()).unwrap();
    let signer: Arc<dyn CheckpointSigner> = Arc::new(local);
    let state = Arc::new(InMemoryAppenderStore::new());
    let service = DocAudit::new(
        Arc::clone(&store),
        SITE,
        config,
        signer,
        keys,
        masker(true),
        state,
        Arc::new(ManualClock::new(OffsetDateTime::UNIX_EPOCH)),
    );
    let report = service
        .appender(0)
        .drain(f.engine.backend(), 1_000)
        .await
        .unwrap();
    assert_eq!(report.appended, 6, "every audit event chains once");
    let ok = service.verify_site(None).await.unwrap();
    assert!(ok.is_ok(), "{:?}", ok.failures);

    store
        .tampered
        .lock()
        .unwrap()
        .push((spot_doc_audit::lane_site(SITE, 0), 3));
    let bad = service.verify_site(None).await.unwrap();
    assert!(!bad.is_ok(), "an altered entry is detected");
    assert!(bad.failures.iter().any(|x| x.seq == 3));
}

// ----------------------------------------------------------------- cause and node (T3)

#[test]
fn a_method_name_is_recorded_only_when_it_is_a_bounded_lower_case_name() {
    for ok in ["submit", "a", "rename_twice", "x9_y", &"a".repeat(64)] {
        assert!(is_recordable_method(ok), "{ok}");
        let (op, extra) = operation_for(Operation::Update, WriteCause::Method { name: ok });
        assert_eq!(op, DocOperation::Method(ok.to_owned()));
        assert_eq!(extra.method.as_deref(), Some(ok));
        assert_eq!(extra.via, None);
    }
    for bad in [
        "",
        &"a".repeat(65),
        "Submit",
        "has space",
        "dash-ed",
        "dot.ted",
        "naïve",
        "evil\nname",
    ] {
        assert!(!is_recordable_method(bad), "{bad:?}");
        let (op, extra) = operation_for(Operation::Update, WriteCause::Method { name: bad });
        assert_eq!(op, DocOperation::Update, "the base operation, not the name");
        assert_eq!(extra.method, None, "the name is dropped: {bad:?}");
        assert_eq!(extra.via.as_deref(), Some("method"));
    }
}

#[test]
fn the_cause_maps_to_the_event_fields() {
    let (op, extra) = operation_for(Operation::Cancel, WriteCause::CorrectCancel);
    assert_eq!(op, DocOperation::Correct);
    assert_eq!(extra.correction_half.as_deref(), Some("cancel"));
    let (op, extra) = operation_for(Operation::Create, WriteCause::CorrectAmend);
    assert_eq!(op, DocOperation::Correct);
    assert_eq!(extra.correction_half.as_deref(), Some("amend"));
    let (op, extra) = operation_for(Operation::Post, WriteCause::Step);
    assert_eq!(
        op,
        DocOperation::Post,
        "the operation of the step's own write"
    );
    assert_eq!(extra.via.as_deref(), Some("step"));
    let (op, extra) = operation_for(Operation::Update, WriteCause::Direct);
    assert_eq!(op, DocOperation::Update);
    assert_eq!(extra, Default::default());
}

#[tokio::test]
async fn the_node_is_recorded_only_when_the_seam_has_one() {
    let actor = user();
    let single = fx().await;
    single.customer(actor, "Acme").await;
    assert_eq!(single.audits().await[0].1.node, None);
    let raw = &single.outbox().await[0].event.payload;
    assert!(!raw.contains("\"node\""), "absent, not null: {raw}");

    let multi = engine_on(
        with_audit_participant(AuditParticipant::new(masker(true)), Vec::new()),
        FakeBackend::new(),
        Some("edge-7"),
        None,
    )
    .await;
    multi.customer(actor, "Acme").await;
    assert_eq!(multi.audits().await[0].1.node.as_deref(), Some("edge-7"));
}

#[test]
fn an_event_of_schema_one_still_parses_and_chains() {
    let old = r#"{"site":"s","doctype":"customer","id":"0190c0a0-0000-7000-8000-000000000001",
        "version":1,"operation":{"method":"submit"},
        "actor":{"actor":"system","on_behalf_of":null,"component":"scheduler"},"at":5,
        "changed_fields":[],"diff":[],"hidden_changes":false,"command_id":null,
        "source":"api","truncated":false,"diff_digest":null}"#;
    let event = spot_doc_audit::decode_event(old).unwrap();
    assert_eq!(event.operation, DocOperation::Method("submit".into()));
    assert_eq!(event.method, None);
    assert_eq!(event.outcome, None);
    assert!(record_of(&event).is_ok());
    // Re-encoding an old event adds nothing: the new fields are skipped when absent.
    let again = spot_doc_audit::encode_event(&event).unwrap();
    assert!(!again.contains("outcome") && !again.contains("node"));
    assert_eq!(spot_doc_audit::SCHEMA_VERSION, 2);
}

// ----------------------------------------------------------------- denials (T3)

/// Refuses every update with a message that names the value being written.
struct NoUpdates;

impl Authorizer for NoUpdates {
    fn principal_for<'a>(&'a self, inv: &'a Invocation) -> BoxFuture<'a, Principal> {
        AllowAllAuthorizer.principal_for(inv)
    }

    fn decide<'a>(
        &'a self,
        principal: &'a Principal,
        action: Action,
        facts: &'a DocFacts,
    ) -> BoxFuture<'a, spot_permissions::Decision> {
        AllowAllAuthorizer.decide(principal, action, facts)
    }

    fn authorize<'a>(
        &'a self,
        _principal: &'a Principal,
        action: Action,
        _doc: &'a Document,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async move {
            if matches!(action, Action::Op(Operation::Update)) {
                return Err(spot_app_api::AppError::new(
                    spot_app_api::ErrorClass::PermissionDenied,
                    "permission.denied",
                    format!("may not write {DENIED_VALUE}"),
                ));
            }
            Ok(())
        })
    }

    fn authorize_write<'a>(
        &'a self,
        principal: &'a Principal,
        action: Action,
        doc: &'a Document,
        changes: &'a [spot_document::engine::WriteChange],
    ) -> BoxFuture<'a, AppResult<()>> {
        AllowAllAuthorizer.authorize_write(principal, action, doc, changes)
    }

    fn readable<'a>(
        &'a self,
        principal: &'a Principal,
        doc: Document,
        def: &'a spot_app_api::DocTypeDef,
    ) -> BoxFuture<'a, AppResult<Document>> {
        AllowAllAuthorizer.readable(principal, doc, def)
    }

    fn readable_query<'a>(
        &'a self,
        principal: &'a Principal,
        query: &'a Query,
    ) -> BoxFuture<'a, AppResult<Option<spot_document::engine::QueryPlan>>> {
        AllowAllAuthorizer.readable_query(principal, query)
    }
}

const DENIED_VALUE: &str = "TAX-DENIED-5550-VALUE";

/// Writes an audit event in its own transaction on the engine's backend: what a host's denial
/// writer does. Needs the multi-thread runtime (it blocks the worker, never the whole runtime).
struct OutboxWriter {
    backend: FakeBackend,
}

impl AuditWriter for OutboxWriter {
    fn write(&self, event: &DocAuditEvent) -> Result<(), DocAuditError> {
        let backend = self.backend.clone();
        let event = event.clone();
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async move {
                let catalog = Arc::new(contract_tests::catalog().unwrap());
                let mut tx = backend
                    .begin(
                        catalog,
                        TxOptions::new(n("docaudit_denial"), Isolation::ReadCommitted),
                    )
                    .await
                    .map_err(|e| DocAuditError::OutboxWrite(e.explanation))?;
                enqueue_audit(&mut tx, &event).await?;
                tx.commit()
                    .await
                    .map_err(|e| DocAuditError::OutboxWrite(e.explanation))
            })
        })
    }
}

/// An engine whose denials go to an [`AuditDenialSink`] over `writer`.
async fn denial_fx(
    make_writer: impl FnOnce(FakeBackend) -> Arc<dyn AuditWriter>,
    participants: Vec<Dyn>,
    authorizer: Option<Arc<dyn Authorizer>>,
    max_in_flight: usize,
) -> (Fx, Arc<DenialStats>) {
    let backend = FakeBackend::new();
    let writer = make_writer(backend.clone());
    let f = engine_on(participants, backend, None, authorizer).await;
    let stats = DenialStats::new();
    f.engine.set_denial_sink(Arc::new(
        AuditDenialSink::new(writer, Arc::clone(&stats)).with_max_in_flight(max_in_flight),
    ));
    (f, stats)
}

fn outbox_writer(backend: FakeBackend) -> Arc<dyn AuditWriter> {
    Arc::new(OutboxWriter { backend })
}

fn audit_participants() -> Vec<Dyn> {
    with_audit_participant(AuditParticipant::new(masker(true)), Vec::new())
}

fn denied(audits: &[(OutboxRecord, DocAuditEvent)]) -> Vec<&DocAuditEvent> {
    audits
        .iter()
        .map(|(_, e)| e)
        .filter(|e| e.outcome.as_deref() == Some("denied"))
        .collect()
}

/// A denial event holds no value and no message, anywhere in its serialised form.
fn assert_clean(f: &Fx, records: &[OutboxRecord], forbidden: &[&str]) {
    let _ = f;
    for record in records {
        for needle in forbidden {
            assert!(
                !record.event.payload.contains(needle),
                "`{needle}` leaked into {}",
                record.event.payload
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_permission_denial_is_audited_after_rollback_without_value_or_message() {
    let (f, stats) = denial_fx(
        outbox_writer,
        audit_participants(),
        Some(Arc::new(NoUpdates)),
        8,
    )
    .await;
    let actor = user();
    let created = f.customer(actor, "Acme").await;
    let before = f.audits().await.len();
    let mut patch = DocumentPatch::default();
    patch.fields.insert(n("tax_id"), DENIED_VALUE.into());
    let err = f
        .engine
        .update(
            &inv(actor),
            "customer",
            created.id(),
            patch,
            created.version().unwrap(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, "permission.denied");

    let audits = f.audits().await;
    assert_eq!(
        audits.len(),
        before + 1,
        "no event for the rolled-back write, one denial"
    );
    let events = denied(&audits);
    assert_eq!(events.len(), 1);
    let e = events[0];
    assert_eq!(e.doctype, "customer");
    assert_eq!(e.id, created.id().to_string());
    assert_eq!(e.operation, DocOperation::Update);
    assert_eq!(e.error_code.as_deref(), Some("permission.denied"));
    assert_eq!(e.error_class.as_deref(), Some(err.class.api_code()));
    assert!(e.actor.actor.starts_with("user:"), "{:?}", e.actor);
    assert!(e.diff.is_empty() && e.changed_fields.is_empty());
    assert_eq!(e.version, 0);
    assert_eq!(stats.recorded(), 1);
    assert_clean(
        &f,
        &f.outbox().await,
        &[DENIED_VALUE, "may not write", SECRET_VALUE],
    );

    // The appender reads it: it chains under its own kind.
    let record = record_of(e).unwrap();
    assert_eq!(record.kind, "doc.denied");
    let service = DocAudit::new(
        Arc::new(InMemoryStore::new()),
        SITE,
        LaneConfig::new(1, 1),
        Arc::new(LocalSigner::from_seed([9u8; 32])),
        KeyRing::new(),
        masker(true),
        Arc::new(InMemoryAppenderStore::new()),
        Arc::new(ManualClock::new(OffsetDateTime::UNIX_EPOCH)),
    );
    let report = service
        .appender(0)
        .drain(f.engine.backend(), 1_000)
        .await
        .unwrap();
    assert_eq!(report.appended, before + 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_version_conflict_and_a_validation_failure_are_each_audited_once() {
    let (f, _) = denial_fx(outbox_writer, audit_participants(), None, 8).await;
    let actor = user();
    let created = f.customer(actor, "Acme").await;

    let mut patch = DocumentPatch::default();
    patch.fields.insert(n("tax_id"), DENIED_VALUE.into());
    let conflict = f
        .engine
        .update(&inv(actor), "customer", created.id(), patch, Version(99))
        .await
        .unwrap_err();

    let mut bad = Document::new(n("customer"), RecordId::new_v7());
    bad.set(n("tax_id"), DENIED_VALUE);
    let invalid = f.engine.create(&inv(actor), bad).await.unwrap_err();

    let audits = f.audits().await;
    let events = denied(&audits);
    assert_eq!(events.len(), 2, "{events:?}");
    assert_eq!(
        events[0].error_code.as_deref(),
        Some(conflict.code.as_str())
    );
    assert_eq!(events[0].operation, DocOperation::Update);
    assert_eq!(events[1].error_code.as_deref(), Some(invalid.code.as_str()));
    assert_eq!(events[1].operation, DocOperation::Create);
    assert_ne!(conflict.code, invalid.code);
    assert_clean(
        &f,
        &f.outbox().await,
        &[
            DENIED_VALUE,
            conflict.message.as_str(),
            invalid.message.as_str(),
        ],
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_participant_refusal_is_audited_and_its_own_audit_event_rolled_back() {
    let (f, _) = denial_fx(
        outbox_writer,
        with_audit_participant(
            AuditParticipant::new(masker(true)),
            vec![Arc::new(RefuseAfter(Operation::Create)) as Dyn],
        ),
        None,
        8,
    )
    .await;
    let mut doc = Document::new(n("customer"), RecordId::new_v7());
    doc.set(n("name"), "Acme");
    doc.set(n("tax_id"), DENIED_VALUE);
    f.engine.create(&inv(user()), doc).await.unwrap_err();
    let audits = f.audits().await;
    assert_eq!(audits.len(), 1, "only the denial survives the rollback");
    let e = &audits[0].1;
    assert_eq!(e.outcome.as_deref(), Some("denied"));
    assert_eq!(
        e.error_code.as_deref(),
        Some("test.later_participant_refused")
    );
    assert_clean(&f, &f.outbox().await, &[DENIED_VALUE, "message"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failing_writer_is_counted_and_does_not_change_the_callers_error() {
    let authorizer: Option<Arc<dyn Authorizer>> = Some(Arc::new(NoUpdates));
    let (f, stats) = denial_fx(
        |_| Arc::new(FailingWriter),
        audit_participants(),
        authorizer,
        8,
    )
    .await;
    let actor = user();
    let created = f.customer(actor, "Acme").await;
    let err = f
        .engine
        .update(
            &inv(actor),
            "customer",
            created.id(),
            DocumentPatch::default(),
            created.version().unwrap(),
        )
        .await
        .unwrap_err();
    assert_eq!(
        err.code, "permission.denied",
        "the caller's error is unchanged"
    );
    assert_eq!(stats.not_recorded(), 1);
    assert_eq!(stats.recorded(), 0);
    assert!(denied(&f.audits().await).is_empty());

    // And the status says so.
    let service = DocAudit::new(
        Arc::new(InMemoryStore::new()),
        SITE,
        LaneConfig::new(1, 1),
        Arc::new(LocalSigner::from_seed([9u8; 32])),
        KeyRing::new(),
        masker(true),
        Arc::new(InMemoryAppenderStore::new()),
        Arc::new(ManualClock::new(OffsetDateTime::UNIX_EPOCH)),
    )
    .with_denial_stats(Arc::clone(&stats));
    let status = service.status(f.engine.backend(), 100).await.unwrap();
    assert_eq!(status.denials_not_recorded, 1);
    assert_eq!(status.denials_dropped, 0);
}

/// A writer that blocks until released, counting how many are inside.
struct GatedWriter {
    entered: std::sync::atomic::AtomicUsize,
    open: (Mutex<bool>, std::sync::Condvar),
}

impl AuditWriter for GatedWriter {
    fn write(&self, _event: &DocAuditEvent) -> Result<(), DocAuditError> {
        self.entered
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        // The sink calls this inline on a runtime worker. A plain condvar wait would pin the worker
        // together with the tasks queued behind it (a second spawned attempt then never runs, the
        // test waits for `entered == 2` forever, and a failing assertion would hang the runtime's
        // shutdown instead of failing). `block_in_place` hands the worker's queue to another thread.
        tokio::task::block_in_place(|| {
            let mut open = self.open.0.lock().unwrap();
            while !*open {
                open = self.open.1.wait(open).unwrap();
            }
        });
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_flood_of_denials_is_bounded_dropped_and_counted_without_stalling_callers() {
    let gate = Arc::new(GatedWriter {
        entered: std::sync::atomic::AtomicUsize::new(0),
        open: (Mutex::new(false), std::sync::Condvar::new()),
    });
    let writer = Arc::clone(&gate);
    let (f, stats) = denial_fx(
        move |_| writer,
        audit_participants(),
        Some(Arc::new(NoUpdates)),
        2,
    )
    .await;
    let f = Arc::new(f);
    let actor = user();
    let created = f.customer(actor, "Acme").await;

    let attempt = |f: Arc<Fx>, id: RecordId, version: Version| {
        tokio::spawn(async move {
            f.engine
                .update(
                    &inv(actor),
                    "customer",
                    id,
                    DocumentPatch::default(),
                    version,
                )
                .await
                .unwrap_err()
                .code
                .clone()
        })
    };
    let mut held = Vec::new();
    for _ in 0..2 {
        held.push(attempt(
            Arc::clone(&f),
            created.id(),
            created.version().unwrap(),
        ));
    }
    let waiting_since = std::time::Instant::now();
    while gate.entered.load(std::sync::atomic::Ordering::SeqCst) < 2 {
        if waiting_since.elapsed() > std::time::Duration::from_secs(30) {
            // Release the gate first: a held writer would otherwise block the runtime's shutdown.
            *gate.open.0.lock().unwrap() = true;
            gate.open.1.notify_all();
            panic!("the two held denial writes never both entered the writer");
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    // Two writes are in flight: the next three callers get their answer at once, and their
    // denials are dropped and counted.
    for _ in 0..3 {
        let code = attempt(Arc::clone(&f), created.id(), created.version().unwrap())
            .await
            .unwrap();
        assert_eq!(code, "permission.denied");
    }
    assert_eq!(stats.dropped(), 3);
    *gate.open.0.lock().unwrap() = true;
    gate.open.1.notify_all();
    for h in held {
        assert_eq!(h.await.unwrap(), "permission.denied");
    }
    assert_eq!(stats.recorded(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_denial_for_an_audit_off_doctype_is_not_written() {
    let (f, stats) = denial_fx(outbox_writer, audit_participants(), None, 8).await;
    // `audit_chain` is on the audit-off list; a create without its required field is refused.
    let doc = Document::new(n("audit_chain"), RecordId::new_v7());
    f.engine.create(&inv(user()), doc).await.unwrap_err();
    assert!(f.audits().await.is_empty());
    assert_eq!(stats.recorded() + stats.dropped() + stats.not_recorded(), 0);
    // The same refusal on a business DocType is written.
    let doc = Document::new(n("customer"), RecordId::new_v7());
    f.engine.create(&inv(user()), doc).await.unwrap_err();
    assert_eq!(denied(&f.audits().await).len(), 1);
}

// ----------------------------------------------------------------- install-time checks (T3)

struct Named;

impl<Tx: Transaction> TxParticipant<Tx> for Named {}

type Tx = <FakeBackend as StorageBackend>::Tx;

/// Participants in the given roles, and the address of each, to tell them apart afterwards.
fn roster(roles: &[Role]) -> (Vec<Placed<Tx>>, Vec<usize>) {
    let mut placed = Vec::new();
    let mut addrs = Vec::new();
    for role in roles {
        let p: Dyn = Arc::new(Named);
        addrs.push(Arc::as_ptr(&p).cast::<()>() as usize);
        placed.push(Placed::new(*role, p));
    }
    (placed, addrs)
}

#[test]
fn checked_participants_orders_audit_first_and_edge_sync_last_keeping_the_rest() {
    let (others, addrs) = roster(&[Role::EdgeSync, Role::Other, Role::OwnerCheck, Role::Other]);
    let out = checked_participants(
        AuditParticipant::new(masker(true)),
        others,
        &spot_controllers::ControllerRegistry::new(),
    )
    .unwrap();
    assert_eq!(out.len(), 5);
    let order: Vec<usize> = out
        .iter()
        .map(|p| Arc::as_ptr(p).cast::<()>() as usize)
        .collect();
    // After the audit participant: owner check, the two others in their order, edge sync last.
    assert_eq!(&order[1..], &[addrs[2], addrs[1], addrs[3], addrs[0]]);
    assert!(
        !addrs.contains(&order[0]),
        "the first is the audit participant"
    );
}

#[test]
fn checked_participants_refuses_an_audit_step_on_an_audited_doctype_and_passes_without() {
    use spot_controllers::{ControllerRegistry, ControllerSource};
    use spot_doc_audit::{AuditStep, NoBeforeImages};
    let step = AuditStep::new(
        "customer",
        Arc::new(FailingWriter),
        masker(true),
        Arc::new(NoBeforeImages),
        Arc::new(ManualClock::new(now())),
    );
    assert_eq!(step.audits(), "customer");
    let mut registry = ControllerRegistry::new();
    registry.register("books", Arc::new(step), ControllerSource::Native);

    let participant =
        || AuditParticipant::new(masker(true)).with_doctypes(["customer", "sales_invoice"]);
    let err = checked_participants::<Tx>(participant(), Vec::new(), &registry)
        .err()
        .unwrap();
    assert_eq!(
        err,
        DocAuditError::DoubleRegistration {
            app: "books".into(),
            doctype: "customer".into()
        }
    );
    assert_eq!(err.code(), "docaudit.double_registration");

    // The step is on `customer`; a participant told only about another DocType passes, and so
    // does a registry holding an ordinary controller.
    assert!(
        checked_participants::<Tx>(
            AuditParticipant::new(masker(true)).with_doctypes(["sales_invoice"]),
            Vec::new(),
            &registry
        )
        .is_ok()
    );
    let mut plain = ControllerRegistry::new();
    plain.register("books", Arc::new(Probe), ControllerSource::Native);
    assert!(checked_participants::<Tx>(participant(), Vec::new(), &plain).is_ok());
}

#[test]
fn a_second_audit_participant_is_refused() {
    let (others, _) = roster(&[Role::Audit]);
    let err = checked_participants(
        AuditParticipant::new(masker(true)),
        others,
        &spot_controllers::ControllerRegistry::new(),
    )
    .err()
    .unwrap();
    assert_eq!(err.code(), "docaudit.double_registration");
}
