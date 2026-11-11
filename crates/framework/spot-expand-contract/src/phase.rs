//! The expand/migrate/contract classification of a plan's changes.
//!
//! ADR-22 says a schema change follows expand/contract: the shape grows first, and the old shape
//! is removed only after the code stopped using it. [`classify`] reads the plan engine's
//! [`ReleasePlan`](spot_migration_plan::ReleasePlan) and gives every schema-shaping change one of
//! three phases. It never re-diffs a schema and never invents a [`Change`](spot_migration_plan::Change):
//! it classifies the changes the planner already produced.
//!
//! | Phase | Change kinds |
//! | --- | --- |
//! | [`Expand`](Phase::Expand) | add a table, a column or an index; add a foreign key; widen a type; relax a constraint; clean up an index; finalise the catalogue; add a DocType or a field; change a default |
//! | [`Migrate`](Phase::Migrate) | a backfill; a validation sweep; a field that becomes optional |
//! | [`Contract`](Phase::Contract) | drop a table, a DocType or a field; rename a field; retype or change a link target; make a field required; set a column `NOT NULL`; a schema change the backend refused (the drop, rename, narrow or tighten it cannot do online) |
//!
//! Non-schema-shaping changes — patches, behaviour steps, permissions, protection, views and
//! naming — carry no phase: they publish metadata, they do not remove a storage object, so the
//! expand/contract gate has nothing to say about them.
//!
//! [`classify_change`] names every kind and has no wildcard arm, so adding a kind to
//! [`ChangeKind`] is a compile error here until its phase — or its explicit "no phase" — is
//! written down. A destructive kind can never be silently classified as a non-schema change.

use spot_migration_plan::risk::rules;
use spot_migration_plan::{Change, ChangeKind, ReleasePlan, Risk};

use crate::object::Object;

/// Where one change sits in the expand/contract sequence.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// The shape grows: an additive change old and new code both tolerate.
    Expand,
    /// Data moves into the expanded shape: a backfill or a validation sweep.
    Migrate,
    /// The old shape is removed: allowed only one release after the code stopped using it.
    Contract,
}

impl Phase {
    /// The phase's name, as a report spells it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Expand => "expand",
            Self::Migrate => "migrate",
            Self::Contract => "contract",
        }
    }

    /// Whether the phase removes an object and is therefore gated by [`crate::check`].
    #[must_use]
    pub fn is_contract(self) -> bool {
        matches!(self, Self::Contract)
    }
}

impl std::fmt::Display for Phase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One plan change with its phase and, for an object-shaping change, the object it acts on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PhaseChange {
    /// The change's stable id, from the plan.
    pub change_id: String,
    /// The change's target, as the plan spells it (`party` or `party.note`).
    pub target: String,
    /// The phase.
    pub phase: Phase,
    /// The risk the plan's rule table assigned (reused, never recomputed).
    pub risk: Risk,
    /// The table or column the change acts on, when it names one.
    pub object: Option<Object>,
}

/// The phase of one change, or `None` when the change does not shape storage.
///
/// The match names **every** [`ChangeKind`] and has no wildcard arm. That is deliberate: this is
/// the enforcement point for ADR-22, so a new kind — say another destructive schema change — must
/// fail to compile here until someone decides its phase, rather than being swept into "not a
/// schema change" by a `_ => None`. `clippy::wildcard_enum_match_arm` is denied on this function
/// so the wildcard cannot creep back in during a refactor.
#[deny(clippy::wildcard_enum_match_arm)]
#[must_use]
pub fn classify_change(change: &Change) -> Option<Phase> {
    match change.kind {
        // The shape grows.
        ChangeKind::SchemaAddEntity
        | ChangeKind::SchemaAddColumn
        | ChangeKind::SchemaAddIndex
        | ChangeKind::SchemaWidenType
        | ChangeKind::SchemaRelaxConstraint
        | ChangeKind::SchemaAddForeignKey
        | ChangeKind::SchemaCleanupIndex
        | ChangeKind::SchemaFinalise
        | ChangeKind::DocTypeAdded
        | ChangeKind::FieldAdded
        | ChangeKind::FieldDefaultChanged => Some(Phase::Expand),
        // Data moves into the expanded shape.
        ChangeKind::SchemaBackfill | ChangeKind::SchemaValidate => Some(Phase::Migrate),
        // The shape is removed or tightened.
        ChangeKind::SchemaSetNotNull
        | ChangeKind::SchemaDropIndex
        | ChangeKind::SchemaBlocked
        | ChangeKind::DocTypeRemoved
        | ChangeKind::FieldRemoved
        | ChangeKind::FieldRetyped
        | ChangeKind::FieldRenamed
        | ChangeKind::LinkTargetChanged => Some(Phase::Contract),
        // One kind covers both directions; the rule the plan recorded names which.
        ChangeKind::FieldRequiredChanged => {
            if change.rule == rules::METADATA_FIELD_REQUIRED_SET {
                Some(Phase::Contract)
            } else {
                Some(Phase::Expand)
            }
        }
        // A naming, protection, permission or view-layer change publishes metadata.
        ChangeKind::NamingChanged
        | ChangeKind::ProtectionChanged
        | ChangeKind::PermissionRuleAdded
        | ChangeKind::PermissionRuleRemoved
        | ChangeKind::ViewLayerChanged => None,
        // A patch change publishes data, not schema.
        ChangeKind::PatchAdded | ChangeKind::PatchMissing => None,
        // A behaviour change publishes steps, methods, schedules, capabilities or seam overrides.
        ChangeKind::StepAdded
        | ChangeKind::StepRemoved
        | ChangeKind::StepReordered
        | ChangeKind::StepReprioritised
        | ChangeKind::MethodAdded
        | ChangeKind::MethodRemoved
        | ChangeKind::MethodSignatureChanged
        | ChangeKind::ScheduleChanged
        | ChangeKind::CapabilityWidened
        | ChangeKind::SeamOverrideChanged => None,
    }
}

/// Every schema-shaping change of a plan, with its phase, in plan order.
#[must_use]
pub fn classify(plan: &ReleasePlan) -> Vec<PhaseChange> {
    plan.changes
        .iter()
        .filter_map(|change| {
            let phase = classify_change(change)?;
            Some(PhaseChange {
                change_id: change.id.clone(),
                target: change.target.clone(),
                phase,
                risk: change.risk,
                object: contract_object(change),
            })
        })
        .collect()
}

/// The object a change acts on, when it acts on a table, a column or an index.
///
/// A rename is the awkward one: the plan's target is the **new** name, and the object the
/// contract removes is the field's previous name, kept in the change's `before` evidence.
#[must_use]
pub fn contract_object(change: &Change) -> Option<Object> {
    if change.kind == ChangeKind::FieldRenamed {
        let doctype = change.target.split('.').next().unwrap_or_default();
        if let Some(previous) = change.before.as_deref().filter(|name| !name.is_empty()) {
            return Object::column(doctype, previous).ok();
        }
    }
    if matches!(
        change.kind,
        ChangeKind::SchemaDropIndex | ChangeKind::SchemaAddIndex | ChangeKind::SchemaCleanupIndex
    ) {
        // The target is `table.index_name`: an index, not a column.
        let (table, name) = change.target.split_once('.')?;
        return Object::index(table, name).ok();
    }
    Object::parse(&change.target).ok()
}
