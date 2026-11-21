//! The patch declaration: what a patch changes, how risky that is and how to undo it.
//!
//! A patch exists in the app-manifest convention (`[[patches]]` rows, [`spot_migration_plan`]'s
//! [`PatchDecl`](spot_migration_plan::PatchDecl)) and is described here in a release-metadata
//! file `release-meta/patches/<id>.toml`. The two are keyed by the same `id`; this crate never
//! redefines the manifest row, it only adds the declaration the migration planner, the
//! expand/contract gate (MIG-09) and the down-patches (MIG-10) read.
//!
//! The text form is deterministic: [`PatchDecl::render`] sorts the DocTypes and emits the fields
//! in a fixed order, so `parse(render(decl)) == decl`.

use std::path::Path;

use serde::{Deserialize, Serialize};
use spot_migration_plan::{PatchDecl as PlanPatchDecl, Risk, SystemDescription};

use crate::error::Error;

/// A reference to the patch that undoes this one.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PatchRef {
    /// The id of the down-patch, in the same manifest convention (`<app>_<id>` is the caller's
    /// choice; the value must name a patch that exists in the release).
    pub id: String,
}

impl PatchRef {
    /// A reference to the patch with this id.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self { id: id.into() }
    }
}

/// One patch's full declaration: intent, DocTypes, risk class, reversibility, idempotency and
/// down-step.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PatchDecl {
    /// The patch id. It must match the file stem (`release-meta/patches/<id>.toml`) and the
    /// manifest row's `id`.
    pub id: String,
    /// What the patch does, in one sentence. Never empty: an undeclared intent is exactly what
    /// this contract forbids.
    pub intent: String,
    /// The DocTypes the patch touches. Empty means "no DocType" (a pure infrastructure patch).
    #[serde(default)]
    pub doctypes: Vec<String>,
    /// The risk class, from [`spot_migration_plan::Risk`]: `additive`, `needs_review` or
    /// `destructive`.
    pub class: Risk,
    /// Whether a down-step exists. `false` means the patch cannot be undone.
    pub reversible: bool,
    /// Whether running the patch twice is safe.
    pub idempotent: bool,
    /// The down-step, when [`PatchDecl::reversible`] is `true`.
    #[serde(default)]
    pub down: Option<PatchRef>,
    /// Whether the patch may only run once the migration epoch is fixed (WP-MIG-12).
    #[serde(default)]
    pub requires_epoch: bool,
    /// The explicit acknowledgement that lets an irreversible patch ship without a down-step.
    /// The brief calls this `irreversible_ack`; it is only honoured by [`PatchDecl::reversible`]
    /// being `false` and [`PatchDecl::down`] being `None`.
    #[serde(default)]
    pub irreversible_ack: bool,
    /// The guard that makes a non-idempotent patch safe to re-run (for example "insert the row
    /// only when the marker is absent"). Required when [`PatchDecl::idempotent`] is `false`.
    #[serde(default)]
    pub guard: Option<String>,
}

impl PatchDecl {
    /// Parses a declaration from TOML. The DocTypes are sorted and de-duplicated, so two texts
    /// that differ only in list order parse to the same value.
    pub fn parse(text: &str) -> Result<Self, Error> {
        Self::parse_at("<toml>", text)
    }

    /// Parses a declaration that came from `path`, reporting the path in any error.
    pub fn parse_at(path: &str, text: &str) -> Result<Self, Error> {
        let mut decl: Self =
            toml::from_str(text).map_err(|e| Error::bad_patch(path, e.to_string()))?;
        if decl.intent.trim().is_empty() {
            return Err(Error::bad_patch(
                path,
                "the patch intent is empty; every patch declares its intent",
            ));
        }
        decl.doctypes.sort();
        decl.doctypes.dedup();
        Ok(decl)
    }

    /// Parses a declaration from the file `path`, refusing when the file stem does not name the
    /// declared id.
    pub fn from_file(path: &str, text: &str) -> Result<Self, Error> {
        let decl = Self::parse_at(path, text)?;
        let stem = Path::new(path)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("");
        if stem != decl.id {
            return Err(Error::bad_patch(
                path,
                format!(
                    "the file name `{stem}` does not match the declared patch id `{}`",
                    decl.id
                ),
            ));
        }
        Ok(decl)
    }

    /// The deterministic text form. Sorts the DocTypes and writes the fields in declaration
    /// order, so a release's declaration files are byte-stable across runs and machines.
    #[must_use]
    pub fn render(&self) -> String {
        let mut canonical = self.clone();
        canonical.doctypes.sort();
        canonical.doctypes.dedup();
        toml::to_string(&canonical).unwrap_or_default()
    }

    /// This declaration as the app-manifest row the plan engine knows.
    ///
    /// The manifest row has no `class`, `doctypes`, `down` or `requires_epoch`; those are what
    /// this contract adds. `intent` is carried through unchanged.
    #[must_use]
    pub fn to_plan_decl(&self) -> PlanPatchDecl {
        PlanPatchDecl {
            id: self.id.clone(),
            intent: if self.intent.is_empty() {
                None
            } else {
                Some(self.intent.clone())
            },
            reversible: self.reversible,
            idempotent: self.idempotent,
            deps: Vec::new(),
        }
    }

    /// Builds a declaration from an app-manifest row plus the release metadata this contract
    /// adds. The row's `id`, `reversible` and `idempotent` are the source of truth.
    #[must_use]
    pub fn from_plan_decl(
        plan: &PlanPatchDecl,
        class: Risk,
        doctypes: Vec<String>,
        down: Option<PatchRef>,
        requires_epoch: bool,
    ) -> Self {
        let mut doctypes = doctypes;
        doctypes.sort();
        doctypes.dedup();
        Self {
            id: plan.id.clone(),
            intent: plan.intent.clone().unwrap_or_default(),
            doctypes,
            class,
            reversible: plan.reversible,
            idempotent: plan.idempotent,
            down,
            requires_epoch,
            irreversible_ack: false,
            guard: None,
        }
    }
}

/// The patches that exist on disk, as the plan engine sees them.
///
/// This is the release's `[[patches]]` rows gathered from every app manifest. Passing these to
/// [`check_release`](crate::check_release) is how an undeclared patch is caught: it has a row
/// here but no `release-meta/patches/<id>.toml`.
#[must_use]
pub fn patches_on_disk(system: &SystemDescription) -> Vec<PlanPatchDecl> {
    system.patches().into_iter().cloned().collect()
}
