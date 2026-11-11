//! Schema tests and compatibility-checker tests for the events contract.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::json;
use spot_storage_contract::{FeedPosition, Name, NodeRef, OutboxRecord, RecordId, Version};
use time::OffsetDateTime;
use time::macros::datetime;

use crate::compat::{Breaking, check};
use crate::envelope::EventEnvelope;
use crate::identity;
use crate::lifecycle::{self, Operation};
use crate::names::{CommandId, SourceId, Topic};
use crate::payload::{Actor, EventMeta, PayloadDocument, PayloadError};
use crate::schema::{EventSchema, FieldSchema, PayloadType};
use crate::system;

/// Test-only unwrapping that names the failure (the workspace denies `unwrap`).
trait Must<T> {
    fn must(self) -> T;
}

impl<T, E: std::fmt::Debug> Must<T> for Result<T, E> {
    fn must(self) -> T {
        self.unwrap_or_else(|e| panic!("unexpected error: {e:?}"))
    }
}

impl<T> Must<T> for Option<T> {
    fn must(self) -> T {
        self.unwrap_or_else(|| panic!("unexpected None"))
    }
}

trait MustErr<E> {
    fn must_err(self) -> E;
}

impl<T: std::fmt::Debug, E> MustErr<E> for Result<T, E> {
    fn must_err(self) -> E {
        match self {
            Ok(v) => panic!("expected an error, got {v:?}"),
            Err(e) => e,
        }
    }
}

fn id(n: u8) -> RecordId {
    // A fixed, valid UUIDv7: version nibble 7, variant 10.
    let mut b = [0u8; 16];
    b[0] = 0x01;
    b[6] = 0x70;
    b[8] = 0x80;
    b[15] = n;
    RecordId::from_bytes(b).must()
}

fn meta() -> EventMeta {
    EventMeta {
        occurred_at: datetime!(2026-09-29 15:00:00 +05:00),
        actor: Actor::User {
            id: id(1),
            via: None,
        },
        command_id: Some(CommandId::new("req-7f3a").must()),
        causation_id: None,
        correlation_id: id(9),
        trace_id: Some("4bf92f3577b34da6a3ce929d0e0e4736".into()),
    }
}

fn subject() -> NodeRef {
    NodeRef {
        entity: Name::new("sales_invoice").must(),
        id: id(42),
    }
}

fn schema(fields: &[(&str, FieldSchema)]) -> EventSchema {
    EventSchema {
        topic: Topic::app("invoice_overdue").must(),
        major: 1,
        revision: 0,
        fields: fields
            .iter()
            .map(|(n, f)| ((*n).to_owned(), f.clone()))
            .collect(),
        retired: BTreeSet::new(),
    }
}

// --- names ---------------------------------------------------------------------------------

