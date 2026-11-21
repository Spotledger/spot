//! The plain, serde model the plan is computed from.
//!
//! [`SystemDescription`] is everything the plan engine needs to know about one state of a
//! site: the installed apps and the manifest parts that matter, the **effective** metadata of
//! every DocType (definitions merged with site customisations), the controller method
//! declarations, the registered lifecycle steps, the patches already applied, the explicit
//! rename hints, and the table statistics. Nothing here reads a database, a clock or the
//! environment: a caller (the runtime, a fixture, a rehearsal clone) supplies the value.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use spot_meta::Definition;

/// One installed app, its version and the parts of its manifest the plan reads.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppVersion {
    /// The app's storage-style name.
    pub name: String,
    /// The app's version, as semver text (never parsed for arithmetic here).
    pub version: String,
    /// The manifest parts the plan reads.
    #[serde(default)]
    pub manifest: ManifestSummary,
}

/// The manifest parts the plan reads: events, schedules, patches, overrides and capabilities.
///
/// The plan does not re-parse or re-validate a manifest (that is `spot-manifest`'s job); it
/// reads the parts a release can change. Every list is optional and defaults to empty.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestSummary {
    /// `[[events]]`: a step subscribed to a DocType hook point.
    #[serde(default)]
    pub events: Vec<EventDecl>,
    /// `[[schedules]]`: a recurring job.
    #[serde(default)]
    pub schedules: Vec<ScheduleDecl>,
    /// `[[patches]]`: a migration.
    #[serde(default)]
    pub patches: Vec<PatchDecl>,
    /// `[[overrides]]`: a change to another app's step.
    #[serde(default)]
    pub overrides: Vec<OverrideDecl>,
    /// `[capabilities]`: what the app may touch.
    #[serde(default)]
    pub capabilities: CapabilitiesSummary,
}

/// One `[[events]]` row: a step at a DocType hook point.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventDecl {
    /// The DocType label.
    pub doctype: String,
    /// The hook point, as the manifest spells it (`validate_save`, `before_create`, `load`,
    /// `field_changed:<field>`).
    pub point: String,
    /// The step, `<app>:<step>`.
    pub step: String,
    /// Lower runs first; ties break by step name.
    #[serde(default)]
    pub priority: i32,
}

/// One `[[schedules]]` row: a recurring job.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduleDecl {
    /// The schedule name, unique in the manifest.
    pub name: String,
    /// The 6-field cron expression.
    pub cron: String,
    /// The step the schedule runs.
    pub step: String,
    /// The queue (`default`, `short` or `long`).
    #[serde(default)]
    pub queue: String,
}

/// One `[[patches]]` row: a migration.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PatchDecl {
    /// The patch id, unique and ascending in the order listed.
    pub id: String,
    /// The declared intent (`schema`, `data` or `metadata`); `None` means undeclared.
    #[serde(default)]
    pub intent: Option<String>,
    /// Whether a down-step exists.
    #[serde(default)]
    pub reversible: bool,
    /// Whether running it twice is safe.
    #[serde(default)]
    pub idempotent: bool,
    /// Patches that must run before this one, by id. The manifest has no such key yet, so it
    /// is normally empty and the manifest order is the dependency order.
    #[serde(default)]
    pub deps: Vec<String>,
}

/// One `[[overrides]]` row: a change to another app's step.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverrideDecl {
    /// The step being changed, `<app>:<step>`.
    pub target: String,
    /// `before`, `after`, `wrap`, `replace` or `disable`.
    pub kind: String,
    /// The step that runs instead of or around the target; absent for `disable`.
    #[serde(default)]
    pub by: Option<String>,
}

/// `[capabilities]`: what an app may touch. Widening any of these is a behaviour change.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilitiesSummary {
    /// Allowed `host:port` endpoints.
    #[serde(default)]
    pub network: Vec<String>,
    /// Whether the app may send e-mail.
    #[serde(default)]
    pub email: bool,
    /// File access: `none`, `read` or `read_write`.
    #[serde(default)]
    pub files: String,
    /// DocTypes the app may read.
    #[serde(default)]
    pub doctypes_read: Vec<String>,
    /// DocTypes the app may write.
    #[serde(default)]
    pub doctypes_write: Vec<String>,
}

/// The effective metadata of one DocType, with the extra effective facts the plan diffs.
///
/// `definition` is the merged definition (app metadata plus site customisations), exactly what
/// [`spot_meta::EffectiveMeta::definitions`] returns. `permissions` and `views` are the
/// effective permission rules and view layers; they live outside the definition model, so they
/// travel beside it as canonical one-line descriptions.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocTypeDescription {
    /// The merged definition.
    pub definition: Definition,
    /// The effective permission rules, each a canonical one-line description.
    #[serde(default)]
    pub permissions: Vec<String>,
    /// The effective view layers, keyed by layer name, each a canonical description or hash.
    #[serde(default)]
    pub views: BTreeMap<String, String>,
}

/// One controller method declaration.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MethodDescription {
    /// The DocType the method belongs to.
    pub doctype: String,
    /// The method name, a storage-style name.
    pub name: String,
    /// Whether the method changes documents.
    #[serde(default)]
    pub mutates: bool,
    /// The method's argument schema, in order.
    #[serde(default)]
    pub args: Vec<ArgSpec>,
}

