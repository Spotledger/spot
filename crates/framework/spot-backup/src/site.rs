//! The ports a backup verification and restore workflow depend on.
//!
//! All of them are object-safe and time-free. The host wires the real ones: the
//! service gate from `spot-pg-supervisor`/`spot-updater`, the migration lifecycle from
//! `spot-migration-apply`, the invariant runner from `harness/rust/l7-invariants`, and
//! a scratch site over `spot-snapshot`.

use std::any::Any;
use std::future::Future;
use std::pin::Pin;

use crate::error::Result;
use crate::package::PackageFile;

/// A boxed future, as every port here returns one.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// One schema check over restored data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaCheck {
    /// Stable check name (for example `schema.tenant_tables`).
    pub name: String,
    /// Whether it passed.
    pub passed: bool,
    /// A plain-language detail; empty when it passed.
    pub detail: String,
}

/// What an invariant runner found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvariantReport {
    /// The runner's name (`l7` for the harness registry).
    pub runner: String,
    /// Whether every invariant held.
    pub ok: bool,
    /// Number of invariants checked.
    pub checks_run: u64,
    /// Violations, one line each.
    pub violations: Vec<String>,
}

impl InvariantReport {
    /// A passing report.
    pub fn passed(runner: impl Into<String>, checks_run: u64) -> InvariantReport {
        InvariantReport {
            runner: runner.into(),
            ok: true,
            checks_run,
            violations: Vec::new(),
        }
    }

    /// A failing report.
    pub fn failed(
        runner: impl Into<String>,
        checks_run: u64,
        violations: Vec<String>,
    ) -> InvariantReport {
        InvariantReport {
            runner: runner.into(),
            ok: false,
            checks_run,
            violations,
        }
    }
}

/// Restored data the invariant runner inspects.
///
/// `as_any` lets a concrete runner (the L7 adapter, over the in-memory storage fake or
/// PostgreSQL) recover the backend it wired when it created the site. Nothing in this
/// crate ever downcasts a production site; it is the seam the tests and the host use.
pub trait RestoredSite: Send + Sync {
    /// The tenant the restored data belongs to.
    fn tenant(&self) -> &str;
    /// The schema version the data is at now.
    fn schema_version(&self) -> u64;
    /// The installed app set, sorted.
    fn app_set(&self) -> Vec<String>;
    /// PostgreSQL major version.
    fn pg_major(&self) -> u32;
    /// Schema checks that need no database connection of their own.
    ///
    /// # Errors
    /// [`crate::BackupError`] when a check cannot run.
    fn schema_checks(&self) -> Result<Vec<SchemaCheck>>;
    /// The concrete site, for a runner that owns the backend.
    fn as_any(&self) -> &dyn Any;
}

/// The L7 invariant registry behind a trait, so verification does not depend on the
/// harness crate at runtime.
pub trait InvariantRunner: Send + Sync {
    /// Runs every invariant against `site`.
    ///
    /// # Errors
    /// [`crate::BackupError`] when the runner itself cannot run.
    fn run<'a>(&'a self, site: &'a dyn RestoredSite) -> BoxFuture<'a, Result<InvariantReport>>;
}

/// A scratch site for `Full` verification: a throwaway restore target the host
/// provides, never the live site.
pub trait ScratchSite: Send + Sync {
    /// Restores `package` into a fresh scratch database and returns it.
    ///
    /// # Errors
    /// [`crate::BackupError`] when the scratch restore fails.
    fn restore<'a>(
        &'a self,
        package: &'a PackageFile,
    ) -> BoxFuture<'a, Result<Box<dyn RestoredSite>>>;
}

/// The service gate a restore closes before touching data and opens only after every
/// check passed.
pub trait ServiceGate: Send + Sync {
    /// Stops accepting work.
    ///
    /// # Errors
    /// [`crate::BackupError`] when the service cannot be stopped.
    fn close(&self) -> Result<()>;

    /// Admits work again.
    ///
    /// # Errors
    /// [`crate::BackupError`] when the service cannot be opened.
    fn open(&self) -> Result<()>;

    /// Whether the gate is currently open.
    fn is_open(&self) -> bool;
}

/// What a forward migration did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationApplied {
    /// Schema version restored from the backup.
    pub from: u64,
    /// Runtime schema version after migrating.
    pub to: u64,
    /// The migration steps' names, in order.
    pub steps: Vec<String>,
}

/// The live target a restore writes into.
pub trait RestoreTargetSite: RestoredSite {
    /// The schema version the running platform expects (before any restore).
    fn runtime_schema_version(&self) -> u64;

    /// Free bytes on the volume that receives the data.
    fn free_space(&self) -> u64;

    /// Whether the site currently holds no tenant data.
    ///
    /// # Errors
    /// [`crate::BackupError`] when emptiness cannot be determined.
    fn is_empty(&self) -> Result<bool>;

    /// Replaces or fills the site with the package's data.
    ///
    /// # Errors
    /// [`crate::BackupError`] when the apply fails.
    fn apply<'a>(&'a self, package: &'a PackageFile, replace: bool) -> BoxFuture<'a, Result<()>>;

    /// Runs the migration lifecycle forward from `from` to `to`.
    ///
    /// # Errors
    /// [`crate::BackupError`] when a migration fails.
    fn migrate_forward<'a>(&'a self, from: u64, to: u64)
    -> BoxFuture<'a, Result<MigrationApplied>>;

    /// Clears node-local state the manifest marks (the node id first).
    ///
    /// # Errors
    /// [`crate::BackupError`] when the reset fails.
    fn reset_node_local<'a>(&'a self) -> BoxFuture<'a, Result<()>>;
}

/// One health condition a failed verification or restore raises.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthCondition {
    /// Stable code (`backup.verify_failed`, `backup.invariants_failed`, ...).
    pub code: String,
    /// `warning` or `critical`.
    pub severity: HealthSeverity,
    /// Plain-language message, safe to show a customer.
    pub message: String,
}

/// How serious a health condition is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthSeverity {
    /// The site still works; a backup needs attention.
    Warning,
    /// The site cannot be restored or is closed.
    Critical,
}

impl HealthSeverity {
    /// The lower-case name.
    pub fn as_str(self) -> &'static str {
        match self {
            HealthSeverity::Warning => "warning",
            HealthSeverity::Critical => "critical",
        }
    }
}

/// Raises a health condition on the site's health surface.
pub trait HealthSink: Send + Sync {
    /// Records `condition`.
    fn raise(&self, condition: HealthCondition);
}

/// An alert a failed verification sends to the operator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupAlert {
    /// The backup the alert is about.
    pub backup_id: String,
    /// The stable code.
    pub code: String,
    /// The message.
    pub message: String,
}

/// Sends alerts (mail, webhook, the notification centre).
pub trait AlertSink: Send + Sync {
    /// Sends `alert`.
    fn alert(&self, alert: BackupAlert);
}

/// A notification that a removable target is waiting for its media.
pub trait MediaNotifier: Send + Sync {
    /// Tells the operator that `volume` is absent.
    fn waiting(&self, volume: &str);
}