#[test]
fn topics_are_storage_names_and_platform_prefixes_are_reserved() {
    assert!(Topic::app("stock_below_reorder_level").is_ok());
    assert!(Topic::app("doc_posted").is_err());
    assert!(Topic::app("sys_restart").is_err());
    assert!(Topic::any("doc_posted").must().is_platform());
    for bad in ["", "Posted", "posted-x", "9x", &"a".repeat(64)] {
        assert!(Topic::any(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn source_ids_are_a_site_with_an_optional_partition() {
    let s = SourceId::new("acme.spotledger.app#branch_khi_01").must();
    assert_eq!(
        (s.site(), s.partition()),
        ("acme.spotledger.app", Some("branch_khi_01"))
    );
    assert_eq!(SourceId::new("localhost").must().partition(), None);
    for bad in ["", "Acme", "acme#", "acme#Branch", "a..b", "acme#x#y"] {
        assert!(SourceId::new(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn command_ids_are_visible_ascii() {
    assert!(CommandId::new("a").is_ok());
    assert!(CommandId::new(&"x".repeat(128)).is_ok());
    for bad in ["", "has space", "tab\t", "é", &"x".repeat(129)] {
        assert!(CommandId::new(bad).is_err(), "{bad:?}");
    }
}

// --- payload document and envelope ---------------------------------------------------------

#[test]
fn payload_round_trips_through_the_storage_outbox_event() {
    let doc = PayloadDocument {
        meta: meta(),
        data: json!({"name": "SINV-2026-00042"}),
    };
    let topic = Operation::Post.topic();
    let ev = doc
        .to_outbox_event(id(7), &topic, subject(), Some(Version(3)), 1)
        .must();
    assert_eq!(ev.topic.as_str(), "doc_posted");
    let back = PayloadDocument::from_outbox_event(&ev).must();
    // Offsets are normalised to UTC.
    assert_eq!(back.meta.occurred_at, datetime!(2026-09-29 10:00:00 UTC));
    assert_eq!(back.meta.occurred_at.offset(), time::UtcOffset::UTC);
    assert_eq!(back.data, doc.data);
    assert!(
        ev.payload
            .contains("\"occurred_at\":\"2026-09-29T10:00:00Z\"")
    );
}

#[test]
fn payload_rules_are_checked() {
    let mut doc = PayloadDocument {
        meta: meta(),
        data: json!([1]),
    };
    let topic = Topic::app("x_happened").must();
    assert_eq!(
        doc.to_outbox_event(id(7), &topic, subject(), None, 1)
            .must_err(),
        PayloadError::DataNotObject
    );
    doc.data = json!({});
    doc.meta.trace_id = Some("XYZ".into());
    assert_eq!(doc.check().must_err(), PayloadError::InvalidTraceId);
    let mut ev = PayloadDocument {
        meta: meta(),
        data: json!({}),
    }
    .to_outbox_event(id(7), &topic, subject(), None, 1)
    .must();
    ev.payload = r#"{"meta": {}, "data": {}}"#.into();
    assert!(matches!(
        PayloadDocument::from_outbox_event(&ev),
        Err(PayloadError::Malformed(_))
    ));
    ev.payload = r#"{"meta": 1, "data": {}, "extra": true}"#.into();
    assert!(matches!(
        PayloadDocument::from_outbox_event(&ev),
        Err(PayloadError::Malformed(_))
    ));
}

#[test]
fn system_actors_and_causation_serialise() {
    let mut m = meta();
    m.actor = Actor::System {
        component: Name::new("scheduler").must(),
    };
    m.causation_id = Some(id(8));
    m.command_id = None;
    let text = serde_json::to_string(&m).must();
    assert!(
        text.contains(r#""actor":{"kind":"system","component":"scheduler"}"#),
        "{text}"
    );
    assert!(!text.contains("command_id"));
    let back: EventMeta = serde_json::from_str(&text).must();
    assert_eq!(back, m);
}

#[test]
fn envelope_is_built_from_a_record_and_has_fixed_field_names() {
    let doc = PayloadDocument {
        meta: meta(),
        data: json!({"name": "SINV-2026-00042"}),
    };
    let event = doc
        .to_outbox_event(
            id(7),
            &Operation::Post.topic(),
            subject(),
            Some(Version(3)),
            1,
        )
        .must();
    let record = OutboxRecord {
        position: FeedPosition::new(2, 1041),
        transaction: id(5),
        ordinal: 0,
        event,
    };
    let source = SourceId::new("acme.spotledger.app").must();
    let env = EventEnvelope::from_record(&source, &record).must();
    assert_eq!(env.feed_position(), FeedPosition::new(2, 1041));
    assert_eq!(env.subject_ref().must(), subject());
    assert_eq!(env.subject_version(), Some(Version(3)));

    let value: serde_json::Value = serde_json::from_str(&env.to_json().must()).must();
    let keys: BTreeSet<&str> = value
        .as_object()
        .must()
        .keys()
        .map(String::as_str)
        .collect();
    let expected: BTreeSet<&str> = [
        "event_id",
        "topic",
        "schema_version",
        "source_id",
        "position",
        "transaction",
        "ordinal",
        "subject",
        "subject_version",
        "meta",
        "data",
    ]
    .into();
    assert_eq!(keys, expected);
    assert_eq!(value["position"], json!({"generation": 2, "seq": 1041}));
    assert_eq!(value["subject"]["entity"], "sales_invoice");
    assert_eq!(value["event_id"], id(7).to_string());

    let back = EventEnvelope::from_json(&env.to_json().must()).must();
    assert_eq!(back, env);
}

#[test]
fn envelope_parsing_rejects_bad_input() {
    let good = {
        let doc = PayloadDocument {
            meta: meta(),
            data: json!({}),
        };
        let event = doc
            .to_outbox_event(
                id(7),
                &Operation::Create.topic(),
                subject(),
                Some(Version(1)),
                1,
            )
            .must();
        let record = OutboxRecord {
            position: FeedPosition::new(1, 1),
            transaction: id(5),
            ordinal: 0,
            event,
        };
        EventEnvelope::from_record(&SourceId::new("localhost").must(), &record)
            .must()
            .to_json()
            .must()
    };
    for (from, to) in [
        ("\"sales_invoice\"", "\"Sales Invoice\""),
        ("\"localhost\"", "\"Local Host\""),
        ("\"data\":{}", "\"data\":[]"),
        ("\"ordinal\":0", "\"ordinal\":0,\"unknown\":1"),
    ] {
        let bad = good.replacen(from, to, 1);
        assert_ne!(bad, good, "replacement {from} did not apply");
        assert!(EventEnvelope::from_json(&bad).is_err(), "accepted {bad}");
    }
    let tx = id(0xab).to_string(); // the transaction id has hex letters, so case matters
    let with_tx = good.replacen(&id(5).to_string(), &tx, 1);
    assert!(EventEnvelope::from_json(&with_tx).is_ok());
    let uppercase_id = with_tx.replacen(&tx, &tx.to_uppercase(), 1);
    assert_ne!(uppercase_id, with_tx);
    assert!(EventEnvelope::from_json(&uppercase_id).is_err());
}

// --- schema validation ---------------------------------------------------------------------

#[test]
fn values_follow_the_native_api_encoding() {
    let s = schema(&[
        ("flag", FieldSchema::optional(PayloadType::Bool)),
        ("count", FieldSchema::optional(PayloadType::Int)),
        ("amount", FieldSchema::optional(PayloadType::Decimal)),
        ("note", FieldSchema::optional(PayloadType::Text)),
        ("due", FieldSchema::optional(PayloadType::Date)),
        ("at", FieldSchema::optional(PayloadType::DateTime)),
        ("customer", FieldSchema::optional(PayloadType::Id)),
        ("extra", FieldSchema::optional(PayloadType::Json)),
    ]);
    let customer = id(0xcd).to_string(); // has hex letters, so case matters
    let good = json!({
        "flag": true, "count": -5, "amount": "-1250.50", "note": "", "due": "2026-10-31",
        "at": "2026-09-29T10:00:00.5Z", "customer": customer, "extra": {"any": [1, 2]}
    });
    assert_eq!(s.validate(&good), Ok(()));
    let bad_cases = [
        ("flag", json!("true")),
        ("count", json!(1.5)),
        ("amount", json!(1250.5)),
        ("amount", json!("1e3")),
        ("amount", json!("+1")),
        ("amount", json!("1.")),
        ("amount", json!(".5")),
        ("due", json!("2026-02-30")),
        ("due", json!("2026-9-1")),
        ("at", json!("2026-09-29T15:00:00+05:00")),
        ("customer", json!("not-a-uuid")),
        ("customer", json!(customer.to_uppercase())),
    ];
    for (field, value) in bad_cases {
        let mut data = good.clone();
        data[field] = value.clone();
        let errs = s.validate(&data).must_err();
        assert_eq!(errs.len(), 1, "{field}={value}: {errs:?}");
        assert_eq!(errs[0].path, field);
    }
}

#[test]
fn required_unknown_nested_and_enum_rules() {
    let line = BTreeMap::from([
        ("item".to_owned(), FieldSchema::required(PayloadType::Id)),
        (
            "qty".to_owned(),
            FieldSchema::required(PayloadType::Decimal),
        ),
    ]);
    let s = schema(&[
        (
            "status",
            FieldSchema::required(PayloadType::Enum {
                values: ["open".to_owned(), "closed".to_owned()].into(),
                open: false,
            }),
        ),
        (
            "lines",
            FieldSchema::required(PayloadType::List(Box::new(PayloadType::Object(line)))),
        ),
    ]);
    let item = id(4).to_string();
    assert_eq!(
        s.validate(&json!({"status": "open", "lines": [{"item": item, "qty": "2"}]})),
        Ok(())
    );

    let errs = s
        .validate(
            &json!({"status": "maybe", "lines": [{"item": item, "qty": 2}, {"qty": "1"}], "x": 1}),
        )
        .must_err();
    let paths: BTreeSet<&str> = errs.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(
        paths,
        ["status", "lines[0].qty", "lines[1].item", "x"].into(),
        "{errs:?}"
    );

    let errs = s.validate(&json!({"status": null})).must_err();
    assert_eq!(errs.len(), 2, "{errs:?}");
    assert!(s.validate(&json!([])).is_err());
}

#[test]
fn schema_definitions_are_checked() {
    let mut s = schema(&[("Bad Name", FieldSchema::optional(PayloadType::Text))]);
    assert!(s.check_definition().is_err());
    s = schema(&[(
        "kind",
        FieldSchema::optional(PayloadType::Enum {
            values: BTreeSet::new(),
            open: true,
        }),
    )]);
    assert!(s.check_definition().is_err());
    s = schema(&[("old", FieldSchema::optional(PayloadType::Text))]);
    s.retired.insert("old".into());
    assert!(s.check_definition().is_err());
    s = schema(&[]);
    s.major = 0;
    assert!(s.check_definition().is_err());
}

// --- compatibility -------------------------------------------------------------------------

fn base() -> EventSchema {
    schema(&[
        ("amount", FieldSchema::required(PayloadType::Decimal)),
        ("note", FieldSchema::optional(PayloadType::Text)),
        (
            "kind",
            FieldSchema::optional(PayloadType::Enum {
                values: ["a".to_owned()].into(),
                open: true,
            }),
        ),
        (
            "mode",
            FieldSchema::optional(PayloadType::Enum {
                values: ["x".to_owned()].into(),
                open: false,
            }),
        ),
    ])
}

fn revised(f: impl FnOnce(&mut EventSchema)) -> EventSchema {
    let mut s = base();
    s.revision += 1;
    f(&mut s);
    s
}

fn breaks(new: &EventSchema) -> Vec<Breaking> {
    check(&base(), new).err().unwrap_or_default()
}

#[test]
fn additive_changes_are_compatible() {
    assert_eq!(check(&base(), &base()), Ok(()));
    let ok = revised(|s| {
        s.fields
            .insert("due".into(), FieldSchema::optional(PayloadType::Date));
        s.fields.get_mut("note").must().deprecated = true;
        if let PayloadType::Enum { values, .. } = &mut s.fields.get_mut("kind").must().ty {
            values.insert("b".into());
        }
    });
    assert_eq!(check(&base(), &ok), Ok(()));
}

#[test]
fn every_breaking_change_within_a_major_is_detected() {
    let mut changed = base();
    changed
        .fields
        .insert("due".into(), FieldSchema::optional(PayloadType::Date));
    assert_eq!(breaks(&changed), vec![Breaking::RevisionNotIncreased]);

    assert_eq!(
        breaks(&revised(|s| {
            s.fields.remove("note");
        })),
        vec![Breaking::FieldRemoved("note".into())]
    );
    assert_eq!(
        breaks(&revised(
            |s| s.fields.get_mut("amount").must().ty = PayloadType::Int
        )),
        vec![Breaking::TypeChanged("amount".into())]
    );
    assert_eq!(
        breaks(&revised(|s| s.fields.get_mut("note").must().required = true)),
        vec![Breaking::RequiredChanged("note".into())]
    );
    assert_eq!(
        breaks(&revised(
            |s| s.fields.get_mut("amount").must().required = false
        )),
        vec![Breaking::RequiredChanged("amount".into())]
    );
    assert_eq!(
        breaks(&revised(|s| {
            s.fields
                .insert("due".into(), FieldSchema::required(PayloadType::Date));
        })),
        vec![Breaking::NewRequiredField("due".into())]
    );
    assert_eq!(
        breaks(&revised(|s| s.fields.get_mut("kind").must().ty =
            PayloadType::Enum {
                values: BTreeSet::new(),
                open: true
            })),
        vec![Breaking::EnumValueRemoved("kind".into(), "a".into())]
    );
    assert_eq!(
        breaks(&revised(|s| {
            if let PayloadType::Enum { values, .. } = &mut s.fields.get_mut("mode").must().ty {
                values.insert("y".into());
            }
        })),
        vec![Breaking::EnumClosedChanged("mode".into())]
    );
    let mut other = base();
    other.topic = Topic::app("something_else").must();
    assert_eq!(breaks(&other), vec![Breaking::TopicChanged]);
}

#[test]
fn nested_changes_are_checked() {
    let obj = |qty: PayloadType| {
        schema(&[(
            "line",
            FieldSchema::required(PayloadType::List(Box::new(PayloadType::Object(
                BTreeMap::from([("qty".to_owned(), FieldSchema::required(qty))]),
            )))),
        )])
    };
    let mut new = obj(PayloadType::Int);
    new.revision = 1;
    assert_eq!(
        check(&obj(PayloadType::Decimal), &new),
        Err(vec![Breaking::TypeChanged("line[].qty".into())])
    );
}

#[test]
fn major_bumps_allow_change_but_retire_removed_names() {
    let mut v2 = base();
    v2.major = 2;
    v2.revision = 0;
    v2.fields.remove("note");
    v2.fields.get_mut("amount").must().ty = PayloadType::Text;
    assert_eq!(
        breaks(&v2),
        vec![Breaking::RemovedNotRetired("note".into())]
    );
    v2.retired.insert("note".into());
    assert_eq!(check(&base(), &v2), Ok(()));

    let mut v3 = v2.clone();
    v3.major = 3;
    v3.retired.clear();
    v3.fields
        .insert("note".into(), FieldSchema::optional(PayloadType::Text));
    let found = check(&v2, &v3).must_err();
    assert!(
        found.contains(&Breaking::RetiredDropped("note".into())),
        "{found:?}"
    );
    assert!(
        found.contains(&Breaking::RetiredReused("note".into())),
        "{found:?}"
    );

    let mut down = base();
    down.major = 0;
    assert!(breaks(&down).contains(&Breaking::MajorDecreased));
}

// --- lifecycle topics ----------------------------------------------------------------------

#[test]
fn lifecycle_topics_are_valid_platform_topics_with_valid_schemas() {
    let all = lifecycle::schemas();
    assert_eq!(all.len(), 7);
    let names: BTreeSet<&str> = all.iter().map(|s| s.topic.as_str()).collect();
    assert_eq!(names.len(), 7);
    for s in &all {
        assert!(s.topic.is_platform(), "{}", s.topic);
        assert_eq!(s.major, 1);
        assert_eq!(s.check_definition(), Ok(()));
        assert_eq!(check(s, s), Ok(()));
    }
    let rename = Operation::Rename.schema();
    assert!(
        rename
            .validate(&json!({"old_name": "CUST-1", "new_name": "CUST-ACME"}))
            .is_ok()
    );
    assert!(rename.validate(&json!({"old_name": "CUST-1"})).is_err());
    let update = Operation::Update.schema();
    assert!(
        update
            .validate(&json!({"name": "SINV-1", "changed_fields": ["remarks", "items"]}))
            .is_ok()
    );
    assert!(update.validate(&json!({"name": "SINV-1"})).is_err());
    assert!(Operation::Post.schema().validate(&json!({})).is_ok());
}

#[test]
fn clock_type_is_the_storage_datetime_type() {
    // EventMeta uses the same instant type as the storage contract's Value::DateTime.
    let _: OffsetDateTime = meta().occurred_at;
}

// --- identity topics -----------------------------------------------------------------------

#[test]
fn identity_topics_are_reserved_platform_topics_with_valid_schemas() {
    let all = identity::schemas();
    assert_eq!(
        all.len(),
        identity::TOPICS.len() + identity::NODE_TOPICS.len()
    );
    let names: BTreeSet<&str> = all.iter().map(|s| s.topic.as_str()).collect();
    assert_eq!(names.len(), all.len(), "topic names are unique");
    for s in &all {
        assert!(s.topic.is_platform(), "{}", s.topic);
        assert!(s.topic.as_str().starts_with("identity_"));
        assert_eq!(s.major, 1);
        assert_eq!(s.check_definition(), Ok(()), "{}", s.topic);
        assert_eq!(check(s, s), Ok(()));
        // No schema may declare a field whose name is on the secret deny list.
        for field in s.fields.keys() {
            assert!(
                !identity::FORBIDDEN_KEY_PARTS
                    .iter()
                    .any(|p| field.contains(p)),
                "{} declares {field}",
                s.topic
            );
        }
    }
    assert!(identity::schema_of("doc_posted").is_none());
}

#[test]
fn an_app_cannot_use_the_identity_prefix() {
    assert!(Topic::app("identity_user_created").is_err());
    assert!(Topic::app("identity_custom").is_err());
    assert!(Topic::app("identity").is_ok());
    assert!(Topic::any("identity_user_created").is_ok());
}

#[test]
fn identity_payloads_validate_and_reject_what_is_wrong() {
    let user = id(7).to_string();
    let session = id(8).to_string();
    assert!(
        identity::validate(
            "identity_sign_in_succeeded",
            &json!({"user": user, "method": "oidc", "session": session, "step_up": false})
        )
        .is_ok()
    );
    // Missing required field, unknown field, wrong type.
    assert!(identity::validate("identity_sign_in_succeeded", &json!({"user": user})).is_err());
    assert!(
        identity::validate(
            "identity_user_enabled",
            &json!({"user": user, "unexpected": 1})
        )
        .is_err()
    );
    assert!(identity::validate("identity_user_enabled", &json!({"user": 5})).is_err());
    // A failed sign-in for an unknown name carries no user at all.
    assert!(
        identity::validate(
            "identity_sign_in_failed",
            &json!({"method": "password", "reason": "unknown_user"})
        )
        .is_ok()
    );
    // Not an identity topic.
    assert!(identity::validate("doc_posted", &json!({})).is_err());
    // A leaver event lists its actions.
    assert!(
        identity::validate(
            "identity_user_disabled",
            &json!({"user": user, "source": "scim", "actions": ["end_sessions", "disable_api_keys"]})
        )
        .is_ok()
    );
}

#[test]
fn identity_open_enums_gain_values_by_a_compatible_revision() {
    let user = id(7).to_string();
    let v1 = identity::schema_of("identity_sign_in_succeeded").must();
    let data = json!({"user": user, "method": "passkey", "step_up": true});
    // Producers validate strictly, so a value the schema does not list is refused...
    assert!(v1.validate(&data).is_err());
    // ...and a later revision that adds it to the open enum is a compatible change.
    let mut v2 = v1.clone();
    v2.revision += 1;
    if let Some(FieldSchema {
        ty: PayloadType::Enum { values, open: true },
        ..
    }) = v2.fields.get_mut("method")
    {
        values.insert("passkey".to_owned());
    } else {
        panic!("method must be an open enum");
    }
    assert_eq!(check(&v1, &v2), Ok(()));
    assert!(v2.validate(&data).is_ok());
}

#[test]
fn identity_events_never_carry_a_secret_at_any_depth() {
    let user = id(7).to_string();
    let key = id(9).to_string();
    // A top-level field named like a secret fails the strict schema check and the scan.
    assert!(
        identity::validate(
            "identity_api_key_created",
            &json!({"key": key, "owner": user, "scope": [], "api_key_secret": "x"})
        )
        .is_err()
    );
    // Hidden inside a free-form Json field, nested and inside an array.
    let nested = json!({
        "assignment": id(1).to_string(), "user": user, "role": "Accounting Manager",
        "source": "manual", "scope": {"company": ["A"], "extra": [{"Refresh_Token": "x"}]}
    });
    let Err(errors) = identity::validate("identity_assignment_created", &nested) else {
        panic!("a nested secret key must be refused");
    };
    assert!(
        errors
            .iter()
            .any(|e| e.path == "scope.extra[0].Refresh_Token"),
        "{errors:?}"
    );
    // Every deny-list entry is caught, in any case.
    for part in identity::FORBIDDEN_KEY_PARTS {
        let data = json!({
            "name": "Role", "change": "updated", "origin": "custom",
            "after": { part.to_ascii_uppercase(): 1 }
        });
        assert!(
            identity::validate("identity_role_changed", &data).is_err(),
            "{part}"
        );
    }
    // A harmless Json payload passes.
    let fine = json!({
        "name": "Accounting Manager", "change": "updated", "origin": "custom",
        "after": {"duties": ["Maintain bank transactions"]}
    });
    assert!(identity::validate("identity_role_changed", &fine).is_ok());
}

#[test]
fn the_field_access_trace_names_the_field_and_never_the_value() {
    let ok = json!({
        "user": id(7).to_string(), "doctype": "Employee", "group": "compensation",
        "field": "salary", "right": "reveal", "authorised": true, "channel": "api"
    });
    assert!(identity::validate("identity_field_accessed", &ok).is_ok());
    let mut with_value = ok.clone();
    with_value["value"] = json!("120000");
    assert!(identity::validate("identity_field_accessed", &with_value).is_err());
}

// --- the actor and impersonation -----------------------------------------------------------

#[test]
fn actor_user_carries_via_only_when_someone_acted_as_another() {
    let plain = Actor::User {
        id: id(1),
        via: None,
    };
    let text = serde_json::to_string(&plain).must();
    assert!(!text.contains("via"), "{text}");
    assert_eq!(serde_json::from_str::<Actor>(&text).must(), plain);

    let acted = Actor::User {
        id: id(1),
        via: Some(id(2)),
    };
    let text = serde_json::to_string(&acted).must();
    assert!(text.contains(&format!("\"via\":\"{}\"", id(2))), "{text}");
    assert_eq!(serde_json::from_str::<Actor>(&text).must(), acted);

    // A 0.1 payload without `via` still reads (additive change), and a malformed via fails.
    let old = format!(r#"{{"kind":"user","id":"{}"}}"#, id(1));
    assert_eq!(serde_json::from_str::<Actor>(&old).must(), plain);
    let bad = format!(r#"{{"kind":"user","id":"{}","via":"nope"}}"#, id(1));
    assert!(serde_json::from_str::<Actor>(&bad).is_err());
}

// --- identity: 0.3.0 --------------------------------------------------------------------------

fn walk_free_form(ty: &PayloadType, field: &str, topic: &str, found: &mut Vec<String>) {
    match ty {
        PayloadType::Text | PayloadType::Json => {
            if !identity::FREE_FORM_FIELDS.contains(&field) {
                found.push(format!("{topic}.{field}"));
            }
        }
        PayloadType::List(inner) => walk_free_form(inner, field, topic, found),
        PayloadType::Object(fields) => {
            for (k, f) in fields {
                walk_free_form(&f.ty, k, topic, found);
            }
        }
        _ => {}
    }
}

#[test]
fn identity_payloads_hold_only_typed_values_and_listed_free_form_fields() {
    // A new Text or Json field is a deliberate addition to FREE_FORM_FIELDS, never an accident.
    let mut found = Vec::new();
    for s in identity::schemas() {
        for (name, f) in &s.fields {
            walk_free_form(&f.ty, name, s.topic.as_str(), &mut found);
        }
    }
    assert!(found.is_empty(), "unlisted free-form fields: {found:?}");
    // Nothing on the list may itself look like a secret, and `key` fields are ids.
    for name in identity::FREE_FORM_FIELDS {
        assert!(
            !identity::FORBIDDEN_KEY_PARTS
                .iter()
                .any(|p| name.contains(p)),
            "{name}"
        );
    }
    for s in identity::schemas() {
        for (name, f) in &s.fields {
            if name == "key" || name.ends_with("_key") {
                assert_eq!(f.ty, PayloadType::Id, "{}.{name} must be a key id", s.topic);
            }
        }
    }
}

#[test]
fn identity_strings_are_bounded() {
    let long = "x".repeat(identity::MAX_STRING_CHARS + 1);
    let ok = "x".repeat(identity::MAX_STRING_CHARS);
    let user = "0192f1c0-0000-7000-8000-000000000001";
    let at = |reason: &str| json!({"user": user, "reason": reason});
    assert!(identity::validate("identity_user_enabled", &at(&ok)).is_ok());
    assert!(identity::validate("identity_user_enabled", &at(&long)).is_err());
    // Inside a free-form Json field too.
    let nested =
        json!({"name": "r", "change": "updated", "origin": "custom", "after": {"a": [long]}});
    assert!(identity::validate("identity_role_changed", &nested).is_err());
}

#[test]
fn every_identity_topic_has_a_valid_example_and_no_stray_files() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/identity");
    let mut files = BTreeSet::new();
    for entry in std::fs::read_dir(&dir).must() {
        files.insert(entry.must().file_name().to_string_lossy().into_owned());
    }
    let mut want = BTreeSet::new();
    for s in identity::schemas() {
        let name = format!("{}.json", s.topic);
        let text = std::fs::read_to_string(dir.join(&name)).must();
        let data: serde_json::Value = serde_json::from_str(&text).must();
        assert_eq!(
            identity::validate(s.topic.as_str(), &data),
            Ok(()),
            "{name}"
        );
        want.insert(name);
    }
    assert_eq!(files, want);
}

#[test]
#[allow(deprecated)]
fn the_0_3_0_topics_exist_and_every_identity_event_is_audit() {
    for t in [
        "identity_session_started",
        "identity_password_changed",
        "identity_second_factor_enrolled",
        "identity_second_factor_removed",
        "identity_recovery_codes_regenerated",
        "identity_sign_in_failed_burst",
        "identity_access_review_completed",
    ] {
        assert!(identity::TOPICS.contains(&t), "{t}");
    }
    const { assert!(identity::AT_LEAST_ONCE) };
    for s in identity::schemas() {
        assert!(identity::is_audit(s.topic.as_str()));
    }
    assert!(!identity::is_audit("doc_posted"));
    // A recovery-code set is reported as a count, never as codes.
    let user = "0192f1c0-0000-7000-8000-000000000001";
    assert!(
        identity::validate(
            "identity_recovery_codes_regenerated",
            &json!({"user": user, "count": 10, "by": "user"})
        )
        .is_ok()
    );
    assert!(
        identity::validate(
            "identity_recovery_codes_regenerated",
            &json!({"user": user, "count": 10, "by": "user", "recovery_code": "abcd"})
        )
        .is_err()
    );
}

// --- sys topics: 0.4.0 -------------------------------------------------------------------------

/// FNV-1a over the `Debug` text of a list of schemas: a stable fingerprint of their definitions.
fn fingerprint(schemas: &[EventSchema]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for s in schemas {
        for b in format!("{s:?}").bytes() {
            hash ^= u64::from(b);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    hash
}

#[test]
fn the_0_4_0_amendment_leaves_every_existing_schema_unchanged() {
    // The lifecycle (7) and identity (35) schemas as 0.3.0 published them. Adding a topic must
    // not move these; if this fails, an existing payload changed, which ADR-056 forbids.
    let mut existing = lifecycle::schemas();
    existing.extend(identity::schemas());
    assert_eq!(existing.len(), 7 + 35);
    assert_eq!(fingerprint(&existing), EXISTING_SCHEMAS_FINGERPRINT);
    for s in &existing {
        assert_eq!(check(s, s), Ok(()), "{}", s.topic);
    }
    // The new topics do not collide with an existing one.
    let old: BTreeSet<_> = existing.iter().map(|s| s.topic.clone()).collect();
    for s in system::schemas() {
        assert!(!old.contains(&s.topic), "{}", s.topic);
    }
}

/// A `FieldSchema` of a payload schema written out in a test.
fn fs(required: bool, ty: PayloadType) -> FieldSchema {
    if required {
        FieldSchema::required(ty)
    } else {
        FieldSchema::optional(ty)
    }
}

/// A nested object type, as `(name, required, type)` triples.
fn obj(fields: &[(&str, bool, PayloadType)]) -> PayloadType {
    PayloadType::Object(
        fields
            .iter()
            .map(|(n, required, ty)| ((*n).to_owned(), fs(*required, ty.clone())))
            .collect(),
    )
}

/// An enum of string values.
fn names(values: &[&str], open: bool) -> PayloadType {
    PayloadType::Enum {
        values: values.iter().map(|v| (*v).to_owned()).collect(),
        open,
    }
}

/// The `sys_doc_audit` v1 payload exactly as contract 0.5.0 published it: the shape this version
/// adds the seven optional schema 2 fields to (ADR-056 amendment A6).
///
/// It is written out rather than read from the registry, because it is the *subject* of the
/// proof: the live schema must differ from it only by those seven optional fields, and the 0.5.0
/// fingerprint proof keeps pinning the published shape after this version moved it.
fn doc_audit_v1_0_5_0() -> EventSchema {
    let mut f: BTreeMap<String, FieldSchema> = BTreeMap::new();
    let put = |f: &mut BTreeMap<String, FieldSchema>, n: &str, required: bool, ty: PayloadType| {
        f.insert(n.to_owned(), fs(required, ty));
    };
    put(&mut f, "site", true, PayloadType::Text);
    put(&mut f, "doctype", true, PayloadType::Text);
    put(&mut f, "id", true, PayloadType::Id);
    put(&mut f, "version", true, PayloadType::Int);
    put(&mut f, "operation", true, PayloadType::Json);
    put(
        &mut f,
        "actor",
        true,
        obj(&[
            ("actor", true, PayloadType::Text),
            ("on_behalf_of", false, PayloadType::Text),
            ("component", false, PayloadType::Text),
        ]),
    );
    put(&mut f, "at", true, PayloadType::Int);
    put(
        &mut f,
        "changed_fields",
        true,
        PayloadType::List(Box::new(PayloadType::Text)),
    );
    put(
        &mut f,
        "diff",
        true,
        PayloadType::List(Box::new(obj(&[
            ("path", true, PayloadType::Text),
            ("before", false, PayloadType::Json),
            ("after", false, PayloadType::Json),
        ]))),
    );
    put(&mut f, "hidden_changes", true, PayloadType::Bool);
    put(&mut f, "command_id", false, PayloadType::Text);
    put(&mut f, "source", true, names(&system::AUDIT_SOURCES, true));
    put(&mut f, "truncated", true, PayloadType::Bool);
    put(&mut f, "diff_digest", false, PayloadType::Text);
    EventSchema {
        topic: Topic::any(system::DOC_AUDIT_TOPIC).must(),
        major: 1,
        revision: 0,
        fields: f,
        retired: BTreeSet::new(),
    }
}

const EXISTING_SCHEMAS_FINGERPRINT: u64 = 7_148_073_286_602_576_770;

/// The lifecycle (7), identity (35) and `sys_*` (2) schemas as 0.4.0 published them.
const EXISTING_SCHEMAS_FINGERPRINT_0_5_0: u64 = 6_444_405_248_901_691_592;

#[test]
fn the_0_5_0_amendment_leaves_every_existing_schema_unchanged() {
    // Everything 0.4.0 published, with the document audit event as 0.5.0 published it (the shape
    // 0.7.0 amends, so it is written out here rather than read from the live registry). Adding
    // `sys_secret_audit` must not move any of it; if this fails, an existing payload changed,
    // which ADR-056 forbids.
    let mut existing = lifecycle::schemas();
    existing.extend(identity::schemas());
    existing.push(doc_audit_v1_0_5_0());
    existing.push(system::schema_of(system::GRAPH_EDGE_TOPIC).must());
    assert_eq!(existing.len(), 7 + 35 + 2);
    assert_eq!(fingerprint(&existing), EXISTING_SCHEMAS_FINGERPRINT_0_5_0);
    for s in &existing {
        assert_eq!(check(s, s), Ok(()), "{}", s.topic);
    }
    // The new topic is not one of them, and it is the only topic this version adds.
    let old: BTreeSet<_> = existing.iter().map(|s| s.topic.clone()).collect();
    let secret = system::schema_of(system::SECRET_AUDIT_TOPIC).must();
    assert!(!old.contains(&secret.topic), "{}", secret.topic);
    // The first three of `TOPICS` are the ones 0.5.0 knew; 0.7.2 appended nine after them.
    let now: BTreeSet<String> = system::TOPICS
        .iter()
        .take(3)
        .map(|t| (*t).to_owned())
        .collect();
    let before: BTreeSet<String> = [system::DOC_AUDIT_TOPIC, system::GRAPH_EDGE_TOPIC]
        .iter()
        .map(|t| (*t).to_owned())
        .collect();
    assert_eq!(now.difference(&before).count(), 1);
    assert!(
        now.difference(&before)
            .any(|t| t == system::SECRET_AUDIT_TOPIC)
    );
}

#[test]
fn sys_topics_are_reserved_platform_topics_with_valid_schemas() {
    let all = system::schemas();
    assert_eq!(all.len(), system::TOPICS.len());
    for s in &all {
        assert_eq!(s.major, 1);
        let want = if s.topic.as_str() == system::DOC_AUDIT_TOPIC {
            system::DOC_AUDIT_REVISION
        } else if s.topic.as_str() == system::SECRET_AUDIT_TOPIC {
            system::SECRET_AUDIT_REVISION
        } else {
            0
        };
        assert_eq!(s.revision, want, "{}", s.topic);
        assert!(s.topic.is_platform());
        assert!(s.topic.as_str().starts_with("sys_"));
        assert_eq!(s.check_definition(), Ok(()), "{}", s.topic);
        assert_eq!(check(s, s), Ok(()));
    }
    assert!(Topic::app(system::DOC_AUDIT_TOPIC).is_err());
    assert!(system::schema_of("doc_posted").is_none());
    assert!(system::validate("doc_posted", &json!({})).is_err());
}

#[test]
#[allow(deprecated)]
fn sys_delivery_and_retention() {
    const { assert!(system::AT_LEAST_ONCE) };
    assert_eq!(
        system::retention_of(system::DOC_AUDIT_TOPIC),
        Some(system::Retention::Audit)
    );
    assert_eq!(
        system::retention_of(system::GRAPH_EDGE_TOPIC),
        Some(system::Retention::Outbox)
    );
    assert!(system::is_audit(system::DOC_AUDIT_TOPIC));
    assert!(!system::is_audit(system::GRAPH_EDGE_TOPIC));
    assert_eq!(
        system::retention_of(system::SECRET_AUDIT_TOPIC),
        Some(system::Retention::Audit)
    );
    assert!(system::is_audit(system::SECRET_AUDIT_TOPIC));
    assert_eq!(system::retention_of("doc_posted"), None);
}

fn sys_walk(ty: &PayloadType, field: &str, topic: &str, found: &mut Vec<String>) {
    match ty {
        PayloadType::Text | PayloadType::Json => {
            if !system::FREE_FORM_FIELDS.contains(&field) {
                found.push(format!("{topic}.{field}"));
            }
        }
        PayloadType::List(inner) => sys_walk(inner, field, topic, found),
        PayloadType::Object(fields) => {
            for (k, f) in fields {
                sys_walk(&f.ty, k, topic, found);
            }
        }
        _ => {}
    }
}

#[test]
fn sys_payloads_hold_only_typed_values_and_listed_free_form_fields() {
    let mut found = Vec::new();
    for s in system::schemas() {
        for (name, f) in &s.fields {
            sys_walk(&f.ty, name, s.topic.as_str(), &mut found);
        }
    }
    assert!(found.is_empty(), "unlisted free-form fields: {found:?}");
    // No field name, at any depth, looks like a secret, and the list itself does not either.
    fn names(fields: &BTreeMap<String, FieldSchema>, out: &mut Vec<String>) {
        for (k, f) in fields {
            out.push(k.clone());
            match &f.ty {
                PayloadType::Object(inner) => names(inner, out),
                PayloadType::List(inner) => {
                    if let PayloadType::Object(o) = inner.as_ref() {
                        names(o, out);
                    }
                }
                _ => {}
            }
        }
    }
    let mut all = Vec::new();
    for s in system::schemas() {
        names(&s.fields, &mut all);
    }
    all.extend(system::FREE_FORM_FIELDS.iter().map(|n| (*n).to_owned()));
    for n in all {
        assert!(
            !identity::FORBIDDEN_KEY_PARTS.iter().any(|p| n.contains(p)),
            "{n}"
        );
    }
    // The only way to send an unbounded string is a listed field, and the validator bounds it.
    let long = "x".repeat(system::MAX_STRING_CHARS + 1);
    let mut edge = sys_example("sys_graph_edge");
    edge["edge_type"] = json!(long);
    assert!(system::validate("sys_graph_edge", &edge).is_err());
}

fn sys_example(name: &str) -> serde_json::Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("examples/sys")
        .join(format!("{name}.json"));
    serde_json::from_str(&std::fs::read_to_string(path).must()).must()
}

#[test]
fn every_sys_example_validates_and_every_topic_has_one() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/sys");
    let mut seen = BTreeSet::new();
    for entry in std::fs::read_dir(&dir).must() {
        let name = entry.must().file_name().to_string_lossy().into_owned();
        let stem = name.strip_suffix(".json").must_str(&name);
        let topic = stem.split('.').next().must();
        let data: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join(&name)).must()).must();
        assert_eq!(system::validate(topic, &data), Ok(()), "{name}");
        seen.insert(topic.to_owned());
    }
    let want: BTreeSet<String> = system::TOPICS.iter().map(|t| (*t).to_owned()).collect();
    assert_eq!(seen, want);
}

trait MustStr<T> {
    fn must_str(self, what: &str) -> T;
}

impl<T> MustStr<T> for Option<T> {
    fn must_str(self, what: &str) -> T {
        self.unwrap_or_else(|| panic!("unexpected None for {what}"))
    }
}

fn invalid(topic: &str, data: &serde_json::Value) -> bool {
    system::validate(topic, data).is_err()
}

#[test]
fn doc_audit_payload_rules() {
    let ok = sys_example("sys_doc_audit");
    assert_eq!(system::validate("sys_doc_audit", &ok), Ok(()));

    // Operations: a name, or exactly {"method": name}; nothing else.
    for bad in [
        json!("explode"),
        json!({"method": ""}),
        json!({"other": "x"}),
        json!(3),
    ] {
        let mut d = ok.clone();
        d["operation"] = bad;
        assert!(invalid("sys_doc_audit", &d));
    }
    // A diff value has one known tag and a typed payload.
    for bad in [
        json!({"float": 1.5}),
        json!({"int": 1.5}),
        json!({"text": 5}),
        json!({"digest": {"len": 1}}),
        json!({"digest": {"len": 1, "digest": "XYZ"}}),
        json!({"bool": true, "int": 1}),
        json!("something"),
    ] {
        let mut d = ok.clone();
        d["diff"][0]["after"] = bad;
        assert!(invalid("sys_doc_audit", &d));
    }
    // A text is bounded, longer content must be a digest.
    let mut d = ok.clone();
    d["diff"][0]["after"] = json!({"text": "y".repeat(system::MAX_DIFF_TEXT_CHARS)});
    assert_eq!(system::validate("sys_doc_audit", &d), Ok(()));
    d["diff"][0]["after"] = json!({"text": "y".repeat(system::MAX_DIFF_TEXT_CHARS + 1)});
    assert!(invalid("sys_doc_audit", &d));
    // Truncation and the digest go together; the digest is lower-case hex of 64 characters.
    let mut d = ok.clone();
    d["truncated"] = json!(true);
    assert!(invalid("sys_doc_audit", &d));
    let mut d = ok.clone();
    d["diff_digest"] = json!("a".repeat(64));
    assert!(invalid("sys_doc_audit", &d));
    let mut d = sys_example("sys_doc_audit.method_truncated");
    d["diff_digest"] = json!("A".repeat(64));
    assert!(invalid("sys_doc_audit", &d));
    // The diff is bounded.
    let mut d = ok.clone();
    let entry = d["diff"][0].clone();
    d["diff"] = json!(vec![entry; system::MAX_DIFF_CHANGES + 1]);
    assert!(invalid("sys_doc_audit", &d));
    // An unknown field, a missing required field and a secret-like key are all refused.
    let mut d = ok.clone();
    d["extra"] = json!(1);
    assert!(invalid("sys_doc_audit", &d));
    let mut d = ok.clone();
    d.as_object_mut().must().remove("version");
    assert!(invalid("sys_doc_audit", &d));
    let mut d = ok;
    d["diff"][0]["secret"] = json!("x");
    assert!(invalid("sys_doc_audit", &d));
}

#[test]
fn graph_edge_payload_rules() {
    let ok = sys_example("sys_graph_edge");
    assert_eq!(system::validate("sys_graph_edge", &ok), Ok(()));
    let with_props = sys_example("sys_graph_edge.relation_update");
    assert_eq!(system::validate("sys_graph_edge", &with_props), Ok(()));

    // The operation set is closed and the ends are typed records.
    let mut d = ok.clone();
    d["op"] = json!("replace");
    assert!(invalid("sys_graph_edge", &d));
    let mut d = ok.clone();
    d["from"]["id"] = json!("not-an-id");
    assert!(invalid("sys_graph_edge", &d));
    let mut d = ok.clone();
    d.as_object_mut().must().remove("to");
    assert!(invalid("sys_graph_edge", &d));
    // There is no sequence field: the sequence is the outbox position.
    let mut d = ok.clone();
    d["sequence"] = json!(5);
    assert!(invalid("sys_graph_edge", &d));

    // Properties are tagged typed values; no float, no untagged value, no secret-like name.
    for bad in [
        json!({"price": 1.5}),
        json!({"price": {"float": 1.5}}),
        json!({"price": {"decimal": 12.5}}),
        json!({"price": {"decimal": "12,5"}}),
        json!({"when": {"date": "15/01/2026"}}),
        json!({"at": {"datetime": "2026-01-15T10:00:00+05:00"}}),
        json!({"at": {"time": "25:00:00"}}),
        json!({"n": {"int": 1, "text": "a"}}),
        json!({"Bad": {"int": 1}}),
        json!({"api_token": {"text": "x"}}),
    ] {
        let mut d = with_props.clone();
        d["properties"] = bad;
        assert!(invalid("sys_graph_edge", &d));
    }
    // A remove carries no properties.
    let mut d = sys_example("sys_graph_edge.remove");
    d["properties"] = json!({"a": {"int": 1}});
    assert!(invalid("sys_graph_edge", &d));
    // Property count is bounded.
    let mut d = with_props;
    let many: serde_json::Map<String, serde_json::Value> = (0..=system::MAX_EDGE_PROPERTIES)
        .map(|i| (format!("p{i}"), json!({"int": i})))
        .collect();
    d["properties"] = serde_json::Value::Object(many);
    assert!(invalid("sys_graph_edge", &d));
}

/// The secret field audit event: the event names the call, never the secret (ADR-056, A4).
#[test]
fn secret_audit_payload_rules() {
    let ok = sys_example("sys_secret_audit");
    assert_eq!(system::validate("sys_secret_audit", &ok), Ok(()));
    let refused = sys_example("sys_secret_audit.refused");
    assert_eq!(system::validate("sys_secret_audit", &refused), Ok(()));
    let rewrap = sys_example("sys_secret_audit.rewrap");
    assert_eq!(system::validate("sys_secret_audit", &rewrap), Ok(()));

    // The operations are the engine's (`SecretOp::as_str`), a closed set. `create_with_secrets`
    // writes `set`; there is no `create` operation, and no `failed` outcome.
    for bad in [
        json!("create"),
        json!("set_secret"),
        json!("SET"),
        json!(""),
        json!(1),
    ] {
        let mut d = ok.clone();
        d["op"] = bad;
        assert!(invalid("sys_secret_audit", &d));
    }
    // The caller kinds, the outcomes and the recorded states are the engine's names too.
    let mut d = ok.clone();
    d["caller"] = json!("api");
    assert!(invalid("sys_secret_audit", &d));
    // The caller set is exactly the engine's five (0.7.5 added `webhooks` and `operator`); a
    // spelling or case variant, or a name the engine does not have, is refused.
    assert_eq!(
        system::SECRET_CALLERS,
        ["login_service", "seed", "webhooks", "operator", "test"]
    );
    for good in system::SECRET_CALLERS {
        let mut d = ok.clone();
        d["caller"] = json!(good);
        assert_eq!(system::validate("sys_secret_audit", &d), Ok(()), "{good}");
    }
    for bad in ["Webhooks", "OPERATOR", "webhook", "operators", "admin", ""] {
        let mut d = ok.clone();
        d["caller"] = json!(bad);
        assert!(invalid("sys_secret_audit", &d), "{bad}");
    }
    let mut d = ok.clone();
    d["outcome"] = json!("failed");
    assert!(invalid("sys_secret_audit", &d));
    for bad in [json!("set"), json!("<sealed>"), json!("")] {
        let mut d = ok.clone();
        d["state"] = bad;
        assert!(invalid("sys_secret_audit", &d));
    }
    // `record` is a record id when the call named one, and absent when it named none.
    let mut d = ok.clone();
    d["record"] = json!("not-a-record");
    assert!(invalid("sys_secret_audit", &d));
    assert_eq!(system::validate("sys_secret_audit", &refused), Ok(()));
    assert!(refused.get("record").is_none());

    // A refusal names the catalogue code it ended with; a success carries none.
    for bad in [
        json!("Not_A_Code"),
        json!("secretfield"),
        json!("secretfield.open failed"),
        json!("secretfield.open_failed: the value could not be opened, key 3f2a"),
        json!(&"a".repeat(system::MAX_NAME_CHARS + 1)),
    ] {
        let mut d = refused.clone();
        d["error"] = bad;
        assert!(invalid("sys_secret_audit", &d));
    }
    let mut d = refused.clone();
    d.as_object_mut().must().remove("error");
    assert!(invalid("sys_secret_audit", &d));
    let mut d = ok.clone();
    d["error"] = json!("secretfield.open_failed");
    assert!(invalid("sys_secret_audit", &d));
    // A code from another crate's catalogue is a code too.
    let mut d = refused.clone();
    d["error"] = json!("document.version_conflict");
    assert_eq!(system::validate("sys_secret_audit", &d), Ok(()));

    // No value, sealed blob, blind index or hex form can ride in `field` (or in `doctype`).
    for bad in [
        json!("4f8a1b2c3d4e5f60718293a4b5c6d7e8"),
        json!(&"0123456789abcdef".repeat(4)),
        json!("c2VjcmV0LXZhbHVl"),
        json!("password__sealed"),
        json!("token__bidx"),
        json!("API Key"),
        json!("two words"),
        json!("dG9rZW4="),
        json!(""),
        json!(1),
    ] {
        let mut d = ok.clone();
        d["field"] = bad;
        assert!(invalid("sys_secret_audit", &d), "field {}", d["field"]);
    }
    let mut d = ok.clone();
    d["doctype"] = json!("0192f1c0000070008000000000000042");
    assert!(invalid("sys_secret_audit", &d));
    // A long but name-shaped field is a name, and a rewrap row is "*".
    let mut d = ok.clone();
    d["field"] = json!(format!("field{}", "n".repeat(58)));
    assert_eq!(system::validate("sys_secret_audit", &d), Ok(()));
    assert_eq!(system::validate("sys_secret_audit", &rewrap), Ok(()));

    // The payload is closed: a value, a blob, an index or a key has no field to arrive in.
    for key in [
        "value",
        "sealed",
        "bidx",
        "iv",
        "key",
        "ciphertext",
        "secret",
    ] {
        let mut d = ok.clone();
        d[key] = json!("...");
        assert!(invalid("sys_secret_audit", &d), "{key}");
    }
    // A required field is required.
    for key in [
        "op", "doctype", "field", "caller", "site", "outcome", "state",
    ] {
        let mut d = ok.clone();
        d.as_object_mut().must().remove(key);
        assert!(invalid("sys_secret_audit", &d), "{key}");
    }
}

#[test]
fn sys_open_enums_gain_values_by_a_compatible_revision() {
    let v1 = system::schema_of("sys_doc_audit").must();
    let mut v1_1 = v1.clone();
    v1_1.revision = v1.revision + 1;
    if let Some(f) = v1_1.fields.get_mut("source") {
        f.ty = PayloadType::Enum {
            values: ["api", "job", "import", "sync", "script", "webhook"]
                .iter()
                .map(|v| (*v).to_owned())
                .collect(),
            open: true,
        };
    }
    assert_eq!(check(&v1, &v1_1), Ok(()));
    // The edge operation set is closed: a new value is breaking.
    let e1 = system::schema_of("sys_graph_edge").must();
    let mut e2 = e1.clone();
    e2.revision = 1;
    if let Some(f) = e2.fields.get_mut("op") {
        f.ty = PayloadType::Enum {
            values: ["add", "remove", "update", "move"]
                .iter()
                .map(|v| (*v).to_owned())
                .collect(),
            open: false,
        };
    }
    assert!(check(&e1, &e2).is_err());
}

// --- sys topics: 0.7.0 -------------------------------------------------------------------------

/// The `sys_doc_audit` v1 schema as 0.5.0 published it, by fingerprint. The written-out shape in
/// `doc_audit_v1_0_5_0` is only worth using as a proof if it is exactly what 0.5.0 published.
const DOC_AUDIT_V1_0_5_0_FINGERPRINT: u64 = 9_863_283_562_322_191_825;

/// The 44 schemas 0.5.0 published other than the one this amendment amends: the 7 lifecycle, the
/// 35 identity, `sys_graph_edge` and `sys_secret_audit`. The last is written out as 0.5.0
/// published it (`secret_audit_v1_0_5_0`), because 0.7.5 amends it.
const OTHER_SCHEMAS_FINGERPRINT_0_7_0: u64 = 11_344_726_056_239_191_768;

/// The `sys_doc_audit` v1 schema as 0.7.0 publishes it, by fingerprint.
const DOC_AUDIT_V1_0_7_0_FINGERPRINT: u64 = 8_760_171_842_247_622_023;

#[test]
fn the_0_7_0_amendment_adds_only_optional_fields_to_the_doc_audit() {
    let old = doc_audit_v1_0_5_0();
    assert_eq!(
        fingerprint(std::slice::from_ref(&old)),
        DOC_AUDIT_V1_0_5_0_FINGERPRINT
    );
    let new = system::schema_of(system::DOC_AUDIT_TOPIC).must();
    assert_eq!(
        fingerprint(std::slice::from_ref(&new)),
        DOC_AUDIT_V1_0_7_0_FINGERPRINT
    );

    // Same payload, one revision later: the compat checker is what says "additive".
    assert_eq!(
        new.major, old.major,
        "no major bump: the payload is still v1"
    );
    assert_eq!(new.revision, old.revision + 1);
    assert_eq!(check(&old, &new), Ok(()));

    // Exactly the seven schema 2 fields are new, and every one of them is optional, so a schema 1
    // payload (which has none of them) keeps validating.
    let added: BTreeSet<&str> = new
        .fields
        .keys()
        .filter(|k| !old.fields.contains_key(*k))
        .map(String::as_str)
        .collect();
    assert_eq!(
        added,
        [
            "correction_half",
            "error_class",
            "error_code",
            "method",
            "node",
            "outcome",
            "via",
        ]
        .into(),
        "the seven schema 2 fields, and nothing else"
    );
    for name in &added {
        assert!(!new.fields[*name].required, "{name} must be optional");
    }
    // Nothing 0.5.0 published moved: same fields, same types, same required-ness.
    assert_eq!(old.fields.len() + added.len(), new.fields.len());
    for (name, field) in &old.fields {
        assert_eq!(new.fields.get(name), Some(field), "{name} moved");
    }
    // The closures are closed sets of one or two values, and `outcome` has exactly `denied`.
    assert_eq!(
        new.fields["correction_half"].ty,
        names(&["cancel", "amend"], false)
    );
    assert_eq!(new.fields["via"].ty, names(&["step", "method"], false));
    assert_eq!(new.fields["outcome"].ty, names(&["denied"], false));
}

#[test]
fn the_0_7_0_amendment_leaves_the_other_0_5_0_schemas_unchanged() {
    let mut existing = lifecycle::schemas();
    existing.extend(identity::schemas());
    existing.push(system::schema_of(system::GRAPH_EDGE_TOPIC).must());
    existing.push(secret_audit_v1_0_5_0());
    assert_eq!(existing.len(), 7 + 35 + 2);
    assert_eq!(fingerprint(&existing), OTHER_SCHEMAS_FINGERPRINT_0_7_0);
    for s in &existing {
        assert_eq!(check(s, s), Ok(()), "{}", s.topic);
    }
    // The audit event is the only topic this version touches, and it is still v1.
    let now = system::schemas().len();
    assert_eq!(now, system::TOPICS.len());
    assert_eq!(system::schema_of(system::DOC_AUDIT_TOPIC).must().major, 1);
}

#[test]
fn schema_1_doc_audit_payloads_still_validate() {
    // The two payloads 0.4.0 published are schema 1: they carry none of the seven fields, they
    // are not rewritten, and they still validate against the v1 schema 0.7.0 publishes.
    for name in ["sys_doc_audit", "sys_doc_audit.method_truncated"] {
        let data = sys_example(name);
        for field in [
            "method",
            "correction_half",
            "via",
            "node",
            "outcome",
            "error_class",
            "error_code",
        ] {
            assert!(
                data.get(field).is_none(),
                "{name} must stay a schema 1 payload"
            );
        }
        assert_eq!(system::validate("sys_doc_audit", &data), Ok(()), "{name}");
    }
}

// --- the typed principal reference: 0.6.0 (ADR-083 section 2.2 item 4, ADR-056 amendment A5) ---

use crate::payload::{PrincipalKind, PrincipalRef};

#[test]
fn actor_consumer_is_a_wire_additive_variant_without_via() {
    let consumer = Actor::Consumer { id: id(3) };
    let text = serde_json::to_string(&consumer).must();
    assert_eq!(text, format!(r#"{{"kind":"consumer","id":"{}"}}"#, id(3)));
    assert_eq!(serde_json::from_str::<Actor>(&text).must(), consumer);
    // A consumer is never impersonated: `via` is not a field of this variant.
    let with_via = format!(
        r#"{{"kind":"consumer","id":"{}","via":"{}"}}"#,
        id(3),
        id(2)
    );
    assert!(serde_json::from_str::<Actor>(&with_via).is_err());
    // The id is a canonical record id, like a user's.
    let bad = r#"{"kind":"consumer","id":"nope"}"#;
    assert!(serde_json::from_str::<Actor>(bad).is_err());
    // The two 0.4.0 variants read and write exactly as before.
    let user = format!(r#"{{"kind":"user","id":"{}"}}"#, id(1));
    assert_eq!(
        serde_json::from_str::<Actor>(&user).must(),
        Actor::User {
            id: id(1),
            via: None
        }
    );
}

#[test]
fn a_principal_reference_is_typed_canonical_and_strict() {
    let user = PrincipalRef::user(id(1));
    let consumer = PrincipalRef::consumer(id(1));
    assert_eq!(user.to_string(), format!("user:{}", id(1)));
    assert_eq!(consumer.to_string(), format!("consumer:{}", id(1)));
    assert_ne!(
        user, consumer,
        "the same id under two kinds is two principals"
    );
    assert_eq!(user.to_string().parse::<PrincipalRef>().must(), user);
    assert_eq!(
        consumer.to_string().parse::<PrincipalRef>().must(),
        consumer
    );
    assert_eq!(consumer.kind(), PrincipalKind::Consumer);
    assert_eq!(consumer.id(), id(1));
    // No bare id, no unknown kind, no other case, no other id spelling, no padding.
    let bare = id(1).to_string();
    // An id with hex letters, so its upper-case spelling differs from the canonical one.
    let upper_id = id(0xab).to_string().to_uppercase();
    assert_ne!(upper_id, id(0xab).to_string());
    for bad in [
        bare.clone(),
        format!("agent:{bare}"),
        format!("Consumer:{bare}"),
        format!("consumer:{upper_id}"),
        format!("consumer:{{{bare}}}"),
        format!(" consumer:{bare}"),
        format!("consumer:{bare} "),
        "consumer:".to_owned(),
        "consumer:asha".to_owned(),
        "system".to_owned(),
        String::new(),
    ] {
        assert!(bad.parse::<PrincipalRef>().is_err(), "{bad:?}");
    }
}

#[test]
fn doc_audit_schema_2_payload_rules() {
    let ok = sys_example("sys_doc_audit");
    // The four examples this version adds: a method call, a correction half, a node-stamped
    // write and a denial.
    for name in [
        "sys_doc_audit.method_call",
        "sys_doc_audit.correction_half",
        "sys_doc_audit.node_stamped",
        "sys_doc_audit.denial",
    ] {
        assert_eq!(
            system::validate("sys_doc_audit", &sys_example(name)),
            Ok(()),
            "{name}"
        );
    }

    // `method`: 1 to 64 bytes of [a-z0-9_], and the operation names the same method.
    for bad in [
        json!(""),
        json!("ApplyDiscount"),
        json!("apply-discount"),
        json!("apply discount"),
        json!("apply_discount: no such field"),
        json!(&"a".repeat(system::MAX_METHOD_BYTES + 1)),
        json!(1),
    ] {
        let mut d = sys_example("sys_doc_audit.method_call");
        d["method"] = bad.clone();
        assert!(invalid("sys_doc_audit", &d), "method {bad}");
        // The operation disagrees with the field, so the write is refused even with a good name.
        d["operation"] = json!({"method": "something_else"});
        assert!(invalid("sys_doc_audit", &d), "method {bad}");
    }
    let mut d = sys_example("sys_doc_audit.method_call");
    d["operation"] = json!("update");
    assert!(
        invalid("sys_doc_audit", &d),
        "method requires operation method"
    );
    // The longest allowed name is a name.
    let mut d = sys_example("sys_doc_audit.method_call");
    let longest = "a".repeat(system::MAX_METHOD_BYTES);
    d["method"] = json!(longest);
    d["operation"] = json!({ "method": longest });
    assert_eq!(system::validate("sys_doc_audit", &d), Ok(()));

    // `correction_half`: two values, and only with operation `correct`.
    for bad in [json!("cancelled"), json!("Cancel"), json!(""), json!(1)] {
        let mut d = sys_example("sys_doc_audit.correction_half");
        d["correction_half"] = bad.clone();
        assert!(invalid("sys_doc_audit", &d), "correction_half {bad}");
    }
    let mut d = sys_example("sys_doc_audit.correction_half");
    d["operation"] = json!("cancel");
    assert!(invalid("sys_doc_audit", &d), "only with operation correct");
    let mut d = sys_example("sys_doc_audit.correction_half");
    d["correction_half"] = json!("amend");
    assert_eq!(system::validate("sys_doc_audit", &d), Ok(()));

    // `via`: a closed set of two. An open value would carry any string under 256 characters.
    for bad in [json!("api"), json!("step "), json!(""), json!(true)] {
        let mut d = sys_example("sys_doc_audit.node_stamped");
        d["via"] = bad.clone();
        assert!(invalid("sys_doc_audit", &d), "via {bad}");
    }
    let mut d = sys_example("sys_doc_audit.node_stamped");
    d["via"] = json!("method");
    assert_eq!(system::validate("sys_doc_audit", &d), Ok(()));

    // `node`: a deployment node name, not a free string.
    for bad in [
        json!("Branch-KHI-01"),
        json!("branch khi 01"),
        json!("branch-khi-01"),
        json!(&"n".repeat(system::MAX_NAME_CHARS + 1)),
        json!(""),
        json!(7),
    ] {
        let mut d = sys_example("sys_doc_audit.node_stamped");
        d["node"] = bad.clone();
        assert!(invalid("sys_doc_audit", &d), "node {bad}");
    }

    // `outcome` has one value, and it is not `ok`: a write that was made carries no outcome.
    for bad in [
        json!("ok"),
        json!("failed"),
        json!("DENIED"),
        json!(""),
        json!(true),
    ] {
        let mut d = sys_example("sys_doc_audit.denial");
        d["outcome"] = bad.clone();
        assert!(invalid("sys_doc_audit", &d), "outcome {bad}");
    }
    let mut d = ok.clone();
    d["outcome"] = json!("denied");
    assert!(invalid("sys_doc_audit", &d), "a write is not a denial");

    // A denial carries no diff, no changed field, no truncation, no digest and no message.
    let d0 = sys_example("sys_doc_audit.denial");
    let entry = ok["diff"][0].clone();
    for (field, value) in [
        ("diff", json!([entry.clone()])),
        ("changed_fields", json!(["status"])),
        ("truncated", json!(true)),
        ("diff_digest", json!("a".repeat(64))),
        ("message", json!("permission denied for user 7f3a")),
        ("reason", json!("no such document")),
        ("note", json!("refused")),
    ] {
        let mut d = d0.clone();
        d[field] = value.clone();
        assert!(invalid("sys_doc_audit", &d), "a denial carrying {field}");
    }
    // The empty list and `false` are what a denial writes, and they are accepted.
    let mut d = d0.clone();
    d["diff"] = json!([]);
    d["changed_fields"] = json!([]);
    d["truncated"] = json!(false);
    assert_eq!(system::validate("sys_doc_audit", &d), Ok(()));
    // A denial of an operation on a document the caller did not name: the producer writes the
    // unknown-record placeholder, version 0 and no command id, and all of that is valid.
    let mut d = d0.clone();
    d["id"] = json!("00000000-0000-7000-8000-000000000000");
    d["version"] = json!(0);
    d.as_object_mut().must().remove("command_id");
    assert_eq!(system::validate("sys_doc_audit", &d), Ok(()));

    // `error_class` and `error_code`: bounded catalogue codes, and only a denial has one.
    for field in ["error_class", "error_code"] {
        for bad in [
            json!("document.not_found: no such document"),
            json!("Document.NotFound"),
            json!("document..not_found"),
            json!(".document.not_found"),
            json!(&"a".repeat(system::MAX_NAME_CHARS + 1)),
            json!(""),
            json!(5),
        ] {
            let mut d = d0.clone();
            d[field] = bad.clone();
            assert!(invalid("sys_doc_audit", &d), "{field} {bad}");
        }
        let mut d = ok.clone();
        d[field] = json!("document.not_found");
        assert!(
            invalid("sys_doc_audit", &d),
            "{field} only a denial carries a code"
        );
    }
    // A two-, three- and four-segment catalogue code is a code, and so is the single-segment
    // `unknown` the producer substitutes for a code it cannot parse.
    for code in [
        "document.not_found",
        "secretfield.open_failed",
        "offline_identity.quarantine.tier1_after_revocation",
        "unknown",
    ] {
        let mut d = d0.clone();
        d["error_class"] = json!(code);
        d["error_code"] = json!(code);
        assert_eq!(system::validate("sys_doc_audit", &d), Ok(()), "{code}");
    }

    // The payload is still closed: the seven fields are the only additions, and anything else is
    // an unknown field whether the event is a write or a denial.
    for extra in ["extra", "diff_text", "fields", "denial_reason"] {
        let mut d = ok.clone();
        d[extra] = json!("x");
        assert!(invalid("sys_doc_audit", &d), "{extra}");
        let mut d = d0.clone();
        d[extra] = json!("x");
        assert!(invalid("sys_doc_audit", &d), "{extra} on a denial");
    }
}

#[test]
fn an_actor_names_its_principal_and_its_text() {
    let user = Actor::User {
        id: id(1),
        via: Some(id(2)),
    };
    let consumer = Actor::Consumer { id: id(1) };
    let system = Actor::System {
        component: Name::new("scheduler").must(),
    };
    assert_eq!(user.principal_ref(), Some(PrincipalRef::user(id(1))));
    assert_eq!(
        consumer.principal_ref(),
        Some(PrincipalRef::consumer(id(1)))
    );
    assert_eq!(system.principal_ref(), None);
    // The text an audit record and a request grant carry.
    assert_eq!(user.principal_text(), format!("user:{}", id(1)));
    assert_eq!(consumer.principal_text(), format!("consumer:{}", id(1)));
    assert_eq!(system.principal_text(), "system");
    // The text of two kinds never collides, so a grant for one never verifies for the other.
    assert_ne!(
        Actor::User {
            id: id(1),
            via: None
        }
        .principal_text(),
        consumer.principal_text()
    );
    // What a text names, without parsing the id: the shape check an authority runs.
    assert_eq!(
        PrincipalKind::of_text(&consumer.principal_text()),
        Some(PrincipalKind::Consumer)
    );
    assert_eq!(
        PrincipalKind::of_text("user:asha"),
        Some(PrincipalKind::User)
    );
    assert_eq!(PrincipalKind::of_text("asha"), None);
    assert_eq!(PrincipalKind::of_text("consumer:"), None);
    assert_eq!(PrincipalKind::of_text("consumer: x"), None);
}

#[test]
fn an_event_meta_with_a_consumer_actor_round_trips() {
    let mut m = meta();
    m.actor = Actor::Consumer { id: id(4) };
    let text = serde_json::to_string(&m).must();
    assert_eq!(serde_json::from_str::<EventMeta>(&text).must(), m);
}

#[test]
fn doc_audit_actor_text_is_a_typed_principal_reference() {
    // The consumer example validates, and so does a system actor.
    let consumer = sys_example("sys_doc_audit.consumer");
    assert_eq!(system::validate("sys_doc_audit", &consumer), Ok(()));
    let mut sys = consumer.clone();
    sys["actor"] = json!({"actor": "system", "component": "sync_apply", "on_behalf_of": null});
    assert_eq!(system::validate("sys_doc_audit", &sys), Ok(()));

    // A bare id, an unknown kind or a malformed id is refused.
    for bad in [
        "0192f1c0-0000-7000-8000-0000000000c1",
        "agent:0192f1c0-0000-7000-8000-0000000000c1",
        "consumer:asha",
        "consumer:",
        "Consumer:0192f1c0-0000-7000-8000-0000000000c1",
    ] {
        let mut d = consumer.clone();
        d["actor"]["actor"] = json!(bad);
        assert!(invalid("sys_doc_audit", &d), "{bad}");
    }
    // `on_behalf_of` is the real person behind a user: a user reference only. A consumer
    // acts as itself, never on behalf of anyone.
    let mut d = sys_example("sys_doc_audit");
    d["actor"]["on_behalf_of"] = json!("user:0192f1c0-0000-7000-8000-000000000003");
    assert_eq!(system::validate("sys_doc_audit", &d), Ok(()));
    d["actor"]["on_behalf_of"] = json!("consumer:0192f1c0-0000-7000-8000-000000000003");
    assert!(invalid("sys_doc_audit", &d));
    let mut d = consumer;
    d["actor"]["on_behalf_of"] = json!("user:0192f1c0-0000-7000-8000-000000000003");
    assert!(invalid("sys_doc_audit", &d));
}

// --- sys_secret_audit: 0.7.5 -------------------------------------------------------------------

/// The `sys_secret_audit` v1 schema as 0.5.0 published it (three callers, revision 0). Written
/// out rather than read from the registry, because it is the subject of the 0.7.5 proof.
fn secret_audit_v1_0_5_0() -> EventSchema {
    let mut s = system::schema_of(system::SECRET_AUDIT_TOPIC).must();
    s.revision = 0;
    if let Some(f) = s.fields.get_mut("caller") {
        f.ty = names(&["login_service", "seed", "test"], false);
    }
    s
}

#[test]
fn the_0_7_5_amendment_adds_only_two_callers_to_the_secret_audit() {
    let old = secret_audit_v1_0_5_0();
    let new = system::schema_of(system::SECRET_AUDIT_TOPIC).must();
    assert_eq!(
        new.major, old.major,
        "no major bump: the payload is still v1"
    );
    assert_eq!(new.revision, old.revision + 1);
    // Nothing but the `caller` set differs: same fields, same types, same required-ness.
    assert_eq!(old.fields.len(), new.fields.len());
    for (name, field) in &old.fields {
        if name != "caller" {
            assert_eq!(new.fields.get(name), Some(field), "{name} moved");
        }
    }
    assert_eq!(
        new.fields["caller"].ty,
        names(
            &["login_service", "seed", "webhooks", "operator", "test"],
            false
        )
    );
    assert!(new.fields["caller"].required);
    // The checker's own rule is that a closed enum gaining a value is breaking
    // (`EnumClosedChanged`). That is the one finding, it is the deliberate exception this
    // amendment asks the architecture owner to approve (ADR-056 A7), and nothing else is reported.
    assert_eq!(
        check(&old, &new),
        Err(vec![Breaking::EnumClosedChanged("caller".to_owned())])
    );
    // The new schema accepts everything the old one did (it only widens), and every 0.5.0 example
    // still validates.
    for name in [
        "sys_secret_audit",
        "sys_secret_audit.refused",
        "sys_secret_audit.rewrap",
    ] {
        assert_eq!(
            system::validate("sys_secret_audit", &sys_example(name)),
            Ok(()),
            "{name}"
        );
    }
}
