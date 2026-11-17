//! The hosted sync policy over a real `EffectiveMeta` (WP-RTB-02.T10): a refused generation keeps
//! the previous, an excluded field never leaves, a master edge write is refused, the default preset
//! applies to a runtime-created DocType, and the hub providers answer from the verified generation.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::json;
use spot_meta::{Customisations, Definition, EffectiveMeta, FieldSpec, FieldType, Registry};
use spot_sync_closure::RecordKey;
use spot_sync_hub_adapters::ScopeKeysOf;
use spot_sync_master::{Doctype, ScopeKey, Subscription};
use spot_sync_policy::{
    Class, Direction, FieldName, PolicySet, Preset, ScopeKeyType, SyncPolicy, Trigger,
};
use spot_sync_policy_host::scope::{DestinationFactory, FieldReader, PolicyScopeKeys};
use spot_sync_policy_host::strip::strip_value;
use spot_sync_policy_host::{Defaults, MASTER_EDGE_WRITE, PolicyHost};
use spot_sync_protocol::NodeId;

fn meta(defs: Vec<Definition>) -> EffectiveMeta {
    Registry::build(defs, &Customisations::new()).expect("metadata builds")
}

fn doctype(name: &str, fields: Vec<FieldSpec>) -> Definition {
    let mut definition = Definition::new(name);
    definition.fields = fields;
    definition
}

fn text(name: &str) -> FieldSpec {
    FieldSpec::new(name, FieldType::Text)
}

fn link(name: &str, target: &str) -> FieldSpec {
    let mut field = FieldSpec::new(name, FieldType::Link);
    field.options = Some(target.to_owned());
    field
}

fn master() -> SyncPolicy {
    SyncPolicy::new(
        Class::Master,
        None,
        Direction::Down,
        Trigger::OnSubmit,
        Default::default(),
    )
}

fn scope() -> ScopeKeyType {
    ScopeKeyType::new("warehouse").unwrap()
}

fn shared(excluded: &[&str]) -> SyncPolicy {
    SyncPolicy::new(
        Class::Shared,
        Some(scope()),
        Direction::Both,
        Trigger::OnSubmit,
        excluded
            .iter()
            .map(|name| FieldName::new(name).unwrap())
            .collect(),
    )
}

fn declared(entries: Vec<(&str, SyncPolicy)>) -> PolicySet {
    let mut set = PolicySet::new();
    for (name, policy) in entries {
        set.declare(name, policy);
    }
    set
}

#[test]
fn a_refused_generation_keeps_the_previous_and_says_why() {
    let good = meta(vec![
        doctype("customer", vec![text("title")]),
        doctype("invoice", vec![text("title")]),
    ]);
    let mut host = PolicyHost::new();
    host.load(
        1,
        &good,
        &declared(vec![("customer", master()), ("invoice", shared(&[]))]),
    )
    .unwrap();
    assert!(host.may_leave("customer", false));
    assert_eq!(host.readiness(), "sync_policy: generation 1");

    // Generation 2: the synced Invoice links to a DocType nobody declared, so it is `local`.
    let bad = meta(vec![
        doctype("note", vec![text("title")]),
        doctype("invoice", vec![link("note", "note")]),
    ]);
    let refused = host
        .load(2, &bad, &declared(vec![("invoice", shared(&[]))]))
        .unwrap_err();
    assert_eq!(refused.generation, 2);
    assert_eq!(refused.code, "sync_policy.closed_link");
    assert_eq!(refused.doctype, "invoice");
    // The previous generation still serves, unchanged.
    assert_eq!(host.generation(), Some(1));
    assert!(host.may_leave("customer", false));
    assert!(!host.may_leave("note", true));
    assert_eq!(
        host.readiness(),
        "sync_policy: refused generation 2 (sync_policy.closed_link on invoice), serving 1"
    );
    // A generation that loads clears the refusal.
    host.load(3, &good, &declared(vec![("customer", master())]))
        .unwrap();
    assert!(host.refused().is_none());
}

#[test]
fn nothing_leaves_for_a_local_doctype_and_the_direction_is_respected() {
    let good = meta(vec![
        doctype("customer", vec![text("title")]),
        doctype("scratch", vec![text("title")]),
    ]);
    let mut host = PolicyHost::new();
    host.load(1, &good, &declared(vec![("customer", master())]))
        .unwrap();
    assert!(host.may_leave("customer", false), "master flows down");
    assert!(!host.may_leave("customer", true), "master never flows up");
    assert!(!host.may_leave("scratch", true));
    assert!(!host.may_leave("scratch", false));
    assert!(!host.may_leave("NoSuchDocType", true));
}

#[test]
fn an_excluded_field_never_leaves_either_way() {
    let good = meta(vec![doctype(
        "invoice",
        vec![text("title"), text("secret")],
    )]);
    let mut host = PolicyHost::new();
    host.load(1, &good, &declared(vec![("invoice", shared(&["secret"]))]))
        .unwrap();
    let snapshot = host.snapshot().unwrap();
    let policy = snapshot.policy("invoice").unwrap();

    // Up: the node's event payload is stripped before it is offered.
    let mut payload = json!({"title": "t", "secret": "x", "lines": [{"secret": "y", "qty": 1}]});
    assert_eq!(strip_value(&mut payload, policy), 2);
    assert_eq!(payload, json!({"title": "t", "lines": [{"qty": 1}]}));

    // Down: the hub's redactor, built from the same generation, strips an image.
    let keys = PolicyScopeKeys::new(Arc::clone(&snapshot), BTreeMap::new(), Arc::new(NoValues));
    let factory = DestinationFactory::new(keys);
    let _ = factory;
    let redactor = spot_sync_policy_host::scope::OwnedRedactor::new(snapshot);
    let mut image = json!({"title": "t", "secret": "x"});
    spot_sync_closure::Redactor::redact_down(&redactor, "invoice", &mut image);
    assert_eq!(image, json!({"title": "t"}));
    // An entity with no policy is not sent at all (fail closed).
    let mut unknown = json!({"title": "t"});
    spot_sync_closure::Redactor::redact_down(&redactor, "NoSuchDocType", &mut unknown);
    assert!(unknown.is_null());
}

