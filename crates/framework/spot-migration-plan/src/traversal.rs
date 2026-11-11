//! The reverse index of a traversable Link field (WP-STO-07.T3, ADR-045, F2).
//!
//! `docs/research/graph-traversal-5m.md` measured a depth-3 traversal at 6.5 ms p95 with the
//! reverse direction indexed and 0.9 to 1.7 s without. A graph traversal that follows a Link
//! field incoming reads the referencing table by its link column, so that column needs a
//! btree. This module makes the plan carry it: whenever a Link field becomes traversable, the
//! plan adds the index (an online build, like every other index), so an app author never has
//! to remember it. A Link field that is not traversable gets none, and nothing else is planned
//! for traversal.
//!
//! # Inputs
//!
//! Traversability is not part of a field's definition, so it travels beside the
//! [`SystemDescription`] as a [`TraversalState`]: the Link fields declared traversable, and the
//! Link fields whose reverse index exists in that state. The caller builds the `from` state from
//! the live system and the `to` state from the target, and [`next_state`] gives the state to use
//! as `from` for the following plan.
//!
//! # Flow
//!
//! The [`TraversalState`] is not written by hand: [`crate::state_from_model`]`(model, indexed,
//! reserved_names)` builds it from the graph model ([`spot_graph_model::GraphModel`]). A Link
//! field is traversable exactly when the model derives a non-child edge over it, either the edge
//! of a Link field of a non-child DocType or one of the two endpoint fields of a relation
//! DocType; a field the model flags with a dangling target is left out. [`crate::plan_with_traversal`]
//! takes that state. The catalogue side takes the output of [`wanted`], maps each
//! [`TraversalIndex`] to a [`spot_meta::ReverseIndex`] (`table`, `column`, `name`), and passes
//! the list to [`spot_meta::EffectiveMeta::catalog_with_reverse_indexes`]. **The caller passes
//! the same list of reverse indexes to both sides.** If the plan and the catalogue are given
//! different lists they disagree about which indexes exist, and the plan flip-flops: it adds an
//! index the catalogue does not carry, then drops it again, then adds it again.
//!
//! # Open decision: Table, TableMultiselect and DynamicLink edges
//!
//! [`crate::state_from_model`] counts no edge over a `Table`, `TableMultiselect` or `DynamicLink`
//! field as traversable today. A child table is reached through its parent, and a dynamic link
//! has no single target DocType. This is a decision recorded by WP-STO-07.T5 that waits for the
//! lead; it is not a property of the design. Until it is decided, no reverse index is planned
//! for such a field, and a traversal that follows one of those edges incoming has no reverse
//! index to use.
//!
//! # Rules
//!
//! - **Add.** A field traversable in `to` whose index is not in `from` gets one index change.
//!   Re-planning from a state that already has the index plans nothing (idempotent). The
//!   change is skipped when the backend planner's schema steps already create the same index.
//! - **Drop, never in the same plan.** A field that stops being traversable keeps its index in
//!   the plan that changes the declaration; the plan after it (the field is then untraversable
//!   in `from` and in `to`) drops the index. Until then queries stay fast and a rollback of
//!   the declaration costs nothing.
//! - **Name.** The index name is derived from the DocType and the field only
//!   ([`index_name`]). Two links that map to the same name, or a name that a catalogue index
//!   already uses, is a plan error ([`crate::PlanError::TraversalNameClash`]).

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::change::{BlockingClass, Change, ChangeKind};
use crate::error::PlanError;
use crate::model::SystemDescription;
use crate::risk::{RiskTable, rules};
use crate::schema::SchemaPlan;

/// The longest storage identifier (PostgreSQL's limit).
const MAX_NAME_LEN: usize = 63;

/// The prefix of every traversal index name.
const PREFIX: &str = "gx";

/// The suffix of every traversal index name: it is the reverse direction.
const SUFFIX: &str = "rev";

/// One Link field: the DocType (table) that holds it and its name (column).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TraversalLink {
    /// The DocType (storage entity) that holds the field.
    pub doctype: String,
    /// The Link field.
    pub field: String,
}

impl TraversalLink {
    /// A link from a DocType and a field name.
    pub fn new(doctype: impl Into<String>, field: impl Into<String>) -> Self {
        Self {
            doctype: doctype.into(),
            field: field.into(),
        }
    }
}

