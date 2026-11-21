//! The change model: one typed, evidenced change per difference the plan found.
//!
//! A [`Change`] carries a stable `id`, the [`Area`] and [`ChangeKind`] it belongs to, the thing
//! it affects, a one-line description for a human, the evidence (`before` and `after` values,
//! never row data), the [`Risk`] the rule table assigned, the reason a review or a refusal
//! gives, and — where one exists — the safe alternative.

use serde::{Deserialize, Serialize};

use crate::risk::Risk;

/// Which of the four diffs a change came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Area {
    /// The storage schema (from the backend planner).
    Schema,
    /// The effective metadata.
    Metadata,
    /// The patches.
    Patch,
    /// The behaviour (steps, methods, schedules, capabilities, seam overrides).
    Behaviour,
}

impl Area {
    /// The area's name, as it is grouped in the rendered plan.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Schema => "schema",
            Self::Metadata => "metadata",
            Self::Patch => "patches",
            Self::Behaviour => "behaviour",
        }
    }
}

/// How a change affects users, for the duration estimate.
///
/// Ordered from least to most disruptive; a plan's blocking class is the maximum over its
/// changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockingClass {
    /// Runs while users work; no lock a user notices.
    Online,
    /// A brief lock: users wait, the site does not close.
    BriefLock,
    /// Needs a maintenance window: users are held out.
    MaintenanceWindow,
}

/// The typed kind of one change.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    /// Create a table.
    SchemaAddEntity,
    /// Add a column.
    SchemaAddColumn,
    /// Create an index.
    SchemaAddIndex,
    /// Widen a numeric column.
    SchemaWidenType,
    /// Relax a constraint.
    SchemaRelaxConstraint,
    /// Add a foreign key.
    SchemaAddForeignKey,
    /// Validate existing records against a constraint.
    SchemaValidate,
    /// Clean up a failed index build.
    SchemaCleanupIndex,
    /// Drop a live index that no running code needs any more (a traversal index whose link is
    /// no longer traversable). Removes capacity, so it is a Contract change.
    SchemaDropIndex,
    /// Record the catalogue version.
    SchemaFinalise,
    /// Fill a new required column in batches.
    SchemaBackfill,
    /// Make a column required.
    SchemaSetNotNull,
    /// A schema change the backend refused.
    SchemaBlocked,
    /// A DocType appears.
    DocTypeAdded,
    /// A DocType disappears.
    DocTypeRemoved,
    /// A field appears.
    FieldAdded,
    /// A field disappears.
    FieldRemoved,
    /// A field's type changes.
    FieldRetyped,
    /// A field is renamed (with a hint).
    FieldRenamed,
    /// A field's required flag changes.
    FieldRequiredChanged,
    /// A field's default changes.
    FieldDefaultChanged,
    /// A DocType's naming changes.
    NamingChanged,
    /// A link field's target changes.
    LinkTargetChanged,
    /// A protection group or a mask changes.
    ProtectionChanged,
    /// A permission rule appears.
    PermissionRuleAdded,
    /// A permission rule disappears.
    PermissionRuleRemoved,
    /// A view layer changes.
    ViewLayerChanged,
    /// A patch appears.
    PatchAdded,
    /// A patch is applied but absent from the new system (a downgrade).
    PatchMissing,
    /// A lifecycle step appears.
    StepAdded,
    /// A lifecycle step disappears.
    StepRemoved,
    /// Lifecycle steps are reordered.
    StepReordered,
    /// A lifecycle step's priority changes.
    StepReprioritised,
    /// A method appears.
    MethodAdded,
    /// A method disappears.
    MethodRemoved,
    /// A method's argument schema changes in a breaking way.
    MethodSignatureChanged,
    /// A schedule changes.
    ScheduleChanged,
    /// A manifest capability widens.
    CapabilityWidened,
    /// A seam override target changes.
    SeamOverrideChanged,
}

