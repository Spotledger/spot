#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
#![cfg(not(feature = "node"))]

//! The four review findings of WP-SEC-10.T2, each pinned by a test that fails on the old code
//! (WP-SEC-10.T3): an unmapped group fails closed, a reveal derives its own field facts, a bulk
//! reveal audits what it revealed and to whom, and a node's sealed write checks its associated
//! data against the place it writes.

use std::collections::BTreeMap;
use std::sync::Arc;

use spot_audit_chain::PayloadValue;
use spot_meta::{Definition, FieldSpec, FieldType, ProtectionGroupSpec, Registry};
use spot_permissions::{
    DocFacts, FieldFacts, GroupName as PermGroup, MaskFormat, Model, ModelTexts, PermissionEngine,
    Principal, Snapshot, Tag, load,
};
use spot_protected_keys::{GroupName, TenantId};
use spot_sealed_fields::{
    BulkRequest, Destination, Loaded, LocalHub, MemoryAuditSink, NodeFieldStore, RevealService,
    SealTarget, SealedAd, ServiceManifest, SyncClass, SyncClassTable, SyncPolicy, ValueSource,
};
use spot_secrets::SecretBytes;

const DOCTYPE: &str = "bank_account";

fn tenant() -> TenantId {
    TenantId::new("acme").expect("tenant")
}

fn group(name: &str) -> GroupName {
    GroupName::new(name).expect("group")
}

// --------------------------------------------------------------------------- 1. class_of

#[test]
fn an_unmapped_group_fails_closed() {
    let mut table = SyncClassTable::new();
    table.set(group("hr"), SyncClass::Sealed);
    let mut tags = BTreeMap::new();
    tags.insert(group("notes"), Vec::<Tag>::new());
    let policy = SyncPolicy::new(table, tags);

    // A group the policy never heard of is an error, never `everywhere`.
    let error = policy
        .class_of(&group("bank_details"))
        .expect_err("unmapped");
    assert_eq!(error.code(), "sealed_fields.unknown_group");
    assert_eq!(
        policy
            .outgoing_filter(&group("bank_details"), Destination::Branch)
            .expect_err("unmapped")
            .code(),
        "sealed_fields.unknown_group"
    );
    assert_eq!(
        policy
            .read_at_branch(&group("bank_details"), Some(b"DE89".to_vec()))
            .expect_err("unmapped")
            .code(),
        "sealed_fields.unknown_group"
    );

    // A declared, untagged group keeps the platform default; a chosen class is honoured.
    assert_eq!(
        policy.class_of(&group("notes")).expect("declared"),
        SyncClass::Everywhere
    );
    assert_eq!(
        policy.class_of(&group("hr")).expect("chosen"),
        SyncClass::Sealed
    );
}

#[test]
fn an_empty_policy_answers_no_group() {
    let policy = SyncPolicy::untagged(SyncClassTable::new());
    for name in ["notes", "bank_details", "hr"] {
        assert_eq!(
            policy.class_of(&group(name)).expect_err(name).code(),
            "sealed_fields.unknown_group",
            "{name}"
        );
    }
}

#[test]
fn a_group_removed_after_fields_were_written_fails_closed() {
    // The group was `sealed` and had rows; the policy a later writer builds no longer lists it.
    let mut table = SyncClassTable::new();
    table.set(group("bank_details"), SyncClass::Sealed);
    let before = SyncPolicy::untagged(table.clone());
    assert_eq!(
        before.class_of(&group("bank_details")).expect("listed"),
        SyncClass::Sealed
    );

    table.remove(&group("bank_details"));
    let after = SyncPolicy::untagged(table);
    let error = after
        .class_of(&group("bank_details"))
        .expect_err("removed group");
    assert_eq!(error.code(), "sealed_fields.unknown_group");
    // And a batch filter built on it refuses rather than letting the field travel.
    assert!(
        after
            .outgoing_filter(&group("bank_details"), Destination::Branch)
            .is_err()
    );
}

// --------------------------------------------------------------------------- reveal fixtures

fn meta() -> Arc<spot_meta::EffectiveMeta> {
    let mut doctype = Definition::new(DOCTYPE);
    doctype.fields = vec![FieldSpec::new("iban", FieldType::Text)];
    doctype.protection_groups = vec![ProtectionGroupSpec::new("bank", "Bank")];
    Arc::new(Registry::build(vec![doctype], &spot_meta::Customisations::new()).expect("meta"))
}

