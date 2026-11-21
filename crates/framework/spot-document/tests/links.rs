//! Link integrity on every write path of the engine (WP-DOC-06.T2): link values are validated in
//! the operation's transaction before storage, a delete or cancel is refused while documents
//! still refer to the target (even documents the caller may not read), a target the caller may
//! not read looks exactly like a missing one, and a rename onto a taken name is refused.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use spot_app_api::context::{Actor, DocumentService, Invocation};
use spot_app_api::prelude::{CommandId, Name, OffsetDateTime, RecordId, SiteId, Value, Version};
use spot_app_api::{AppError, ChildRow, DocState, Document, DocumentPatch, ErrorClass};
use spot_core::ManualClock;
use spot_document::engine::{Engine, MemoryPrincipals, PermissionAuthorizer, V7Ids};
use spot_meta::{Definition, EffectiveMeta, FieldSpec, FieldType, Registry};
use spot_permissions::{
    ModelTexts, PermissionEngine, Principal, Snapshot, TENANT_ADMINISTRATOR, load,
};
use spot_seams::RegistryBuilder;
use spot_storage_fake::FakeBackend;

// -------------------------------------------------------------------------------------------
// Fixture
// -------------------------------------------------------------------------------------------

fn n(s: &str) -> Name {
    Name::new(s).unwrap()
}

fn link(name: &str, target: &str) -> FieldSpec {
    let mut f = FieldSpec::new(name, FieldType::Link);
    f.options = Some(target.into());
    f
}

fn child(name: &str, fields: Vec<FieldSpec>) -> Definition {
    let mut d = Definition::new(name);
    d.is_child = true;
    d.fields = fields;
    d
}

fn table(name: &str, child: &str) -> FieldSpec {
    let mut f = FieldSpec::new(name, FieldType::Table);
    f.options = Some(child.into());
    f
}

/// Region and tag (plain targets); customer (named from its `name`, links a region, holds a
/// table multiselect of tags); invoice and payment (posted; the payment points at an invoice,
/// and may be re-pointed after posting); line rows that link a customer; a note with a dynamic
/// link; a memo whose title is shared display text (no `naming`).
fn definitions() -> Vec<Definition> {
    let mut region = Definition::new("region");
    region.fields = vec![FieldSpec::new("name", FieldType::Text)];
    let mut tag = Definition::new("tag");
    tag.fields = vec![FieldSpec::new("name", FieldType::Text)];

    let mut customer = Definition::new("customer");
    customer.title_field = Some("name".into());
    customer.naming = Some("field:name".into());
    let mut tags = FieldSpec::new("tags", FieldType::TableMultiselect);
    tags.options = Some("customer_tag".into());
    customer.fields = vec![
        FieldSpec::new("name", FieldType::Text),
        link("region", "region"),
        tags,
    ];
    let customer_tag = child("customer_tag", vec![link("tag", "tag")]);

    let mut invoice = Definition::new("invoice");
    invoice.is_posted = true;
    invoice.title_field = Some("number".into());
    invoice.fields = vec![
        FieldSpec::new("number", FieldType::Text),
        link("customer", "customer"),
        table("lines", "invoice_line"),
    ];
    let line = child(
        "invoice_line",
        vec![
            FieldSpec::new("sku", FieldType::Text),
            link("billing_customer", "customer"),
        ],
    );

    let mut payment = Definition::new("payment");
    payment.is_posted = true;
    payment.title_field = Some("reference".into());
    let mut pay_invoice = link("invoice", "invoice");
    pay_invoice.allow_on_posted = true;
    payment.fields = vec![FieldSpec::new("reference", FieldType::Text), pay_invoice];

    let mut note = Definition::new("note");
    let mut about = FieldSpec::new("about", FieldType::DynamicLink);
    about.options = Some("about_type".into());
    note.fields = vec![
        FieldSpec::new("subject", FieldType::Text),
        FieldSpec::new("about_type", FieldType::Text),
        about,
    ];

    let mut memo = Definition::new("memo");
    memo.title_field = Some("subject".into());
    memo.fields = vec![FieldSpec::new("subject", FieldType::Text)];

    vec![
        region,
        tag,
        customer,
        customer_tag,
        invoice,
        line,
        payment,
        note,
        memo,
    ]
}