#[test]
fn an_edge_may_not_write_master_data() {
    let good = meta(vec![
        doctype("customer", vec![text("title")]),
        doctype("invoice", vec![text("title")]),
    ]);
    let mut host = PolicyHost::new();
    host.load(
        1,
        &good,
        &declared(vec![("customer", master()), ("invoice", shared(&[]))]),
    )
    .unwrap();
    assert_eq!(host.edge_may_write("customer"), Err(MASTER_EDGE_WRITE));
    assert!(host.edge_may_write("invoice").is_ok());
}

#[test]
fn the_default_preset_applies_to_a_runtime_doctype_and_not_to_an_apps_own() {
    let mut app_owned = doctype("item", vec![text("title")]);
    app_owned.app = Some("erp".to_owned());
    let runtime = doctype("widget", vec![text("title")]);
    let generation = meta(vec![app_owned, runtime]);

    let mut host = PolicyHost::with_defaults(Defaults {
        preset: Preset::Master,
        owner_scope: None,
    });
    host.load(1, &generation, &PolicySet::new()).unwrap();
    let snapshot = host.snapshot().unwrap();
    assert!(host.may_leave("widget", false));
    assert_eq!(snapshot.preset("widget"), Preset::Master);
    assert!(
        !host.may_leave("item", false),
        "an app's own DocType stays local"
    );

    // A default that cannot hold refuses the generation: `record` needs an owner scope.
    let mut host = PolicyHost::with_defaults(Defaults {
        preset: Preset::Record,
        owner_scope: None,
    });
    let refused = host.load(1, &generation, &PolicySet::new()).unwrap_err();
    assert_eq!(refused.code, "sync_policy.owner_scope_missing");
    assert_eq!(host.generation(), None);
    assert!(!host.may_leave("widget", true));
}

struct NoValues;

impl FieldReader for NoValues {
    fn scope_value(&self, _entity: &str, _id: &str, _field: &str) -> Option<String> {
        None
    }
}

struct Stored(BTreeMap<(String, String), String>);

impl FieldReader for Stored {
    fn scope_value(&self, entity: &str, id: &str, _field: &str) -> Option<String> {
        self.0.get(&(entity.to_owned(), id.to_owned())).cloned()
    }
}

#[test]
fn the_hub_providers_answer_from_the_verified_generation_and_fail_closed() {
    let generation = meta(vec![
        doctype("customer", vec![text("title")]),
        doctype("invoice", vec![text("title"), text("warehouse")]),
        doctype("scratch", vec![text("title")]),
    ]);
    let mut host = PolicyHost::new();
    host.load(
        1,
        &generation,
        &declared(vec![("customer", master()), ("invoice", shared(&[]))]),
    )
    .unwrap();
    let snapshot = host.snapshot().unwrap();
    let mut values = BTreeMap::new();
    values.insert(("invoice".to_owned(), "i1".to_owned()), "W7".to_owned());
    values.insert(("invoice".to_owned(), "i2".to_owned()), "W8".to_owned());
    let mut fields = BTreeMap::new();
    fields.insert(
        ("invoice".to_owned(), "warehouse".to_owned()),
        "warehouse".to_owned(),
    );
    let keys = PolicyScopeKeys::new(Arc::clone(&snapshot), fields, Arc::new(Stored(values)));

    assert_eq!(
        keys.scope_keys(&RecordKey::new("invoice", "i1")),
        Some(vec!["warehouse:W7".to_owned()])
    );
    // Master data with no owner scope is for every subscriber of its DocType.
    assert_eq!(
        keys.scope_keys(&RecordKey::new("customer", "c1")),
        Some(Vec::new())
    );
    // No value, a local DocType and an unknown DocType are all "not known": not covered.
    assert_eq!(keys.scope_keys(&RecordKey::new("invoice", "missing")), None);
    assert_eq!(keys.scope_keys(&RecordKey::new("scratch", "s1")), None);
    assert_eq!(keys.scope_keys(&RecordKey::new("Nope", "n1")), None);

    // A destination subscribed to Invoice records of warehouse W7 only.
    let node = NodeId::new("edge-khi#branch_01").unwrap();
    let subscription = Subscription::new(
        node,
        [ScopeKey::new("warehouse", "W7").unwrap()],
        [Doctype::new("invoice").unwrap()],
    );
    let factory = DestinationFactory::new(keys);
    let destination = factory.destination("branch_01", subscription, report());
    let scope = &destination.scope;
    assert!(scope.covers_key(&RecordKey::new("invoice", "i1")));
    assert!(
        !scope.covers_key(&RecordKey::new("invoice", "i2")),
        "W8 is not W7"
    );
    assert!(
        !scope.covers_key(&RecordKey::new("customer", "c1")),
        "not subscribed"
    );
    assert!(!scope.covers_key(&RecordKey::new("scratch", "s1")), "local");
}

fn report() -> spot_sync_skew::NodeReport {
    use std::collections::{BTreeMap, BTreeSet};
    spot_sync_skew::NodeReport {
        software_version: spot_sync_skew::SoftwareVersion::new(1, 0, 0),
        metadata_version: 1,
        migration_epoch: 1,
        epoch_verified: true,
        mid_migration: false,
        decodable_majors: BTreeMap::<String, BTreeSet<u32>>::new(),
    }
}
