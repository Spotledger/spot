//! Typed errors of the supervisor. Every failure mode named by the task brief has its
//! own visible variant so callers (and tests) match on meaning, not on strings.

use std::time::Duration;

/// Everything that can go wrong while supervising a bundled PostgreSQL.
#[derive(Debug, thiserror::Error)]
pub enum SupervisorError {
    /// A configured path or value is invalid (e.g. data dir inside the bin dir).
    #[error("invalid layout: {0}")]
    Layout(String),

    /// A layout check failed because required binaries are missing.
    #[error("missing PostgreSQL binaries in {dir}: {missing:?}")]
    MissingBinaries {
        /// Directory that was searched.
        dir: String,
        /// Binary names that are absent (`postgres`, `initdb`, `pg_ctl`).
        missing: Vec<String>,
    },

    /// The configured port could not be free-bound during validation.
    #[error("port {port} is already in use: {source}")]
    PortBusy {
        /// The port that was probed.
        port: u16,
        /// Underlying bind error.
        source: std::io::Error,
    },

    /// `initdb` failed. The password file is unlinked before this error is built; the
    /// password itself never reaches the error string.
    #[error("initdb failed with status {status}: {stderr_tail}")]
    InitdbFailed {
        /// Exit status of the failed run.
        status: String,
        /// Last lines of the captured stderr (truncated; `--pwfile=<path>` may appear,
        /// never the file's contents).
        stderr_tail: String,
    },

    /// The data directory is half-initialised: it exists, holds files, but has no
    /// `PG_VERSION`. Never deleted automatically — recovery is a human decision.
    #[error(
        "data directory {dir} is half-initialised (has files but no PG_VERSION); refusing to delete: recovery is a human decision"
    )]
    HalfInitialised {
        /// The refused data directory.
        dir: String,
    },

    /// The data directory is empty (or missing) but could not be created.
    #[error("failed to prepare data directory {dir}: {source}")]
    DataDirSetup {
        /// Directory that could not be prepared.
        dir: String,
        /// Underlying error.
        source: std::io::Error,
    },

    /// A required file (`PG_VERSION`, `postmaster.pid`) could not be read.
    #[error("failed to read {path}: {source}")]
    ReadFile {
        /// Path that could not be read.
        path: String,
        /// Underlying error (missing files are handled before this).
        source: std::io::Error,
    },

    /// A stale-looking `postmaster.pid` could not be removed.
    #[error("failed to remove stale postmaster.pid at {path}: {source}")]
    RemoveStaleLock {
        /// Path of the pid file.
        path: String,
        /// Underlying error.
        source: std::io::Error,
    },

    /// `postmaster.pid` names a live process: refused, never killed.
    #[error(
        "postmaster.pid at {path} names a live process (pid {pid}); refusing to start: stop that PostgreSQL first"
    )]
    LivePostmaster {
        /// Pid named by the file.
        pid: u32,
        /// Path of the pid file.
        path: String,
    },

    /// `postmaster.pid` does not parse (its first line is not a pid): refused, like a
    /// real postmaster refuses a corrupt lock file.
    #[error(
        "postmaster.pid at {path} does not contain a readable pid (first line: {first_line:?})"
    )]
    CorruptPidFile {
        /// Path of the pid file.
        path: String,
        /// The unparsable first line.
        first_line: String,
    },

    /// Spawning, writing the log or signalling the child failed.
    #[error("{context}: {source}")]
    ChildProcess {
        /// What the supervisor was doing.
        context: String,
        /// Underlying error.
        source: std::io::Error,
    },

    /// The child did not become ready within `start_timeout`.
    #[error("postgres did not become ready within {:?}", wait)]
    StartTimeout {
        /// The configured start timeout that elapsed.
        wait: Duration,
    },

    /// Restart budget exhausted: the supervisor stays `Failed` until `reset()`.
    #[error("restart budget exhausted ({} restarts within the last {:?})", budget.max_restarts, budget.window)]
    RestartBudgetExhausted {
        /// The budget that was exhausted.
        budget: crate::config::RestartBudget,
    },

    /// The operation is not valid in the current state (e.g. `start()` while ready).
    #[error("cannot {action} while the supervisor is {state:?}")]
    InvalidState {
        /// What was attempted.
        action: String,
        /// Current state.
        state: crate::supervisor::SupervisorState,
    },

    /// The supervisor reached `Failed { reason }` while the caller waited for another
    /// outcome (start timeout, restart budget, live postmaster).
    #[error("the supervisor failed: {reason}")]
    FailedState {
        /// The failure reason carried by the state.
        reason: String,
    },

    /// `initdb` needs a superuser password to set (through `--pwfile`); none was given.
    #[error(
        "ensure_initialised requires a superuser password (passed to initdb via a protected --pwfile)"
    )]
    MissingPassword,

    /// The bundled PostgreSQL has no usable PostGIS: the control file or the shared
    /// library the control file names is absent from the installation prefix. Raised by
    /// [`crate::layout::PgLayout::validate`], before `initdb` runs, so a bundle that
    /// would install a database without PostGIS never gets that far (ADR-087 sections
    /// 4.2 and 4.7).
    #[error(
        "the PostgreSQL bundle at {prefix} carries no usable PostGIS (ADR-087 requires it on every tier): missing {missing:?}"
    )]
    PostgisMissingFromBundle {
        /// The installation prefix that was searched (the parent of `bin_dir`).
        prefix: String,
        /// The paths that are absent, relative to the prefix.
        missing: Vec<String>,
    },

    /// The first-start `extension.install` of PostGIS failed: the binaries are not
    /// installed, the version is below the contract's minimum, the role lacks the
    /// privilege, or the call itself errored. The supervisor publishes
    /// `Failed { reason }` and never `Ready`; nothing downgrades this to a warning
    /// (ADR-087 sections 4.3 and 4.7, principle 6).
    #[error("{extension} could not be provisioned in the bundled database: {reason}")]
    PostgisUnprovisionable {
        /// The extension the host failed to install (`postgis`).
        extension: String,
        /// The host's typed failure text, naming the cause. Carries no secret.
        reason: String,
    },
}
