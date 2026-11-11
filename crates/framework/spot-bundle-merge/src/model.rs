//! The merge model: the item-level three-way outcome, the conflicts it leaves, the resolution
//! choices a reviewer makes, and the result the merge returns.
//!
//! A merge is **item-wise**: for every section kind, [`spot_custom_bundle::Section::items`]
//! decomposes the section's files into a stable key to canonical bytes, and the three versions of
//! one key are compared. The same change on both sides is clean; a change on one side only is
//! taken; anything else is a [`Conflict`] carrying all three versions. Nothing is resolved
//! silently: a conflicted item is **absent** from [`MergeResult::merged`] until
//! [`resolve`](crate::resolve) supplies a choice for every conflict.

use std::collections::BTreeMap;

use spot_custom_bundle::{Bundle, BundleHeader, Files, Limits};

use crate::error::codes;
use crate::items::ItemMap;

/// One item's three versions: its canonical bytes on the base, ours and theirs side.
///
/// `None` means the item is absent on that side (added on the others, or removed).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ItemVersion {
    /// The section kind the item belongs to, for example `meta` or `scripts`.
    pub section: String,
    /// The item's stable key within its section (a custom field, an override, a layer, a script,
    /// a rule, a fixture DocType, or an unknown section's file path).
    pub key: String,
    /// The item's bytes on the base side, or `None` when absent.
    pub base: Option<Vec<u8>>,
    /// The item's bytes on our side, or `None` when absent.
    pub ours: Option<Vec<u8>>,
    /// The item's bytes on their side, or `None` when absent.
    pub theirs: Option<Vec<u8>>,
}

/// Why an item could not be merged cleanly.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ConflictKind {
    /// Both sides changed the item, differently.
    BothModified,
    /// One side changed the item the other side removed.
    ModifiedVsRemoved,
    /// Both sides added an item with the same key, differently.
    BothAddedDifferently,
    /// A conflict that only a section-aware check can see (see [`SemanticKind`]).
    Semantic(SemanticKind),
    /// A conflict a platform upgrade causes in the bundle, carried from `spot-migration-conflicts`.
    Upgrade,
}

impl ConflictKind {
    /// The stable `bundle_merge.*` code for this kind.
    pub fn code(self) -> &'static str {
        match self {
            Self::BothModified => codes::CONFLICT_BOTH_MODIFIED,
            Self::ModifiedVsRemoved => codes::CONFLICT_MODIFIED_VS_REMOVED,
            Self::BothAddedDifferently => codes::CONFLICT_BOTH_ADDED_DIFFERENTLY,
            Self::Semantic(kind) => kind.code(),
            Self::Upgrade => codes::UPGRADE_CONFLICT,
        }
    }

    /// A short lower-case name, the last part of a conflict id.
    pub fn slug(self) -> &'static str {
        match self {
            Self::BothModified => "both_modified",
            Self::ModifiedVsRemoved => "modified_vs_removed",
            Self::BothAddedDifferently => "both_added_differently",
            Self::Semantic(kind) => kind.slug(),
            Self::Upgrade => "upgrade",
        }
    }

    /// Whether this is a section-aware semantic conflict.
    pub fn is_semantic(self) -> bool {
        matches!(self, Self::Semantic(_))
    }
}

/// The section-aware conflict kinds: breaks that survive a clean item-wise merge because they
/// span more than one item.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SemanticKind {
    /// An override targets a custom field the other side removed.
    OverrideOnRemovedField,
    /// A custom field anchors (`insert_after`) after a field the merge removed.
    InsertAfterRemovedField,
    /// Two sides anchor different custom fields to the same predecessor, so the order is ambiguous.
    InsertAfterCollision,
    /// Two sides changed different scripts bound to the same hook point.
    ScriptHookCollision,
    /// The merged metadata fails `spot_meta::Registry::build`.
    InvalidMeta,
}

impl SemanticKind {
    /// The stable `bundle_merge.*` code for this kind.
    pub fn code(self) -> &'static str {
        match self {
            Self::OverrideOnRemovedField => codes::SEMANTIC_OVERRIDE_ON_REMOVED_FIELD,
            Self::InsertAfterRemovedField => codes::SEMANTIC_INSERT_AFTER_REMOVED_FIELD,
            Self::InsertAfterCollision => codes::SEMANTIC_INSERT_AFTER_COLLISION,
            Self::ScriptHookCollision => codes::SEMANTIC_SCRIPT_HOOK_COLLISION,
            Self::InvalidMeta => codes::SEMANTIC_INVALID_META,
        }
    }

    /// A short lower-case name, the last part of a conflict id.
    pub fn slug(self) -> &'static str {
        match self {
            Self::OverrideOnRemovedField => "override_on_removed_field",
            Self::InsertAfterRemovedField => "insert_after_removed_field",
            Self::InsertAfterCollision => "insert_after_collision",
            Self::ScriptHookCollision => "script_hook_collision",
            Self::InvalidMeta => "invalid_meta",
        }
    }
}

/// One edit a resolution applies to one item: set its bytes, or remove it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edit {
    /// The section kind the edit touches.
    pub section: String,
    /// The item key the edit touches.
    pub key: String,
    /// The bytes to set, or `None` to remove the item.
    pub value: Option<Vec<u8>>,
}

/// The edits each resolution choice applies, in order.
///
/// `ours` takes our side of every item the conflict covers, `theirs` their side, and `base` the
/// common ancestor. A [`Choice::Custom`] overrides `ours` for the conflict's primary item only.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Variants {
    /// The edits `Choice::Ours` applies.
    pub ours: Vec<Edit>,
    /// The edits `Choice::Theirs` applies.
    pub theirs: Vec<Edit>,
    /// The edits `Choice::Base` applies.
    pub base: Vec<Edit>,
}

