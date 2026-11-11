//! The policy redactor: excluded things never leave, no policy means nothing leaves.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};

use proptest::prelude::*;
use serde_json::{Map, Value, json};
use spot_app_api::{ChildRow, Document};
use spot_storage_contract::{Name, RecordId, Value as SValue, Version};
use spot_sync_closure::{ClosureError, ImageSource, RecordKey, Redactor, References, SourceImage};
use spot_sync_hub::{
    Destination, EntryRole, HubRelay, MemoryQueue, MemoryStream, RelayConfig, StreamEntry,
};
use spot_sync_hub_adapters::{MasterScope, PolicyBook, PolicyRedactor, ScopeKeysOf};
use spot_sync_master::{Doctype, ScopeKey, Subscription};
use spot_sync_obligations::Lane;
use spot_sync_policy::{Class, Direction, FieldName, SyncPolicy, Trigger, outgoing_view};
use spot_sync_protocol::NodeId;
use spot_sync_skew::{NodeReport, SoftwareVersion};

fn policy(excluded: &[&str]) -> SyncPolicy {
    SyncPolicy::new(
        Class::Master,
        None,
        Direction::Down,
        Trigger::OnSubmit,
        excluded
            .iter()
            .map(|f| FieldName::new(f).unwrap())
            .collect(),
    )
}

fn book(entity: &str, excluded: &[&str]) -> PolicyBook {
    let mut b = PolicyBook::new();
    b.insert(entity, policy(excluded));
    b
}

#[test]
fn excluded_field_and_table_are_removed_and_nothing_else() {
    let b = book("customer", &["credit_limit", "bank_rows"]);
    let r = PolicyRedactor::new(&b);
    let mut image = json!({
        "name": "Acme",
        "credit_limit": "5000.00",
        "bank_rows": [{"iban": "X"}],
        "contacts": [{"phone": "1", "credit_limit": "9"}, {"phone": "2"}],
        "city": "Lahore",
    });
    r.redact_down("customer", &mut image);
    assert_eq!(
        image,
        json!({"name": "Acme", "contacts": [{"phone": "1"}, {"phone": "2"}], "city": "Lahore"})
    );
    assert!(r.unredacted_entities().is_empty());
}

