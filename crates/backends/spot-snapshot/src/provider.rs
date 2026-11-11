//! The provider interface: what every snapshot backend offers, what it can do, and how
//! progress is observed. Providers are object-safe (the futures are boxed) so a host
//! can hold several of them behind one registry — the platform-specific ones of
//! WP-MIG-05.T2 plug in here.

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::str::FromStr;
use std::time::Duration;

use crate::error::{SnapshotError, SnapshotResult};
use crate::manifest::Snapshot;

/// What one provider can do. Every field is factual: callers pick a provider by
/// capability, never by name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capabilities {
    /// `true` when `take` yields a snapshot consistent with one transaction horizon
    /// while the database keeps running (the pg-dump family).
    pub consistent_online: bool,
    /// `true` when `take` requires the instance to be stopped (the file-copy family).
    pub needs_stopped_instance: bool,
    /// `true` when the provider can take incremental snapshots. `false` for both
    /// providers here; the flag exists so later providers need no interface change.
    pub incremental: bool,
    /// The parallelism the provider uses itself (pg_dump `--jobs`, copy workers).
    pub max_parallel: u32,
    /// A rough constant measured on the reference machine, bytes per millisecond,
    /// so callers can size windows; 0 means "not measured".
    pub estimated_throughput: u64,
}

/// Which tenant data one snapshot covers. A snapshot is tenant data (ADR-058 rule 1):
/// it names exactly one tenant-scoped target, and nothing else is ever included.
#[derive(Debug, Clone)]
pub enum SnapshotTarget {
    /// One tenant database behind a connection string (the pg-dump family).
    Database {
        /// PostgreSQL connection string (`postgres://user:***@host:port/dbname`).
        /// The password, if any, is moved into the child's environment only; it never
        /// reaches a command line, a log or the manifest.
        connection: String,
        /// The tenant's identity, recorded in the manifest verbatim.
        tenant: String,
        /// Database name to dump; must match the connection's database and is checked.
        database: String,
    },
    /// One managed instance to stop and copy (the file-copy family).
    Instance {
        /// Where the bundled binaries, data and logs live.
        layout: spot_pg_supervisor::layout::PgLayout,
        /// The tenant's identity, recorded in the manifest verbatim.
        tenant: String,
        /// The login role's password, handed to the supervisor through its own safe
        /// path and kept out of every log and manifest.
        password: String,
    },
}

impl SnapshotTarget {
    /// The tenant this target belongs to.
    pub fn tenant(&self) -> &str {
        match self {
            SnapshotTarget::Database { tenant, .. } | SnapshotTarget::Instance { tenant, .. } => {
                tenant
            }
        }
    }
}

/// Where a snapshot is written, and nothing more. The caller decides the location
/// (ADR-058); this crate never moves data anywhere else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Destination {
    /// Directory that receives `snapshot.toml` plus the provider's data files. It is
    /// created when missing and must be empty or absent: a snapshot is never taken
    /// over an existing one, so an abort can never eat someone else's data.
    pub dir: PathBuf,
}

/// Where a restore puts the data. A restore into a populated database or data
/// directory requires [`RestoreOptions::replace`] with the confirmation token.
#[derive(Debug, Clone)]
pub enum RestoreTarget {
    /// A database on a server reachable with this connection string.
    Database {
        /// PostgreSQL connection string to the server; the target database name is
        /// `database`.
        connection: String,
        /// Database to create or, with replace, to rebuild.
        database: String,
    },
    /// A stopped instance whose data directory is switched atomically.
    Instance {
        /// Layout of the target instance (binaries, data dir, logs, port).
        layout: spot_pg_supervisor::layout::PgLayout,
        /// The login role's password, used only to bring the instance back up after
        /// the switch and kept out of every log and manifest.
        password: String,
    },
}

impl RestoreTarget {
    /// The confirmation token a replace of this target requires: the target's own
    /// name, so the caller must say exactly what it is about to move aside.
    pub fn replace_token(&self) -> String {
        match self {
            RestoreTarget::Database { database, .. } => database.clone(),
            RestoreTarget::Instance { layout, .. } => layout.data_dir.display().to_string(),
        }
    }
}

