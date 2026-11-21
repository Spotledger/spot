//! The catalog-wide preset lints L1 to L7 of ADR-063 section 6 (WP-SYN-01.T2).
//!
//! Inconsistent preset policy is rejected when it is written, with an explanation. [`lint_presets`]
//! checks a [`PresetTable`] against the effective metadata and returns **every** problem, sorted
//! and without duplicates; [`lint_preset_entity`] returns the findings that involve one DocType
//! (as the subject or as the target of a link), which is what the metadata save path calls.
//!
//! | Code | Lint | Rejects |
//! | --- | --- | --- |
//! | `sync_policy.lint_closed_link` | L1 | a synced entity linking to a `local` entity |
//! | `sync_policy.lint_dynamic_link_undeclared` | L1 | a dynamic link on a synced entity with no `allowed_targets` |
//! | `sync_policy.lint_master_depends_on_branch` | L2 | a `master` linking to a `record` or `handover` entity unless the link is `requires_adopted` |
//! | `sync_policy.lint_excluded_required` | L3 | an excluded field that is required and has no default |
//! | `sync_policy.lint_excluded_unknown_field` | L3 | an excluded field the entity does not have |
//! | `sync_policy.lint_scope_field` | L4 | a selector field that is not a link, enum or key-typed field |
//! | `sync_policy.lint_child_policy` | L5 | a child table that declares a synced policy |
//! | `sync_policy.lint_derived_synced` | L5 | a derived entity that syncs |
//! | `sync_policy.lint_unique_key` | L6 | a unique key on a `record` or `handover` entity that collides between nodes |
//! | `sync_policy.lint_reach_gap` | L7 | a `master` whose referenced master is not reached by every node the referrer reaches |
//! | `sync_policy.lint_reach_undecided` | L7 | the same, when the reach of two selectors cannot be compared statically |
//! | `sync_policy.lint_requires_adopted_record` | S2 | a `requires_adopted` link to a `record` entity, which is never adopted |
//! | `sync_policy.lint_adopt_never` | S2 | a `handover` entity with `adopt = never` |
//!
//! Metadata the lints need that `spot-meta` does not carry yet (derived flag, `allowed_targets`,
//! `requires_adopted`, origin-scoped uniqueness, key-typed fields, reference-carried links) comes
//! through the [`MetaExtras`] argument; [`PlainExtras`] is a plain-data implementation.
//!
//! The lints read no clock, no random source and no I/O; the output depends only on the inputs.

mod adoption;
mod entity;
mod extras;
mod links;
mod reach;

use std::collections::BTreeMap;

use spot_meta::{Definition, EffectiveMeta};

use crate::lint::PolicyProblem;
use crate::preset::{Preset, PresetPolicy};

pub use extras::{KeyScope, MetaExtras, PlainExtras};
pub use links::Edge;

/// The stable dotted codes of the preset lints. Each is catalogued in `sync_policy.toml`.
pub mod codes {
    /// L1: a synced entity links to a `local` entity.
    pub const LINT_CLOSED_LINK: &str = "sync_policy.lint_closed_link";
    /// L1: a dynamic link on a synced entity declares no `allowed_targets`.
    pub const LINT_DYNAMIC_LINK_UNDECLARED: &str = "sync_policy.lint_dynamic_link_undeclared";
    /// L2: a `master` links to a `record` or `handover` entity without `requires_adopted`.
    pub const LINT_MASTER_DEPENDS_ON_BRANCH: &str = "sync_policy.lint_master_depends_on_branch";
    /// L3: an excluded field is required and has no default.
    pub const LINT_EXCLUDED_REQUIRED: &str = "sync_policy.lint_excluded_required";
    /// L3: an excluded field is not a field of the entity.
    pub const LINT_EXCLUDED_UNKNOWN_FIELD: &str = "sync_policy.lint_excluded_unknown_field";
    /// L4: a selector reads a field that cannot name a key.
    pub const LINT_SCOPE_FIELD: &str = "sync_policy.lint_scope_field";
    /// L5: a child table declares a synced policy of its own.
    pub const LINT_CHILD_POLICY: &str = "sync_policy.lint_child_policy";
    /// L5: a derived entity has a synced policy.
    pub const LINT_DERIVED_SYNCED: &str = "sync_policy.lint_derived_synced";
    /// L6: a unique key on a `record` or `handover` entity can collide between nodes.
    pub const LINT_UNIQUE_KEY: &str = "sync_policy.lint_unique_key";
    /// L7: a referenced master is definitely not reached by every node the referrer reaches.
    pub const LINT_REACH_GAP: &str = "sync_policy.lint_reach_gap";
    /// L7: whether a referenced master is reached by every node the referrer reaches cannot be
    /// decided from the selectors.
    pub const LINT_REACH_UNDECIDED: &str = "sync_policy.lint_reach_undecided";
    /// S2: a link marked `requires_adopted` points to a `record` entity, which is never adopted.
    pub const LINT_REQUIRES_ADOPTED_RECORD: &str = "sync_policy.lint_requires_adopted_record";
    /// S2: a `handover` entity sets `adopt = never`.
    pub const LINT_ADOPT_NEVER: &str = "sync_policy.lint_adopt_never";
}