fn model() -> Model {
    load(&ModelTexts {
        grants: r#"
[[entries]]
role = "Clerk"
doctype = "bank_account"
rights = ["read"]
[[entries]]
role = "MaskedReader"
doctype = "bank_account"
rights = ["read"]
"#
        .to_owned(),
        field_grants: r#"
[[entries]]
role = "Clerk"
doctype = "bank_account"
target = { group = "bank" }
access = ["read", "reveal"]
[[entries]]
role = "MaskedReader"
doctype = "bank_account"
target = { group = "bank" }
access = ["read_masked"]
"#
        .to_owned(),
        groups: "[[entries]]\nname = \"bank\"\n".to_owned(),
        shares: String::new(),
    })
    .expect("model")
}

/// An engine whose registry says `iban` is in the `bank` group, masked to its last four.
fn engine() -> Arc<PermissionEngine> {
    let snapshot = Snapshot::build(&meta(), &model())
        .expect("snapshot")
        .with_field_facts(
            DOCTYPE,
            vec![FieldFacts {
                path: "iban".to_owned(),
                groups: vec![PermGroup("bank".to_owned())],
                tags: Vec::new(),
                mask: Some(MaskFormat::LastChars(4)),
            }],
        )
        .expect("facts");
    Arc::new(PermissionEngine::new(Arc::new(snapshot)))
}

/// A hub store holding the listed documents; any other id is absent.
struct Source {
    docs: Vec<(&'static str, &'static [u8])>,
}

impl ValueSource for Source {
    fn load(&self, _doctype: &str, id: &str, _field: &str) -> Option<Loaded> {
        let (_, value) = self.docs.iter().find(|(doc, _)| *doc == id)?;
        Some(Loaded {
            value: SecretBytes::new(value.to_vec()),
            doc_facts: DocFacts {
                doctype: DOCTYPE.to_owned(),
                id: Some(id.to_owned()),
                created_by: Some("alice".to_owned()),
                ..DocFacts::default()
            },
        })
    }
}

fn service(docs: Vec<(&'static str, &'static [u8])>) -> (RevealService, Arc<MemoryAuditSink>) {
    let sink = Arc::new(MemoryAuditSink::new());
    let service = RevealService::new(engine(), Arc::new(Source { docs }), sink.clone());
    (service, sink)
}

fn clerk() -> Principal {
    Principal::with_roles("alice", &["Clerk"])
}

fn masked_reader() -> Principal {
    Principal::with_roles("bob", &["MaskedReader"])
}

// --------------------------------------------------------------------------- 2. facts

#[test]
fn reveal_takes_its_facts_from_the_registry() {
    let (service, sink) = service(vec![("BA-1", b"DE89")]);

    // The registry says `iban` is protected: a masked reader is refused. There is no facts
    // argument for a caller to forge.
    let error = service
        .reveal(&masked_reader(), DOCTYPE, "BA-1", "iban", 1)
        .expect_err("protected by the registry");
    assert_eq!(error.code(), "sealed_fields.reveal_denied");
    assert_eq!(
        sink.records()[0].payload.get("allowed"),
        Some(&PayloadValue::Bool(false))
    );

    // The clerk holds the right the registry's group asks for.
    let revealed = service
        .reveal(&clerk(), DOCTYPE, "BA-1", "iban", 2)
        .expect("clerk");
    assert_eq!(revealed.expose(), b"DE89");
}

#[test]
fn a_field_the_registry_does_not_know_is_refused() {
    let (service, sink) = service(vec![("BA-1", b"DE89")]);
    for (doctype, field) in [(DOCTYPE, "ghost"), ("ghost_doctype", "iban")] {
        let error = service
            .reveal(&clerk(), doctype, "BA-1", field, 1)
            .expect_err("unknown to the registry");
        assert_eq!(
            error.code(),
            "sealed_fields.reveal_denied",
            "{doctype}.{field}"
        );
    }
    assert_eq!(sink.len(), 2, "each refusal is audited");
    assert!(
        sink.records()
            .iter()
            .all(|record| record.payload.get("allowed") == Some(&PayloadValue::Bool(false)))
    );
}

