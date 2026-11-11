//! Shared fixtures for the quarantine tests: a small catalog, the in-memory backend, record
//! builders, and a permission engine.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use spot_meta::{Definition, FieldSpec, FieldType as MetaFieldType, Registry};
use spot_permissions::{
    AssignmentContext, Model, PermissionEngine, Principal, Right, Rights, Role, RoleGrant, Snapshot,
};
use spot_replica_apply::ReceivedRecord;
use spot_storage_contract::{
    Catalog, Collation, DocumentData, EntityDef, EntityKind, FieldDef, FieldType, Fields,
    Isolation, Name, RecordId, TxOptions, Value,
};
use spot_storage_fake::FakeBackend;
use spot_sync_protocol::{RecordName, SourceId};

/// A validated storage name.
pub fn name(s: &str) -> Name {
    Name::new(s).unwrap_or_else(|e| panic!("{s}: {e}"))
}

fn text() -> FieldType {
    FieldType::Text { max_chars: None }
}

fn field(field: &str, ty: FieldType) -> FieldDef {
    FieldDef {
        name: name(field),
        ty,
        nullable: true,
    }
}

/// An entity shaped like `note`: a title and the ADR-058 origin fields.
fn plain_def(entity: &str) -> EntityDef {
    EntityDef {
        name: name(entity),
        kind: EntityKind::Document,
        fields: vec![
            field("title", text()),
            field("origin_node", text()),
            field("origin_id", text()),
            field("origin_version", FieldType::Int),
        ],
        children: vec![],
        indexes: vec![],
        exclusions: vec![],
        search: None,
    }
}

/// The catalog every test runs against. `ghost` is present only after the "metadata upgrade".
pub fn catalog(with_ghost: bool) -> Catalog {
    let mut entities = vec![plain_def("note")];
    if with_ghost {
        entities.push(plain_def("ghost"));
    }
    entities.push(EntityDef {
        name: name("gl_entry"),
        kind: EntityKind::Document,
        fields: vec![
            field("account", text()),
            field("origin_node", text()),
            field("origin_id", text()),
            field("origin_version", FieldType::Int),
        ],
        children: vec![],
        indexes: vec![],
        exclusions: vec![],
        search: None,
    });
    Catalog::new(
        1,
        Collation::new("en").unwrap_or_else(|e| panic!("{e}")),
        entities,
    )
    .unwrap_or_else(|e| panic!("{e}"))
}

/// A fresh in-memory backend.
pub fn backend() -> FakeBackend {
    FakeBackend::new()
}

/// The sender every test replicates from.
pub fn source() -> SourceId {
    SourceId::new("head.office.app#hub").unwrap_or_else(|e| panic!("{e}"))
}

/// The apply transaction options.
pub fn options() -> TxOptions {
    TxOptions::new(name("quarantine_apply"), Isolation::ReadCommitted)
}

/// A field map from text and int parts.
pub fn fields(parts: &[(&str, Value)]) -> Fields {
    parts.iter().map(|(k, v)| (name(k), v.clone())).collect()
}

fn document(entity: &str, id: RecordId, title: &str, version: i64) -> DocumentData {
    DocumentData {
        entity: name(entity),
        id,
        fields: fields(&[
            ("title", Value::Text(title.to_owned())),
            ("origin_node", Value::Null),
            ("origin_id", Value::Null),
            ("origin_version", Value::Int(version)),
        ]),
        children: BTreeMap::new(),
    }
}

/// A `note` document at `version`.
pub fn note(id: RecordId, title: &str, version: i64) -> DocumentData {
    document("note", id, title, version)
}

/// A `ghost` document: valid in shape, but the initial catalog does not declare the entity.
pub fn ghost(id: RecordId, title: &str, version: i64) -> DocumentData {
    document("ghost", id, title, version)
}

/// A `note` document with an undeclared field, so it is refused as `unknown_field` while the
/// entity stays `note` (the permission tests need a `note`-entity item).
pub fn note_with_extra(id: RecordId, version: i64) -> DocumentData {
    let mut doc = document("note", id, "extra", version);
    doc.fields
        .insert(name("secret"), Value::Text("nope".to_owned()));
    doc
}

/// A received record for `document` at `version`, with `entries`.
pub fn record(
    document: DocumentData,
    version: u64,
    seq: u64,
    entries: Vec<DocumentData>,
) -> ReceivedRecord {
    ReceivedRecord {
        name: RecordName::new(&format!("{}:{}", document.entity.as_str(), document.id))
            .unwrap_or_else(|e| panic!("{e}")),
        version,
        seq,
        source: source(),
        topic: None,
        conversation: None,
        meta_version: 1,
        document,
        entries,
    }
}

fn meta_definition(entity: &str) -> Definition {
    let mut def = Definition::new(entity);
    def.fields = vec![
        FieldSpec::new("title", MetaFieldType::Text),
        FieldSpec::new("origin_node", MetaFieldType::Text),
        FieldSpec::new("origin_id", MetaFieldType::Text),
        FieldSpec::new("origin_version", MetaFieldType::Int),
    ];
    def
}

/// A permission engine where the `Editor` role may read and write `note` but not delete it, and
/// `ghost` has no grants at all.
pub fn engine() -> Arc<PermissionEngine> {
    let meta = Registry::new(vec![meta_definition("note"), meta_definition("ghost")])
        .unwrap_or_else(|e| panic!("{e:?}"))
        .effective();
    let model = Model {
        grants: vec![RoleGrant {
            doctype: "note".to_owned(),
            role: Role("Editor".to_owned()),
            rights: Rights::new([Right::Read, Right::Write]),
            if_owner: false,
        }],
        field_grants: vec![],
        groups: vec![],
        shares: vec![],
    };
    let snapshot = Snapshot::build(&meta, &model).unwrap_or_else(|e| panic!("{e}"));
    Arc::new(PermissionEngine::new(Arc::new(snapshot)))
}

/// A principal with the `Editor` role.
pub fn editor() -> Principal {
    Principal::user(
        "alice",
        vec![AssignmentContext {
            id: "a1".to_owned(),
            roles: vec![Role("Editor".to_owned())],
            scope: vec![],
        }],
    )
}

/// A principal with no roles at all.
pub fn outsider() -> Principal {
    Principal::user("bob", vec![])
}

/// The platform acting as itself.
pub fn system() -> Principal {
    Principal::system()
}
