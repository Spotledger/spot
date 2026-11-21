//! The traits the executor drives. It never talks to a database, the document engine or the wasm
//! host directly: every effect goes through one of these, all keyed by [`SiteId`] so one executor
//! instance can never address two sites in one call.
//!
//! A real adapter wires these to the site's registry, `spot-pg-migrate`, the document engine and
//! `spot-wasm`; the fakes in [`crate::testing`] (feature `testing`) prove the executor's logic,
//! including every failure point. The methods are object-safe with boxed futures, the same shape
//! as `spot-jobs`.

use std::future::Future;
use std::pin::Pin;

use semver::Version;
use spot_bundle::Digest;
use spot_core::SiteId;

use crate::error::{LEASE_HELD, STEP_FAILED};
use crate::journal::{InstallJournal, SiteLifecycle, SnapshotId};
use crate::model::{GrantedCapabilities, InstalledApp, SiteState};
use crate::outcome::ExecError;

/// A boxed future, so the traits are object-safe without a macro crate.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A refused lifecycle move: the stored flag was not the one the caller expected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LifecycleConflict {
    /// The flag the store holds.
    pub stored: SiteLifecycle,
    /// The flag the caller said it read.
    pub expected: SiteLifecycle,
    /// The flag the caller wanted to move to.
    pub target: SiteLifecycle,
}

impl LifecycleConflict {
    /// The catalogued error for this conflict: `installer.lease_held` when a second installer
    /// tried to start an install on a site that is already installing, otherwise
    /// `installer.step_failed`. The message names the three states and no payload.
    #[must_use]
    pub fn to_exec_error(self, site: &SiteId) -> ExecError {
        let code = if self.target == SiteLifecycle::Installing
            && self.stored == SiteLifecycle::Installing
        {
            LEASE_HELD
        } else {
            STEP_FAILED
        };
        ExecError::new(
            code,
            format!(
                "site {site}: the lifecycle is {}, not {}; it was not moved to {}",
                self.stored.as_str(),
                self.expected.as_str(),
                self.target.as_str()
            ),
        )
    }
}

/// Why a conditional lifecycle move did not happen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LifecycleError {
    /// The stored flag was not the expected one; nothing changed.
    Conflict(LifecycleConflict),
    /// The store could not be read or written.
    Failed(ExecError),
}

impl LifecycleError {
    /// The error the executor reports, naming `site` for a conflict.
    #[must_use]
    pub fn into_exec_error(self, site: &SiteId) -> ExecError {
        match self {
            LifecycleError::Conflict(conflict) => conflict.to_exec_error(site),
            LifecycleError::Failed(error) => error,
        }
    }
}

impl From<ExecError> for LifecycleError {
    fn from(error: ExecError) -> Self {
        LifecycleError::Failed(error)
    }
}

/// The registry change for an install.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstallChange {
    /// The version installed.
    pub version: Version,
    /// The bundle's canonical digest.
    pub digest: Digest,
    /// The capabilities approved for the app.
    pub approved: GrantedCapabilities,
    /// The patch ids applied, in the order they ran.
    pub applied_patches: Vec<String>,
}

/// The registry change for an upgrade.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpgradeChange {
    /// The version the app is moving from; the store refuses the commit when the live version
    /// does not match, so a lost race is a refusal, not a silent overwrite.
    pub from: Version,
    /// The version installed.
    pub to: Version,
    /// The new bundle's canonical digest.
    pub digest: Digest,
    /// The capabilities approved for the new version.
    pub approved: GrantedCapabilities,
    /// The patch ids applied, in the order they ran.
    pub applied_patches: Vec<String>,
}

/// How one patch ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PatchOutcome {
    /// The patch ran and changed the site.
    Applied,
    /// The patch had already run (it is idempotent); nothing changed.
    AlreadyApplied,
}

impl PatchOutcome {
    /// Whether the patch changed the site, so a rollback must undo it.
    #[must_use]
    pub fn is_applied(self) -> bool {
        matches!(self, PatchOutcome::Applied)
    }
}

/// The site's registry: which apps are installed, and the install journal.
///
/// One call is one transaction. The `commit_*` calls write the registry change and the journal
/// row the executor passes in the same transaction, so a committed row and the change it
/// describes cannot disagree, and a failed commit leaves neither. A database implementation
/// passes [`crate::testing::contract::run_all`] (feature `testing`).
pub trait AppRegistryStore: Send + Sync {
    /// The apps installed on `site`, as the registry knows them.
    fn load(&self, site: &SiteId) -> BoxFuture<'_, Result<SiteState, ExecError>>;

    /// Whether the site is idle or running an install.
    fn lifecycle(&self, site: &SiteId) -> BoxFuture<'_, Result<SiteLifecycle, ExecError>>;

    /// Moves the site's lifecycle flag from `from` to `to`, only if the stored flag is still
    /// `from`, in one atomic compare-and-set. A site with no stored flag is `Idle`.
    ///
    /// Of N callers racing `Idle -> Installing`, exactly one gets `Ok`; the rest get
    /// [`LifecycleError::Conflict`] and the stored flag is left as it was. A wrong `from` changes
    /// nothing. There is deliberately no unconditional setter: every caller names the state it
    /// read, so a stale view of the site can never overwrite a newer one.
    fn set_lifecycle(
        &self,
        site: &SiteId,
        from: SiteLifecycle,
        to: SiteLifecycle,
    ) -> BoxFuture<'_, Result<(), LifecycleError>>;