#[test]
fn a_bulk_reveal_of_a_field_the_registry_does_not_know_is_refused() {
    let (service, sink) = service(vec![("BA-1", b"DE89")]);
    let ids = vec!["BA-1".to_owned()];
    let error = service
        .reveal_bulk(BulkRequest {
            principal: &Principal::system(),
            manifest: &ServiceManifest::new(vec![group("bank")]),
            group: &group("bank"),
            doctype: DOCTYPE,
            ids: &ids,
            field: "ghost",
            at: 1,
        })
        .expect_err("unknown field");
    assert_eq!(error.code(), "sealed_fields.reveal_denied");
    assert_eq!(sink.len(), 1);
}

#[test]
fn the_explanation_is_derived_not_supplied() {
    let (service, _) = service(vec![("BA-1", b"DE89")]);
    let text = service.explain(&masked_reader(), DOCTYPE, "BA-1", "iban");
    assert!(text.contains("bank_account.iban"));
    assert!(!text.contains("DE89"));
}

// --------------------------------------------------------------------------- 3. bulk audit

fn bulk<'a>(
    principal: &'a Principal,
    manifest: &'a ServiceManifest,
    group: &'a GroupName,
    ids: &'a [String],
    at: i64,
) -> BulkRequest<'a> {
    BulkRequest {
        principal,
        manifest,
        group,
        doctype: DOCTYPE,
        ids,
        field: "iban",
        at,
    }
}

fn text(record: &spot_audit_chain::AuditRecord, key: &str) -> String {
    match record.payload.get(key) {
        Some(PayloadValue::Text(text)) => text.clone(),
        other => panic!("payload `{key}` is {other:?}"),
    }
}

#[test]
fn a_bulk_reveal_audits_the_documents_the_field_and_the_requester() {
    let (service, sink) = service(vec![("BA-1", b"DE1"), ("BA-2", b"DE2"), ("BA-3", b"DE3")]);
    let ids = vec!["BA-1".to_owned(), "BA-2".to_owned(), "BA-3".to_owned()];
    let system = Principal::system();
    let manifest = ServiceManifest::new(vec![group("bank")]);
    let values = service
        .reveal_bulk(bulk(&system, &manifest, &group("bank"), &ids, 7))
        .expect("bulk");
    assert_eq!(values.len(), 3);

    assert_eq!(sink.len(), 1, "one record for the whole bulk");
    let record = &sink.records()[0];
    assert_eq!(record.kind, spot_sealed_fields::REVEAL_BULK_AUDIT_KIND);
    assert_eq!(record.at, 7);
    assert_eq!(record.actor, "system");
    // The documents: each id, length-prefixed so no id can forge another.
    assert_eq!(text(record, "documents"), "4:BA-1,4:BA-2,4:BA-3");
    assert_eq!(text(record, "doctype"), DOCTYPE);
    assert_eq!(text(record, "field"), "iban");
    assert_eq!(text(record, "group"), "bank");
    assert_eq!(text(record, "subject"), "system");
    assert_eq!(record.payload.get("count"), Some(&PayloadValue::Int(3)));
    assert_eq!(
        record.payload.get("allowed"),
        Some(&PayloadValue::Bool(true))
    );
    // The subject names the DocType and the documents.
    let subject = record.subject.as_ref().expect("subject");
    assert_eq!(subject.doctype, DOCTYPE);
    assert_eq!(subject.id, "4:BA-1,4:BA-2,4:BA-3");
    // And never a value.
    assert!(!record.payload.values().any(
        |value| matches!(value, PayloadValue::Text(t) if t.contains("DE1") || t.contains("DE2"))
    ));
}

#[test]
fn a_refused_bulk_is_audited_with_its_documents_and_reveals_none() {
    // BA-9 does not exist: the policy is all-or-nothing, so none of the others is revealed.
    let (service, sink) = service(vec![("BA-1", b"DE1"), ("BA-2", b"DE2")]);
    let ids = vec!["BA-1".to_owned(), "BA-9".to_owned(), "BA-2".to_owned()];
    let system = Principal::system();
    let manifest = ServiceManifest::new(vec![group("bank")]);
    let error = service
        .reveal_bulk(bulk(&system, &manifest, &group("bank"), &ids, 3))
        .expect_err("one document is refused");
    assert_eq!(error.code(), "sealed_fields.reveal_denied");

    assert_eq!(sink.len(), 1);
    let record = &sink.records()[0];
    assert_eq!(
        record.payload.get("allowed"),
        Some(&PayloadValue::Bool(false))
    );
    assert_eq!(text(record, "documents"), "4:BA-1,4:BA-9,4:BA-2");
    assert_eq!(record.payload.get("count"), Some(&PayloadValue::Int(3)));
}

