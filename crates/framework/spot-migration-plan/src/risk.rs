//! Risk classes and the explicit, documented rule table that assigns them.
//!
//! Every change carries a **rule** (a stable string such as `metadata/field_removed`) and a
//! [`Risk`] read from a [`RiskTable`]. The table's defaults are compiled in and documented
//! here; a site policy may **raise** a rule's risk (for example a site that cannot tolerate a
//! brief lock marks every metadata change `destructive`), never lower it. A plan's risk is the
//! maximum over its changes.
//!
//! The rule keys use `/`, not `.`, so they are never mistaken for an error code.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::error::PlanError;

/// How risky a change is. Ordered, so the maximum is the plan's risk.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Risk {
    /// Safe: no data at risk, no lock a user notices.
    Additive,
    /// A human must look: a backfill, a widening capability, a weakened protection, an
    /// undeclared patch.
    NeedsReview,
    /// Destructive or irreversible: a removal, a narrowing, a rename, a refused schema change.
    Destructive,
}

/// The rule names the built-in table knows. A change names one of these.
pub mod rules {
    /// Create a table.
    pub const SCHEMA_ADD_ENTITY: &str = "schema/add_entity";
    /// Add a column.
    pub const SCHEMA_ADD_COLUMN: &str = "schema/add_column";
    /// Create an index.
    pub const SCHEMA_ADD_INDEX: &str = "schema/add_index";
    /// Widen a numeric column.
    pub const SCHEMA_WIDEN_TYPE: &str = "schema/widen_type";
    /// Relax a constraint (drop `NOT NULL`, widen or drop a length limit).
    pub const SCHEMA_RELAX_CONSTRAINT: &str = "schema/relax_constraint";
    /// Add a foreign key.
    pub const SCHEMA_ADD_FOREIGN_KEY: &str = "schema/add_foreign_key";
    /// Validate existing records against a constraint.
    pub const SCHEMA_VALIDATE: &str = "schema/validate";
    /// Clean up a failed index build.
    pub const SCHEMA_CLEANUP_INDEX: &str = "schema/cleanup_index";
    /// Record the catalogue version.
    pub const SCHEMA_FINALISE: &str = "schema/finalise";
    /// A backfill of a new required column.
    pub const SCHEMA_BACKFILL: &str = "schema/backfill";
    /// Make a column required.
    pub const SCHEMA_SET_NOT_NULL: &str = "schema/set_not_null";
    /// Drop the reverse index of a link field that is no longer traversable.
    pub const SCHEMA_DROP_TRAVERSAL_INDEX: &str = "schema/drop_traversal_index";
    /// A schema change the backend refused.
    pub const SCHEMA_BLOCKED: &str = "schema/blocked";
    /// A DocType appears.
    pub const METADATA_DOCTYPE_ADDED: &str = "metadata/doctype_added";
    /// A DocType disappears.
    pub const METADATA_DOCTYPE_REMOVED: &str = "metadata/doctype_removed";
    /// A field appears.
    pub const METADATA_FIELD_ADDED: &str = "metadata/field_added";
    /// A field disappears.
    pub const METADATA_FIELD_REMOVED: &str = "metadata/field_removed";
    /// A field's type changes.
    pub const METADATA_FIELD_RETYPED: &str = "metadata/field_retyped";
    /// A field is renamed (with a hint).
    pub const METADATA_FIELD_RENAMED: &str = "metadata/field_renamed";
    /// A field becomes required.
    pub const METADATA_FIELD_REQUIRED_SET: &str = "metadata/field_required_set";
    /// A field stops being required.
    pub const METADATA_FIELD_REQUIRED_CLEARED: &str = "metadata/field_required_cleared";
    /// A field's default changes.
    pub const METADATA_FIELD_DEFAULT_CHANGED: &str = "metadata/field_default_changed";
    /// A DocType's naming changes.
    pub const METADATA_NAMING_CHANGED: &str = "metadata/naming_changed";
    /// A link field's target changes.
    pub const METADATA_LINK_TARGET_CHANGED: &str = "metadata/link_target_changed";
    /// A protection group or a mask changes.
    pub const METADATA_PROTECTION_CHANGED: &str = "metadata/protection_changed";
    /// A permission rule appears.
    pub const METADATA_PERMISSION_ADDED: &str = "metadata/permission_added";
    /// A permission rule disappears.
    pub const METADATA_PERMISSION_REMOVED: &str = "metadata/permission_removed";
    /// A view layer changes.
    pub const METADATA_VIEW_CHANGED: &str = "metadata/view_changed";
    /// A patch appears.
    pub const PATCH_ADDED: &str = "patch/added";
    /// A patch is in the applied list but not in the new system (a downgrade).
    pub const PATCH_MISSING: &str = "patch/missing";
    /// A patch declares no intent.
    pub const PATCH_UNDECIDED: &str = "patch/undecided";
    /// A lifecycle step appears.
    pub const BEHAVIOUR_STEP_ADDED: &str = "behaviour/step_added";
    /// A lifecycle step disappears.
    pub const BEHAVIOUR_STEP_REMOVED: &str = "behaviour/step_removed";
    /// Lifecycle steps are reordered.
    pub const BEHAVIOUR_STEP_REORDERED: &str = "behaviour/step_reordered";
    /// A lifecycle step's priority changes.
    pub const BEHAVIOUR_STEP_REPRIORITISED: &str = "behaviour/step_reprioritised";
    /// A method appears.
    pub const BEHAVIOUR_METHOD_ADDED: &str = "behaviour/method_added";
    /// A method disappears.
    pub const BEHAVIOUR_METHOD_REMOVED: &str = "behaviour/method_removed";
    /// A method's argument schema changes in a breaking way.
    pub const BEHAVIOUR_METHOD_SIGNATURE_CHANGED: &str = "behaviour/method_signature_changed";
    /// A schedule changes.
    pub const BEHAVIOUR_SCHEDULE_CHANGED: &str = "behaviour/schedule_changed";
    /// A manifest capability widens.
    pub const BEHAVIOUR_CAPABILITY_WIDENED: &str = "behaviour/capability_widened";
    /// A seam override target changes.
    pub const BEHAVIOUR_SEAM_OVERRIDE_CHANGED: &str = "behaviour/seam_override_changed";
}

