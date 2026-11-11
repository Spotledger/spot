//! `spot-winsvc`: the Windows Service host for `spot serve` and the bundled PostgreSQL
//! supervisor (WP-PRD-02.T8).
//!
//! The service `SpotledgerServer` runs under the virtual account `NT
//! SERVICE\SpotledgerServer`, automatic (delayed) start, no dependencies, and recovers
//! from a crash with the SCM's own restart ladder (5 s, 30 s, 60 s, reset after 1 day).
//! `run` is the SCM entry point: it starts the bundled PostgreSQL
//! ([`spot_pg_supervisor`]), waits for it, then starts `spot serve` as a child process,
//! and reports `SERVICE_RUNNING` only once `/api/health/ready` answers
//! ([`orchestrator::Orchestrator`]). `upgrade` is the entry point the updater calls to
//! switch to a staged version ([`upgrade::run_upgrade`]).
//!
//! Everything in this crate builds and tests on Linux except the actual Service Control
//! Manager glue, which is `cfg(windows)` ([`service`]) and uses the `windows` crate (the
//! workspace's established Win32-bindings crate, see `spot-agent-windows` — chosen over
//! adding a new `windows-service` dependency; see the crate-level `## Open questions` in
//! the task report). Every portable module is reachable and unit-tested from here:
//!
//! - [`error`]: the typed [`error::WinsvcError`] and its silent exit code,
//! - [`config`]: [`config::ServiceConfig`], the `--config` file,
//! - [`dirs`]: `%ProgramFiles%\Spotledger` / `%ProgramData%\Spotledger`,
//! - [`acl`]: the ACL rules for both directories, as data (never applied here),
//! - [`secrets`]: first-start secret generation and restrictive-permission storage,
//! - [`pg`]: the [`pg::PgControl`] seam over [`spot_pg_supervisor::Supervisor`],
//! - [`app`]: the [`app::AppControl`] seam over the `spot serve` child process,
//! - [`orchestrator`]: the start/stop ordering and crash-readiness state machine,
//! - [`upgrade`]: the staged-version pointer switch and health-gated rollback,
//! - [`cli`]: the `install|uninstall|start|stop|run|upgrade` command shape.

#![warn(missing_docs)]

pub mod acl;
pub mod app;
pub mod cli;
pub mod config;
pub mod dirs;
pub mod error;
pub mod logging;
pub mod orchestrator;
pub mod pg;
pub mod run;
pub mod secrets;
pub mod signal;
pub mod upgrade;

#[cfg(windows)]
pub mod service;
