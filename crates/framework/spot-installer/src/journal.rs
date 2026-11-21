//! The install journal: one row per step, written before and after the step runs, so a run that
//! is interrupted at any journal write can be rolled back or completed by [`crate::Executor::resume`].
//!
//! A row carries the plan hash, the step's identity and index, the step's state, when it started,
//! and — for a step whose effect can be undone — the [`Undo`] data that puts the site back. The
//! executor writes the row for a registry commit in the same transaction as the commit itself
//! (see [`crate::AppRegistryStore`]), so a committed row and the registry change it describes
//! cannot disagree.

use semver::Version;
use spot_bundle::Digest;

use crate::model::InstalledApp;

/// What kind of step a journal row describes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum StepKind {
    /// The whole-site snapshot taken before a plan with an irreversible step.
    Snapshot,
    /// One migration patch applied.
    Patch,
    /// One fixture file loaded.
    Fixture,
    /// One fixture file removed (an uninstall).
    RemoveFixture,
    /// The registry change for one action, committed.
    Commit,
    /// The new component loaded.
    Load,
    /// The app's component unloaded (an uninstall).
    Unload,
    /// The app's declared health entry point called.
    Smoke,
}

impl StepKind {
    /// The stable name of the kind, for the journal and the error message.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            StepKind::Snapshot => "snapshot",
            StepKind::Patch => "patch",
            StepKind::Fixture => "fixture",
            StepKind::RemoveFixture => "remove_fixture",
            StepKind::Commit => "commit",
            StepKind::Load => "load",
            StepKind::Unload => "unload",
            StepKind::Smoke => "smoke",
        }
    }
}

/// The identity of one step within a run: a monotonic index, its kind, and the thing it names
/// (a patch id, a fixture path or an app).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct StepId {
    /// The step's position in the run, from zero. Unique within a run and the journal's order.
    pub index: u32,
    /// What kind of step it is.
    pub kind: StepKind,
    /// What the step names: a patch id, a fixture path, or an app.
    pub name: String,
}

impl StepId {
    /// A step identity.
    #[must_use]
    pub fn new(index: u32, kind: StepKind, name: impl Into<String>) -> Self {
        Self {
            index,
            kind,
            name: name.into(),
        }
    }

    /// The plain sentence an operator reads for this step.
    #[must_use]
    pub fn describe(&self) -> String {
        if self.name.is_empty() {
            self.kind.as_str().to_owned()
        } else {
            format!("{} {}", self.kind.as_str(), self.name)
        }
    }
}

/// How far one step got.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepState {
    /// The step's journal row is written and its effect is about to run, or is running.
    Started,
    /// The step's effect ran and its "after" row is written.
    Done,
    /// The step was undone during a rollback.
    Undone,
}

impl StepState {
    /// The stable name of the state, for the journal.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            StepState::Started => "started",
            StepState::Done => "done",
            StepState::Undone => "undone",
        }
    }
}

/// A component version serving on a site, kept so a rollback can put the previous one back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadedComponent {
    /// The version.
    pub version: Version,
    /// The bundle's canonical digest.
    pub digest: Digest,
}

impl LoadedComponent {
    /// A loaded component from its version and digest.
    #[must_use]
    pub fn new(version: Version, digest: Digest) -> Self {
        Self { version, digest }
    }
}

/// A snapshot a [`crate::Snapshotter`] took, named so a rollback can restore it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SnapshotId(String);

impl SnapshotId {
    /// Names a snapshot.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The snapshot's name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SnapshotId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// What a rollback does for one completed step.
///
/// The variant says which direction to put the site back: a patch that was applied is undone
/// (only when the manifest declared it reversible), a fixture that was loaded is removed, a
/// fixture that was removed is loaded, the registry entry is set back to the installed app it
/// was, and the component serving is set back to the one that served before (or to none).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Undo {
    /// Restore a snapshot taken before the plan changed anything.
    Snapshot(SnapshotId),
    /// Undo a patch, when it is reversible.
    Patch {
        /// The app.
        app: String,
        /// The patch id.
        patch_id: String,
        /// Whether the manifest declared it reversible.
        reversible: bool,
    },
    /// Remove a fixture this run loaded.
    FixtureLoaded {
        /// The app.
        app: String,
        /// The fixture file.
        fixture: String,
    },
    /// Re-load a fixture this run removed.
    FixtureRemoved {
        /// The app.
        app: String,
        /// The fixture file.
        fixture: String,
    },
    /// Set the registry entry back to the app as it was, or remove it when there was none.
    Registry {
        /// The app.
        app: String,
        /// The installed app as it was, or `None` when the app was not installed.
        before: Option<Box<InstalledApp>>,
        /// The component the commit installs, or `None` when the commit removes the app. A
        /// `resume` after the commit loads (or unloads) this, then smoke-tests, to finish a run
        /// that was interrupted between the commit and the load.
        after: Option<LoadedComponent>,
        /// The DocTypes the app owned, listed before an uninstall so a later task can offer an
        /// export. Empty for an install or upgrade.
        doctypes: Vec<String>,
    },
    /// Set the component serving back to what served before, or to none.
    Component {
        /// The app.
        app: String,
        /// The component that served before, or `None` when none served.
        before: Option<LoadedComponent>,
    },
}

/// One row of the install journal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstallJournal {
    /// The hash of the plan the run is executing.
    pub plan_hash: String,
    /// The step this row describes.
    pub step: StepId,
    /// How far the step got.
    pub state: StepState,
    /// When the step started, UTC seconds from the injected clock.
    pub started_at: i64,
    /// How a rollback puts the site back after this step, when the step can be undone.
    pub undo: Option<Undo>,
}

impl InstallJournal {
    /// A journal row.
    #[must_use]
    pub fn new(
        plan_hash: impl Into<String>,
        step: StepId,
        state: StepState,
        started_at: i64,
        undo: Option<Undo>,
    ) -> Self {
        Self {
            plan_hash: plan_hash.into(),
            step,
            state,
            started_at,
            undo,
        }
    }
}

/// Whether a site is running an install or is idle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SiteLifecycle {
    /// No installer is running on the site.
    Idle,
    /// An install, upgrade or uninstall is in progress (or stopped and needing attention); the
    /// Admin Center and the migration lifecycle read this.
    Installing,
}

impl SiteLifecycle {
    /// The stable name, for storage and logs.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            SiteLifecycle::Idle => "idle",
            SiteLifecycle::Installing => "installing",
        }
    }
}