/// The declared preset policies by DocType. A DocType that is absent is `local` (nothing syncs
/// unless declared, ADR-062 rule 1).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PresetTable {
    policies: BTreeMap<String, PresetPolicy>,
}

impl PresetTable {
    /// An empty table: everything is `local`.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Declares (or replaces) the policy of `doctype`.
    pub fn set(&mut self, doctype: &str, policy: PresetPolicy) {
        self.policies.insert(doctype.to_owned(), policy);
    }

    /// The declared policy of `doctype`, if any.
    #[must_use]
    pub fn get(&self, doctype: &str) -> Option<&PresetPolicy> {
        self.policies.get(doctype)
    }

    /// The preset in force for `doctype`: the declared one, else [`Preset::Local`].
    #[must_use]
    pub fn preset_of(&self, doctype: &str) -> Preset {
        self.get(doctype).map_or(Preset::Local, |p| p.preset)
    }

    /// Every declared policy, in DocType order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &PresetPolicy)> {
        self.policies.iter().map(|(k, v)| (k.as_str(), v))
    }
}

/// A finding with the DocTypes it involves (the subject first), for [`lint_preset_entity`].
pub(crate) struct Finding {
    pub(crate) problem: PolicyProblem,
    pub(crate) involves: Vec<String>,
}

impl Finding {
    pub(crate) fn new(
        code: &'static str,
        subject: &str,
        detail: impl Into<String>,
        others: &[&str],
    ) -> Self {
        let mut involves = vec![subject.to_owned()];
        involves.extend(others.iter().map(|d| (*d).to_owned()));
        Self {
            problem: PolicyProblem::new(code, subject, detail),
            involves,
        }
    }
}

/// What every lint reads: the definitions by name, the declared policies and the extras.
pub(crate) struct Ctx<'a> {
    pub(crate) defs: BTreeMap<&'a str, &'a Definition>,
    pub(crate) table: &'a PresetTable,
    pub(crate) extras: &'a dyn MetaExtras,
}

impl<'a> Ctx<'a> {
    fn new(table: &'a PresetTable, meta: &'a EffectiveMeta, extras: &'a dyn MetaExtras) -> Self {
        let defs = meta
            .definitions()
            .iter()
            .map(|d| (d.name.as_str(), d))
            .collect();
        Self {
            defs,
            table,
            extras,
        }
    }

    /// The definitions that are not child tables, in name order: the entities a policy applies to.
    pub(crate) fn roots(&self) -> impl Iterator<Item = &'a Definition> + '_ {
        self.defs.values().copied().filter(|d| !d.is_child)
    }

    pub(crate) fn preset(&self, doctype: &str) -> Preset {
        self.table.preset_of(doctype)
    }
}

fn findings(table: &PresetTable, meta: &EffectiveMeta, extras: &dyn MetaExtras) -> Vec<Finding> {
    let ctx = Ctx::new(table, meta, extras);
    let mut out = Vec::new();
    entity::lint_entities(&ctx, &mut out);
    links::lint_links(&ctx, &mut out);
    reach::lint_reach(&ctx, &mut out);
    adoption::lint_adoption(&ctx, &mut out);
    out
}

fn sorted(mut problems: Vec<PolicyProblem>) -> Vec<PolicyProblem> {
    problems.sort_by(|a, b| {
        (a.doctype(), a.code(), a.detail()).cmp(&(b.doctype(), b.code(), b.detail()))
    });
    problems.dedup();
    problems
}

/// Every preset lint problem in `table`, checked against `meta`, all at once. The output is
/// sorted by DocType, code and detail, without duplicates, and depends only on the inputs.
#[must_use]
pub fn lint_presets(
    table: &PresetTable,
    meta: &EffectiveMeta,
    extras: &dyn MetaExtras,
) -> Vec<PolicyProblem> {
    sorted(
        findings(table, meta, extras)
            .into_iter()
            .map(|f| f.problem)
            .collect(),
    )
}

/// The preset lint problems that involve `doctype`: it is the subject of the finding, or the
/// target of the link the finding is about. The metadata save path calls this for the DocType
/// being saved. Sorted like [`lint_presets`].
#[must_use]
pub fn lint_preset_entity(
    table: &PresetTable,
    meta: &EffectiveMeta,
    extras: &dyn MetaExtras,
    doctype: &str,
) -> Vec<PolicyProblem> {
    sorted(
        findings(table, meta, extras)
            .into_iter()
            .filter(|f| f.involves.iter().any(|d| d == doctype))
            .map(|f| f.problem)
            .collect(),
    )
}
