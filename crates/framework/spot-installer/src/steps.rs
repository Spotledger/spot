//! The ordered steps of a run and the small helpers the executor shares.
//!
//! [`Executor::run`](crate::Executor::run) turns a plan into a `Vec<Step>` before it changes
//! anything: patches, fixtures, the registry commit, the component load and the smoke test, in
//! plan order, with a whole-site snapshot in front when a step is irreversible. Splitting the
//! step vocabulary out of the executor keeps each file small and each concern named.

use semver::Version;
use spot_bundle::Digest;
use spot_core::SiteId;

use crate::error::JOURNAL_CORRUPT;
use crate::journal::{InstallJournal, LoadedComponent, StepId, StepState};
use crate::model::{InstalledApp, SiteState};
use crate::outcome::{ExecError, Failure};
use crate::plan::Action;
use crate::traits::{InstallChange, UpgradeChange};

/// One ordered step of a run.
pub(crate) enum Step {
    /// Snapshot the whole site before an irreversible change.
    Snapshot,
    /// Run one migration patch.
    Patch {
        /// The app.
        app: String,
        /// The patch id.
        patch_id: String,
        /// Whether the manifest declared it reversible.
        reversible: bool,
    },
    /// Load one fixture file.
    Fixture {
        /// The app.
        app: String,
        /// The fixture file.
        fixture: String,
    },
    /// Remove one fixture file (an uninstall).
    RemoveFixture {
        /// The app.
        app: String,
        /// The fixture file.
        fixture: String,
    },
    /// Commit an install.
    CommitInstall {
        /// The app.
        app: String,
        /// The registry change.
        change: InstallChange,
        /// The app as it was, or `None` when it was not installed.
        before: Option<InstalledApp>,
    },
    /// Commit an upgrade.
    CommitUpgrade {
        /// The app.
        app: String,
        /// The registry change.
        change: UpgradeChange,
        /// The installed app as it was.
        before: InstalledApp,
    },
    /// Commit an uninstall.
    CommitUninstall {
        /// The app.
        app: String,
        /// Whether the app's data is deleted too.
        purge: bool,
        /// The installed app as it was.
        before: InstalledApp,
        /// The DocTypes the app owned, for the export hook.
        doctypes: Vec<String>,
    },
    /// Load the new component.
    Load {
        /// The app.
        app: String,
        /// The new version.
        version: Version,
        /// The new bundle's digest.
        digest: Digest,
        /// The component serving before, or `None`.
        before: Option<LoadedComponent>,
    },
    /// Unload the app's component (an uninstall).
    Unload {
        /// The app.
        app: String,
        /// The component serving before.
        before: LoadedComponent,
    },
    /// Call the app's health entry point.
    Smoke {
        /// The app.
        app: String,
    },
}

/// Why a run stopped.
pub(crate) enum Stop {
    /// A step failed; the run rolls back.
    Failed(Failure),
    /// The lease was lost; the journal is left for the next holder.
    LeaseLost(ExecError),
}

/// The per-site lease name.
pub(crate) fn lease_name(site: &SiteId) -> String {
    format!("installer:{site}")
}

/// Adds the patch steps of an action.
pub(crate) fn push_patches(steps: &mut Vec<Step>, app: &str, action: &Action) {
    for patch in &action.patches {
        steps.push(Step::Patch {
            app: app.to_owned(),
            patch_id: patch.id.clone(),
            reversible: patch.reversible,
        });
    }
}

/// Adds the fixture steps of an action.
pub(crate) fn push_fixtures(steps: &mut Vec<Step>, app: &str, action: &Action) {
    for fixture in &action.fixtures_to_load {
        steps.push(Step::Fixture {
            app: app.to_owned(),
            fixture: fixture.clone(),
        });
    }
}

/// The installed app `app` on the site, or a refusal when the plan and the site disagree.
pub(crate) fn require_installed<'a>(
    before: &'a SiteState,
    app: &str,
) -> Result<&'a InstalledApp, ExecError> {
    before.get(app).ok_or_else(|| {
        ExecError::new(
            JOURNAL_CORRUPT,
            format!("`{app}` is not installed, so its action cannot run"),
        )
    })
}

/// The previous component of an installed app, for a rollback.
pub(crate) fn loaded_from(app: Option<&InstalledApp>) -> Option<LoadedComponent> {
    app.map(|app| LoadedComponent::new(app.version.clone(), app.digest))
}

/// The applied patch ids after an upgrade: the ones already applied, then the new ones.
pub(crate) fn merged_patches(applied: &[String], action: &Action) -> Vec<String> {
    let mut merged = applied.to_vec();
    for patch in &action.patches {
        if !merged.contains(&patch.id) {
            merged.push(patch.id.clone());
        }
    }
    merged
}

/// Inserts a journal row, replacing the row for the same step index.
pub(crate) fn upsert(entries: &mut Vec<InstallJournal>, row: InstallJournal) {
    if let Some(existing) = entries
        .iter_mut()
        .find(|entry| entry.step.index == row.step.index)
    {
        *existing = row;
    } else {
        entries.push(row);
    }
}

/// The steps that finished, in order.
pub(crate) fn done_steps(entries: &[InstallJournal]) -> Vec<StepId> {
    let mut done: Vec<StepId> = entries
        .iter()
        .filter(|entry| entry.state == StepState::Done)
        .map(|entry| entry.step.clone())
        .collect();
    done.sort_by_key(|step| step.index);
    done
}

/// The steps already marked undone, in order.
pub(crate) fn undone_so_far(entries: &[InstallJournal]) -> Vec<StepId> {
    let mut undone: Vec<StepId> = entries
        .iter()
        .filter(|entry| entry.state == StepState::Undone)
        .map(|entry| entry.step.clone())
        .collect();
    undone.sort_by_key(|step| step.index);
    undone
}