fn meta() -> Arc<EffectiveMeta> {
    Registry::new(definitions()).unwrap().effective()
}

/// `clerk` writes everything but may not read `region`; `regional` may read and delete `region`
/// and nothing else; `canceller` may read and cancel invoices and nothing else.
const GRANTS: &str = r#"
[[entries]]
doctype = "customer"
role = "clerk"
rights = ["read", "write", "create", "delete"]
[[entries]]
doctype = "tag"
role = "clerk"
rights = ["read", "write", "create", "delete"]
[[entries]]
doctype = "invoice"
role = "clerk"
rights = ["read", "write", "create", "delete", "submit", "cancel", "correct"]
[[entries]]
doctype = "payment"
role = "clerk"
rights = ["read", "write", "create", "delete", "submit", "cancel", "correct", "update_posted"]
[[entries]]
doctype = "note"
role = "clerk"
rights = ["read", "write", "create", "delete"]
[[entries]]
doctype = "memo"
role = "clerk"
rights = ["read", "write", "create", "delete"]
[[entries]]
doctype = "invoice_line"
role = "clerk"
rights = ["read", "write", "create", "delete"]
[[entries]]
doctype = "customer_tag"
role = "clerk"
rights = ["read", "write", "create", "delete"]
[[entries]]
doctype = "region"
role = "regional"
rights = ["read", "delete"]
[[entries]]
doctype = "invoice"
role = "canceller"
rights = ["read", "cancel"]
"#;

struct World {
    engine: Engine<FakeBackend>,
    ids: BTreeMap<&'static str, RecordId>,
}

const USERS: [&str; 4] = ["admin", "clerk", "regional", "canceller"];

async fn world() -> World {
    let meta = meta();
    let model = load(&ModelTexts {
        grants: GRANTS.to_owned(),
        field_grants: String::new(),
        groups: String::new(),
        shares: String::new(),
    })
    .unwrap();
    let snapshot = Snapshot::build(&meta, &model).unwrap();
    let decisions = Arc::new(PermissionEngine::new(Arc::new(snapshot)));
    let ids: BTreeMap<&'static str, RecordId> =
        USERS.iter().map(|u| (*u, RecordId::new_v7())).collect();
    let mut principals = MemoryPrincipals::new();
    for user in USERS {
        let role = if user == "admin" {
            TENANT_ADMINISTRATOR
        } else {
            user
        };
        principals = principals.with(&ids[user], Principal::with_roles(user, &[role]));
    }
    let engine = Engine::new(
        FakeBackend::new(),
        meta,
        Arc::new(RegistryBuilder::new().build().unwrap()),
        Arc::new(ManualClock::new(now())),
        Arc::new(V7Ids),
    )
    .unwrap()
    .with_authorizer(Arc::new(PermissionAuthorizer::new(
        decisions,
        Arc::new(principals),
    )));
    engine.prepare().await.unwrap();
    World { engine, ids }
}

fn now() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_790_000_000).unwrap()
}

impl World {
    fn inv(&self, user: &str) -> Invocation {
        Invocation {
            site: SiteId::new("links.test").unwrap(),
            actor: Actor::User {
                id: self.ids[user],
                via: None,
            },
            command_id: CommandId::new("cmd-links").ok(),
            language: None,
            now: now(),
        }
    }

    async fn make(&self, user: &str, doctype: &str, fields: &[(&str, Value)]) -> Document {
        self.try_make(user, doctype, fields).await.unwrap()
    }

    async fn try_make(
        &self,
        user: &str,
        doctype: &str,
        fields: &[(&str, Value)],
    ) -> Result<Document, AppError> {
        let mut doc = Document::new(n(doctype), RecordId::new_v7());
        for (field, value) in fields {
            doc.set(n(field), value.clone());
        }
        Ok(self.engine.create(&self.inv(user), doc).await?.value)
    }

    async fn reload(&self, doctype: &str, id: RecordId) -> Option<Document> {
        self.engine.peek(doctype, id).await.unwrap()
    }

    async fn post(&self, doctype: &str, doc: &Document) -> Result<Document, AppError> {
        Ok(self
            .engine
            .post(&self.inv("clerk"), doctype, doc.id(), version(doc))
            .await?
            .value)
    }