/// The rule-to-risk table. Defaults are compiled in; a site policy may only raise.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RiskTable {
    rules: BTreeMap<String, Risk>,
}

impl Default for RiskTable {
    fn default() -> Self {
        Self::builtin()
    }
}

impl RiskTable {
    /// The compiled-in defaults, one per rule in [`rules`].
    ///
    /// The table is deliberately conservative: a schema change the backend can do online is
    /// `additive`; a backfill or a `SET NOT NULL` is `needs_review`; anything the backend
    /// refuses is `destructive`. On the metadata side, additions are `additive`; removals,
    /// retypes, renames and link-target changes are `destructive`; protection, permission,
    /// view, naming and required-toggle changes are `needs_review` (weakening protection is
    /// always at least `needs_review`). Patches are `additive` unless undeclared
    /// (`needs_review`) or missing from the new system (`destructive`). Behaviour additions
    /// are `additive`; removals and breaking signature changes are `destructive`; reordering,
    /// reprioritising, schedule, capability and seam changes are `needs_review`.
    pub fn builtin() -> Self {
        use rules::*;
        let mut t = BTreeMap::new();
        t.insert(SCHEMA_ADD_ENTITY.to_owned(), Risk::Additive);
        t.insert(SCHEMA_ADD_COLUMN.to_owned(), Risk::Additive);
        t.insert(SCHEMA_ADD_INDEX.to_owned(), Risk::Additive);
        t.insert(SCHEMA_WIDEN_TYPE.to_owned(), Risk::Additive);
        t.insert(SCHEMA_RELAX_CONSTRAINT.to_owned(), Risk::Additive);
        t.insert(SCHEMA_ADD_FOREIGN_KEY.to_owned(), Risk::Additive);
        t.insert(SCHEMA_VALIDATE.to_owned(), Risk::Additive);
        t.insert(SCHEMA_CLEANUP_INDEX.to_owned(), Risk::Additive);
        t.insert(SCHEMA_FINALISE.to_owned(), Risk::Additive);
        t.insert(SCHEMA_BACKFILL.to_owned(), Risk::NeedsReview);
        t.insert(SCHEMA_SET_NOT_NULL.to_owned(), Risk::NeedsReview);
        t.insert(SCHEMA_DROP_TRAVERSAL_INDEX.to_owned(), Risk::NeedsReview);
        t.insert(SCHEMA_BLOCKED.to_owned(), Risk::Destructive);
        t.insert(METADATA_DOCTYPE_ADDED.to_owned(), Risk::Additive);
        t.insert(METADATA_DOCTYPE_REMOVED.to_owned(), Risk::Destructive);
        t.insert(METADATA_FIELD_ADDED.to_owned(), Risk::Additive);
        t.insert(METADATA_FIELD_REMOVED.to_owned(), Risk::Destructive);
        t.insert(METADATA_FIELD_RETYPED.to_owned(), Risk::Destructive);
        t.insert(METADATA_FIELD_RENAMED.to_owned(), Risk::Destructive);
        t.insert(METADATA_FIELD_REQUIRED_SET.to_owned(), Risk::NeedsReview);
        t.insert(METADATA_FIELD_REQUIRED_CLEARED.to_owned(), Risk::Additive);
        t.insert(METADATA_FIELD_DEFAULT_CHANGED.to_owned(), Risk::Additive);
        t.insert(METADATA_NAMING_CHANGED.to_owned(), Risk::NeedsReview);
        t.insert(METADATA_LINK_TARGET_CHANGED.to_owned(), Risk::Destructive);
        t.insert(METADATA_PROTECTION_CHANGED.to_owned(), Risk::NeedsReview);
        t.insert(METADATA_PERMISSION_ADDED.to_owned(), Risk::NeedsReview);
        t.insert(METADATA_PERMISSION_REMOVED.to_owned(), Risk::Destructive);
        t.insert(METADATA_VIEW_CHANGED.to_owned(), Risk::NeedsReview);
        t.insert(PATCH_ADDED.to_owned(), Risk::Additive);
        t.insert(PATCH_MISSING.to_owned(), Risk::Destructive);
        t.insert(PATCH_UNDECIDED.to_owned(), Risk::NeedsReview);
        t.insert(BEHAVIOUR_STEP_ADDED.to_owned(), Risk::Additive);
        t.insert(BEHAVIOUR_STEP_REMOVED.to_owned(), Risk::Destructive);
        t.insert(BEHAVIOUR_STEP_REORDERED.to_owned(), Risk::NeedsReview);
        t.insert(BEHAVIOUR_STEP_REPRIORITISED.to_owned(), Risk::NeedsReview);
        t.insert(BEHAVIOUR_METHOD_ADDED.to_owned(), Risk::Additive);
        t.insert(BEHAVIOUR_METHOD_REMOVED.to_owned(), Risk::Destructive);
        t.insert(
            BEHAVIOUR_METHOD_SIGNATURE_CHANGED.to_owned(),
            Risk::Destructive,
        );
        t.insert(BEHAVIOUR_SCHEDULE_CHANGED.to_owned(), Risk::NeedsReview);
        t.insert(BEHAVIOUR_CAPABILITY_WIDENED.to_owned(), Risk::NeedsReview);
        t.insert(
            BEHAVIOUR_SEAM_OVERRIDE_CHANGED.to_owned(),
            Risk::NeedsReview,
        );
        Self { rules: t }
    }

