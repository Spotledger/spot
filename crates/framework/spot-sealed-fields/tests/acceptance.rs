#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
#![cfg(not(feature = "node"))]

//! The acceptance criteria of WP-SEC-10.T2, end to end.
//!
//! *A sealed field is unreadable on the node that wrote it; reveal works only online.* Each
//! test below is one sentence of the brief (deliverables 2 to 7) walked through the real
//! permission engine, the real key hierarchy and the real settings layer.

use std::collections::BTreeSet;
use std::sync::Arc;

use spot_audit_chain::PayloadValue;
use spot_meta::{Definition, FieldSpec, FieldType, ProtectionGroupSpec, Registry};
use spot_permissions::{
    DocFacts, FieldFacts, GroupName as PermGroup, MaskFormat, Model, ModelTexts, PermissionEngine,
    Principal, Snapshot, Tag, load,
};
use spot_protected_keys::{
    Duration, FieldAd, GroupName, Hub, KeyEpoch, MemoryRootKeyProvider, MonoTime, NodeId,
    NodeKeyStore, NodeWrapKey, TenantId, Tier, TierTable,
};
use spot_sealed_fields::{
    BranchValue, BulkRequest, ChannelGuard, Destination, Emission, EncryptedStore, HubPublicKey,
    Loaded, LocalHub, MemoryAuditSink, NodeFieldStore, NodeIdentity, OutgoingField, OutgoingValue,
    REVEAL_AUDIT_KIND, RevealService, SealTarget, SealedAd, SealingKey, ServiceManifest, SyncClass,
    SyncClassTable, ValueSource, builtin_registry, filter_outgoing_batch, read_at_branch,
};
use spot_secrets::SecretBytes;
use spot_settings::{Chain, MemoryStore, RegistryBuilder, ScopeRef};

const DOCTYPE: &str = "bank_account";
const TENANT: &str = "acme";

fn tenant() -> TenantId {
    TenantId::new(TENANT).expect("tenant")
}

fn group(name: &str) -> GroupName {
    GroupName::new(name).expect("group")
}

fn node(name: &str) -> NodeId {
    NodeId::new(name).expect("node")
}

fn tag(name: &str) -> Tag {
    Tag(name.to_owned())
}

// --------------------------------------------------------------------------- sealed fields

fn target<'a>(group: &'a GroupName, key: &'a HubPublicKey) -> SealTarget<'a> {
    SealTarget {
        group,
        class: SyncClass::Sealed,
        field: "iban",
        document: "BA-1",
        hub_key: key,
    }
}

#[test]
fn a_node_writes_a_sealed_field_and_cannot_read_it_back() {
    let hub = LocalHub::generate(tenant()).expect("hub");
    let mut node_store = NodeFieldStore::new(node("branch-1"), tenant(), DOCTYPE, hub.public_key());
    let ad = SealedAd::of_field(tenant(), DOCTYPE, "iban", "BA-1");

    let (g, key) = (group("bank_details"), hub.public_key());
    node_store
        .write(&target(&g, &key), b"DE89370400440532013000", &ad)
        .expect("write");

    // The value exists on the node and only as a blob.
    assert_eq!(node_store.len(), 1);
    assert!(node_store.blob("BA-1", "iban").is_some());

    // Reading it back is the specific refusal, not a missing field.
    let error = node_store
        .read("BA-1", "iban")
        .expect_err("a node cannot read it");
    assert_eq!(error.code(), "sealed_fields.sealed_unreadable");
    assert!(error.to_string().contains("iban"));
}

#[test]
fn the_hub_unseals_what_the_node_sealed() {
    let hub = LocalHub::generate(tenant()).expect("hub");
    let mut node_store = NodeFieldStore::new(node("branch-1"), tenant(), DOCTYPE, hub.public_key());
    let ad = SealedAd::of_field(tenant(), DOCTYPE, "iban", "BA-1");
    let (g, key) = (group("bank_details"), hub.public_key());
    node_store
        .write(&target(&g, &key), b"DE89", &ad)
        .expect("write");
    let blob = node_store.blob("BA-1", "iban").expect("blob").clone();

    let opened = hub.unseal(&blob, &ad).expect("hub unseals");
    assert_eq!(opened.expose(), b"DE89");

    // The blob is bound to its place: the hub refuses it elsewhere.
    let moved = SealedAd::of_field(tenant(), DOCTYPE, "iban", "BA-2");
    assert_eq!(
        hub.unseal(&blob, &moved).expect_err("moved").code(),
        "sealed_fields.tampered"
    );
}