/// Options for one `take`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotOptions {
    /// Parallel workers the provider may use (pg_dump `--jobs`, copy fan-out).
    /// Clamped to the provider's `max_parallel`.
    pub parallel: u32,
    /// Cancellation budget: how long the whole take may run. Zero means no budget;
    /// the check happens at await points, never by killing a child mid-write.
    pub deadline: Duration,
}

impl Default for SnapshotOptions {
    fn default() -> Self {
        SnapshotOptions {
            parallel: 4,
            deadline: Duration::ZERO,
        }
    }
}

/// Options for one `restore`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreOptions {
    /// Parallel workers for the restore (pg_restore `--jobs`, copy fan-out).
    pub parallel: u32,
    /// Replace a populated target: the old database or data directory is renamed
    /// aside to a clearly named neighbour and kept — never dropped by this crate.
    /// Requires the exact confirmation token of the target.
    pub replace: bool,
    /// The confirmation token; must equal [`RestoreTarget::replace_token`] when
    /// `replace` is set, or the operation fails with `snapshot.replace_token_mismatch`.
    pub replace_token: String,
    /// Cancellation budget, as in [`SnapshotOptions::deadline`].
    pub deadline: Duration,
}

impl Default for RestoreOptions {
    fn default() -> Self {
        RestoreOptions {
            parallel: 4,
            replace: false,
            replace_token: String::new(),
            deadline: Duration::ZERO,
        }
    }
}

impl RestoreOptions {
    /// Checks the replace consent against the target.
    ///
    /// # Errors
    /// A set `replace` with a wrong or empty token fails with
    /// `snapshot.replace_token_mismatch`; `replace = false` always passes.
    pub fn check_replace(&self, target: &RestoreTarget) -> Result<(), SnapshotError> {
        if !self.replace {
            return Ok(());
        }
        let expected = target.replace_token();
        if self.replace_token != expected {
            return Err(SnapshotError::ReplaceTokenMismatch { expected });
        }
        Ok(())
    }
}

/// Coarse progress events, observed through [`ProgressSink`] only. There is no
/// printing anywhere in this crate: the host decides what surfaces to the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Progress {
    /// The take or restore started; `files_total` is known when not 0.
    Started {
        /// 1 for take, 2 for restore; callers report the phases apart.
        phase: u32,
        /// Estimated total number of files, when the provider knows; else 0.
        files_total: u64,
    },
    /// One more file (or directory-format member) is done.
    FileDone {
        /// Bytes completed so far, integer; sizes are never floats.
        bytes_done: u64,
    },
    /// The operation finished (successfully or not).
    Finished {
        /// `true` when the operation ended in success.
        ok: bool,
    },
}

/// The host's view of progress. Implemented by the caller; the crate only calls it.
pub trait ProgressSink: Send + Sync {
    /// Called for every progress event. Must be quick; long work belongs in the host.
    fn on(&self, event: Progress);
}

/// The outcome of a successful `take`.
#[derive(Debug, Clone)]
pub struct TakeOutcome {
    /// The completed snapshot (manifest written last, atomically).
    pub snapshot: Snapshot,
}

/// One snapshot/restore backend over PostgreSQL data.
///
/// Implementations must honour the brief's guarantees: `take` either completes with a
/// manifest or leaves nothing [`Snapshot::open`] would accept (the `.partial` rule);
/// `restore` never drops data (rename aside only); `verify` re-checks every checksum.
/// The futures are boxed so the trait stays object-safe.
pub trait SnapshotProvider: Send + Sync {
    /// The provider's factual capabilities (see [`Capabilities`]).
    fn capabilities(&self) -> Capabilities;

    /// The provider's name, as the manifest records it (`provider_kind`).
    fn name(&self) -> &'static str;