    async fn customer(&self, name: &str) -> Document {
        self.make("clerk", "customer", &[("name", text(name))])
            .await
    }

    async fn invoice(&self, number: &str, customer: &Document) -> Document {
        self.make(
            "clerk",
            "invoice",
            &[("number", text(number)), ("customer", id(customer))],
        )
        .await
    }

    async fn payment(&self, reference: &str, invoice: &Document) -> Document {
        self.make(
            "clerk",
            "payment",
            &[("reference", text(reference)), ("invoice", id(invoice))],
        )
        .await
    }
}

fn version(doc: &Document) -> Version {
    doc.version().unwrap()
}

fn text(value: &str) -> Value {
    Value::Text(value.to_owned())
}

fn id(doc: &Document) -> Value {
    Value::Id(doc.id())
}

fn fields_of(error: &AppError) -> Vec<(String, String)> {
    error
        .fields
        .iter()
        .map(|f| (f.field.to_string(), f.code.clone()))
        .collect()
}

fn patch(field: &str, value: Value) -> DocumentPatch {
    DocumentPatch {
        fields: BTreeMap::from([(n(field), value)]),
        ..DocumentPatch::default()
    }
}

fn row(fields: &[(&str, Value)]) -> ChildRow {
    ChildRow {
        id: RecordId::new_v7(),
        fields: fields.iter().map(|(k, v)| (n(k), v.clone())).collect(),
    }
}

/// The field error code of the engine's refusal of a link to nothing the caller can see: the
/// code the wire has always used for it.
const MISSING: &str = "document.invalid_link";

// -------------------------------------------------------------------------------------------
// Validation on every write path
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn create_refuses_a_link_to_a_missing_record_with_the_field_path() {
    let w = world().await;
    let error = w
        .try_make(
            "clerk",
            "invoice",
            &[
                ("number", text("INV-1")),
                ("customer", Value::Id(RecordId::new_v7())),
            ],
        )
        .await
        .unwrap_err();
    assert_eq!(error.class, ErrorClass::Validation);
    assert_eq!(
        fields_of(&error),
        [("customer".to_owned(), MISSING.to_owned())]
    );
}

#[tokio::test]
async fn create_refuses_a_link_to_the_wrong_doctype() {
    let w = world().await;
    let tag = w.make("clerk", "tag", &[("name", text("vip"))]).await;
    // A tag's id in a customer link: it is not among the customers.
    let error = w
        .try_make(
            "clerk",
            "invoice",
            &[("number", text("INV-1")), ("customer", id(&tag))],
        )
        .await
        .unwrap_err();
    assert_eq!(
        fields_of(&error),
        [("customer".to_owned(), MISSING.to_owned())]
    );
}

#[tokio::test]
async fn a_bad_row_link_names_the_exact_path() {
    let w = world().await;
    let acme = w.customer("Acme").await;
    let mut doc = Document::new(n("invoice"), RecordId::new_v7());
    doc.set(n("number"), "INV-2");
    doc.set(n("customer"), id(&acme));
    for billing in [id(&acme), Value::Id(RecordId::new_v7())] {
        doc.rows_mut(n("lines"))
            .push(row(&[("sku", text("A")), ("billing_customer", billing)]));
    }
    let error = w.engine.create(&w.inv("clerk"), doc).await.unwrap_err();
    assert_eq!(
        fields_of(&error),
        [("lines[2].billing_customer".to_owned(), MISSING.to_owned())]
    );
}

#[tokio::test]
async fn a_table_multiselect_with_one_bad_value_names_that_row() {
    let w = world().await;
    let vip = w.make("clerk", "tag", &[("name", text("vip"))]).await;
    let mut doc = Document::new(n("customer"), RecordId::new_v7());
    doc.set(n("name"), "Acme");
    doc.rows_mut(n("tags")).push(row(&[("tag", id(&vip))]));
    doc.rows_mut(n("tags"))
        .push(row(&[("tag", Value::Id(RecordId::new_v7()))]));
    let error = w.engine.create(&w.inv("clerk"), doc).await.unwrap_err();
    assert_eq!(
        fields_of(&error),
        [("tags[2].tag".to_owned(), MISSING.to_owned())]
    );
}

