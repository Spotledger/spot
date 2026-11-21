//! Shared fixtures for the integration tests: a six-DocType metadata set with every link kind
//! and a relation DocType, the edge schema derived from it, and helpers to build documents.
//!
//! The DocType names avoid the reserved system columns (`id`, `version`, `parent`, `parent_field`,
//! `idx`) and the engine fields (`doc_state`, `amended_from`).

#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use spot_app_api::document::{ChildRow, Document};
use spot_app_api::prelude::{Name, RecordId, Value};
use spot_edge_sync::EdgeSchema;
use spot_graph_model::RelationSpec;
use spot_meta::{Customisations, Definition, EffectiveMeta, FieldSpec, FieldType, Registry};

/// A link field to `target`.
pub fn link(name: &str, target: &str) -> FieldSpec {
    let mut field = FieldSpec::new(name, FieldType::Link);
    field.options = Some(target.to_owned());
    field
}

/// A text field.
pub fn text(name: &str) -> FieldSpec {
    FieldSpec::new(name, FieldType::Text)
}

/// `party`: a master DocType.
pub fn party() -> Definition {
    let mut def = Definition::new("party");
    def.fields = vec![text("code")];
    def
}

/// `order`: a link, a dynamic link, a table and a table-multiselect.
pub fn order() -> Definition {
    let mut def = Definition::new("order");
    let mut kind = FieldSpec::new("ref_kind", FieldType::Select);
    kind.options = Some("party\nproject".to_owned());
    let mut ref_field = FieldSpec::new("ref", FieldType::DynamicLink);
    ref_field.options = Some("ref_kind".to_owned());
    let mut lines = FieldSpec::new("lines", FieldType::Table);
    lines.options = Some("order_line".to_owned());
    let mut watchers = FieldSpec::new("watchers", FieldType::TableMultiselect);
    watchers.options = Some("order_watcher".to_owned());
    def.fields = vec![
        text("number"),
        link("customer", "party"),
        kind,
        ref_field,
        lines,
        watchers,
    ];
    def
}

/// `order_line`: the child of `order.lines`, with a link of its own.
pub fn order_line() -> Definition {
    let mut def = Definition::new("order_line");
    def.is_child = true;
    def.fields = vec![link("item", "party"), text("qty")];
    def
}

/// `order_watcher`: the child of `order.watchers`, one link field.
pub fn order_watcher() -> Definition {
    let mut def = Definition::new("order_watcher");
    def.is_child = true;
    def.fields = vec![link("party", "party")];
    def
}

/// `project`: a master with one link.
pub fn project() -> Definition {
    let mut def = Definition::new("project");
    def.fields = vec![text("title"), link("customer", "party")];
    def
}

/// `task_link`: a relation DocType between `order` and `project` with a `role` property.
pub fn task_link() -> Definition {
    let mut def = Definition::new("task_link");
    def.fields = vec![
        link("order", "order"),
        link("project", "project"),
        text("role"),
    ];
    def
}

/// Every fixture definition.
pub fn definitions() -> Vec<Definition> {
    vec![
        party(),
        order(),
        order_line(),
        order_watcher(),
        project(),
        task_link(),
    ]
}

/// The relation spec that turns `task_link` into one typed edge.
pub fn relation_spec() -> RelationSpec {
    RelationSpec {
        doctype: "task_link".to_owned(),
        from_field: "order".to_owned(),
        to_field: "project".to_owned(),
        edge_name: "task_link.relates".to_owned(),
        aggregate: None,
    }
}

/// The effective metadata, built through `spot_meta::Registry` exactly as the platform builds it.
pub fn meta() -> Arc<EffectiveMeta> {
    let built = Registry::build(definitions(), &Customisations::new())
        .unwrap_or_else(|errors| panic!("fixtures do not build: {errors:?}"));
    Arc::new(built)
}

/// The effective metadata with a custom link field `custom_ref` added to `order` at runtime.
pub fn meta_with_custom_link() -> Arc<EffectiveMeta> {
    let mut customs = Customisations::new();
    customs.add_field("order", link("custom_ref", "party"), None);
    let built = Registry::build(definitions(), &customs)
        .unwrap_or_else(|errors| panic!("fixtures with a custom field do not build: {errors:?}"));
    Arc::new(built)
}

/// The edge schema over the fixture metadata and the relation spec.
pub fn schema() -> EdgeSchema {
    EdgeSchema::derive(&meta(), &[relation_spec()])
}

/// A storage name.
pub fn name(value: &str) -> Name {
    Name::new(value).unwrap_or_else(|error| panic!("`{value}` is a valid name: {error}"))
}

/// A deterministic UUIDv7 record id built from `n`.
pub fn rid(n: u64) -> RecordId {
    let mut bytes = [0u8; 16];
    bytes[0..8].copy_from_slice(&n.to_be_bytes());
    bytes[6] = 0x70 | (bytes[6] & 0x0f);
    bytes[8] = 0x80;
    RecordId::from_bytes(bytes).unwrap_or_else(|error| panic!("id {n} must be a UUIDv7: {error}"))
}

/// A new document of `doctype` with id `id`.
pub fn doc(doctype: &str, id: RecordId) -> Document {
    Document::new(name(doctype), id)
}

/// Sets a field from a value.
pub fn set(doc: &mut Document, field: &str, value: Value) {
    doc.set(name(field), value);
}

/// A child row with `fields`.
pub fn row(id: RecordId, fields: &[(&str, Value)]) -> ChildRow {
    let mut map = std::collections::BTreeMap::new();
    for (field, value) in fields {
        map.insert(name(field), value.clone());
    }
    ChildRow { id, fields: map }
}

/// Runs a future the in-memory writers never suspend to completion.
pub fn block_on<F: std::future::Future>(future: F) -> F::Output {
    use std::task::{Context, Poll, Waker};
    let mut future = Box::pin(future);
    let waker = Waker::noop();
    let mut context = Context::from_waker(waker);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => return value,
            Poll::Pending => std::thread::yield_now(),
        }
    }
}

/// A writer that refuses every change, to prove a failing edge write surfaces.
#[derive(Debug, Default)]
pub struct FailingWriter {
    /// How many calls have been attempted.
    pub calls: usize,
}

impl spot_edge_sync::EdgeWriter for FailingWriter {
    fn add<'a>(
        &'a mut self,
        _edge: &'a spot_edge_sync::Edge,
    ) -> spot_edge_sync::EdgeFuture<'a, ()> {
        Box::pin(async move {
            self.calls += 1;
            Err(spot_edge_sync::EdgeSyncError::WriteFailed {
                message: "test writer refuses every change".to_owned(),
            })
        })
    }

    fn remove<'a>(
        &'a mut self,
        _edge: &'a spot_edge_sync::Edge,
    ) -> spot_edge_sync::EdgeFuture<'a, ()> {
        self.add(_edge)
    }

    fn update<'a>(
        &'a mut self,
        _edge: &'a spot_edge_sync::Edge,
    ) -> spot_edge_sync::EdgeFuture<'a, ()> {
        self.add(_edge)
    }

    fn scan_by_source<'a>(
        &'a mut self,
        _source: &'a spot_storage_contract::NodeRef,
    ) -> spot_edge_sync::EdgeFuture<'a, Vec<spot_edge_sync::Edge>> {
        Box::pin(async move { Ok(Vec::new()) })
    }
}
