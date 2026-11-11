//! The conflict model: one plan item per root cause, with its affected artefacts.
//!
//! A release changes the platform; this crate reports what those changes break in *this site's*
//! customisations. Each [`ConflictItem`] is one root cause — a release item such as "the field
//! `sales_invoice.internal_note` is removed" — with every artefact it breaks grouped under it, so
//! one removed field that a view, three scripts and a fixture all mention is one plan item, not
//! five. The item carries the stable [`code`](codes), the [`Risk`], the
//! [`Cause`] (release item and effect), the affected [`ArtefactRef`]s and a suggested resolution.
//!
//! [`ConflictItem::to_change`] turns one item into a
//! [`spot_migration_plan::Change`], so a caller can fold the conflicts into the release plan the
//! admin approves. The plan's [`ChangeKind`](spot_migration_plan::ChangeKind) is a closed enum
//! owned by `spot-migration-plan`, so the conversion reuses the closest existing kind and keeps
//! the conflict's own identity in the change's `id` and `rule`.

use spot_migration_plan::{BlockingClass, Change, ChangeKind, Risk};

use crate::codes;

/// The category of one conflict: what kind of customisation the release breaks and how.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ConflictCategory {
    /// A custom field's name collides with a field the release adds to the same DocType.
    CustomFieldCollision,
    /// A custom field sits on a DocType the release removes.
    CustomFieldDocTypeRemoved,
    /// A property override targets a field the release removes or renames.
    OverrideFieldRemoved,
    /// A property override targets a DocType the release removes.
    OverrideDocTypeRemoved,
    /// A site `options` override no longer matches the new option list of the field.
    OverrideOptionsConflict,
    /// A fixture document sets a field the release removes.
    FixtureFieldRemoved,
    /// A fixture targets a DocType the release removes.
    FixtureDocTypeRemoved,
    /// A view layer references a field the release removes.
    ViewFieldRemoved,
    /// A view layer targets a DocType the release removes.
    ViewDocTypeRemoved,
    /// A view layer hides a required field with no default.
    ViewHiddenRequired,
    /// A script references a field that is not in the new declarations for its DocType.
    ScriptTypeError,
    /// A script no longer transpiles (syntax or a script-tier rule).
    ScriptSyntaxError,
    /// A script is bound to a DocType the release removes.
    ScriptDocTypeRemoved,
    /// A rule or field expression no longer parses.
    RuleParseError,
    /// A rule or field expression references a field the release removes.
    RuleFieldRemoved,
    /// An installed app targets a WIT world the new host does not serve.
    AppWitWorldMismatch,
    /// An installed app needs a DocType the release removes.
    AppDocTypeRemoved,
    /// An installed app subscribes to a `field_changed` hook whose field the release removes.
    AppHookRemoved,
    /// An installed app's `requires_host` range excludes the new host version.
    AppHostIncompatible,
    /// The new metadata cannot generate the TypeScript declarations the script check needs.
    DeclarationsError,
}

