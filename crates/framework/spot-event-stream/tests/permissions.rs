//! The permission filter: the delivered set equals a per-document `can(read)` oracle, and a
//! `fields` consumer never sees a protected value (the field-guard leak harness over the
//! serialised frames).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use spot_core::ManualClock;
use spot_event_stream::{
    BatchPolicy, ConsumerSpec, ParkPolicy, PayloadMode, PermissionVisibility, PrincipalSource,
    Start, SubjectFacts, TopicSelector, Visibility,
};
use spot_field_guard::leakcheck::{Corpus, Output, assert_no_leak};
use spot_field_guard::records::{AuditRecord, BoundedQueue, FieldAccessRecord};
use spot_field_guard::source::DocProtection;
use spot_field_guard::{Channel, FieldGuard, StaticProtection};
use spot_meta::{Definition, FieldSpec, FieldType, ProtectionGroupSpec, Registry};
use spot_permissions::{
    Action, DocFacts, FieldFacts, FieldGrant, FieldRight, FieldTarget, GroupName, Model,
    PermissionEngine, Principal, ProtectionGroup, Right, Rights, Role, RoleGrant, Snapshot,
};
use spot_storage_contract::RecordId;
use support::{doc_record, fixture_full, rid};

const BIG: BatchPolicy = BatchPolicy {
    max_events: 1_000,
    max_bytes: 64 * 1024 * 1024,
    window: 1_000,
    page_limit: 1_000,
    max_wait: std::time::Duration::from_secs(25),
};

fn definition() -> Definition {
    let mut d = Definition::new("task");
    let mut iban = FieldSpec::new("iban", FieldType::Text);
    iban.protection_group = Some("financial".into());
    d.fields = vec![FieldSpec::new("status", FieldType::Text), iban];
    d.protection_groups = vec![ProtectionGroupSpec::new("financial", "Financial")];
    d
}

fn engine() -> PermissionEngine {
    let meta = Registry::new(vec![definition()]).unwrap().effective();
    let model = Model {
        grants: vec![
            RoleGrant {
                doctype: "task".into(),
                role: Role("Reader".into()),
                rights: Rights::new([Right::Read]),
                if_owner: false,
            },
            RoleGrant {
                doctype: "task".into(),
                role: Role("Owner".into()),
                rights: Rights::new([Right::Read]),
                if_owner: true,
            },
            RoleGrant {
                doctype: "task".into(),
                role: Role("FieldReader".into()),
                rights: Rights::new([Right::Read, Right::Report]),
                if_owner: false,
            },
            RoleGrant {
                doctype: "task".into(),
                role: Role("Reporter".into()),
                rights: Rights::new([Right::Read, Right::Report]),
                if_owner: false,
            },
        ],
        field_grants: vec![FieldGrant {
            doctype: "task".into(),
            role: Role("FieldReader".into()),
            target: FieldTarget::Group(GroupName("financial".into())),
            access: BTreeSet::from([FieldRight::Read, FieldRight::Report]),
            deny: false,
            condition: None,
        }],
        groups: vec![ProtectionGroup {
            name: GroupName("financial".into()),
            trace: false,
        }],
        shares: vec![],
    };
    let snapshot = Snapshot::build(&meta, &model)
        .unwrap()
        .with_field_facts(
            "task",
            vec![
                FieldFacts::plain("status"),
                FieldFacts {
                    path: "iban".into(),
                    groups: vec![GroupName("financial".into())],
                    ..FieldFacts::default()
                },
            ],
        )
        .unwrap();
    PermissionEngine::new(Arc::new(snapshot))
}