    /// Takes a snapshot of `target` into `dest`, reporting through `progress`.
    ///
    /// # Errors
    /// Typed [`SnapshotError`]; a cancelled or failing take leaves at most a staging
    /// neighbour of the destination, never a snapshot [`Snapshot::open`] accepts.
    fn take(
        &self,
        target: &SnapshotTarget,
        dest: &Destination,
        opts: &SnapshotOptions,
        progress: std::sync::Arc<dyn ProgressSink>,
    ) -> Pin<Box<dyn Future<Output = SnapshotResult<TakeOutcome>> + Send + '_>>;

    /// Restores `snapshot` into `target`, reporting through `progress`.
    ///
    /// # Errors
    /// Typed [`SnapshotError`]; a populated target needs [`RestoreOptions::replace`]
    /// and the matching confirmation token.
    fn restore(
        &self,
        snapshot: &Snapshot,
        target: &RestoreTarget,
        opts: &RestoreOptions,
        progress: std::sync::Arc<dyn ProgressSink>,
    ) -> Pin<Box<dyn Future<Output = SnapshotResult<crate::restore::Restored>> + Send + '_>>;

    /// Re-checks every file's checksum against the manifest.
    ///
    /// # Errors
    /// `snapshot.verify_failed` naming the file whose byte changed.
    fn verify(
        &self,
        snapshot: &Snapshot,
    ) -> Pin<Box<dyn Future<Output = SnapshotResult<VerifyReport>> + Send + '_>>;

    /// Deletes a snapshot the caller owns, from its own recorded location.
    ///
    /// # Errors
    /// Typed [`SnapshotError`].
    fn delete(
        &self,
        snapshot: &Snapshot,
    ) -> Pin<Box<dyn Future<Output = SnapshotResult<()>> + Send + '_>>;
}

/// The outcome of a successful `verify`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyReport {
    /// Number of files checked, integer.
    pub files_checked: u64,
    /// Total bytes hashed, integer; no floats for sizes anywhere in this crate.
    pub bytes_checked: u64,
}

/// Extra environment for one child process, assembled by the providers. Secrets travel
/// only here — into the child's environment via `Command::env` — never onto a command
/// line or into the manifest or a log. The map is redacted in `Debug` output.
#[derive(Clone, Default)]
pub struct ChildEnv(pub(crate) HashMap<String, String>);

impl std::fmt::Debug for ChildEnv {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_set().entries(self.0.keys()).finish()
    }
}

impl ChildEnv {
    /// Splits a `postgres://` connection string into a credential-free keyword
    /// connection string for the command line and the env pairs (the password under
    /// `PGPASSWORD`) for the child.
    ///
    /// # Errors
    /// `snapshot.io` when the string is not a usable `postgres://` URL.
    pub fn from_connection(connection: &str) -> SnapshotResult<(String, ChildEnv)> {
        let parsed = tokio_postgres::Config::from_str(connection).map_err(|e| {
            SnapshotError::io(format!(
                "the connection string is not a usable postgres:// URL: {e}"
            ))
        })?;
        let mut env = ChildEnv::default();
        let mut parts: Vec<String> = Vec::new();
        if let Some(user) = parsed.get_user() {
            parts.push(format!("user={user}"));
        }
        if let Some(password) = parsed.get_password() {
            env.0.insert(
                "PGPASSWORD".to_owned(),
                String::from_utf8_lossy(password).into_owned(),
            );
        }
        match parsed.get_hosts().first() {
            Some(tokio_postgres::config::Host::Tcp(host)) => parts.push(format!("host={host}")),
            _ => return Err(SnapshotError::io("the connection string names no TCP host")),
        }
        if let Some(port) = parsed.get_ports().first() {
            parts.push(format!("port={port}"));
        }
        if let Some(dbname) = parsed.get_dbname() {
            parts.push(format!("dbname={dbname}"));
        }
        Ok((parts.join(" "), env))
    }

    /// Applies the pairs to a `Command`; the pairs never reach a command line.
    pub fn apply(&self, cmd: &mut tokio::process::Command) {
        for (key, value) in &self.0 {
            cmd.env(key, value);
        }
    }

    /// `true` when the pairs hold no key at all.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}