#[tokio::test]
async fn update_refuses_a_link_moved_to_a_missing_record_and_changes_nothing() {
    let w = world().await;
    let acme = w.customer("Acme").await;
    let invoice = w.invoice("INV-1", &acme).await;
    let error = w
        .engine
        .update(
            &w.inv("clerk"),
            "invoice",
            invoice.id(),
            patch("customer", Value::Id(RecordId::new_v7())),
            version(&invoice),
        )
        .await
        .unwrap_err();
    assert_eq!(
        fields_of(&error),
        [("customer".to_owned(), MISSING.to_owned())]
    );
    let stored = w.reload("invoice", invoice.id()).await.unwrap();
    assert_eq!(stored.get("customer"), &id(&acme), "nothing was written");
    assert_eq!(stored.version(), invoice.version());
}

#[tokio::test]
async fn a_dynamic_link_checks_its_type_and_its_target() {
    let w = world().await;
    let acme = w.customer("Acme").await;
    let ok = w
        .try_make(
            "clerk",
            "note",
            &[("about_type", text("customer")), ("about", id(&acme))],
        )
        .await;
    assert!(ok.is_ok(), "{ok:?}");
    for (kind, value, code) in [
        // The type names no DocType, or one a note may not link (a child table).
        ("widget", id(&acme), "links.dynamic_type_unknown"),
        ("customer_tag", id(&acme), "links.dynamic_type_not_allowed"),
        // The type is allowed but the id is not one of that type (or does not exist).
        ("tag", id(&acme), MISSING),
        ("customer", Value::Id(RecordId::new_v7()), MISSING),
    ] {
        let error = w
            .try_make(
                "clerk",
                "note",
                &[("about_type", text(kind)), ("about", value)],
            )
            .await
            .unwrap_err();
        assert_eq!(
            fields_of(&error),
            [("about".to_owned(), code.to_owned())],
            "{kind}"
        );
    }
}

#[tokio::test]
async fn post_refuses_a_document_that_points_at_a_draft() {
    let w = world().await;
    let acme = w.customer("Acme").await;
    let invoice = w.invoice("INV-1", &acme).await;
    let payment = w.payment("PAY-1", &invoice).await;
    // A draft payment may point at a draft invoice; a posted one may not.
    let error = w.post("payment", &payment).await.unwrap_err();
    assert_eq!(
        fields_of(&error),
        [("invoice".to_owned(), "links.draft_target".to_owned())]
    );
    let stored = w.reload("payment", payment.id()).await.unwrap();
    assert_eq!(
        stored.state(),
        Some(DocState::Draft),
        "the post rolled back"
    );
    // Once the invoice is posted, the payment can be.
    w.post("invoice", &invoice).await.unwrap();
    let payment = w.reload("payment", payment.id()).await.unwrap();
    w.post("payment", &payment).await.unwrap();
}

#[tokio::test]
async fn update_of_a_posted_document_validates_its_links() {
    let w = world().await;
    let acme = w.customer("Acme").await;
    let posted = w.invoice("INV-1", &acme).await;
    let posted = w.post("invoice", &posted).await.unwrap();
    let other = w.invoice("INV-2", &acme).await;
    let payment = w.payment("PAY-1", &posted).await;
    let payment = w.post("payment", &payment).await.unwrap();

    let repoint = |target: Value| async {
        w.engine
            .update(
                &w.inv("clerk"),
                "payment",
                payment.id(),
                patch("invoice", target),
                version(&payment),
            )
            .await
    };
    let error = repoint(Value::Id(RecordId::new_v7())).await.unwrap_err();
    assert_eq!(
        fields_of(&error),
        [("invoice".to_owned(), MISSING.to_owned())]
    );
    let error = repoint(id(&other)).await.unwrap_err();
    assert_eq!(
        fields_of(&error),
        [("invoice".to_owned(), "links.draft_target".to_owned())]
    );
    // Neither attempt wrote anything.
    let stored = w.reload("payment", payment.id()).await.unwrap();
    assert_eq!(stored.get("invoice"), &id(&posted));
}

