//! Sender-side field lists: default deny, a field that is not listed never appears, and a
//! personal or sensitive field must be listed by name rather than swept in by a namespace.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::topic;
use spot_data_policy::Classification;
use spot_tenant_link::{
    ExchangePayload, FieldCatalog, FieldList, FieldName, FieldValue, Topic, TopicKind,
};

fn name(text: &str) -> FieldName {
    FieldName::new(text).unwrap()
}

fn value(text: &str) -> FieldValue {
    FieldValue::Text(text.to_owned())
}

fn payload() -> ExchangePayload {
    let mut payload = ExchangePayload::new();
    payload.insert(name("message.body"), value("hello"));
    payload.insert(name("message.ssn"), value("SECRET-VALUE-NEVER-LEAVES"));
    payload.insert(name("message.note"), value("internal"));
    payload
}

/// The metadata an app declares for the topic in these tests.
struct Catalog;

impl FieldCatalog for Catalog {
    fn classified(&self, _topic: &Topic, _kind: TopicKind) -> Vec<(FieldName, Classification)> {
        vec![
            (name("message.body"), Classification::Public),
            (name("message.ssn"), Classification::Pii),
            (name("message.note"), Classification::Internal),
        ]
    }
}

#[test]
fn the_default_is_that_nothing_leaves() {
    let filtered = FieldList::empty().filter(&payload());
    assert!(filtered.values().is_empty());
    assert_eq!(
        filtered.withheld(),
        &[
            name("message.body"),
            name("message.note"),
            name("message.ssn")
        ]
    );
}

#[test]
fn a_field_that_is_not_listed_never_appears() {
    let list = FieldList::only([name("message.body")]);
    let filtered = list.filter(&payload());
    assert_eq!(filtered.values().len(), 1);
    assert!(filtered.values().get(&name("message.body")).is_some());
    assert!(filtered.values().get(&name("message.ssn")).is_none());
    // Not in the values, not in the serialised form of the crossing payload…
    let wire = serde_json::to_string(filtered.values()).unwrap();
    assert!(!wire.contains("SECRET-VALUE-NEVER-LEAVES"));
    // …and the withheld list carries the name only.
    let whole = serde_json::to_string(&filtered).unwrap();
    assert!(!whole.contains("SECRET-VALUE-NEVER-LEAVES"));
    assert!(whole.contains("message.ssn"));
}

#[test]
fn a_protected_field_must_be_listed_by_name() {
    let namespace_only = FieldList::only([name("message.body")]).with_namespace(name("message"));
    let error = namespace_only
        .lint(&Catalog, &topic(), TopicKind::Message)
        .unwrap_err();
    assert_eq!(error.code(), "tenant_link.field_classification");
    assert!(error.to_string().contains("message.ssn"));

    let explicit = FieldList::only([name("message.body"), name("message.ssn")])
        .with_namespace(name("message"));
    explicit
        .lint(&Catalog, &topic(), TopicKind::Message)
        .expect("every protected field is named");
}

#[test]
fn an_internal_field_under_a_namespace_is_not_a_lint_error() {
    let list = FieldList::only([name("message.body")]).with_namespace(name("message.note"));
    list.lint(&Catalog, &topic(), TopicKind::Message)
        .expect("a namespace over an internal field is allowed");
}

#[test]
fn a_namespace_stops_at_its_own_segment() {
    let list = FieldList::empty().with_namespace(name("message"));
    assert!(list.includes(&name("message.body")));
    assert!(!list.includes(&name("messages.body")));
    assert!(!list.includes(&name("messagebody")));
}

#[test]
fn filtering_keeps_field_order_and_withheld_names_only() {
    let list = FieldList::only([name("message.note"), name("message.body")]);
    let filtered = list.filter(&payload());
    let kept: Vec<_> = filtered
        .values()
        .iter()
        .map(|(field, _)| field.as_str())
        .collect();
    assert_eq!(kept, vec!["message.body", "message.note"]);
    assert_eq!(filtered.withheld(), &[name("message.ssn")]);
    assert!(!format!("{:?}", filtered.withheld()).contains("SECRET"));
}

#[test]
fn the_service_sender_check_drops_an_unlisted_field() {
    use common::{linked, topic};
    use spot_tenant_link::{
        Direction, LinkService, LinkStore, MemoryAuditLog, MemoryLinkStore, SendRequest,
    };

    let store = MemoryLinkStore::new();
    let audit = MemoryAuditLog::new();
    let service = LinkService::new(&store, &audit);
    let link = store.insert(linked().a).unwrap();
    let mut payload = ExchangePayload::new();
    payload.insert(name("message.body"), value("hello"));
    payload.insert(name("message.ssn"), value("SECRET-VALUE-NEVER-LEAVES"));
    let filtered = service
        .prepare_send(&SendRequest {
            link: &link.id,
            topic: &topic(),
            direction: Direction::Send,
            now: 2_000,
            actor: "user:a",
            payload: &payload,
            oracle: None,
        })
        .unwrap();
    assert!(filtered.values().get(&name("message.body")).is_some());
    assert!(filtered.values().get(&name("message.ssn")).is_none());
    let wire = serde_json::to_string(filtered.values()).unwrap();
    assert!(!wire.contains("SECRET-VALUE-NEVER-LEAVES"));
    assert_eq!(
        audit.events().unwrap()[0].kind,
        spot_tenant_link::LinkEventKind::Allowed,
        "the sender check audits its use"
    );
}
