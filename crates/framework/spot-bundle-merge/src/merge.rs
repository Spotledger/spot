//! The three-way merge: item-wise over every section kind, with the section-aware semantic
//! checks layered on top.
//!
//! For one item key, the three versions are compared:
//!
//! - the same bytes on both sides — clean (taken, or dropped when both removed it);
//! - changed on one side only — that side is taken;
//! - changed differently — a [`Conflict`], left out of the merged bundle until
//!   [`resolve`](crate::resolve) supplies a choice.
//!
//! Every map is a `BTreeMap`, so the outcome is deterministic and independent of the order the
//! input bundles happened to carry their entries in.

use std::collections::{BTreeMap, BTreeSet};

use spot_custom_bundle::{Bundle, Files, Limits, SectionRegistry};
use spot_meta::Definition;

use crate::error::BundleMergeError;
use crate::items::{ItemMap, build_bundle, item_map};
use crate::model::{Conflict, ConflictKind, Edit, ItemVersion, MergeResult, Variants};
use crate::semantic;

/// Merges three bundle versions item-wise, returning the cleanly merged bundle and every conflict.
///
/// This is the brief's `merge3(base, ours, theirs)`. The section-aware checks that need the app
/// metadata (a `spot_meta::Registry::build` validation of the merged result) run only in
/// [`merge3_with`]; a bundle does not carry the app definitions, so this form runs the checks that
/// need no app metadata.
pub fn merge3(
    base: &Bundle,
    ours: &Bundle,
    theirs: &Bundle,
) -> Result<MergeResult, BundleMergeError> {
    merge3_with(base, ours, theirs, &[])
}

/// As [`merge3`], with the app definitions the merged metadata is validated against.
///
/// `definitions` are the apps' definitions **without** the site's customisations; the merged
/// customisations are rebuilt over them with `spot_meta::Registry::build`, and every error it
/// raises becomes a [`ConflictKind::Semantic`] conflict with
/// [`SemanticKind::InvalidMeta`](crate::SemanticKind::InvalidMeta). Pass an empty slice to skip
/// that check.
pub fn merge3_with(
    base: &Bundle,
    ours: &Bundle,
    theirs: &Bundle,
    definitions: &[Definition],
) -> Result<MergeResult, BundleMergeError> {
    let registry = SectionRegistry::new();
    let limits = Limits::default();

    let mut kinds: BTreeSet<String> = BTreeSet::new();
    for bundle in [base, ours, theirs] {
        kinds.extend(bundle.sections().keys().cloned());
    }

    let mut clean: BTreeMap<String, ItemMap> = BTreeMap::new();
    let mut unknown: BTreeMap<String, Files> = BTreeMap::new();
    let mut sides: BTreeMap<String, (ItemMap, ItemMap, ItemMap)> = BTreeMap::new();
    let mut conflicts: Vec<Conflict> = Vec::new();

    for kind in &kinds {
        let base_items = item_map(base, kind, &registry);
        let ours_items = item_map(ours, kind, &registry);
        let theirs_items = item_map(theirs, kind, &registry);
        let mut merged = ItemMap::new();
        merge_item_map(
            kind,
            &base_items,
            &ours_items,
            &theirs_items,
            &mut merged,
            &mut conflicts,
        );
        if registry.get(kind).is_some() {
            clean.insert(kind.clone(), merged);
            sides.insert(kind.clone(), (base_items, ours_items, theirs_items));
        } else {
            unknown.insert(kind.clone(), merged);
        }
    }

    semantic::check(&sides, &clean, definitions, &mut conflicts);

    for conflict in &mut conflicts {
        conflict.id = conflict_id(conflict);
    }
    conflicts.sort_by(|a, b| {
        (&a.primary.section, &a.primary.key, a.kind.slug()).cmp(&(
            &b.primary.section,
            &b.primary.key,
            b.kind.slug(),
        ))
    });
    let mut seen = BTreeSet::new();
    conflicts.retain(|conflict| seen.insert(conflict.id.clone()));

    let header = ours.header().clone();
    let merged = build_bundle(&header, &clean, &unknown, &limits)?;
    Ok(MergeResult {
        merged,
        conflicts,
        header,
        clean,
        unknown,
        limits,
    })
}

/// The stable id of a conflict: its section, primary key and kind slug.
pub(crate) fn conflict_id(conflict: &Conflict) -> String {
    format!(
        "{}:{}:{}",
        conflict.primary.section,
        conflict.primary.key,
        conflict.kind.slug()
    )
}

/// Merges one section's item maps into `clean`, pushing every conflict into `conflicts`.
fn merge_item_map(
    section: &str,
    base: &ItemMap,
    ours: &ItemMap,
    theirs: &ItemMap,
    clean: &mut ItemMap,
    conflicts: &mut Vec<Conflict>,
) {
    let mut keys: BTreeSet<&String> = BTreeSet::new();
    keys.extend(base.keys());
    keys.extend(ours.keys());
    keys.extend(theirs.keys());
    for key in keys {
        let b = base.get(key);
        let o = ours.get(key);
        let t = theirs.get(key);
        if o == t {
            if let Some(value) = o {
                clean.insert(key.clone(), value.clone());
            }
            continue;
        }
        if o == b {
            if let Some(value) = t {
                clean.insert(key.clone(), value.clone());
            }
            continue;
        }
        if t == b {
            if let Some(value) = o {
                clean.insert(key.clone(), value.clone());
            }
            continue;
        }
        let kind = if b.is_none() {
            ConflictKind::BothAddedDifferently
        } else if o.is_none() || t.is_none() {
            ConflictKind::ModifiedVsRemoved
        } else {
            ConflictKind::BothModified
        };
        conflicts.push(item_conflict(section, key, b, o, t, kind));
    }
}

/// Builds an item-level conflict with the three versions and the per-choice edits.
pub(crate) fn item_conflict(
    section: &str,
    key: &str,
    base: Option<&Vec<u8>>,
    ours: Option<&Vec<u8>>,
    theirs: Option<&Vec<u8>>,
    kind: ConflictKind,
) -> Conflict {
    let primary = ItemVersion {
        section: section.to_owned(),
        key: key.to_owned(),
        base: base.cloned(),
        ours: ours.cloned(),
        theirs: theirs.cloned(),
    };
    let edit = |value: Option<&Vec<u8>>| Edit {
        section: section.to_owned(),
        key: key.to_owned(),
        value: value.cloned(),
    };
    Conflict {
        id: String::new(),
        kind,
        primary,
        related: Vec::new(),
        explanation: explain(kind, section, key),
        variants: Variants {
            ours: vec![edit(ours)],
            theirs: vec![edit(theirs)],
            base: vec![edit(base)],
        },
        upgrade: None,
    }
}

/// A plain-language sentence for an item-level conflict.
fn explain(kind: ConflictKind, section: &str, key: &str) -> String {
    match kind {
        ConflictKind::BothModified => {
            format!("both sides changed `{section}/{key}` differently")
        }
        ConflictKind::ModifiedVsRemoved => {
            format!("one side changed `{section}/{key}` while the other removed it")
        }
        ConflictKind::BothAddedDifferently => {
            format!("both sides added `{section}/{key}` differently")
        }
        ConflictKind::Semantic(_) => {
            format!("a section-aware check found a conflict around `{section}/{key}`")
        }
        ConflictKind::Upgrade => {
            format!("a platform upgrade conflicts with `{section}/{key}`")
        }
    }
}