#[test]
fn key_order_of_what_is_kept_is_untouched() {
    let b = book("customer", &["b"]);
    let r = PolicyRedactor::new(&b);
    let mut image: Value =
        serde_json::from_str(r#"{"z":1,"b":2,"a":3,"m":[{"y":1,"b":0,"x":2}]}"#).unwrap();
    r.redact_down("customer", &mut image);
    let text = serde_json::to_string(&image).unwrap();
    // Whatever order the map keeps, it must equal the order it had before the removal.
    let mut expected: Value =
        serde_json::from_str(r#"{"z":1,"b":2,"a":3,"m":[{"y":1,"b":0,"x":2}]}"#).unwrap();
    if let Value::Object(m) = &mut expected {
        m.remove("b");
        if let Some(Value::Array(rows)) = m.get_mut("m")
            && let Some(Value::Object(row)) = rows.first_mut()
        {
            row.remove("b");
        }
    }
    assert_eq!(text, serde_json::to_string(&expected).unwrap());
}

#[test]
fn unknown_entity_fails_closed_to_null_and_is_counted() {
    let b = book("customer", &["secret"]);
    let r = PolicyRedactor::new(&b);
    let mut image = json!({"name": "x", "secret": "s", "other": 1});
    r.redact_down("supplier", &mut image);
    assert_eq!(image, Value::Null);
    let mut again = json!({"a": 1});
    r.redact_down("supplier", &mut again);
    r.redact_down("asset", &mut json!({"a": 1}));
    assert_eq!(again, Value::Null);
    assert_eq!(r.unredacted_entities(), vec!["asset", "supplier"]);
}

#[test]
fn an_image_that_is_not_an_object_is_not_sent() {
    let b = book("customer", &["secret"]);
    let r = PolicyRedactor::new(&b);
    for original in [json!("secret text"), json!([1, 2]), json!(7)] {
        let mut image = original;
        r.redact_down("customer", &mut image);
        assert_eq!(image, Value::Null);
    }
}

#[test]
fn an_empty_book_leaks_nothing() {
    let b = PolicyBook::new();
    let r = PolicyRedactor::new(&b);
    let mut image = json!({"anything": "at all"});
    r.redact_down("customer", &mut image);
    assert_eq!(image, Value::Null);
}

#[test]
fn missing_for_lists_exactly_the_uncovered_entities() {
    let mut b = PolicyBook::new();
    b.insert("a", policy(&[]));
    b.insert("c", policy(&[]));
    let want: Vec<String> = ["d", "c", "b", "a", "b"]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
    assert_eq!(b.missing_for(&want), vec!["b", "d"]);
    assert!(b.missing_for(&[]).is_empty());
    assert_eq!(PolicyBook::new().missing_for(&["x".to_owned()]), vec!["x"]);
}

// ---- property: equals outgoing_view through a JSON <-> Document conversion ----

/// A valid UUIDv7 record id made of one repeated byte.
fn rid(n: u8) -> RecordId {
    let mut b = [n; 16];
    b[6] = 0x70 | (n & 0x0f);
    b[8] = 0x80 | (n & 0x3f);
    RecordId::from_bytes(b).unwrap()
}

fn to_document(image: &Map<String, Value>) -> Document {
    let mut fields = BTreeMap::new();
    let mut tables: BTreeMap<Name, Vec<ChildRow>> = BTreeMap::new();
    for (k, v) in image {
        match v {
            Value::Array(rows) => {
                let rows = rows
                    .iter()
                    .enumerate()
                    .map(|(i, row)| ChildRow {
                        id: rid(i as u8 + 1),
                        fields: row
                            .as_object()
                            .unwrap()
                            .iter()
                            .map(|(k, v)| {
                                (
                                    Name::new(k).unwrap(),
                                    SValue::Text(v.as_str().unwrap().into()),
                                )
                            })
                            .collect(),
                    })
                    .collect();
                tables.insert(Name::new(k).unwrap(), rows);
            }
            other => {
                fields.insert(
                    Name::new(k).unwrap(),
                    SValue::Text(other.as_str().unwrap().into()),
                );
            }
        }
    }
    Document::loaded(
        Name::new("doc").unwrap(),
        rid(9),
        Version::FIRST,
        None,
        fields,
        tables,
    )
}

fn from_document(doc: &Document) -> Map<String, Value> {
    let mut m = Map::new();
    for (k, v) in doc.fields() {
        let SValue::Text(t) = v else {
            panic!("text only")
        };
        m.insert(k.as_str().to_owned(), Value::String(t.clone()));
    }
    for (k, rows) in doc.tables() {
        let rows = rows
            .iter()
            .map(|r| {
                let mut o = Map::new();
                for (k, v) in &r.fields {
                    let SValue::Text(t) = v else {
                        panic!("text only")
                    };
                    o.insert(k.as_str().to_owned(), Value::String(t.clone()));
                }
                Value::Object(o)
            })
            .collect();
        m.insert(k.as_str().to_owned(), Value::Array(rows));
    }
    m
}

proptest! {
    #[test]
    fn equals_outgoing_view(
        fields in proptest::collection::btree_map("f[0-5]", "[a-z]{0,4}", 0..6),
        tables in proptest::collection::btree_map(
            "t[0-3]",
            proptest::collection::vec(
                proptest::collection::btree_map("r[0-3]", "[a-z]{0,3}", 0..4), 0..3),
            0..4),
        excluded in proptest::collection::btree_set(
            prop_oneof!["f[0-5]", "t[0-3]"], 0..5),
    ) {
        let mut image = Map::new();
        for (k, v) in &fields {
            image.insert(k.clone(), Value::String(v.clone()));
        }
        for (k, rows) in &tables {
            let rows = rows.iter().map(|r| {
                Value::Object(r.iter().map(|(k, v)| (k.clone(), Value::String(v.clone()))).collect())
            }).collect();
            image.insert(k.clone(), Value::Array(rows));
        }
        let ex: Vec<&str> = excluded.iter().map(String::as_str).collect();
        let p = policy(&ex);
        let expected = from_document(&outgoing_view(&to_document(&image), &p));

        let b = book("doc", &ex);
        let r = PolicyRedactor::new(&b);
        let mut got = Value::Object(image);
        r.redact_down("doc", &mut got);
        prop_assert_eq!(got, Value::Object(expected));
    }
}

// ---- end to end: a relay tick queues a bundle with no excluded field ----

struct Registry;
impl ScopeKeysOf for Registry {
    fn scope_keys(&self, _k: &RecordKey) -> Option<Vec<String>> {
        Some(Vec::new())
    }
}
struct NoRefs;
impl References for NoRefs {
    fn references(&self, _e: &str, _i: &Value) -> Result<Vec<RecordKey>, ClosureError> {
        Ok(Vec::new())
    }
}
struct Src(BTreeMap<RecordKey, SourceImage>);
impl ImageSource for Src {
    fn image(&self, key: &RecordKey) -> Result<Option<SourceImage>, ClosureError> {
        Ok(self.0.get(key).cloned())
    }
}

fn entry(position: u64, entity: &str, id: &str) -> StreamEntry {
    StreamEntry {
        position,
        tenant: "t1".into(),
        source: "hub".into(),
        origin: None,
        txn: format!("t{position}"),
        role: EntryRole::Root,
        key: RecordKey::new(entity, id),
        version: 1,
        topic: entity.into(),
        major: 1,
        lane: Lane::Bulk,
    }
}

fn report() -> NodeReport {
    NodeReport {
        software_version: SoftwareVersion::new(1, 0, 0),
        metadata_version: 1,
        migration_epoch: 1,
        epoch_verified: true,
        mid_migration: false,
        decodable_majors: ["price_list", "orphan"]
            .iter()
            .map(|e| {
                (
                    (*e).to_owned(),
                    [1u32].into_iter().collect::<BTreeSet<u32>>(),
                )
            })
            .collect(),
    }
}

#[test]
fn a_relay_tick_queues_no_excluded_field_and_no_unpoliced_entity() {
    let covered = RecordKey::new("price_list", "P1");
    let orphan = RecordKey::new("orphan", "O1");
    let mut images = BTreeMap::new();
    images.insert(
        covered.clone(),
        SourceImage::new(
            covered,
            json!({"rate": "1.00", "cost": "0.40", "tiers": [{"qty": "1", "cost": "0.3"}]}),
            1,
        ),
    );
    images.insert(
        orphan.clone(),
        SourceImage::new(orphan, json!({"secret": "leaks if unpoliced"}), 2),
    );
    let mut stream = MemoryStream::new();
    stream.append(entry(1, "price_list", "P1"));
    stream.append(entry(2, "orphan", "O1"));

    let b: &'static PolicyBook = Box::leak(Box::new(book("price_list", &["cost"])));
    let sub: &'static Subscription = Box::leak(Box::new(Subscription::new(
        NodeId::new("edge-1").unwrap(),
        std::iter::empty::<ScopeKey>(),
        [
            Doctype::new("price_list").unwrap(),
            Doctype::new("orphan").unwrap(),
        ],
    )));
    let reg: &'static Registry = Box::leak(Box::new(Registry));

    let mut relay = HubRelay::new(
        RelayConfig::new("t1", "hub"),
        stream,
        MemoryQueue::new(),
        Src(images),
        Box::new(NoRefs),
    )
    .unwrap();
    relay
        .add_destination(Destination::new(
            "edge-1",
            Box::new(MasterScope::new(sub, reg)),
            Box::new(PolicyRedactor::new(b)),
            report(),
        ))
        .unwrap();
    relay.tick(10).unwrap();

    let images: Vec<(String, Value)> = relay
        .queue()
        .all("edge-1")
        .iter()
        .flat_map(|p| {
            p.item
                .bundle
                .records
                .iter()
                .map(|r| (r.key.entity.clone(), r.image.clone()))
        })
        .collect();
    assert!(!images.is_empty());
    for (entity, image) in &images {
        match entity.as_str() {
            "price_list" => assert_eq!(
                image,
                &json!({"rate": "1.00", "tiers": [{"qty": "1"}]}),
                "excluded field `cost` must not be queued"
            ),
            _ => assert_eq!(image, &Value::Null, "an unpoliced entity must queue Null"),
        }
    }
    assert!(
        !serde_json::to_string(&images)
            .unwrap()
            .contains("leaks if unpoliced")
    );
}
