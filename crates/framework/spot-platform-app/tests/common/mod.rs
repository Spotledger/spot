//! Shared helpers: the app's metadata, and a permission snapshot over it.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use spot_meta::{Customisations, EffectiveMeta, Registry};
use spot_permissions::model::FieldFacts;
use spot_permissions::{PermissionEngine, Snapshot};
use spot_platform_app::{definitions, permissions};
use spot_storage_contract::Collation;

pub fn meta() -> EffectiveMeta {
    Registry::build(definitions().unwrap(), &Customisations::new())
        .unwrap_or_else(|e| panic!("the platform app does not build: {e:?}"))
}

pub fn collation() -> Collation {
    Collation::new("en").unwrap()
}

/// A snapshot with the shipped grants and every field's real protection facts.
pub fn snapshot(meta: &EffectiveMeta) -> Snapshot {
    let model = permissions().unwrap();
    let mut snapshot = Snapshot::build(meta, &model).unwrap();
    let protection = meta.field_protection();
    for view in meta.doctypes() {
        let doctype = view.name.as_str();
        let facts: Vec<FieldFacts> = protection
            .iter()
            .filter_map(|p| {
                let field = p.path.strip_prefix(&format!("{doctype}."))?;
                Some(FieldFacts {
                    path: field.to_owned(),
                    groups: p
                        .groups
                        .iter()
                        .map(|g| spot_permissions::GroupName(g.clone()))
                        .collect(),
                    tags: p
                        .tags
                        .iter()
                        .map(|t| spot_permissions::Tag(t.clone()))
                        .collect(),
                    mask: None,
                })
            })
            .collect();
        snapshot = snapshot.with_field_facts(doctype, facts).unwrap();
    }
    snapshot
}

pub fn engine(meta: &EffectiveMeta) -> PermissionEngine {
    PermissionEngine::new(Arc::new(snapshot(meta)))
}
