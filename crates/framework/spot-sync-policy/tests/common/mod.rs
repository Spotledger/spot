//! Shared helpers for the integration tests.
#![allow(dead_code, missing_docs)]

use spot_meta::{Customisations, Definition, EffectiveMeta, FieldSpec, FieldType, Registry};

use spot_sync_policy::{Class, Direction, PolicySet, ScopeKeyType, SyncPolicy, Trigger};

/// Builds effective metadata from definitions (a runtime-created DocType is just a definition
/// with no Rust type).
pub fn build(defs: Vec<Definition>) -> EffectiveMeta {
    Registry::build(defs, &Customisations::new())
        .unwrap_or_else(|e| panic!("metadata builds: {e:?}"))
}

/// A DocType with the given fields.
pub fn doctype(name: &str, fields: Vec<FieldSpec>) -> Definition {
    let mut d = Definition::new(name);
    d.fields = fields;
    d
}

/// A child DocType (a table's target must be one).
pub fn child(name: &str, fields: Vec<FieldSpec>) -> Definition {
    let mut d = doctype(name, fields);
    d.is_child = true;
    d
}

/// A text field.
pub fn text(name: &str) -> FieldSpec {
    FieldSpec::new(name, FieldType::Text)
}

/// A required text field.
pub fn required_text(name: &str) -> FieldSpec {
    let mut f = text(name);
    f.required = true;
    f
}

/// A link field to `target`.
pub fn link(name: &str, target: &str) -> FieldSpec {
    let mut f = FieldSpec::new(name, FieldType::Link);
    f.options = Some(target.to_owned());
    f
}

/// A child table of `target`.
pub fn table(name: &str, target: &str) -> FieldSpec {
    let mut f = FieldSpec::new(name, FieldType::Table);
    f.options = Some(target.to_owned());
    f
}

/// A synced policy: `master` flowing down.
pub fn master() -> SyncPolicy {
    SyncPolicy::new(
        Class::Master,
        None,
        Direction::Down,
        Trigger::OnSubmit,
        Default::default(),
    )
}

/// A policy that flows up, so it needs an owner scope.
pub fn transactional(owner_scope: Option<ScopeKeyType>) -> SyncPolicy {
    SyncPolicy::new(
        Class::Transactional,
        owner_scope,
        Direction::Up,
        Trigger::OnSubmit,
        Default::default(),
    )
}

/// A one-DocType policy set.
pub fn declared(doctype: &str, policy: SyncPolicy) -> PolicySet {
    let mut set = PolicySet::new();
    set.declare(doctype, policy);
    set
}