impl ConflictCategory {
    /// The stable `migration_conflicts.*` code for this category.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::CustomFieldCollision => codes::CUSTOM_FIELD_COLLISION,
            Self::CustomFieldDocTypeRemoved => codes::CUSTOM_FIELD_DOCTYPE_REMOVED,
            Self::OverrideFieldRemoved => codes::OVERRIDE_FIELD_REMOVED,
            Self::OverrideDocTypeRemoved => codes::OVERRIDE_DOCTYPE_REMOVED,
            Self::OverrideOptionsConflict => codes::OVERRIDE_OPTIONS_CONFLICT,
            Self::FixtureFieldRemoved => codes::FIXTURE_FIELD_REMOVED,
            Self::FixtureDocTypeRemoved => codes::FIXTURE_DOCTYPE_REMOVED,
            Self::ViewFieldRemoved => codes::VIEW_FIELD_REMOVED,
            Self::ViewDocTypeRemoved => codes::VIEW_DOCTYPE_REMOVED,
            Self::ViewHiddenRequired => codes::VIEW_HIDDEN_REQUIRED,
            Self::ScriptTypeError => codes::SCRIPT_TYPE_ERROR,
            Self::ScriptSyntaxError => codes::SCRIPT_SYNTAX_ERROR,
            Self::ScriptDocTypeRemoved => codes::SCRIPT_DOCTYPE_REMOVED,
            Self::RuleParseError => codes::RULE_PARSE_ERROR,
            Self::RuleFieldRemoved => codes::RULE_FIELD_REMOVED,
            Self::AppWitWorldMismatch => codes::APP_WIT_WORLD_MISMATCH,
            Self::AppDocTypeRemoved => codes::APP_DOCTYPE_REMOVED,
            Self::AppHookRemoved => codes::APP_HOOK_REMOVED,
            Self::AppHostIncompatible => codes::APP_HOST_INCOMPATIBLE,
            Self::DeclarationsError => codes::DECLARATIONS_ERROR,
        }
    }

    /// A short lower-case name for the category, used to build stable ids.
    #[must_use]
    pub fn slug(self) -> &'static str {
        match self {
            Self::CustomFieldCollision => "custom_field_collision",
            Self::CustomFieldDocTypeRemoved => "custom_field_doctype_removed",
            Self::OverrideFieldRemoved => "override_field_removed",
            Self::OverrideDocTypeRemoved => "override_doctype_removed",
            Self::OverrideOptionsConflict => "override_options_conflict",
            Self::FixtureFieldRemoved => "fixture_field_removed",
            Self::FixtureDocTypeRemoved => "fixture_doctype_removed",
            Self::ViewFieldRemoved => "view_field_removed",
            Self::ViewDocTypeRemoved => "view_doctype_removed",
            Self::ViewHiddenRequired => "view_hidden_required",
            Self::ScriptTypeError => "script_type_error",
            Self::ScriptSyntaxError => "script_syntax_error",
            Self::ScriptDocTypeRemoved => "script_doctype_removed",
            Self::RuleParseError => "rule_parse_error",
            Self::RuleFieldRemoved => "rule_field_removed",
            Self::AppWitWorldMismatch => "app_wit_world_mismatch",
            Self::AppDocTypeRemoved => "app_doctype_removed",
            Self::AppHookRemoved => "app_hook_removed",
            Self::AppHostIncompatible => "app_host_incompatible",
            Self::DeclarationsError => "declarations_error",
        }
    }

    /// The risk this category carries.
    ///
    /// A broken script, a removed DocType or a view that strands a save is `destructive`: the
    /// release cannot go out without someone acting. An option list that no longer matches, a
    /// rule that fails to parse or a hook that needs a new field is `needs_review`.
    #[must_use]
    pub fn risk(self) -> Risk {
        match self {
            Self::OverrideOptionsConflict
            | Self::ViewFieldRemoved
            | Self::ViewDocTypeRemoved
            | Self::RuleParseError
            | Self::RuleFieldRemoved
            | Self::AppHookRemoved
            | Self::DeclarationsError => Risk::NeedsReview,
            _ => Risk::Destructive,
        }
    }

    /// The area of the release plan the conflict belongs to, for the plan item.
    fn area_kind(self) -> ChangeKind {
        match self {
            Self::ScriptTypeError
            | Self::ScriptSyntaxError
            | Self::ScriptDocTypeRemoved
            | Self::RuleParseError
            | Self::RuleFieldRemoved
            | Self::AppWitWorldMismatch
            | Self::AppDocTypeRemoved
            | Self::AppHookRemoved
            | Self::AppHostIncompatible => ChangeKind::StepRemoved,
            _ => ChangeKind::FieldRemoved,
        }
    }

    /// The blocking class the plan item carries: a destructive conflict needs a window, a
    /// review-only one a brief lock.
    fn blocking(self) -> BlockingClass {
        match self.risk() {
            Risk::Destructive => BlockingClass::MaintenanceWindow,
            Risk::NeedsReview => BlockingClass::BriefLock,
            Risk::Additive => BlockingClass::Online,
        }
    }
}

/// The kind of customisation one [`ArtefactRef`] names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ArtefactKind {
    /// A custom field.
    CustomField,
    /// A property override.
    PropertyOverride,
    /// A fixture set.
    Fixture,
    /// A view layer.
    ViewLayer,
    /// A script.
    Script,
    /// A rule or field expression.
    Rule,
    /// An installed app.
    App,
    /// The generated declarations themselves (a [`ConflictCategory::DeclarationsError`]).
    Declarations,
}

impl ArtefactKind {
    /// The kind's stable name, the prefix of every artefact id.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CustomField => "custom_field",
            Self::PropertyOverride => "override",
            Self::Fixture => "fixture",
            Self::ViewLayer => "view",
            Self::Script => "script",
            Self::Rule => "rule",
            Self::App => "app",
            Self::Declarations => "declarations",
        }
    }
}

/// A pointer to the owning customisation an item breaks, and to the artefact inside it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ArtefactRef {
    /// What kind of customisation it is.
    pub kind: ArtefactKind,
    /// The artefact's stable id within its kind, for example `sales_invoice.custom_x` for a
    /// custom field or `script:nightly_rollup` for a script.
    pub id: String,
}

