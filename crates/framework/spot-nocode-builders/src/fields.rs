//! The palette a builder may offer its author: only fields the author may **read and report
//! on**, and only links into record types whose fields the author may report on.
//!
//! The offer is not a security boundary — the published report is checked again, for each
//! viewer, by the report engine's runner — it is what keeps a designer from building a report
//! that will only fail for them at run time. The permission core answers both questions:
//! `reportable_fields` for the fields (a `report` right and the field's protection groups),
//! and `filter_ok` for whether a field may also be grouped, filtered and summarised (the
//! no-inference rule needs an unmasked read).

use serde::{Deserialize, Serialize};

use spot_app_api::meta::FieldKind;
use spot_meta::EffectiveMeta;
use spot_permissions::{DocFacts, FieldSet, PermissionEngine, Principal};

/// How a field is shown in the palette, in terms a business user knows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OfferKind {
    /// Words.
    Text,
    /// A whole number.
    Int,
    /// An exact decimal number (money, quantities).
    Decimal,
    /// A calendar date.
    Date,
    /// Yes or no.
    Bool,
    /// A link to another record.
    Link,
    /// Anything else the builder shows but does not summarise beyond counting.
    Other,
}

impl OfferKind {
    /// The stable name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Int => "int",
            Self::Decimal => "decimal",
            Self::Date => "date",
            Self::Bool => "bool",
            Self::Link => "link",
            Self::Other => "other",
        }
    }

    /// The kind of an app-API field kind.
    #[must_use]
    pub fn of(kind: &FieldKind) -> Self {
        match kind {
            FieldKind::Bool => Self::Bool,
            FieldKind::Int { .. } => Self::Int,
            FieldKind::Decimal { .. } => Self::Decimal,
            FieldKind::Text { .. } | FieldKind::LongText { .. } | FieldKind::Select { .. } => {
                Self::Text
            }
            FieldKind::Date | FieldKind::Time | FieldKind::DateTime => Self::Date,
            FieldKind::Link { .. } | FieldKind::DynamicLink { .. } => Self::Link,
            _ => Self::Other,
        }
    }

    /// Whether a column of this kind can be summed or averaged.
    #[must_use]
    pub fn is_numeric(self) -> bool {
        matches!(self, Self::Int | Self::Decimal)
    }
}

/// One field the author may use.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldOffer {
    /// The path a column, filter or grouping names: `field`, or `alias.field` on a link.
    pub path: String,
    /// The heading a person reads.
    pub label: String,
    /// What kind of value it holds.
    pub kind: OfferKind,
    /// Whether it may be grouped, filtered or sorted (an unmasked read).
    pub groupable: bool,
    /// Whether it may be summed, averaged, counted, minimised or maximised.
    pub summarisable: bool,
}

impl FieldOffer {
    /// An offer for `path`.
    #[must_use]
    pub fn new(path: &str, label: &str, kind: OfferKind, groupable: bool) -> Self {
        Self {
            path: path.to_owned(),
            label: label.to_owned(),
            kind,
            groupable,
            summarisable: kind != OfferKind::Other,
        }
    }
}

/// A link the author may follow, with the fields it offers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinOffer {
    /// The alias column paths use.
    pub alias: String,
    /// The heading a person reads.
    pub label: String,
    /// The record type it points at.
    pub doctype: String,
    /// The link field on the record type being reported.
    pub on: String,
    /// The fields of the linked record the author may use.
    pub fields: Vec<FieldOffer>,
}

/// Everything a builder may offer for one base record type and one author.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldOffers {
    /// The base record type.
    pub doctype: String,
    /// Its heading.
    pub label: String,
    /// The base fields.
    pub fields: Vec<FieldOffer>,
    /// The links the author may follow.
    pub joins: Vec<JoinOffer>,
}

impl FieldOffers {
    /// No offers at all (the record type is unknown, or the author may report on nothing).
    #[must_use]
    pub fn empty(doctype: &str) -> Self {
        Self {
            doctype: doctype.to_owned(),
            label: String::new(),
            fields: Vec::new(),
            joins: Vec::new(),
        }
    }