#[tokio::test]
async fn a_link_to_a_cancelled_document_is_refused() {
    let w = world().await;
    let acme = w.customer("Acme").await;
    let invoice = w.invoice("INV-1", &acme).await;
    let invoice = w.post("invoice", &invoice).await.unwrap();
    let cancelled = w
        .engine
        .cancel(&w.inv("clerk"), "invoice", invoice.id(), version(&invoice))
        .await
        .unwrap()
        .value;
    let error = w
        .try_make(
            "clerk",
            "payment",
            &[("reference", text("PAY-1")), ("invoice", id(&cancelled))],
        )
        .await
        .unwrap_err();
    assert_eq!(
        fields_of(&error),
        [("invoice".to_owned(), "links.cancelled_target".to_owned())]
    );
}

#[tokio::test]
async fn a_correction_points_at_the_cancelled_document_it_replaces() {
    let w = world().await;
    let acme = w.customer("Acme").await;
    let invoice = w.invoice("INV-1", &acme).await;
    let invoice = w.post("invoice", &invoice).await.unwrap();
    // A correction cancels the posted document and drafts its replacement; the draft points at
    // the now cancelled original, which the cancelled-target rule exempts (`amended_from`).
    let draft = w
        .engine
        .correct(&w.inv("clerk"), "invoice", invoice.id(), version(&invoice))
        .await
        .unwrap()
        .value;
    assert_eq!(draft.state(), Some(DocState::Draft));
    assert_eq!(draft.get("amended_from"), &id(&invoice));
    let original = w.reload("invoice", invoice.id()).await.unwrap();
    assert_eq!(original.state(), Some(DocState::Cancelled));
}

// -------------------------------------------------------------------------------------------
// No existence leak
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_target_the_caller_cannot_read_looks_exactly_like_a_missing_one() {
    let w = world().await;
    // The region exists, but the clerk has no read right on `region`.
    let region = w.make("admin", "region", &[("name", text("North"))]).await;
    let missing = Value::Id(RecordId::new_v7());
    let hidden = w
        .try_make(
            "clerk",
            "customer",
            &[("name", text("A")), ("region", id(&region))],
        )
        .await
        .unwrap_err();
    let absent = w
        .try_make(
            "clerk",
            "customer",
            &[("name", text("B")), ("region", missing)],
        )
        .await
        .unwrap_err();
    assert_eq!(hidden.code, absent.code);
    assert_eq!(hidden.class, absent.class);
    assert_eq!(hidden.message, absent.message);
    assert_eq!(fields_of(&hidden), fields_of(&absent));
    assert_eq!(
        hidden.fields.iter().map(|f| &f.message).collect::<Vec<_>>(),
        absent.fields.iter().map(|f| &f.message).collect::<Vec<_>>()
    );
    // Whoever may read it can link to it.
    let ok = w
        .try_make(
            "admin",
            "customer",
            &[("name", text("C")), ("region", id(&region))],
        )
        .await;
    assert!(ok.is_ok(), "{ok:?}");
}

#[tokio::test]
async fn a_dynamic_link_to_an_unreadable_type_looks_like_a_missing_target() {
    let w = world().await;
    let region = w.make("admin", "region", &[("name", text("North"))]).await;
    let error = w
        .try_make(
            "clerk",
            "note",
            &[("about_type", text("region")), ("about", id(&region))],
        )
        .await
        .unwrap_err();
    assert_eq!(
        fields_of(&error),
        [("about".to_owned(), MISSING.to_owned())]
    );
}

// -------------------------------------------------------------------------------------------
// Delete and cancel with referrers
// -------------------------------------------------------------------------------------------

fn referrer_groups(error: &AppError) -> Vec<(String, u64)> {
    error.details["referrers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| {
            (
                g["doctype"].as_str().unwrap().to_owned(),
                g["count"].as_u64().unwrap(),
            )
        })
        .collect()
}

#[tokio::test]
async fn delete_of_a_referenced_document_lists_the_referrers() {
    let w = world().await;
    let acme = w.customer("Acme").await;
    let invoice = w.invoice("INV-1", &acme).await;
    let error = w
        .engine
        .delete(&w.inv("clerk"), "customer", acme.id(), version(&acme))
        .await
        .unwrap_err();
    assert_eq!(error.class, ErrorClass::Linked);
    assert_eq!(
        error.code, "document.linked",
        "the code a delete always had"
    );
    assert_eq!(error.details["reason"], "links.referenced");
    assert_eq!(referrer_groups(&error), [("invoice".to_owned(), 1)]);
    assert!(w.reload("customer", acme.id()).await.is_some());
    // Once the referrer is gone the delete goes through.
    w.engine
        .delete(&w.inv("clerk"), "invoice", invoice.id(), version(&invoice))
        .await
        .unwrap();
    w.engine
        .delete(&w.inv("clerk"), "customer", acme.id(), version(&acme))
        .await
        .unwrap();
}