/// The extra facts a platform-upgrade conflict carries through from `spot-migration-conflicts`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpgradeRef {
    /// The stable `migration_conflicts.*` code.
    pub code: String,
    /// The migration conflict category slug.
    pub category: String,
    /// The risk the category carries (`additive`, `needs_review`, `destructive`).
    pub risk: String,
    /// The release item that caused the conflict.
    pub release_item: String,
    /// The affected customisation pointers (`<kind>:<id>`).
    pub artefacts: Vec<String>,
}

/// One unresolved conflict: a root cause and every item it covers, with the three versions of each.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conflict {
    /// A stable id, unique in a result and unchanged between runs of the same inputs.
    pub id: String,
    /// Why it conflicts.
    pub kind: ConflictKind,
    /// The item the conflict is filed under (the item that changed, or the surviving item).
    pub primary: ItemVersion,
    /// Every other item the conflict covers (a removed field an override names, a sibling script
    /// on the same hook, and so on), sorted by key.
    pub related: Vec<ItemVersion>,
    /// One plain-language sentence a reviewer can read.
    pub explanation: String,
    /// The edits each resolution choice applies.
    pub variants: Variants,
    /// The upgrade facts, when [`ConflictKind::Upgrade`]; `None` otherwise.
    pub upgrade: Option<UpgradeRef>,
}

impl Conflict {
    /// The conflict's section kind.
    pub fn section(&self) -> &str {
        &self.primary.section
    }

    /// The conflict's primary item key.
    pub fn key(&self) -> &str {
        &self.primary.key
    }

    /// Every item the conflict covers: the primary first, then the related items.
    pub fn versions(&self) -> impl Iterator<Item = &ItemVersion> {
        std::iter::once(&self.primary).chain(self.related.iter())
    }
}

/// The outcome of a three-way merge: the cleanly merged bundle and every conflict left over.
///
/// `merged` carries only the items that merged cleanly; a conflicted item is absent until a
/// resolution supplies it, so nothing is chosen silently.
#[derive(Clone, Debug)]
pub struct MergeResult {
    /// The cleanly merged bundle (conflicted items omitted).
    pub merged: Bundle,
    /// The conflicts, sorted deterministically by section, key and kind.
    pub conflicts: Vec<Conflict>,
    /// The header the merged bundle is built from (our side's metadata).
    pub(crate) header: BundleHeader,
    /// The cleanly merged items of every known section kind.
    pub(crate) clean: BTreeMap<String, ItemMap>,
    /// The cleanly merged files of every section kind the registry does not know.
    pub(crate) unknown: BTreeMap<String, Files>,
    /// The limits the merged bundle is packed under.
    pub(crate) limits: Limits,
}

impl MergeResult {
    /// Whether the merge is clean (no conflicts).
    pub fn is_clean(&self) -> bool {
        self.conflicts.is_empty()
    }

    /// How many conflicts the merge left.
    pub fn conflict_count(&self) -> usize {
        self.conflicts.len()
    }

    /// The conflict with `id`, if any.
    pub fn conflict(&self, id: &str) -> Option<&Conflict> {
        self.conflicts.iter().find(|conflict| conflict.id == id)
    }

    /// The conflicts of one section kind, in result order.
    pub fn conflicts_in(&self, section: &str) -> Vec<&Conflict> {
        self.conflicts
            .iter()
            .filter(|conflict| conflict.section() == section)
            .collect()
    }

    /// A reviewable text report of every conflict, with context.
    pub fn render_text(&self) -> String {
        crate::report::render(self)
    }
}

/// How a reviewer resolves one conflict.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Choice {
    /// Take our side of every item the conflict covers.
    Ours,
    /// Take their side of every item the conflict covers.
    Theirs,
    /// Take the common ancestor of every item the conflict covers.
    Base,
    /// Replace the conflict's primary item with these bytes, leaving the other items as merged.
    Custom(Vec<u8>),
}

/// One reviewer's resolution: which conflict, which choice, who chose it and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolution {
    /// The id of the conflict being resolved.
    pub conflict: String,
    /// The choice.
    pub choice: Choice,
    /// Who made the choice (an agent id, a user, a role).
    pub by: String,
    /// An optional note recorded with the choice.
    pub note: Option<String>,
}

impl Resolution {
    /// A resolution of `conflict` with `choice`, chosen by `by`.
    pub fn new(conflict: impl Into<String>, choice: Choice, by: impl Into<String>) -> Self {
        Self {
            conflict: conflict.into(),
            choice,
            by: by.into(),
            note: None,
        }
    }

    /// Attaches a note to the resolution.
    pub fn with_note(mut self, note: impl Into<String>) -> Self {
        self.note = Some(note.into());
        self
    }
}

/// One line of the resolution log: who chose what, for which conflict.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolutionEntry {
    /// The conflict id.
    pub conflict: String,
    /// The conflict kind, so the log reads on its own.
    pub kind: ConflictKind,
    /// The choice that was applied.
    pub choice: Choice,
    /// Who made it.
    pub by: String,
    /// The note, if any.
    pub note: Option<String>,
}

/// The record of every resolution applied, in conflict order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResolutionLog {
    /// The entries, one per resolved conflict.
    pub entries: Vec<ResolutionEntry>,
}

impl ResolutionLog {
    /// How many conflicts the log resolves.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the log is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// A fully resolved merge: the complete bundle and the log of who chose what.
#[derive(Clone, Debug)]
pub struct Resolved {
    /// The complete bundle, every conflict resolved.
    pub bundle: Bundle,
    /// Who chose what.
    pub log: ResolutionLog,
}