    /// Whether the palette holds anything at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.fields.is_empty() && self.joins.is_empty()
    }

    /// The base field called `path`, when the palette offers it.
    #[must_use]
    pub fn field(&self, path: &str) -> Option<&FieldOffer> {
        self.fields.iter().find(|f| f.path == path)
    }

    /// The link under `alias`, when the palette offers it.
    #[must_use]
    pub fn join(&self, alias: &str) -> Option<&JoinOffer> {
        self.joins.iter().find(|j| j.alias == alias)
    }

    /// Whether the author may name `path` — `field`, or `alias.field` through a link.
    #[must_use]
    pub fn allows(&self, path: &str) -> bool {
        match path.split_once('.') {
            None => self.field(path).is_some(),
            Some((alias, field)) => self
                .join(alias)
                .is_some_and(|j| j.fields.iter().any(|f| f.path == field)),
        }
    }

    /// The offer for `path`, wherever it lives.
    #[must_use]
    pub fn get(&self, path: &str) -> Option<&FieldOffer> {
        match path.split_once('.') {
            None => self.field(path),
            Some((alias, field)) => self
                .join(alias)
                .and_then(|j| j.fields.iter().find(|f| f.path == field)),
        }
    }
}

/// The palette `principal` may use for reports over `doctype`.
///
/// An unknown record type, or a principal with no `read` and `report` rights on it, gives an
/// empty palette: a builder offers nothing rather than a field that would be refused later.
/// The synthetic `id` field is always offered — it is the record's identity, carries no
/// business data — so a report can always count rows.
#[must_use]
pub fn offers(
    engine: &PermissionEngine,
    meta: &EffectiveMeta,
    principal: &Principal,
    doctype: &str,
) -> FieldOffers {
    let Some(def) = meta.doctype(doctype) else {
        return FieldOffers::empty(doctype);
    };
    let facts = DocFacts {
        doctype: doctype.to_owned(),
        ..DocFacts::default()
    };
    let reportable = engine.reportable_fields(principal, doctype, &facts);
    let mut fields = vec![FieldOffer {
        path: "id".to_owned(),
        label: "Record".to_owned(),
        kind: OfferKind::Text,
        groupable: false,
        summarisable: true,
    }];
    let mut joins = Vec::new();
    for field in &def.fields {
        let name = field.name.as_str();
        if !reportable.contains(name) {
            continue;
        }
        let kind = OfferKind::of(&field.kind);
        let groupable = engine.filter_ok(principal, doctype, name);
        fields.push(FieldOffer::new(name, &field.label, kind, groupable));
        if let FieldKind::Link { doctype: target } = &field.kind {
            let linked = linked_offers(engine, meta, principal, target.as_str());
            if !linked.is_empty() {
                joins.push(JoinOffer {
                    alias: name.to_owned(),
                    label: field.label.clone(),
                    doctype: target.as_str().to_owned(),
                    on: name.to_owned(),
                    fields: linked,
                });
            }
        }
    }
    FieldOffers {
        doctype: doctype.to_owned(),
        label: def.label.clone(),
        fields,
        joins,
    }
}

/// The fields of a linked record type a principal may use, without following its links (one
/// hop is what a report builder offers; a deeper path is a hand-written report's business).
fn linked_offers(
    engine: &PermissionEngine,
    meta: &EffectiveMeta,
    principal: &Principal,
    doctype: &str,
) -> Vec<FieldOffer> {
    let Some(def) = meta.doctype(doctype) else {
        return Vec::new();
    };
    let facts = DocFacts {
        doctype: doctype.to_owned(),
        ..DocFacts::default()
    };
    let reportable: FieldSet = engine.reportable_fields(principal, doctype, &facts);
    def.fields
        .iter()
        .filter(|f| reportable.contains(f.name.as_str()))
        .filter(|f| {
            !matches!(
                f.kind,
                FieldKind::Table { .. } | FieldKind::TableMultiselect { .. }
            )
        })
        .map(|f| {
            FieldOffer::new(
                f.name.as_str(),
                &f.label,
                OfferKind::of(&f.kind),
                engine.filter_ok(principal, doctype, f.name.as_str()),
            )
        })
        .collect()
}
