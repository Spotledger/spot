//! The host agent's Linux system modules, part 2 (WP-ADM-04.T2, blueprint WS22, ADR-44).
//!
//! This is the second half of the Linux administration modules: storage and snapshots
//! (btrfs, LVM and ZFS, plus XFS/ext4 facts), network configuration through
//! NetworkManager or systemd-networkd with a connectivity revert guard, the system trust
//! store, chrony time sync, kernel parameters (sysctl) and SELinux/AppArmor status and
//! mode. Part 1 (packages, services, users, firewall) is a sibling crate with the same
//! shape.
//!
//! # The shape of a module
//!
//! Every subsystem is a module with the same three methods, so the console can preview a
//! change and the agent can verify it:
//!
//! - `plan` reads the machine and returns a [`preview::Preview`] of what would change,
//!   with **no side effects**;
//! - `apply` makes the change and returns a [`preview::Outcome`];
//! - `verify` re-reads the machine and returns a [`preview::Check`].
//!
//! Every handler is idempotent: applying a change the machine already has makes no
//! difference, and the second `plan` is empty.
//!
//! # No shell, no ambient authority
//!
//! All machine access goes through the [`sys::CommandRunner`] and [`sys::FileSystem`]
//! traits. A command is an argv vector ([`sys::CommandSpec`]): the program is validated,
//! a shell is refused, each argument is a separate entry, the environment is allow-listed
//! and the output is capped. Nothing in this crate builds a command line by string
//! concatenation, and a source-scan test proves no shell is ever named.
//!
//! # Time and secrets
//!
//! The modules never read a wall clock. The network revert guard measures its window on an
//! injected [`spot_core::Clock`] (tests drive it with a manual clock), and every other
//! instant is an argument. No secret is ever logged or returned: a command line never
//! carries one, and the environment is allow-listed so no ambient secret leaks to a tool.

#![forbid(unsafe_code)]

pub mod catalogue;
pub mod certs;
pub mod chrony;
pub mod error;
pub mod facts;
pub mod handler;
pub mod host;
pub mod mac;
pub mod network;
pub mod preview;
pub mod provider;
pub mod real;
pub mod revert;
pub mod storage;
pub mod sys;
pub mod sysctl;

#[cfg(any(test, feature = "testing"))]
pub mod testing;

pub use catalogue::{LinuxSysModule, operations};
pub use certs::{CertificateInfo, CertificatesModule, CsrRequest, DistroFamily};
pub use chrony::{Source, TimeModule, TimeStatus, Tracking};
pub use error::{SysError, SysResult};
pub use facts::{BlockDevice, FactsReader, Mount};
pub use handler::LinuxSysHandler;
pub use host::{REVERT_SUBCOMMAND, RevertOutcome, revert_command_for, run_revert};
pub use mac::{DenialSummary, MacKind, MacMode, MacModule, MacStatus};
pub use network::{
    Addressing, Applied, NetworkBackend, NetworkChange, NetworkFacts, NetworkModule, Recovery,
    RevertRun, RevertTicket,
};
pub use preview::{Change, Check, Outcome, Preview};
pub use provider::LinuxSnapshotProvider;
pub use real::{StdCommandRunner, StdFileSystem, StdSleeper};
pub use revert::{DEFAULT_STATE_DIR, RevertScheduler, SystemdRunScheduler};
pub use storage::{SnapshotBackend, SnapshotInfo, SnapshotRequest, StorageFacts, StorageModule};
pub use sys::{CommandOutput, CommandRunner, CommandSpec, FileLock, FileSystem, Sleeper};
pub use sysctl::{KeyRule, SysctlModule};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-agent-linux-sys";