#[test]
fn only_the_hub_public_key_is_needed_to_seal() {
    let hub = LocalHub::generate(tenant()).expect("hub");
    let public: HubPublicKey = hub.public_key();
    let writer = SealingKey::new(public);
    let sealed = spot_sealed_fields::seal(b"salary line", &writer.public_key()).expect("seal");
    let opened = hub.unseal(&sealed, &SealedAd::unbound()).expect("unseal");
    assert_eq!(opened.expose(), b"salary line");
}

// --------------------------------------------------------------------------- sync classes

#[test]
fn a_tagged_group_without_an_explicit_class_is_an_error() {
    let table = SyncClassTable::new();
    for name in ["secret", "pii", "financial"] {
        let error = table
            .class_for(&group("bank_details"), &[tag(name)])
            .expect_err("no default");
        assert_eq!(error.code(), "sealed_fields.class_unset", "{name}");
    }
    // An untagged group keeps the platform default.
    assert_eq!(
        table.class_for(&group("notes"), &[]).expect("default"),
        SyncClass::Everywhere
    );
}

#[test]
fn a_tenant_chooses_the_class_through_settings() {
    let registry = RegistryBuilder::new()
        .add_spec(SyncClassTable::setting_spec())
        .build()
        .expect("registry");
    let store = MemoryStore::new();
    let scope = ScopeRef::Tenant(TENANT.to_owned());
    let chain = Chain::new(vec![scope.clone()]).expect("chain");

    let mut table = SyncClassTable::new();
    table.set(group("bank_details"), SyncClass::Sealed);
    table.set(group("salary"), SyncClass::HomeOnly);
    table.save(&store, &scope).expect("save");

    let loaded = SyncClassTable::load(&store, &chain, &registry).expect("load");
    assert_eq!(
        loaded
            .class_for(&group("bank_details"), &[tag("secret")])
            .expect("chosen"),
        SyncClass::Sealed
    );
    assert_eq!(
        loaded
            .class_for(&group("salary"), &[tag("pii")])
            .expect("chosen"),
        SyncClass::HomeOnly
    );
    // A group nobody chose still has no default when tagged.
    assert_eq!(
        loaded
            .class_for(&group("card"), &[tag("financial")])
            .expect_err("unset")
            .code(),
        "sealed_fields.class_unset"
    );
}

// --------------------------------------------------------------------------- encrypted reads

/// A hub, a node and a live one-hour lease on `bank_details`.
fn leased_node(now: MonoTime) -> (NodeKeyStore, GroupName, KeyEpoch) {
    let mut hub = Hub::new(MemoryRootKeyProvider::new(tenant()).expect("root"));
    let g = group("bank_details");
    let epoch = hub.create_group(&g, MonoTime::ZERO).expect("group");
    let n = node("branch-1");
    let hub_copy = NodeWrapKey::generate(n.clone()).expect("wrap");
    let node_copy = hub_copy.duplicate().expect("duplicate");
    hub.register_node(&n, hub_copy).expect("register");
    let mut tiers = TierTable::new();
    tiers.grant(n.clone(), g.clone(), Tier::one());
    let release = hub.release(&n, &g, now, &tiers).expect("release");
    let mut store = NodeKeyStore::new(n, node_copy);
    store.accept(&release, now).expect("accept");
    (store, g, epoch)
}

#[test]
fn an_encrypted_read_fails_closed_after_the_lease() {
    let now = MonoTime::from_millis(1_000);
    let (mut keys, g, epoch) = leased_node(now);
    let ad = FieldAd::new(tenant(), DOCTYPE, "iban", "BA-1", epoch);
    let mut store = EncryptedStore::new(&mut keys);
    let field = store.write(&g, epoch, &ad, b"DE89", now).expect("write");

    // While the lease is live the node reads it.
    assert_eq!(
        store.read(&field, &ad, now).expect("live").expose(),
        b"DE89"
    );

    // One hour later (the tier-1 bound) it fails closed with the specific code.
    let after = now.plus(Duration::from_hours(1));
    let error = store.read(&field, &ad, after).expect_err("expired");
    assert_eq!(error.code(), "sealed_fields.lease_expired");
}

// --------------------------------------------------------------------------- home only

