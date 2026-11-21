//! The hosted sync policy (WP-RTB-02.T10): the settings, the handle over a real `EffectiveMeta`
//! (a refused generation keeps the previous, a new generation moves the watch, the default preset
//! applies to a runtime-created DocType), the hub providers, and `spot sync policy show`.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::sync::Arc;

use spot_cli::services::sync_policy::{
    self, DECLARATION_INVALID, DEFAULT_PRESET_UNKNOWN, PolicyHandle, Settings, describe,
};
use spot_meta::{Customisations, Definition, EffectiveMeta, FieldSpec, FieldType, Registry};
use spot_sync_policy::{PolicySet, Preset};
use spot_sync_policy_host::scope::FieldReader;
use spot_sync_policy_host::{Defaults, PolicyHost};

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

fn settings(toml_text: &str) -> Settings {
    toml::from_str(toml_text).expect("settings parse")
}

const DECLARED: &str = r#"
default_preset = "local"

[declare.customer]
sync_class = "master"
sync_direction = "down"

[declare.invoice]
sync_class = "shared"
sync_direction = "both"
sync_owner_scope = "warehouse"
sync_exclude_fields = "secret"

[[scope_field]]
doctype = "invoice"
key = "warehouse"
field = "warehouse"
"#;

#[test]
fn the_module_runs_before_the_modules_that_read_it() {
    const { assert!(sync_policy::ORDER > spot_cli::services::node_role::ORDER) };
    const { assert!(sync_policy::ORDER < spot_cli::services::sync_hub_parts::ORDER) };
}

#[test]
fn settings_default_to_local_and_refuse_what_they_do_not_know() {
    let defaults = Settings::default().defaults().unwrap();
    assert_eq!(defaults.preset, Preset::Local);
    assert!(defaults.owner_scope.is_none());

    let bad = settings("default_preset = \"everything\"");
    assert!(
        bad.defaults()
            .unwrap_err()
            .0
            .contains(DEFAULT_PRESET_UNKNOWN)
    );
    let unknown: Result<Settings, _> = toml::from_str("speed = 1");
    assert!(unknown.is_err());

    let broken = settings("[declare.customer]\nsync_class = \"sometimes\"");
    let error = broken.declared().unwrap_err();
    assert!(error.0.contains(DECLARATION_INVALID));
    assert!(error.0.contains("customer.sync_class"));
    assert!(!error.0.contains("sometimes"), "never the value");
}

#[test]
fn a_declared_policy_loads_and_a_refused_generation_keeps_the_previous() {
    let s = settings(DECLARED);
    let handle = PolicyHandle::new(PolicyHost::new(), s.scope_fields());
    let declared = s.declared().unwrap();
    let mut changes = handle.changes();
    assert_eq!(*changes.borrow(), 0);

    let good = meta(vec![
        doctype("customer", vec![text("title")]),
        doctype(
            "invoice",
            vec![text("title"), text("secret"), text("warehouse")],
        ),
    ]);
    handle.load(1, &good, &declared).unwrap();
    assert!(changes.has_changed().unwrap());
    assert_eq!(*changes.borrow_and_update(), 1);
    assert!(handle.may_leave("customer", false));
    assert!(!handle.may_leave("customer", true));
    assert_eq!(
        handle.edge_may_write("customer"),
        Err("sync_policy.master_edge_write")
    );
    assert_eq!(handle.readiness(), "sync_policy: generation 1");

    // The next generation links the synced invoice to a local DocType: refused whole.
    let bad = meta(vec![
        doctype("customer", vec![text("title")]),
        doctype("note", vec![text("title")]),
        doctype(
            "invoice",
            vec![link("note", "note"), text("secret"), text("warehouse")],
        ),
    ]);
    let line = handle.load(2, &bad, &declared).unwrap_err();
    assert!(line.contains("refused generation 2"), "{line}");
    assert!(line.contains("serving 1"), "{line}");
    assert!(handle.refused());
    assert!(
        !changes.has_changed().unwrap(),
        "a refused generation moves nothing"
    );
    assert!(handle.may_leave("customer", false));
}

#[test]
fn nothing_leaves_before_a_generation_loads() {
    let handle = PolicyHandle::new(PolicyHost::new(), BTreeMap::new());
    assert!(!handle.may_leave("customer", true));
    assert!(!handle.may_leave("customer", false));
    assert!(handle.snapshot().is_none());
    assert!(
        handle.destination_factory(Arc::new(NoValues)).is_none(),
        "no destination is built without a generation"
    );
    assert_eq!(
        handle.readiness(),
        "sync_policy: no generation loaded, nothing syncs"
    );
}

struct NoValues;

impl FieldReader for NoValues {
    fn scope_value(&self, _entity: &str, _id: &str, _field: &str) -> Option<String> {
        None
    }
}

#[test]
fn the_default_preset_follows_a_runtime_doctype_through_the_handle() {
    let handle = PolicyHandle::new(
        PolicyHost::with_defaults(Defaults {
            preset: Preset::Master,
            owner_scope: None,
        }),
        BTreeMap::new(),
    );
    let generation = meta(vec![doctype("widget", vec![text("title")])]);
    handle.load(1, &generation, &PolicySet::new()).unwrap();
    assert!(handle.may_leave("widget", false));
    assert!(handle.destination_factory(Arc::new(NoValues)).is_some());
}

#[test]
fn show_prints_names_only() {
    let table: toml::Value = toml::from_str(DECLARED).unwrap();
    let lines = describe(Some(&table)).unwrap();
    assert_eq!(lines[0], "default preset: local");
    assert_eq!(lines[1], "default owner scope: none");
    assert_eq!(lines[2], "declared DocTypes: 2");
    let joined = lines.join("\n");
    assert!(joined.contains("customer: master (direction down), preset master"));
    assert!(joined.contains("invoice: shared (direction both), preset handover"));
    assert!(joined.contains("owner scope warehouse"));
    assert!(joined.contains("excluded fields: secret"));
    assert!(joined.contains("scope field: invoice.warehouse <- warehouse"));
    assert_eq!(
        describe(None).unwrap(),
        [
            "default preset: local",
            "default owner scope: none",
            "declared DocTypes: 0"
        ]
    );
    let broken: toml::Value = toml::from_str("default_preset = \"x\"").unwrap();
    assert!(describe(Some(&broken)).is_err());
}
