//! spot-agent-windows — Windows operation modules for the Spotledger host agent
//! (WP-ADM-05.T1, blueprint WS22, ADR-44).
//!
//! The Admin Center drives a Windows server through typed operations, signed by
//! administrators and verified locally by [`spot_agent_core`]. This crate is the Windows
//! half of the operation catalogue: services, Windows Update, the Windows Firewall and
//! local users/groups, each implemented as a [`spot_agent_core::OperationHandler`] over a
//! single, typed seam to the operating system.
//!
//! # The seam
//!
//! Everything the modules do to Windows goes through [`api::WinApi`]: a safe, typed trait
//! whose methods map one-to-one onto Service Control Manager calls, the Windows Update
//! Agent COM wrapper, the `INetFwPolicy2` firewall policy object and the NetAPI user/group
//! calls. No module ever builds a command line or shells out (the brief forbids a
//! PowerShell string built from input). Two implementations exist:
//!
//! - [`fake::FakeWin`] — a complete, in-memory model used by every test. It compiles and
//!   runs on Linux, which is where the main CI host tests this crate.
//! - [`real`] — the thin `#[cfg(windows)]` implementation on the `windows` crate. Linux
//!   builds never compile it, so the `windows` dependency is target-specific.
//!
//! # Plan, apply, verify
//!
//! Every module exposes [`services::Services`], [`updates::Updates`],
//! [`firewall::Firewall`] and [`users::Users`], each with the same three-phase shape:
//! `plan` computes what would change without touching the machine, `apply` performs it
//! (skipping when the plan is empty), and `verify` re-reads the machine and confirms the
//! new state. `apply` is idempotent: applying an already-satisfied plan changes nothing.
//! [`handlers::register`] wires the modules into a [`spot_agent_core::HandlerRegistry`]
//! under the catalogue ids from [`catalogue`].
//!
//! # Safety guards
//!
//! [`guards::AgentConfig`] names the services, accounts and firewall rules that carry the
//! agent's own management connection. Stopping the agent or the runtime service, removing
//! the outbound rule that carries the management session or the RDP rule, and touching a
//! built-in account all require an explicit flag in the operation arguments; without it
//! the module refuses before any change.
//!
//! # Secrets
//!
//! A reset account's new password is generated from the OS CSPRNG by a
//! [`secrets::PasswordSource`], handed to [`api::WinApi::set_user_password`] and delivered
//! through a [`secrets::SealedResultChannel`]. It is never returned in a handler outcome,
//! never rendered in a `Debug`, and never written to an error message.
//!
//! # Purity
//!
//! No clock is read (every instant is an argument), no float is used, and the Linux core
//! carries no `unsafe`; see the crate's `Cargo.toml` for the one `#[cfg(windows)]`
//! exception.

pub mod api;
pub mod args;
pub mod catalogue;
pub mod error;
pub mod fake;
pub mod firewall;
pub mod guards;
pub mod handlers;
pub mod plan;
pub mod secrets;
pub mod services;
pub mod updates;
pub mod users;

#[cfg(windows)]
pub mod real;

pub use api::{
    Direction, FirewallRule, LocalGroup, LocalUser, Profile, Protocol, RecoveryAction,
    RecoveryConfig, RuleAction, ServiceInfo, ServiceState, Severity, StartType, UpdateInfo, WinApi,
};
pub use catalogue::{AgentWindowsModule, MODULE_NAME};
pub use error::{CRATE_NAME, WindowsError};
pub use fake::FakeWin;
pub use firewall::Firewall;
pub use guards::AgentConfig;
pub use plan::{Change, Check, Outcome, Preview};
pub use secrets::{OsPasswordSource, PasswordSource, SealedResultChannel, Secret, SecretLabel};
pub use services::Services;
pub use updates::Updates;
pub use users::Users;
