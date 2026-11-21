//! The patch contract's shape changes and the check that each one has an upcaster.
//!
//! A patch that changes the stored shape of an entity must ship the upcaster that rewrites
//! images written before the change (ADR-061 s6). The patch contract declares its shape changes
//! as [`ShapeChange`] rows; [`check_patch`] reports every row that needs an upcaster and has none,
//! which is what CI will fail on.

use serde::{Deserialize, Serialize};

use crate::registry::Registry;

/// The kinds of stored-shape change a patch can declare.
///
/// The variant names are the ones `ChangeKind` uses in `crates/framework/spot-migration-plan`
/// (`FieldAdded`, `FieldRenamed`, `FieldRetyped`, ...): a shape change is described in the same
/// words wherever it appears. The two enums cannot be the same type — this crate depends on
/// `serde`, `serde_json` and `thiserror` only — so the spelling is pinned by a test.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShapeChangeKind {
    /// An entity appears. Nothing older exists to rewrite: additive.
    DocTypeAdded,
    /// An entity disappears. Its old images need a step that says what becomes of them.
    DocTypeRemoved,
    /// A field appears. Additive: old images simply lack it, and the reader fills the default.
    FieldAdded,
    /// A field disappears.
    FieldRemoved,
    /// A field is renamed, keeping its value.
    FieldRenamed,
    /// A field's type changes, so its stored value is encoded differently.
    FieldRetyped,
    /// A field becomes required, so old images need a value they never stored.
    FieldRequiredChanged,
    /// A link field's target changes, so stored ids mean another entity.
    LinkTargetChanged,
}

impl ShapeChangeKind {
    /// The kind's name, as the patch contract writes it (snake case, like the wire form).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DocTypeAdded => "doc_type_added",
            Self::DocTypeRemoved => "doc_type_removed",
            Self::FieldAdded => "field_added",
            Self::FieldRemoved => "field_removed",
            Self::FieldRenamed => "field_renamed",
            Self::FieldRetyped => "field_retyped",
            Self::FieldRequiredChanged => "field_required_changed",
            Self::LinkTargetChanged => "link_target_changed",
        }
    }

    /// Whether this kind changes the stored shape of images that already exist, so that a patch
    /// declaring it needs an upcaster.
    ///
    /// Additive kinds do not: a new entity has no older images, and a new field is absent from
    /// the old ones, which the reader fills from the declared default. Everything else rewrites
    /// or removes something an image holds, so an image written before the change cannot be read
    /// under the new schema without a step.
    #[must_use]
    pub fn needs_upcaster(self) -> bool {
        !matches!(self, Self::DocTypeAdded | Self::FieldAdded)
    }
}

/// One shape change a patch declares: entity `entity` moved from schema `from` to schema `to` in
/// the way `kind` names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShapeChange {
    /// The entity whose stored shape changes.
    pub entity: String,
    /// The catalog version before the change.
    pub from: u64,
    /// The catalog version after the change.
    pub to: u64,
    /// How the stored shape changes.
    pub kind: ShapeChangeKind,
}

impl ShapeChange {
    /// A shape change of `entity` from `from` to `to`.
    #[must_use]
    pub fn new(entity: impl Into<String>, from: u64, to: u64, kind: ShapeChangeKind) -> Self {
        Self {
            entity: entity.into(),
            from,
            to,
            kind,
        }
    }
}

/// One shape change that has no upcaster covering it.
///
/// A finding names the entity, the change's range and the version the chain stops at, so the
/// patch author knows exactly which step is missing and which entity needs it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckFinding {
    /// The entity whose shape change is uncovered.
    entity: String,
    /// The catalog version before the change.
    from: u64,
    /// The catalog version after the change.
    to: u64,
    /// How the stored shape changes.
    kind: ShapeChangeKind,
    /// The version the chain stops at: the lower end of the first missing step.
    missing_from: u64,
    /// The version that step has to reach: the change's own target.
    missing_to: u64,
}

impl CheckFinding {
    /// The entity whose shape change is uncovered.
    #[must_use]
    pub fn entity(&self) -> &str {
        &self.entity
    }

    /// The catalog version before the change.
    #[must_use]
    pub fn from(&self) -> u64 {
        self.from
    }

    /// The catalog version after the change.
    #[must_use]
    pub fn to(&self) -> u64 {
        self.to
    }

    /// How the stored shape changes.
    #[must_use]
    pub fn kind(&self) -> ShapeChangeKind {
        self.kind
    }

    /// The lower end of the first missing step.
    #[must_use]
    pub fn missing_from(&self) -> u64 {
        self.missing_from
    }

    /// The version that missing step has to reach.
    #[must_use]
    pub fn missing_to(&self) -> u64 {
        self.missing_to
    }
}

impl std::fmt::Display for CheckFinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "entity {}: shape change {} from schema {} to schema {} has no upcaster covering schema {} to schema {}",
            self.entity,
            self.kind.as_str(),
            self.from,
            self.to,
            self.missing_from,
            self.missing_to
        )
    }
}

/// Every declared shape change that has no upcaster covering its version range.
///
/// An empty result means the patch may ship: every change that rewrites or removes a stored
/// value has a chain of registered steps from its `from` version to its `to` version.
///
/// * An additive change ([`ShapeChangeKind::FieldAdded`], [`ShapeChangeKind::DocTypeAdded`]) is
///   skipped: old images read under the new schema unchanged.
/// * A change whose `from` is not below its `to` asks for no rewrite and is skipped; the patch
///   contract owns refusing a backwards change.
/// * The chain is checked as a whole, so a change covered by two consecutive registered steps
///   passes, and one covered by neither reports the first version it cannot cross.
#[must_use]
pub fn check_patch(registry: &Registry, changes: &[ShapeChange]) -> Vec<CheckFinding> {
    let mut findings = Vec::new();
    for change in changes {
        if !change.kind.needs_upcaster() || change.from >= change.to {
            continue;
        }
        if let Some(missing_from) = registry.missing_step(&change.entity, change.from, change.to) {
            findings.push(CheckFinding {
                entity: change.entity.clone(),
                from: change.from,
                to: change.to,
                kind: change.kind,
                missing_from,
                missing_to: change.to,
            });
        }
    }
    findings
}
