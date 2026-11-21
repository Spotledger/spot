//! The host agent's Linux operation modules (WP-ADM-04.T1, blueprint WS22, ADR-44).
//!
//! This is the first half of the Linux module suite: packages and updates (`dnf`, `apt`),
//! systemd services, users and groups, and the firewall (firewalld, ufw, nftables). Each
//! operation follows one shape — [`LinuxOperation::plan`] (what would change, no side effects),
//! [`LinuxOperation::apply`] (run it) and [`LinuxOperation::verify`] (confirm it landed) — so the
//! console can show a preview and the agent can verify a change.
//!
//! # How it touches the operating system
//!
//! Nothing here spawns a process or reads a file directly. A module assembles a
//! [`CommandRequest`](command::CommandRequest) — a program and a **vector** of separate
//! arguments, an allow-listed environment, a timeout and an output cap — and hands it to an
//! injected [`CommandRunner`](command::CommandRunner); file reads go through
//! [`FileSystem`](command::FileSystem). [`real`] holds the `std::process`/`std::fs`
//! implementations, and [`testing::FakeSystem`] a recording fake the golden transcripts drive.
//! No command line is ever built by string concatenation and nothing runs through a shell.
//!
//! # The operations
//!
//! [`catalogue`] registers the `os.package.*`, `os.update.*`, `os.service.*`, `os.user.*` and
//! `os.firewall.*` operations; [`handlers::register_handlers`] registers one
//! [`OperationHandler`](spot_agent_core::OperationHandler) per operation, so the host agent can
//! run exactly the catalogue's operations and nothing else.
//!
//! # The guards
//!
//! Three safety rules are built in, each a refusal rather than a silent no-op:
//!
//! - the package module refuses to run while another transaction holds the manager's lock;
//! - the service module refuses to stop, restart, disable or mask the agent's own unit or the
//!   runtime's without the operation's `critical` flag;
//! - the firewall module refuses to close the rule carrying management or SSH access without the
//!   `critical` flag, while the preview always shows the removal and a guard note;
//! - the user module refuses to touch an account below the UID floor unless its UID is
//!   allow-listed.
//!
//! # Purity
//!
//! No wall clock is read (time is an argument everywhere else; the real runner's monotonic
//! `Instant` is a process timeout, not the time of day), no floats, no `unsafe` (the project
//! coding rules), and no secret is ever logged or returned in a `Debug`.

#![forbid(unsafe_code)]
#![deny(clippy::float_arithmetic)]

pub mod args;
pub mod catalogue;
pub mod command;
pub mod context;
pub mod error;
pub mod firewall;
pub mod handlers;
pub mod ids;
pub mod packages;
pub mod preview;
pub mod real;
pub mod services;
pub mod testing;
pub mod users;

pub use args::Args;
pub use catalogue::{LinuxModule, build_catalogue, specs};
pub use command::{CommandOutput, CommandRequest, CommandRunner, FileSystem};
pub use context::LinuxContext;
pub use error::LinuxError;
pub use firewall::{FirewallBackend, FirewallList, FirewallRule, FirewallStatus, RuleSet};
pub use handlers::{LinuxHandler, modules, register_handlers};
pub use packages::{
    PackageBackend, PackageEnv, PackageInstall, PackageList, PackageRemove, PackageRepositories,
    UpdateApply, UpdateCheck,
};
pub use preview::{Check, LinuxOperation, Outcome, Preview, Step, run_steps};
pub use real::{StdCommandRunner, StdFileSystem};
pub use services::{ServiceControl, ServiceEffect, ServiceFailed, ServiceStatus, UnitState};
pub use testing::FakeSystem;
pub use users::{UserCreate, UserDelete, UserGroupMembership, UserGroups, UserList, UserLock};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-agent-linux";