#[test]
fn home_only_never_appears_in_an_outgoing_batch() {
    let batch = vec![
        OutgoingField::plain("number", OutgoingValue::Plain(b"BA-1".to_vec())),
        OutgoingField::protected(
            "iban",
            group("bank_details"),
            SyncClass::HomeOnly,
            OutgoingValue::Plain(b"DE89".to_vec()),
        ),
        OutgoingField::protected(
            "salary",
            group("salary"),
            SyncClass::Encrypted,
            OutgoingValue::Encrypted(vec![1, 2, 3]),
        ),
        OutgoingField::protected(
            "memo",
            group("notes"),
            SyncClass::Everywhere,
            OutgoingValue::Plain(b"hi".to_vec()),
        ),
        OutgoingField::protected(
            "sealed_note",
            group("sealed"),
            SyncClass::Sealed,
            OutgoingValue::Sealed(vec![7, 7]),
        ),
    ];
    let filtered = filter_outgoing_batch(batch, Destination::Branch);
    let names: BTreeSet<&str> = filtered.iter().map(|f| f.field.as_str()).collect();
    assert!(
        !names.contains("iban"),
        "home_only leaked into a branch batch"
    );
    assert!(
        !names.contains("sealed_note"),
        "a sealed blob was sent to a branch"
    );
    assert_eq!(names, BTreeSet::from(["number", "salary", "memo"]));

    // At the hub the batch is unchanged.
    assert_eq!(filter_outgoing_batch(filtered, Destination::Hub).len(), 3);
}

#[test]
fn a_branch_shows_home_only_as_unavailable() {
    assert_eq!(
        read_at_branch(SyncClass::HomeOnly, Some(b"DE89".to_vec())),
        BranchValue::Unavailable
    );
}

// --------------------------------------------------------------------------- reveal

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

struct Source {
    value: Option<Vec<u8>>,
}

impl ValueSource for Source {
    fn load(&self, _doctype: &str, id: &str, _field: &str) -> Option<Loaded> {
        let value = self
            .value
            .as_ref()
            .map(|bytes| SecretBytes::new(bytes.clone()))?;
        Some(Loaded {
            value,
            doc_facts: DocFacts {
                doctype: DOCTYPE.to_owned(),
                id: Some(id.to_owned()),
                created_by: Some("alice".to_owned()),
                ..DocFacts::default()
            },
        })
    }
}

fn service(value: &[u8]) -> (RevealService, Arc<MemoryAuditSink>) {
    let sink = Arc::new(MemoryAuditSink::new());
    let service = RevealService::new(
        engine(),
        Arc::new(Source {
            value: Some(value.to_vec()),
        }),
        sink.clone(),
    );
    (service, sink)
}

fn clerk() -> Principal {
    Principal::with_roles("alice", &["Clerk"])
}

#[test]
fn every_reveal_writes_exactly_one_audit_entry() {
    let (service, sink) = service(b"DE89");
    for at in 0..5 {
        service
            .reveal(&clerk(), DOCTYPE, "BA-1", "iban", at)
            .expect("reveal");
    }
    let records = sink.records();
    assert_eq!(records.len(), 5, "one record per reveal");
    for (index, record) in records.iter().enumerate() {
        assert_eq!(record.kind, REVEAL_AUDIT_KIND);
        assert_eq!(record.actor, "user:alice");
        assert_eq!(record.at, index as i64);
        let subject = record.subject.as_ref().expect("subject");
        assert_eq!(subject.doctype, DOCTYPE);
        assert_eq!(subject.id, "BA-1");
        // The record names the field and the API, and never carries the value.
        assert_eq!(
            record.payload.get("field"),
            Some(&PayloadValue::Text("iban".to_owned()))
        );
        assert_eq!(
            record.payload.get("api"),
            Some(&PayloadValue::Text("api.reveal".to_owned()))
        );
        assert!(
            !record
                .payload
                .values()
                .any(|value| value == &PayloadValue::Text("DE89".to_owned())),
            "the audit record leaked the value"
        );
    }
}

#[test]
fn a_refused_reveal_is_audited_too() {
    let (service, sink) = service(b"DE89");
    let masked = Principal::with_roles("bob", &["MaskedReader"]);
    let error = service
        .reveal(&masked, DOCTYPE, "BA-1", "iban", 1)
        .expect_err("no reveal right");
    assert_eq!(error.code(), "sealed_fields.reveal_denied");
    assert_eq!(sink.len(), 1);
    assert_eq!(
        sink.records()[0].payload.get("allowed"),
        Some(&PayloadValue::Bool(false))
    );
}

#[test]
fn a_revoked_node_cannot_reveal() {
    let (service, sink) = service(b"DE89");
    let service = service.at_node(NodeIdentity::revoked(node("branch-1")));
    let error = service
        .reveal(&clerk(), DOCTYPE, "BA-1", "iban", 1)
        .expect_err("revoked");
    assert_eq!(error.code(), "sealed_fields.node_revoked");
    assert_eq!(sink.len(), 1, "the refusal is audited");
}