    /// Adds an app to the registry, atomically with `journal`.
    fn commit_install(
        &self,
        site: &SiteId,
        app: &str,
        change: InstallChange,
        journal: InstallJournal,
    ) -> BoxFuture<'_, Result<(), ExecError>>;

    /// Moves an installed app to a new version, atomically with `journal`.
    fn commit_upgrade(
        &self,
        site: &SiteId,
        app: &str,
        change: UpgradeChange,
        journal: InstallJournal,
    ) -> BoxFuture<'_, Result<(), ExecError>>;

    /// Removes an app's registry row, atomically with `journal`. `purge` records that the app's
    /// data is deleted too; the store never deletes data without it.
    fn commit_uninstall(
        &self,
        site: &SiteId,
        app: &str,
        purge: bool,
        journal: InstallJournal,
    ) -> BoxFuture<'_, Result<(), ExecError>>;

    /// Sets the registry entry for `app` back to exactly `app_state` (inserting it when the app
    /// was uninstalled), atomically with `journal`; the rollback of a registry commit.
    fn restore(
        &self,
        site: &SiteId,
        app: &InstalledApp,
        journal: InstallJournal,
    ) -> BoxFuture<'_, Result<(), ExecError>>;

    /// Writes one journal row on its own (a step that is not a registry commit).
    fn record(
        &self,
        site: &SiteId,
        journal: InstallJournal,
    ) -> BoxFuture<'_, Result<(), ExecError>>;

    /// The journal rows for the site's current run, ordered by step index. Empty when no run was
    /// interrupted.
    fn journal(&self, site: &SiteId) -> BoxFuture<'_, Result<Vec<InstallJournal>, ExecError>>;

    /// Clears the site's journal: the run is finished (applied or rolled back).
    fn clear_journal(&self, site: &SiteId) -> BoxFuture<'_, Result<(), ExecError>>;

    /// The DocTypes the app owns, listed before an uninstall so a later task can offer an export
    /// first. The executor records it in the journal; it never deletes data itself.
    fn owned_doctypes(
        &self,
        site: &SiteId,
        app: &str,
    ) -> BoxFuture<'_, Result<Vec<String>, ExecError>>;
}

/// Runs the migration patches of an app.
///
/// A patch is idempotent or reversible, as its manifest declares; `undo` is called only for a
/// patch the manifest marks reversible.
pub trait PatchRunner: Send + Sync {
    /// Runs one patch. A patch that already ran returns [`PatchOutcome::AlreadyApplied`].
    fn apply(
        &self,
        site: &SiteId,
        app: &str,
        patch_id: &str,
    ) -> BoxFuture<'_, Result<PatchOutcome, ExecError>>;

    /// Undoes one reversible patch, putting the site back.
    fn undo(
        &self,
        site: &SiteId,
        app: &str,
        patch_id: &str,
    ) -> BoxFuture<'_, Result<(), ExecError>>;

    /// Whether the patch has already run on the site.
    fn is_applied(
        &self,
        site: &SiteId,
        app: &str,
        patch_id: &str,
    ) -> BoxFuture<'_, Result<bool, ExecError>>;
}

/// Loads, unloads and health-checks an app's component.
///
/// The real adapter uses `spot-wasm`; `smoke_test` calls the app's declared health entry point.
pub trait ComponentHost: Send + Sync {
    /// Loads the app's component at `version` (replacing whatever served before).
    fn load(
        &self,
        site: &SiteId,
        app: &str,
        version: &Version,
        digest: Digest,
    ) -> BoxFuture<'_, Result<(), ExecError>>;

    /// Unloads the app's component.
    fn unload(&self, site: &SiteId, app: &str) -> BoxFuture<'_, Result<(), ExecError>>;

    /// Calls the app's declared health entry point.
    fn smoke_test(&self, site: &SiteId, app: &str) -> BoxFuture<'_, Result<(), ExecError>>;
}

/// Loads and removes an app's fixture files.
pub trait FixtureLoader: Send + Sync {
    /// Loads one fixture file for the app.
    fn load(&self, site: &SiteId, app: &str, fixture: &str)
    -> BoxFuture<'_, Result<(), ExecError>>;

    /// Removes one fixture file the app loaded.
    fn remove(
        &self,
        site: &SiteId,
        app: &str,
        fixture: &str,
    ) -> BoxFuture<'_, Result<(), ExecError>>;

    /// The fixture files the app currently owns on the site; an uninstall removes these.
    fn owned(&self, site: &SiteId, app: &str) -> BoxFuture<'_, Result<Vec<String>, ExecError>>;
}

/// Takes and restores whole-site snapshots.
///
/// A plan whose every step is reversible never snapshots; a plan with an irreversible step (an
/// irreversible patch, or a purge) snapshots first and restores on failure.
pub trait Snapshotter: Send + Sync {
    /// Takes a snapshot of the site before the plan changes anything.
    fn take(&self, site: &SiteId) -> BoxFuture<'_, Result<SnapshotId, ExecError>>;

    /// Restores a snapshot taken earlier.
    fn restore(&self, site: &SiteId, id: &SnapshotId) -> BoxFuture<'_, Result<(), ExecError>>;
}
