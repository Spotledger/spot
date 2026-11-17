//! The seams of the site operations. The handlers hold no storage, no path and no process of
//! their own: everything they do goes through these ports, which the host module
//! (`spot-cli/src/services/mgmt_site.rs`) implements over the very commands the CLI runs.

use std::sync::Arc;

use spot_migration_ops::{Actor, LifecycleContext, LifecycleError};

pub use spot_mgmt_api::ports::BoxFuture;

/// One `spot` command line to run (without the program name and without `--root`, which the
/// host adds). Every element is one argument: there is no shell.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CliRequest {
    /// The arguments, for example `["backup", "run", "--site=demo", "--target=nightly"]`.
    pub argv: Vec<String>,
    /// How long the command may run before it is stopped.
    pub timeout_secs: u64,
}

/// How a command ended: the CLI's exit code (0 done, 1 usage, 2 refused with nothing changed,
/// 3 failed) and what it printed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CliOutput {
    /// The exit code.
    pub exit: u8,
    /// Standard output.
    pub stdout: String,
    /// Standard error.
    pub stderr: String,
}

/// The command could not be run to its own end. The variants are fixed words, never text a
/// child printed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CliError {
    /// The program could not be started.
    Spawn,
    /// The command ran past its timeout and was stopped.
    Timeout,
    /// Its output could not be read.
    Io,
}

/// Runs a `spot` command exactly as the operator's shell would. Dropping the returned future
/// stops the command (cancel); a handler drops it only for operations that are safe to stop.
pub trait SiteCli: Send + Sync {
    /// Runs the command and returns how it ended.
    ///
    /// # Errors
    /// [`CliError`] when the command could not be run to its own end.
    fn run(&self, request: CliRequest) -> BoxFuture<'_, Result<CliOutput, CliError>>;
}

/// The maintenance gate as the host can read it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Maintenance {
    /// Writes are accepted.
    Open,
    /// Writes are refused.
    Closed {
        /// Why, when the gate records it.
        reason: Option<String>,
    },
    /// The host cannot read the gate (reported as `unknown`, never guessed).
    Unknown,
}

/// What `site.status` reads without running a command.
pub trait SiteFacts: Send + Sync {
    /// The running build's version.
    fn version(&self) -> String;
    /// The maintenance gate.
    fn maintenance(&self) -> Maintenance;
}

/// Builds the migration lifecycle context of this site for an actor. The host provides it when
/// the lifecycle's stores, approvals and gate are wired; without it every `site.migrate.*`
/// operation refuses with `migration.port_unavailable`.
pub trait MigrationContexts: Send + Sync {
    /// The context for `actor`.
    ///
    /// # Errors
    /// [`LifecycleError`] when a store of the lifecycle cannot be reached.
    fn context(&self, actor: Actor) -> Result<Arc<LifecycleContext>, LifecycleError>;
}

/// Timeouts of the commands, in seconds. Reads are short; a backup or a restore may take hours.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timeouts {
    /// `backup list`, `backup verify`, `restore preflight`, `maintenance leave`, status reads.
    pub short_secs: u64,
    /// `backup run`, `restore apply`, `maintenance enter` (which drains in-flight work).
    pub long_secs: u64,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            short_secs: 600,
            long_secs: 6 * 3600,
        }
    }
}

/// Everything the handlers of one site need.
#[derive(Clone)]
pub struct SiteContext {
    /// This host's site id: a `site` parameter naming another site is `mgmt.forbidden`.
    pub site: String,
    /// Runs the CLI commands.
    pub cli: Arc<dyn SiteCli>,
    /// Version and maintenance state.
    pub facts: Arc<dyn SiteFacts>,
    /// The migration lifecycle, when wired.
    pub migrations: Option<Arc<dyn MigrationContexts>>,
    /// The command timeouts.
    pub timeouts: Timeouts,
}