/// One argument of a method's schema.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArgSpec {
    /// The argument name.
    pub name: String,
    /// The argument type, as text.
    pub ty: String,
    /// Whether the caller must supply it.
    #[serde(default)]
    pub required: bool,
}

/// One registered lifecycle step, as the seam registry's plan returns it.
///
/// The order of the list is the order the registry would run them (priority, then step name);
/// an override that wraps or replaces another step records it in `overridden`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedStep {
    /// The DocType the step runs for.
    pub doctype: String,
    /// The hook point.
    pub point: String,
    /// The step name, `<app>:<step>`.
    pub step: String,
    /// Lower runs first; ties break by step name.
    #[serde(default)]
    pub priority: i32,
    /// The step this one wraps or replaces, when an override applies.
    #[serde(default)]
    pub overridden: Option<String>,
}

/// An explicit hint that a field was renamed: `doctype.from` becomes `doctype.to`.
///
/// Without a hint a rename cannot be told from a removal plus an addition, so the plan reports
/// the pair and flags it; with a hint it reports one [`crate::ChangeKind::FieldRenamed`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenameHint {
    /// The DocType.
    pub doctype: String,
    /// The old field name.
    pub from: String,
    /// The new field name.
    pub to: String,
}

/// Row counts and sizes per entity, supplied by the caller from the database or a fixture.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TableStats {
    /// Statistics keyed by the storage entity (table) name.
    #[serde(default)]
    pub tables: BTreeMap<String, TableStat>,
}

/// The statistics of one entity. Either value may be absent; an absent row count is *unknown*,
/// never zero, and the estimate widens its range rather than guessing.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TableStat {
    /// The row count, when known.
    #[serde(default)]
    pub rows: Option<u64>,
    /// The size in bytes, when known.
    #[serde(default)]
    pub bytes: Option<u64>,
}

impl TableStats {
    /// The row count of `entity`, or `None` when it is unknown or has no entry.
    pub fn rows(&self, entity: &str) -> Option<u64> {
        self.tables.get(entity).and_then(|t| t.rows)
    }
}

/// The state of one site the plan is computed from.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemDescription {
    /// The installed apps, in any order (the plan sorts them).
    #[serde(default)]
    pub apps: Vec<AppVersion>,
    /// The effective metadata of every DocType, in any order.
    #[serde(default)]
    pub doctypes: Vec<DocTypeDescription>,
    /// The controller method declarations, in any order.
    #[serde(default)]
    pub methods: Vec<MethodDescription>,
    /// The registered lifecycle steps, in run order per DocType and hook point.
    #[serde(default)]
    pub seam_steps: Vec<PlannedStep>,
    /// The ids of the patches already applied.
    #[serde(default)]
    pub applied_patches: Vec<String>,
    /// The explicit rename hints.
    #[serde(default)]
    pub rename_hints: Vec<RenameHint>,
    /// The table statistics.
    #[serde(default)]
    pub stats: TableStats,
    /// The metadata version, when the caller knows it.
    #[serde(default)]
    pub meta_version: u64,
}

impl SystemDescription {
    /// An empty system: no apps, no DocTypes, no patches.
    pub fn new() -> Self {
        Self::default()
    }

    /// The effective metadata of one DocType, by name.
    pub fn doctype(&self, name: &str) -> Option<&DocTypeDescription> {
        self.doctypes.iter().find(|d| d.definition.name == name)
    }

    /// Every patch declared by the installed apps, in manifest order.
    pub fn patches(&self) -> Vec<&PatchDecl> {
        self.apps
            .iter()
            .flat_map(|a| a.manifest.patches.iter())
            .collect()
    }

    /// A copy with every order-insensitive list sorted, so the plan document is deterministic
    /// whatever order the caller built the lists in.
    ///
    /// The registered lifecycle steps keep their order: that order is the run order and is
    /// meaningful, so it is never sorted.
    pub fn canonical(&self) -> Self {
        let mut system = self.clone();
        system.apps.sort_by(|a, b| a.name.cmp(&b.name));
        for app in &mut system.apps {
            app.manifest.events.sort_by(|a, b| {
                (&a.doctype, &a.point, &a.step).cmp(&(&b.doctype, &b.point, &b.step))
            });
            app.manifest.schedules.sort_by(|a, b| a.name.cmp(&b.name));
            app.manifest.patches.sort_by(|a, b| a.id.cmp(&b.id));
            app.manifest
                .overrides
                .sort_by(|a, b| a.target.cmp(&b.target));
            app.manifest.capabilities.network.sort();
            app.manifest.capabilities.doctypes_read.sort();
            app.manifest.capabilities.doctypes_write.sort();
        }
        system
            .doctypes
            .sort_by(|a, b| a.definition.name.cmp(&b.definition.name));
        for dt in &mut system.doctypes {
            dt.definition.fields.sort_by(|a, b| a.name.cmp(&b.name));
            dt.permissions.sort();
        }
        system
            .methods
            .sort_by(|a, b| (&a.doctype, &a.name).cmp(&(&b.doctype, &b.name)));
        system.applied_patches.sort();
        system
            .rename_hints
            .sort_by(|a, b| (&a.doctype, &a.from, &a.to).cmp(&(&b.doctype, &b.from, &b.to)));
        system
    }
}
