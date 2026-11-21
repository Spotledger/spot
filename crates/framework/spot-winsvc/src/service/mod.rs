//! The Windows Service Control Manager glue (`cfg(windows)` only; unverified on this
//! Linux CI host — see the task's `## Open questions`). Everything the brief asks for
//! that is not pure logic lives here: registering/removing the service, the recovery
//! ladder, and the SCM dispatcher that calls [`crate::orchestrator::Orchestrator`].
//!
//! Chosen over adding a new `windows-service` dependency: `windows` is already the
//! workspace's Win32-bindings crate (`spot-agent-windows`'s precedent), and the brief
//! does not name `windows-service` explicitly (the project coding rules forbid an
//! undeclared dependency) — flagged for the lead to confirm.

pub mod dispatch;
pub mod eventlog;
pub mod install;
pub mod win_acl;

/// The service name the brief fixes: `SpotledgerServer`.
pub const SERVICE_NAME: &str = "SpotledgerServer";

/// The virtual account the service runs under (brief: "virtual account
/// `NT SERVICE\SpotledgerServer`").
pub const SERVICE_ACCOUNT: &str = "NT SERVICE\\SpotledgerServer";

/// The recovery ladder (brief: "recovery restart after 5 s, 30 s, 60 s with reset period
/// 1 day"), in milliseconds, in order.
pub const RESTART_DELAYS_MS: [u32; 3] = [5_000, 30_000, 60_000];

/// The restart-counter reset period (brief: "reset period 1 day"), in seconds.
pub const RESET_PERIOD_SECS: u32 = 24 * 60 * 60;

/// The service description (brief: "description set").
pub const SERVICE_DESCRIPTION: &str =
    "Runs the Spotledger native API host and its bundled PostgreSQL database.";