fn guard(engine: &PermissionEngine) -> FieldGuard {
    let protection = StaticProtection::new(vec![DocProtection {
        name: "task".into(),
        fields: vec![FieldFacts {
            path: "iban".into(),
            groups: vec![GroupName("financial".into())],
            ..FieldFacts::default()
        }],
        ..DocProtection::default()
    }]);
    let clock = Arc::new(ManualClock::new(time::OffsetDateTime::UNIX_EPOCH));
    FieldGuard::new(
        Arc::new(engine.clone()),
        Arc::new(protection),
        Arc::new(BoundedQueue::<FieldAccessRecord>::new(usize::MAX)),
        Arc::new(BoundedQueue::<AuditRecord>::new(usize::MAX)),
        clock,
    )
}

fn facts_for(id: RecordId, creator: &str) -> DocFacts {
    DocFacts {
        doctype: "task".into(),
        id: Some(id.to_string()),
        created_by: Some(creator.to_owned()),
        state: None,
        values: BTreeMap::new(),
    }
}

fn visibility(
    engine: &PermissionEngine,
    principals: Vec<(&str, Principal)>,
    docs: &[(RecordId, &str)],
) -> Arc<dyn Visibility> {
    let mut facts = spot_event_stream::MapSubjectFacts::new();
    for (id, creator) in docs {
        facts = facts.with("task", id, facts_for(*id, creator));
    }
    let mut principals_map = spot_event_stream::MapPrincipals::new();
    for (name, principal) in principals {
        principals_map = principals_map.with(name, principal);
    }
    let facts: Arc<dyn SubjectFacts> = Arc::new(facts);
    let principals: Arc<dyn PrincipalSource> = Arc::new(principals_map);
    Arc::new(PermissionVisibility::new(
        Arc::new(engine.clone()),
        guard(engine),
        facts,
        principals,
    ))
}

fn ids_only_spec(name: &str, owner: &str) -> ConsumerSpec {
    ConsumerSpec {
        name: name.to_owned(),
        owner: owner.to_owned(),
        topics: vec![TopicSelector::Prefix("doc".to_owned())],
        filter: None,
        partitions: 1,
        payload_mode: PayloadMode::IdsOnly,
        start: Start::FromBeginning,
    }
}

#[tokio::test]
async fn the_delivered_set_equals_a_can_read_oracle() {
    let engine = engine();
    // Six documents, each created by a different user.
    let docs: Vec<(RecordId, &str)> = vec![
        (rid(1), "u_owner"),
        (rid(2), "u_all"),
        (rid(3), "u_other"),
        (rid(4), "u_admin"),
        (rid(5), "someone"),
        (rid(6), "u_none"),
    ];
    let principals: Vec<(&str, Principal)> = vec![
        ("u_all", Principal::with_roles("u_all", &["Reader"])),
        ("u_owner", Principal::with_roles("u_owner", &["Owner"])),
        ("u_none", Principal::with_roles("u_none", &[])),
        (
            "u_admin",
            Principal::with_roles("u_admin", &["Reader", "Owner"]),
        ),
        ("u_other", Principal::with_roles("u_other", &["Owner"])),
        ("system", Principal::system()),
    ];
    let vis = visibility(&engine, principals.clone(), &docs);
    let f = fixture_full(
        vis,
        Arc::new(spot_event_stream::AllowAllStreamRights),
        BIG,
        ParkPolicy::default(),
    )
    .await;
    for (seq, (id, _)) in docs.iter().enumerate() {
        f.feed.push(doc_record(0, seq as u64 + 1, *id));
    }

    for (owner, principal) in &principals {
        let name = format!("consumer_{owner}");
        f.engine
            .register(ids_only_spec(&name, owner), &Principal::system())
            .await
            .unwrap();
        let lease = f.engine.acquire(&name).await.unwrap();
        let batch = f.engine.pull(&lease, 0, None, false).await.unwrap();
        let delivered: BTreeSet<RecordId> = batch.events.iter().map(|e| e.subject.id).collect();

        let expected: BTreeSet<RecordId> = docs
            .iter()
            .filter(|(id, creator)| {
                engine
                    .can(principal, Action::Read, &facts_for(*id, creator))
                    .allowed
            })
            .map(|(id, _)| *id)
            .collect();
        assert_eq!(
            delivered, expected,
            "the delivered set for {owner} does not equal the oracle"
        );
    }
}