#[test]
fn a_person_refused_in_bulk_is_audited_with_the_documents_and_who_asked() {
    let (service, sink) = service(vec![("BA-1", b"DE1"), ("BA-2", b"DE2")]);
    let ids = vec!["BA-1".to_owned(), "BA-2".to_owned()];
    let person = clerk();
    let manifest = ServiceManifest::new(vec![group("bank")]);
    let error = service
        .reveal_bulk(bulk(&person, &manifest, &group("bank"), &ids, 5))
        .expect_err("a person");
    assert_eq!(error.code(), "sealed_fields.reveal_bulk_denied");
    let record = &sink.records()[0];
    assert_eq!(record.actor, "user:alice");
    assert_eq!(text(record, "subject"), "user:alice");
    assert_eq!(text(record, "documents"), "4:BA-1,4:BA-2");
}

// --------------------------------------------------------------------------- 4. node write

fn node_store() -> NodeFieldStore {
    let hub = LocalHub::generate(tenant()).expect("hub");
    NodeFieldStore::new(
        spot_protected_keys::NodeId::new("branch-1").expect("node"),
        tenant(),
        DOCTYPE,
        hub.public_key(),
    )
}

fn write(
    store: &mut NodeFieldStore,
    field: &str,
    document: &str,
    ad: &SealedAd,
) -> Result<spot_sealed_fields::SealedField, spot_sealed_fields::SealedFieldsError> {
    let (g, key) = (group("bank_details"), store.hub_key());
    store.write(
        &SealTarget {
            group: &g,
            class: SyncClass::Sealed,
            field,
            document,
            hub_key: &key,
        },
        b"DE89",
        ad,
    )
}

#[test]
fn a_write_whose_ad_names_another_document_is_refused() {
    let mut store = node_store();
    let ad_for_a = SealedAd::of_field(tenant(), DOCTYPE, "iban", "BA-1");
    let error = write(&mut store, "iban", "BA-2", &ad_for_a).expect_err("document swapped");
    assert_eq!(error.code(), "sealed_fields.malformed");
    assert!(store.is_empty(), "nothing is stored on a refusal");
}

#[test]
fn a_write_whose_ad_names_another_field_is_refused() {
    let mut store = node_store();
    let ad_for_x = SealedAd::of_field(tenant(), DOCTYPE, "iban", "BA-1");
    let error = write(&mut store, "swift", "BA-1", &ad_for_x).expect_err("field swapped");
    assert_eq!(error.code(), "sealed_fields.malformed");
    assert!(store.is_empty());
}

#[test]
fn a_write_with_an_unbound_ad_is_refused() {
    let mut store = node_store();
    let error = write(&mut store, "iban", "BA-1", &SealedAd::unbound()).expect_err("unbound");
    assert_eq!(error.code(), "sealed_fields.malformed");
    assert!(store.is_empty());
}

#[test]
fn a_write_whose_ad_matches_is_stored() {
    let mut store = node_store();
    let ad = SealedAd::of_field(tenant(), DOCTYPE, "iban", "BA-1");
    write(&mut store, "iban", "BA-1", &ad).expect("matching ad");
    assert_eq!(store.len(), 1);
}

#[test]
fn a_write_whose_ad_names_another_tenant_is_refused() {
    let mut store = node_store();
    let ad = SealedAd::of_field(
        spot_protected_keys::TenantId::new("other").expect("tenant"),
        DOCTYPE,
        "iban",
        "BA-1",
    );
    let error = write(&mut store, "iban", "BA-1", &ad).expect_err("tenant swapped");
    assert_eq!(error.code(), "sealed_fields.malformed");
    assert!(store.is_empty());
}

#[test]
fn a_write_whose_ad_names_another_doctype_is_refused() {
    let mut store = node_store();
    let ad = SealedAd::of_field(tenant(), "Other DocType", "iban", "BA-1");
    let error = write(&mut store, "iban", "BA-1", &ad).expect_err("doctype swapped");
    assert_eq!(error.code(), "sealed_fields.malformed");
    assert!(store.is_empty());
}