    /// Loads a table from TOML, **raising** the compiled-in defaults:
    ///
    /// ```toml
    /// [rules]
    /// "metadata/field_removed" = "destructive"
    /// ```
    ///
    /// The built-in defaults stay for every rule the file does not name, and a file can only
    /// raise a rule's risk, never lower it (a site policy may not make a removal safe). A file
    /// that names a rule the defaults do not know adds it.
    pub fn from_toml(text: &str) -> Result<Self, PlanError> {
        #[derive(Deserialize)]
        struct File {
            #[serde(default)]
            rules: BTreeMap<String, Risk>,
        }
        let file: File = toml::from_str(text).map_err(|e| PlanError::BadRiskTable {
            message: e.to_string(),
        })?;
        let mut table = Self::builtin();
        for (rule, risk) in file.rules {
            let current = table.rules.get(&rule).copied().unwrap_or(Risk::Additive);
            table.rules.insert(rule, current.max(risk));
        }
        Ok(table)
    }

    /// The risk a rule carries, or [`Risk::NeedsReview`] for a rule the table does not know
    /// (an unknown rule is never silently `additive`).
    pub fn risk_of(&self, rule: &str) -> Risk {
        self.rules.get(rule).copied().unwrap_or(Risk::NeedsReview)
    }

    /// The table's rules, for an admin listing.
    pub fn rules(&self) -> &BTreeMap<String, Risk> {
        &self.rules
    }

    /// Applies a site policy: every rule the policy names keeps the **higher** of its current
    /// risk and the policy's. A policy can therefore raise a risk and never lower one.
    pub fn with_policy(&self, policy: &RiskPolicy) -> Self {
        let mut rules = self.rules.clone();
        for (rule, raised) in &policy.raise {
            let current = rules.get(rule).copied().unwrap_or(Risk::Additive);
            rules.insert(rule.clone(), current.max(*raised));
        }
        Self { rules }
    }
}

/// A site policy that raises the risk of named rules.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RiskPolicy {
    /// The rules to raise, by rule name.
    raise: BTreeMap<String, Risk>,
}

impl RiskPolicy {
    /// An empty policy: nothing is raised.
    pub fn new() -> Self {
        Self::default()
    }

    /// Loads a policy from TOML:
    ///
    /// ```toml
    /// [raise]
    /// "metadata/field_added" = "needs_review"
    /// ```
    pub fn from_toml(text: &str) -> Result<Self, PlanError> {
        #[derive(Deserialize)]
        struct File {
            #[serde(default)]
            raise: BTreeMap<String, Risk>,
        }
        let file: File = toml::from_str(text).map_err(|e| PlanError::BadRiskTable {
            message: e.to_string(),
        })?;
        Ok(Self { raise: file.raise })
    }

    /// Raises one rule, for building a policy in code.
    #[must_use]
    pub fn raise(mut self, rule: &str, risk: Risk) -> Self {
        self.raise.insert(rule.to_owned(), risk);
        self
    }

    /// The rules the policy names, for an admin listing.
    pub fn raises(&self) -> &BTreeMap<String, Risk> {
        &self.raise
    }
}
