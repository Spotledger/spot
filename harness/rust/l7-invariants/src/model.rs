//! The storage model the platform-level L7 suite runs on.
//!
//! The framework promises its invariants for any catalog, so the generic invariants (child,
//! link and unique-key integrity) read the catalog they are given. The workload itself needs a
//! concrete one: this small model has every shape the invariants speak about and no business
//! meaning.
//!
//! - `l7_party`: a link target with two unique keys (`name`, `marker`).
//! - `l7_order`: a document with child rows (`lines`), a database-checked link (`party`), an
//!   engine-only link (`watch`, which may dangle after the target's committed delete), and a
//!   unique `marker`.
//! - `l7_counter`: a hot document that many workers increment.
//! - `l7_ticket`: one row per gapless-sequence number handed out, unique per `(series, number)`.

use std::sync::Arc;

use spot_storage_contract::{
    Catalog, ChildTableDef, Collation, EntityDef, EntityKind, FieldDef, FieldType, IndexDef,
    LinkCheck, LinkTarget, Name, StorageResult,
};

/// The catalog and the names the driver and the built-in invariants use.
#[derive(Clone, Debug)]
pub struct Model {
    /// The catalog every transaction of a run uses.
    pub catalog: Arc<Catalog>,
    /// Entity `l7_party`.
    pub party: Name,
    /// Entity `l7_order`.
    pub order: Name,
    /// Child entity `l7_order_line`.
    pub line: Name,
    /// Entity `l7_counter`.
    pub counter: Name,
    /// Entity `l7_ticket`.
    pub ticket: Name,
    /// Field `name` of a party.
    pub name: Name,
    /// Field `marker` (parties, orders, tickets): a unique client-supplied value.
    pub marker: Name,
    /// Order link field `party` (database-checked).
    pub party_link: Name,
    /// Order link field `watch` (engine-only).
    pub watch_link: Name,
    /// Order field `total`.
    pub total: Name,
    /// Order field `tag`.
    pub tag: Name,
    /// Order child-table field `lines`.
    pub lines: Name,
    /// Line field `item`.
    pub item: Name,
    /// Line field `qty`.
    pub qty: Name,
    /// Counter field `label`.
    pub label: Name,
    /// Counter field `value`.
    pub value: Name,
    /// Ticket field `series`.
    pub series: Name,
    /// Ticket field `number`.
    pub number: Name,
    /// The outbox topic of a transaction's marker event.
    pub topic_commit: Name,
    /// The outbox topic of extra events.
    pub topic_note: Name,
}

impl Model {
    /// Builds the model.
    pub fn new() -> StorageResult<Model> {
        let n = Name::new;
        let text = |max: u32| FieldType::Text {
            max_chars: Some(max),
        };
        let field = |name: &str, ty: FieldType, nullable: bool| -> StorageResult<FieldDef> {
            Ok(FieldDef {
                name: n(name)?,
                ty,
                nullable,
            })
        };
        let link = |target: &str, check: LinkCheck| -> StorageResult<FieldType> {
            Ok(FieldType::Link {
                target: LinkTarget::Entity(n(target)?),
                check,
            })
        };
        let index = |name: &str, fields: &[&str], unique: bool| -> StorageResult<IndexDef> {
            Ok(IndexDef {
                name: n(name)?,
                fields: fields
                    .iter()
                    .map(|f| n(f))
                    .collect::<StorageResult<Vec<_>>>()?,
                unique,
                method: spot_storage_contract::IndexMethod::BTree,
                opclass: None,
                include: vec![],
            })
        };
        let catalog = Catalog::new(
            1,
            Collation::new("en")?,
            vec![
                EntityDef {
                    name: n("l7_party")?,
                    kind: EntityKind::Document,
                    fields: vec![
                        field("name", text(140), false)?,
                        field("marker", text(80), false)?,
                    ],
                    children: vec![],
                    indexes: vec![
                        index("l7_party_name", &["name"], true)?,
                        index("l7_party_marker", &["marker"], true)?,
                    ], exclusions: vec![], search: None, },
                EntityDef {
                    name: n("l7_order")?,
                    kind: EntityKind::Document,
                    fields: vec![
                        field("marker", text(80), false)?,
                        field("party", link("l7_party", LinkCheck::Database)?, true)?,
                        field(
                            "watch",
                            link(
                                "l7_party",
                                LinkCheck::EngineOnly {
                                    reason: "a watch outlives its target; the engine decides what a dangling watch means".into(),
                                },
                            )?,
                            true,
                        )?,
                        field("total", FieldType::Int, false)?,
                        field("tag", text(40), true)?,
                    ],
                    children: vec![ChildTableDef {
                        field: n("lines")?,
                        entity: n("l7_order_line")?,
                    }],
                    indexes: vec![
                        index("l7_order_marker", &["marker"], true)?,
                        index("l7_order_party", &["party"], false)?,
                    ], exclusions: vec![], search: None, },
                EntityDef {
                    name: n("l7_order_line")?,
                    kind: EntityKind::Child {
                        parent: n("l7_order")?,
                    },
                    fields: vec![
                        field("item", text(80), false)?,
                        field("qty", FieldType::Int, false)?,
                    ],
                    children: vec![],
                    indexes: vec![index("l7_order_line_parent", &["parent", "idx"], false)?], exclusions: vec![], search: None, },
                EntityDef {
                    name: n("l7_counter")?,
                    kind: EntityKind::Document,
                    fields: vec![
                        field("label", text(40), false)?,
                        field("value", FieldType::Int, false)?,
                    ],
                    children: vec![],
                    indexes: vec![index("l7_counter_label", &["label"], true)?], exclusions: vec![], search: None, },
                EntityDef {
                    name: n("l7_ticket")?,
                    kind: EntityKind::Document,
                    fields: vec![
                        field("series", text(80), false)?,
                        field("number", FieldType::Int, false)?,
                        field("marker", text(80), false)?,
                    ],
                    children: vec![],
                    indexes: vec![index("l7_ticket_series_number", &["series", "number"], true)?], exclusions: vec![], search: None, },
            ],
        )?;
        Ok(Model {
            catalog: Arc::new(catalog),
            party: n("l7_party")?,
            order: n("l7_order")?,
            line: n("l7_order_line")?,
            counter: n("l7_counter")?,
            ticket: n("l7_ticket")?,
            name: n("name")?,
            marker: n("marker")?,
            party_link: n("party")?,
            watch_link: n("watch")?,
            total: n("total")?,
            tag: n("tag")?,
            lines: n("lines")?,
            item: n("item")?,
            qty: n("qty")?,
            label: n("label")?,
            value: n("value")?,
            series: n("series")?,
            number: n("number")?,
            topic_commit: n("l7_commit")?,
            topic_note: n("l7_note")?,
        })
    }

    /// The document entities (not the child entity) of the model.
    pub fn document_entities(&self) -> [&Name; 4] {
        [&self.party, &self.order, &self.counter, &self.ticket]
    }
}