#[tokio::test]
async fn a_row_reference_counts_under_its_parent_document() {
    let w = world().await;
    let acme = w.customer("Acme").await;
    let bill_to = w.customer("Bill To").await;
    let mut doc = Document::new(n("invoice"), RecordId::new_v7());
    doc.set(n("number"), "INV-1");
    doc.set(n("customer"), id(&acme));
    doc.rows_mut(n("lines")).push(row(&[
        ("sku", text("A")),
        ("billing_customer", id(&bill_to)),
    ]));
    w.engine.create(&w.inv("clerk"), doc).await.unwrap();
    let error = w
        .engine
        .delete(&w.inv("clerk"), "customer", bill_to.id(), version(&bill_to))
        .await
        .unwrap_err();
    assert_eq!(error.code, "document.linked");
    assert_eq!(referrer_groups(&error), [("invoice".to_owned(), 1)]);
}

#[tokio::test]
async fn a_referrer_the_caller_cannot_read_still_blocks_a_delete_without_being_named() {
    let w = world().await;
    let region = w.make("admin", "region", &[("name", text("North"))]).await;
    let customer = w
        .make(
            "admin",
            "customer",
            &[("name", text("Acme")), ("region", id(&region))],
        )
        .await;
    // `regional` may delete the region but cannot read the customer that points at it.
    let error = w
        .engine
        .delete(&w.inv("regional"), "region", region.id(), version(&region))
        .await
        .unwrap_err();
    assert_eq!(error.class, ErrorClass::Linked);
    assert_eq!(error.code, "document.linked");
    assert!(!error.details.contains_key("referrers"), "{error:?}");
    assert!(!error.details.contains_key("count"), "{error:?}");
    let text = format!("{error:?}");
    assert!(!text.contains(&customer.id().to_string()), "{text}");
    assert!(
        !text.contains("customer"),
        "no referrer DocType leaks: {text}"
    );
    assert!(
        w.reload("region", region.id()).await.is_some(),
        "not orphaned"
    );
    // Someone who can read the referrer is told who it is.
    let error = w
        .engine
        .delete(&w.inv("admin"), "region", region.id(), version(&region))
        .await
        .unwrap_err();
    assert_eq!(referrer_groups(&error), [("customer".to_owned(), 1)]);
}

#[tokio::test]
async fn cancel_of_a_referenced_posted_document_is_refused_until_the_referrers_are_cancelled() {
    let w = world().await;
    let acme = w.customer("Acme").await;
    let invoice = w.invoice("INV-1", &acme).await;
    let invoice = w.post("invoice", &invoice).await.unwrap();
    let payment = w.payment("PAY-1", &invoice).await;
    let payment = w.post("payment", &payment).await.unwrap();

    let cancel_invoice = || async {
        w.engine
            .cancel(&w.inv("clerk"), "invoice", invoice.id(), version(&invoice))
            .await
    };
    let error = cancel_invoice().await.unwrap_err();
    assert_eq!(error.code, "links.referenced");
    assert_eq!(error.class, ErrorClass::Linked);
    assert_eq!(referrer_groups(&error), [("payment".to_owned(), 1)]);
    let stored = w.reload("invoice", invoice.id()).await.unwrap();
    assert_eq!(stored.state(), Some(DocState::Posted), "nothing changed");

    // The rule is "cancel the referencing documents first": a cancelled referrer no longer
    // blocks, or the invoice could never be cancelled.
    w.engine
        .cancel(&w.inv("clerk"), "payment", payment.id(), version(&payment))
        .await
        .unwrap();
    let cancelled = cancel_invoice().await.unwrap().value;
    assert_eq!(cancelled.state(), Some(DocState::Cancelled));
}