impl ChangeKind {
    /// The area this kind belongs to.
    pub fn area(self) -> Area {
        match self {
            Self::SchemaAddEntity
            | Self::SchemaAddColumn
            | Self::SchemaAddIndex
            | Self::SchemaWidenType
            | Self::SchemaRelaxConstraint
            | Self::SchemaAddForeignKey
            | Self::SchemaValidate
            | Self::SchemaCleanupIndex
            | Self::SchemaDropIndex
            | Self::SchemaFinalise
            | Self::SchemaBackfill
            | Self::SchemaSetNotNull
            | Self::SchemaBlocked => Area::Schema,
            Self::DocTypeAdded
            | Self::DocTypeRemoved
            | Self::FieldAdded
            | Self::FieldRemoved
            | Self::FieldRetyped
            | Self::FieldRenamed
            | Self::FieldRequiredChanged
            | Self::FieldDefaultChanged
            | Self::NamingChanged
            | Self::LinkTargetChanged
            | Self::ProtectionChanged
            | Self::PermissionRuleAdded
            | Self::PermissionRuleRemoved
            | Self::ViewLayerChanged => Area::Metadata,
            Self::PatchAdded | Self::PatchMissing => Area::Patch,
            Self::StepAdded
            | Self::StepRemoved
            | Self::StepReordered
            | Self::StepReprioritised
            | Self::MethodAdded
            | Self::MethodRemoved
            | Self::MethodSignatureChanged
            | Self::ScheduleChanged
            | Self::CapabilityWidened
            | Self::SeamOverrideChanged => Area::Behaviour,
        }
    }
}

/// One change in the plan.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    /// A stable id, unique in the plan and unchanged between runs of the same inputs.
    pub id: String,
    /// The area.
    pub area: Area,
    /// The typed kind.
    pub kind: ChangeKind,
    /// The risk rule this change names (see [`crate::risk::RiskTable`]).
    pub rule: String,
    /// The thing affected, for example `party.note` or `erp_selling:check_credit_limit`.
    pub target: String,
    /// One line for a human.
    pub description: String,
    /// The value before the change, when there is one. Never row data.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,
    /// The value after the change, when there is one. Never row data.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
    /// The risk the rule table assigned.
    pub risk: Risk,
    /// Why the change needs review or is refused, when it does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The safe alternative, when one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alternative: Option<String>,
    /// How the change affects users.
    pub blocking: BlockingClass,
}

impl Change {
    /// A change with the required fields; `before`, `after`, `reason` and `alternative` start
    /// empty.
    pub fn new(
        id: impl Into<String>,
        kind: ChangeKind,
        rule: impl Into<String>,
        target: impl Into<String>,
        description: impl Into<String>,
        risk: Risk,
        blocking: BlockingClass,
    ) -> Self {
        Self {
            id: id.into(),
            area: kind.area(),
            kind,
            rule: rule.into(),
            target: target.into(),
            description: description.into(),
            before: None,
            after: None,
            risk,
            reason: None,
            alternative: None,
            blocking,
        }
    }

    /// Sets the evidence.
    #[must_use]
    pub fn evidence(mut self, before: impl Into<String>, after: impl Into<String>) -> Self {
        self.before = Some(before.into());
        self.after = Some(after.into());
        self
    }

    /// Sets the reason a review or a refusal gives.
    #[must_use]
    pub fn why(mut self, reason: impl Into<String>) -> Self {
        self.reason = Some(reason.into());
        self
    }

    /// Sets the safe alternative.
    #[must_use]
    pub fn alternative(mut self, alternative: impl Into<String>) -> Self {
        self.alternative = Some(alternative.into());
        self
    }

    /// Sets the blocking class.
    #[must_use]
    pub fn blocking(mut self, blocking: BlockingClass) -> Self {
        self.blocking = blocking;
        self
    }
}

/// The differences [`crate::verify_applied`] found between a plan's target and the result.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mismatches {
    /// Every residual difference: a change that should have happened but did not, or one that
    /// happened and was not planned.
    pub mismatches: Vec<Change>,
}

impl Mismatches {
    /// Whether the result matches the plan's target exactly.
    pub fn is_empty(&self) -> bool {
        self.mismatches.is_empty()
    }

    /// How many differences were found.
    pub fn len(&self) -> usize {
        self.mismatches.len()
    }
}