#[test]
fn reveal_is_online_only() {
    let error = spot_sealed_fields::offline_reveal().expect_err("offline");
    assert_eq!(error.code(), "sealed_fields.reveal_offline");
    assert_eq!(
        error.to_string(),
        "sealed_fields.reveal_offline: reveal needs a live connection to the hub"
    );

    // A branch with no live hub call has no local fallback, whatever the grants are.
    let caller = spot_sealed_fields::RemoteReveal::offline();
    assert!(!caller.is_online());
    let error = caller
        .reveal(&clerk(), DOCTYPE, "BA-1", "iban", 1)
        .expect_err("offline");
    assert_eq!(error.code(), "sealed_fields.reveal_offline");
    match error {
        spot_sealed_fields::RevealError::Offline { explanation_key } => {
            assert_eq!(explanation_key, spot_sealed_fields::OFFLINE_EXPLANATION)
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn an_online_caller_reveals_through_the_hub_port() {
    let (service, _sink) = service(b"DE89");
    let caller = spot_sealed_fields::RemoteReveal::online(Arc::new(service));
    assert!(caller.is_online());
    // The hub derives the field facts from its own snapshot; the call goes through the hub and
    // not a local read.
    let revealed = caller
        .reveal(&clerk(), DOCTYPE, "BA-1", "iban", 1)
        .expect("reveal");
    assert_eq!(revealed.expose(), b"DE89");
}

#[test]
fn a_bulk_reveal_needs_a_service_that_declares_the_group() {
    let (service, sink) = service(b"DE89");
    let ids = vec!["BA-1".to_owned()];

    // A person is refused.
    let error = service
        .reveal_bulk(BulkRequest {
            principal: &clerk(),
            manifest: &ServiceManifest::new(vec![group("bank")]),
            group: &group("bank"),
            doctype: DOCTYPE,
            ids: &ids,
            field: "iban",
            at: 1,
        })
        .expect_err("person");
    assert_eq!(error.code(), "sealed_fields.reveal_bulk_denied");

    // A service whose manifest omits the group is refused.
    let error = service
        .reveal_bulk(BulkRequest {
            principal: &Principal::system(),
            manifest: &ServiceManifest::new(vec![group("other")]),
            group: &group("bank"),
            doctype: DOCTYPE,
            ids: &ids,
            field: "iban",
            at: 2,
        })
        .expect_err("undeclared");
    assert_eq!(error.code(), "sealed_fields.group_undeclared");

    // A declared service reveals, and the record carries the count, not the documents.
    let values = service
        .reveal_bulk(BulkRequest {
            principal: &Principal::system(),
            manifest: &ServiceManifest::new(vec![group("bank")]),
            group: &group("bank"),
            doctype: DOCTYPE,
            ids: &ids,
            field: "iban",
            at: 3,
        })
        .expect("bulk");
    assert_eq!(values.len(), 1);
    assert_eq!(sink.len(), 3, "every attempt is audited");
    let last = sink.records().last().expect("record").clone();
    assert_eq!(last.kind, spot_sealed_fields::REVEAL_BULK_AUDIT_KIND);
    assert_eq!(last.payload.get("count"), Some(&PayloadValue::Int(1)));
}

// --------------------------------------------------------------------------- channels

#[test]
fn every_channel_adr_070_names_has_a_guard() {
    let registry = builtin_registry();
    for name in spot_sealed_fields::ADR_CHANNELS {
        assert!(
            registry.get(name).is_some(),
            "channel `{name}` has no guard registration"
        );
    }
    assert_eq!(registry.len(), spot_sealed_fields::ADR_CHANNELS.len());
}

#[test]
fn no_channel_emits_a_home_only_or_sealed_value_where_it_must_not() {
    let registry = builtin_registry();
    for name in registry.names() {
        let destination = registry.get(name).expect("registered").destination();
        let class = SyncClass::HomeOnly;
        let emission = registry.emission(name, class).expect("registered");
        match destination {
            Destination::Hub => assert_eq!(emission, Emission::Emit, "{name}"),
            _ => assert_eq!(emission, Emission::Drop, "{name}"),
        }
        if destination == Destination::Branch {
            assert_eq!(
                registry
                    .emission(name, SyncClass::Sealed)
                    .expect("registered"),
                Emission::Drop,
                "a sealed value reached a branch through {name}"
            );
        }
    }
    // The same answer from the bare guard.
    assert_eq!(
        ChannelGuard::decide(SyncClass::HomeOnly, Destination::Branch),
        Emission::Drop
    );
}