impl ArtefactRef {
    /// An artefact of `kind` with `id`.
    #[must_use]
    pub fn new(kind: ArtefactKind, id: impl Into<String>) -> Self {
        Self {
            kind,
            id: id.into(),
        }
    }

    /// The pointer the report shows: `<kind>:<id>`, which is also the item's stable identity
    /// inside its kind.
    #[must_use]
    pub fn pointer(&self) -> String {
        format!("{}:{}", self.kind.as_str(), self.id)
    }
}

/// What release item broke something, and what it did.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Cause {
    /// The release item: the plan change's id, such as
    /// `metadata/sales_invoice/field_removed/internal_note`, or a synthetic
    /// `release/<what>` id when the release itself has no plan change for it.
    pub release_item: String,
    /// One sentence of what the release item does to the site, for example "the field
    /// `sales_invoice.internal_note` no longer exists".
    pub effect: String,
}

impl Cause {
    /// A cause from a release item and its effect.
    #[must_use]
    pub fn new(release_item: impl Into<String>, effect: impl Into<String>) -> Self {
        Self {
            release_item: release_item.into(),
            effect: effect.into(),
        }
    }
}

/// One conflict: one root cause and every artefact it breaks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConflictItem {
    /// A stable id, unique in a report and unchanged between runs of the same inputs.
    pub id: String,
    /// The stable `migration_conflicts.*` code.
    pub code: &'static str,
    /// The category.
    pub category: ConflictCategory,
    /// What release item broke it, and what it did.
    pub cause: Cause,
    /// The affected artefacts, sorted by [`ArtefactRef::pointer`] and without repeats.
    pub artefacts: Vec<ArtefactRef>,
    /// The risk the category carries.
    pub risk: Risk,
    /// What the site owner can do, one sentence.
    pub resolution: String,
}

impl ConflictItem {
    /// The conflict as a plan change, so a caller can fold it into a
    /// [`spot_migration_plan::ReleasePlan`].
    ///
    /// The plan's `ChangeKind` is a closed enum owned by `spot-migration-plan`; this conversion
    /// reuses the closest existing kind (`FieldRemoved` for a metadata conflict, `StepRemoved`
    /// for a behaviour one) and keeps the conflict's own identity in the change's `id` and
    /// `rule` (`conflict/<category>`), so nothing is lost even though the kind is generic.
    #[must_use]
    pub fn to_change(&self) -> Change {
        let target = self
            .artefacts
            .first()
            .map_or_else(|| self.cause.release_item.clone(), ArtefactRef::pointer);
        let count = self.artefacts.len();
        let description = if count == 1 {
            format!(
                "A release change conflicts with 1 customisation: {}",
                self.cause.effect
            )
        } else {
            format!(
                "A release change conflicts with {count} customisations: {}",
                self.cause.effect
            )
        };
        Change::new(
            self.id.clone(),
            self.category.area_kind(),
            format!("conflict/{}", self.category.slug()),
            target,
            description,
            self.risk,
            self.category.blocking(),
        )
        .evidence(self.cause.release_item.clone(), "conflict")
        .why(self.cause.effect.clone())
        .alternative(self.resolution.clone())
    }
}

/// The result of one detection run: every conflict the release causes on this site.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConflictReport {
    /// The digest of the plan the report was computed against, so the report is bound to exactly
    /// the release it reviewed.
    pub plan_digest: String,
    /// The conflicts, in a deterministic order (by code, then cause, then first artefact).
    pub items: Vec<ConflictItem>,
}

impl ConflictReport {
    /// An empty report against `plan_digest`.
    #[must_use]
    pub fn new(plan_digest: impl Into<String>) -> Self {
        Self {
            plan_digest: plan_digest.into(),
            items: Vec::new(),
        }
    }

    /// Whether the release breaks nothing in the site's customisations.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// How many conflict items the report carries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// The report's risk: the maximum over its items, `additive` when it is empty.
    #[must_use]
    pub fn risk(&self) -> Risk {
        self.items
            .iter()
            .map(|item| item.risk)
            .max()
            .unwrap_or(Risk::Additive)
    }

    /// Every item of one category, in report order.
    #[must_use]
    pub fn by_category(&self, category: ConflictCategory) -> Vec<&ConflictItem> {
        self.items
            .iter()
            .filter(|item| item.category == category)
            .collect()
    }

    /// The report as plan changes, in report order.
    #[must_use]
    pub fn to_plan_changes(&self) -> Vec<Change> {
        self.items.iter().map(ConflictItem::to_change).collect()
    }
}

/// A slug for a stable id: lower-case letters, digits and single dashes, never empty.
pub(crate) fn slug(text: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash && !out.is_empty() {
            out.push('-');
            last_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        "item".to_owned()
    } else {
        out
    }
}