/// What the plan needs to know about traversal in one state of a site.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraversalState {
    /// The Link fields declared traversable.
    #[serde(default)]
    pub traversable: Vec<TraversalLink>,
    /// The Link fields whose reverse index exists (or, for a target state, will exist).
    #[serde(default)]
    pub indexed: Vec<TraversalLink>,
    /// Index names the catalogue already uses for other purposes; a traversal index may not
    /// take one of them.
    #[serde(default)]
    pub reserved_names: Vec<String>,
}

/// One reverse index: where it goes and what it is called.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct TraversalIndex {
    /// The index name ([`index_name`]).
    pub name: String,
    /// The table (DocType) it is on.
    pub table: String,
    /// The indexed column: the Link field.
    pub column: String,
}

impl TraversalIndex {
    fn of(link: &TraversalLink) -> Self {
        Self {
            name: index_name(&link.doctype, &link.field),
            table: link.doctype.clone(),
            column: link.field.clone(),
        }
    }

    /// `table.name`, the target a plan change names.
    fn target(&self) -> String {
        format!("{}.{}", self.table, self.name)
    }
}

/// The deterministic name of the reverse index of `doctype.field`: `gx_<doctype>_<field>_rev`.
///
/// A name over the 63-byte limit is cut and given a hash of the full name, so it stays unique
/// and stable. The same inputs always give the same name.
pub fn index_name(doctype: &str, field: &str) -> String {
    let full = format!("{PREFIX}_{doctype}_{field}_{SUFFIX}");
    if full.len() <= MAX_NAME_LEN {
        return full;
    }
    let mut hash: u32 = 0x811c_9dc5;
    for byte in full.bytes() {
        hash = (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193);
    }
    // Names are ASCII storage names, so cutting at a byte offset is safe; if one is not, the
    // char boundary search keeps the cut valid.
    let mut cut = MAX_NAME_LEN - 9;
    while !full.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}_{hash:08x}", &full[..cut])
}

/// The reverse indexes a target state needs, sorted and checked.
///
/// # Errors
/// [`PlanError::BadInput`] when a traversable field names no DocType of `system`, no such field
/// or a field that is not a Link; [`PlanError::TraversalNameClash`] when two links (or a link and
/// a reserved catalogue index) map to the same index name.
pub fn wanted(
    system: &SystemDescription,
    state: &TraversalState,
) -> Result<Vec<TraversalIndex>, PlanError> {
    let mut by_name: BTreeMap<String, &TraversalLink> = BTreeMap::new();
    let reserved: BTreeSet<&str> = state.reserved_names.iter().map(String::as_str).collect();
    for link in &state.traversable {
        check_link(system, link)?;
        let name = index_name(&link.doctype, &link.field);
        if reserved.contains(name.as_str()) {
            return Err(clash(&name, link, None));
        }
        match by_name.get(&name) {
            Some(first) if **first != *link => return Err(clash(&name, link, Some(first))),
            _ => {
                by_name.insert(name, link);
            }
        }
    }
    Ok(by_name
        .values()
        .map(|link| TraversalIndex::of(link))
        .collect())
}

/// A name-clash error.
fn clash(name: &str, link: &TraversalLink, other: Option<&&TraversalLink>) -> PlanError {
    let second = match other {
        Some(other) => format!("`{}.{}`", other.doctype, other.field),
        None => "an index the catalogue already declares".to_owned(),
    };
    PlanError::TraversalNameClash {
        name: name.to_owned(),
        first: format!("`{}.{}`", link.doctype, link.field),
        second,
    }
}

/// A traversable field must be a Link field of a DocType the system has.
fn check_link(system: &SystemDescription, link: &TraversalLink) -> Result<(), PlanError> {
    let bad = |what: &str| PlanError::BadInput {
        message: format!(
            "`{}.{}` is declared traversable but {what}",
            link.doctype, link.field
        ),
    };
    let doctype = system
        .doctype(&link.doctype)
        .ok_or_else(|| bad("the DocType does not exist"))?;
    let field = doctype
        .definition
        .fields
        .iter()
        .find(|f| f.name == link.field)
        .ok_or_else(|| bad("the field does not exist"))?;
    if field.ty != spot_meta::FieldType::Link {
        return Err(bad("it is not a Link field"));
    }
    Ok(())
}

/// The links to build and the links to drop between two states.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Delta {
    add: Vec<TraversalIndex>,
    drop: Vec<TraversalIndex>,
}