#[tokio::test]
async fn a_fields_consumer_never_sees_a_protected_value() {
    let engine = engine();
    let secret = "GB82WEST12345698765432";
    let docs: Vec<(RecordId, &str)> = vec![(rid(1), "u_reader")];
    let principals: Vec<(&str, Principal)> = vec![
        ("u_reader", Principal::with_roles("u_reader", &["Reporter"])),
        (
            "u_field",
            Principal::with_roles("u_field", &["Reporter", "FieldReader"]),
        ),
    ];
    let vis = visibility(&engine, principals, &docs);
    let f = fixture_full(
        vis,
        Arc::new(spot_event_stream::AllowAllStreamRights),
        BIG,
        ParkPolicy::default(),
    )
    .await;
    let record = support::record(
        0,
        1,
        "doc_created",
        "task",
        rid(1),
        1,
        serde_json::json!({"status": "Open", "iban": secret}),
    );
    f.feed.push(record);

    // The reader may read the document but not the protected field.
    let mut spec = ids_only_spec("reader_fields", "u_reader");
    spec.payload_mode = PayloadMode::Fields;
    f.engine.register(spec, &Principal::system()).await.unwrap();
    let lease = f.engine.acquire("reader_fields").await.unwrap();
    let batch = f.engine.pull(&lease, 0, None, false).await.unwrap();
    assert_eq!(batch.events.len(), 1);
    let data = batch.events[0].data.as_object().unwrap();
    assert_eq!(data.get("status").unwrap(), "Open");
    assert!(
        !data.contains_key("iban"),
        "a protected field reached the reader"
    );

    // The whole serialised frame carries no secret, in any trivial encoding.
    let frames: Vec<Output> = batch
        .events
        .iter()
        .map(|e| Output::Bytes(spot_event_stream::sse::frame("reader_fields", e)))
        .collect();
    assert_no_leak(&Corpus::standard().only(&["iban"]), &frames);

    // A principal with the field grant does see the value.
    let mut spec = ids_only_spec("field_reader", "u_field");
    spec.payload_mode = PayloadMode::Fields;
    f.engine.register(spec, &Principal::system()).await.unwrap();
    let lease = f.engine.acquire("field_reader").await.unwrap();
    let batch = f.engine.pull(&lease, 0, None, false).await.unwrap();
    assert_eq!(
        batch.events[0]
            .data
            .as_object()
            .unwrap()
            .get("iban")
            .unwrap(),
        secret
    );
}

#[tokio::test]
async fn a_skipped_event_names_no_subject() {
    let engine = engine();
    let docs: Vec<(RecordId, &str)> = vec![(rid(1), "u_none")];
    let principals: Vec<(&str, Principal)> = vec![("u_none", Principal::with_roles("u_none", &[]))];
    let vis = visibility(&engine, principals, &docs);
    let f = fixture_full(
        vis,
        Arc::new(spot_event_stream::AllowAllStreamRights),
        BIG,
        ParkPolicy::default(),
    )
    .await;
    f.feed.push(doc_record(0, 1, rid(1)));
    f.engine
        .register(ids_only_spec("denied", "u_none"), &Principal::system())
        .await
        .unwrap();
    let lease = f.engine.acquire("denied").await.unwrap();
    let batch = f.engine.pull(&lease, 0, None, false).await.unwrap();
    assert!(batch.events.is_empty());
    assert_eq!(batch.skipped, 1);
    assert!(batch.next_position.is_none());
}

/// The channel choice is stated: a data subscription is guarded as `Channel::Export`.
#[test]
fn the_stream_channel_is_export() {
    assert_eq!(Channel::Export.as_str(), "export");
    assert!(Channel::Export.carries_values());
}