#[tokio::test]
async fn a_referrer_the_caller_cannot_read_still_blocks_a_cancel() {
    let w = world().await;
    let acme = w.customer("Acme").await;
    let invoice = w.invoice("INV-1", &acme).await;
    let invoice = w.post("invoice", &invoice).await.unwrap();
    let payment = w.payment("PAY-1", &invoice).await;
    w.post("payment", &payment).await.unwrap();
    // `canceller` reads and cancels invoices only.
    let error = w
        .engine
        .cancel(
            &w.inv("canceller"),
            "invoice",
            invoice.id(),
            version(&invoice),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, "links.referenced");
    assert!(!error.details.contains_key("referrers"), "{error:?}");
    assert!(!format!("{error:?}").contains("payment"), "{error:?}");
    let stored = w.reload("invoice", invoice.id()).await.unwrap();
    assert_eq!(stored.state(), Some(DocState::Posted));
}

#[tokio::test]
async fn a_failed_write_leaves_no_dangling_link_behind() {
    let w = world().await;
    let acme = w.customer("Acme").await;
    // The second row is bad: the whole create rolls back, the first row's customer is intact.
    let mut doc = Document::new(n("invoice"), RecordId::new_v7());
    doc.set(n("number"), "INV-1");
    doc.set(n("customer"), id(&acme));
    doc.rows_mut(n("lines"))
        .push(row(&[("billing_customer", Value::Id(RecordId::new_v7()))]));
    let new_id = doc.id();
    w.engine.create(&w.inv("clerk"), doc).await.unwrap_err();
    assert!(w.reload("invoice", new_id).await.is_none());
    // And the customer is not blocked by a phantom referrer.
    w.engine
        .delete(&w.inv("clerk"), "customer", acme.id(), version(&acme))
        .await
        .unwrap();
}

// -------------------------------------------------------------------------------------------
// Rename
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_rename_onto_a_taken_name_is_refused_for_a_named_doctype() {
    let w = world().await;
    let _acme = w.customer("Acme").await;
    let beta = w.customer("Beta").await;
    let error = w
        .engine
        .rename(
            &w.inv("clerk"),
            "customer",
            beta.id(),
            "Acme",
            version(&beta),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, "links.name_taken");
    assert_eq!(error.class, ErrorClass::Duplicate);
    let stored = w.reload("customer", beta.id()).await.unwrap();
    assert_eq!(stored.get("name"), &text("Beta"));
    // A free name is fine.
    let renamed = w
        .engine
        .rename(
            &w.inv("clerk"),
            "customer",
            beta.id(),
            "Gamma",
            version(&beta),
        )
        .await
        .unwrap()
        .value;
    assert_eq!(renamed.get("name"), &text("Gamma"));
}

#[tokio::test]
async fn a_rename_keeps_every_reference_valid() {
    let w = world().await;
    let acme = w.customer("Acme").await;
    let invoice = w.invoice("INV-1", &acme).await;
    w.engine
        .rename(
            &w.inv("clerk"),
            "customer",
            acme.id(),
            "Acme Ltd",
            version(&acme),
        )
        .await
        .unwrap();
    let stored = w.reload("invoice", invoice.id()).await.unwrap();
    assert_eq!(stored.get("customer"), &id(&acme));
}

#[tokio::test]
async fn titles_that_are_display_text_may_be_shared() {
    // `memo` has no `naming`: its title is a label several documents may carry.
    let w = world().await;
    let _first = w
        .make("clerk", "memo", &[("subject", text("Weekly"))])
        .await;
    let second = w
        .make("clerk", "memo", &[("subject", text("Monthly"))])
        .await;
    let renamed = w
        .engine
        .rename(
            &w.inv("clerk"),
            "memo",
            second.id(),
            "Weekly",
            version(&second),
        )
        .await
        .unwrap()
        .value;
    assert_eq!(renamed.get("subject"), &text("Weekly"));
}

// -------------------------------------------------------------------------------------------
// Tenancy
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_link_cannot_reach_a_record_of_another_site() {
    // Two worlds are two sites: each has its own store, so an id from one is a missing record in
    // the other.
    let a = world().await;
    let b = world().await;
    let customer_in_b = b.customer("Acme").await;
    let error = a
        .try_make(
            "clerk",
            "invoice",
            &[("number", text("INV-1")), ("customer", id(&customer_in_b))],
        )
        .await
        .unwrap_err();
    assert_eq!(
        fields_of(&error),
        [("customer".to_owned(), MISSING.to_owned())]
    );
}