fn delta(
    from_state: &TraversalState,
    to_system: &SystemDescription,
    to_state: &TraversalState,
) -> Result<Delta, PlanError> {
    let wanted_to = wanted(to_system, to_state)?;
    let indexed: BTreeSet<TraversalIndex> =
        from_state.indexed.iter().map(TraversalIndex::of).collect();
    let add = wanted_to
        .iter()
        .filter(|i| !indexed.contains(*i))
        .cloned()
        .collect();
    // The drop: indexed in `from`, not traversable in `from` (so the declaration was dropped by
    // an earlier plan) and not traversable in `to`.
    let traversable_from: BTreeSet<TraversalIndex> = from_state
        .traversable
        .iter()
        .map(TraversalIndex::of)
        .collect();
    let traversable_to: BTreeSet<&TraversalIndex> = wanted_to.iter().collect();
    let drop = indexed
        .iter()
        .filter(|i| !traversable_from.contains(*i) && !traversable_to.contains(*i))
        .cloned()
        .collect();
    Ok(Delta { add, drop })
}

/// The state to use as `from` for the plan after `from -> to`: the indexes that plan builds are
/// there, the indexes it drops are gone, and a field that just stopped being traversable still
/// has its index (the next plan drops it).
///
/// # Errors
/// The same errors as [`wanted`].
pub fn next_state(
    from_state: &TraversalState,
    to_system: &SystemDescription,
    to_state: &TraversalState,
) -> Result<TraversalState, PlanError> {
    let delta = delta(from_state, to_system, to_state)?;
    let dropped: BTreeSet<&TraversalIndex> = delta.drop.iter().collect();
    let mut indexed: BTreeSet<TraversalLink> = from_state
        .indexed
        .iter()
        .filter(|l| !dropped.contains(&TraversalIndex::of(l)))
        .cloned()
        .collect();
    indexed.extend(
        delta
            .add
            .iter()
            .map(|i| TraversalLink::new(i.table.clone(), i.column.clone())),
    );
    Ok(TraversalState {
        traversable: to_state.traversable.clone(),
        indexed: indexed.into_iter().collect(),
        reserved_names: to_state.reserved_names.clone(),
    })
}

/// The plan changes for the traversal indexes between two states, in the order the plan places
/// them (schema area).
///
/// A change is skipped when `schema` already creates (or drops) the same index, so a catalogue
/// that declares it as well is not planned twice.
///
/// # Errors
/// The same errors as [`wanted`].
pub(crate) fn changes(
    from_state: &TraversalState,
    to_system: &SystemDescription,
    to_state: &TraversalState,
    schema: &SchemaPlan,
    table: &RiskTable,
) -> Result<Vec<Change>, PlanError> {
    let delta = delta(from_state, to_system, to_state)?;
    let in_schema: BTreeSet<String> = schema
        .steps
        .iter()
        .map(|s| match &s.object {
            Some(object) => format!("{}.{object}", s.entity),
            None => s.entity.clone(),
        })
        .collect();
    let mut out = Vec::new();
    for index in &delta.add {
        if in_schema.contains(&index.target()) {
            continue;
        }
        out.push(
            Change::new(
                format!("schema/traversal_index/add:{}", index.target()),
                ChangeKind::SchemaAddIndex,
                rules::SCHEMA_ADD_INDEX,
                index.target(),
                format!(
                    "Create index `{}` on `{}` ({}) so a traversal can follow the link \
                     incoming.",
                    index.name, index.table, index.column
                ),
                table.risk_of(rules::SCHEMA_ADD_INDEX),
                BlockingClass::Online,
            )
            .evidence("absent", "present"),
        );
    }
    for index in &delta.drop {
        if in_schema.contains(&index.target()) {
            continue;
        }
        out.push(
            Change::new(
                format!("schema/traversal_index/drop:{}", index.target()),
                ChangeKind::SchemaDropIndex,
                rules::SCHEMA_DROP_TRAVERSAL_INDEX,
                index.target(),
                format!(
                    "Drop index `{}` on `{}`: the field is no longer traversable.",
                    index.name, index.table
                ),
                table.risk_of(rules::SCHEMA_DROP_TRAVERSAL_INDEX),
                BlockingClass::Online,
            )
            .evidence("present", "absent")
            .why(
                "an incoming traversal of this field costs a table scan per level without the \
                 index",
            ),
        );
    }
    Ok(out)
}
