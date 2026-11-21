//! The handler registry (WP-ADM-03.T2): `build.rs` scans `src/handlers/*.rs`; each file
//! declares `pub const NAMESPACE: &str` and `pub fn register(reg, ctx)`. A namespace that
//! `allowed_modules` does not enable is **not registered at all**, so a command for it is
//! answered `agent.unsupported`. Later handler sets (runtime, hardening, break-glass) are
//! new files here.

use std::path::PathBuf;
use std::sync::Arc;

use spot_agent_core::HandlerRegistry;
use spot_core::{Clock, SystemClock};

use crate::config::AgentConfig;
use crate::privilege::PrivilegedRunner;
use crate::state::default_state_dir;

/// What handler sets are built from: the command runners (every one is the
/// [`PrivilegedRunner`] in production), the filesystems, the clock and the state dir.
#[derive(Clone)]
pub struct HandlerCtx {
    /// Runner for `spot-agent-linux`.
    pub linux_runner: Arc<dyn spot_agent_linux::CommandRunner>,
    /// Filesystem for `spot-agent-linux`.
    pub linux_fs: Arc<dyn spot_agent_linux::FileSystem>,
    /// Runner for `spot-agent-linux-sys`.
    pub sys_runner: Arc<dyn spot_agent_linux_sys::CommandRunner>,
    /// Filesystem for `spot-agent-linux-sys`.
    pub sys_fs: Arc<dyn spot_agent_linux_sys::FileSystem>,
    /// The clock handlers read time through.
    pub clock: Arc<dyn Clock>,
    /// The agent state directory (network revert tickets live under it).
    pub state_dir: PathBuf,
}

impl HandlerCtx {
    /// The production context: every command goes through the privilege helper or is
    /// refused.
    #[must_use]
    pub fn production() -> Self {
        let runner = Arc::new(PrivilegedRunner::default());
        Self {
            linux_runner: runner.clone(),
            linux_fs: Arc::new(spot_agent_linux::StdFileSystem::new()),
            sys_runner: runner,
            sys_fs: Arc::new(spot_agent_linux_sys::StdFileSystem::new()),
            clock: Arc::new(SystemClock::new()),
            state_dir: default_state_dir(),
        }
    }
}

include!(concat!(env!("OUT_DIR"), "/handlers_gen.rs"));

/// The handlers for the namespaces `config.allowed_modules` enables (production context).
#[must_use]
pub fn build_registry(config: &AgentConfig) -> HandlerRegistry {
    build_registry_with(config, &HandlerCtx::production())
}

/// As [`build_registry`] with an explicit context (tests).
#[must_use]
pub fn build_registry_with(config: &AgentConfig, ctx: &HandlerCtx) -> HandlerRegistry {
    let mut registry = HandlerRegistry::new();
    register_all(&mut registry, ctx, &|namespace| config.allows(namespace));
    registry
}

/// The operation catalogue the agent verifies commands against: the union of the
/// catalogues the compiled handler sets declare (a handler file adds its own with
/// `pub fn register_catalogue`).
///
/// # Errors
/// The catalogue builder's problems (a duplicate id between sets).
pub fn build_catalogue() -> Result<spot_mgmt_ops::Catalogue, Vec<spot_mgmt_ops::CatalogueError>> {
    let mut builder = spot_mgmt_ops::CatalogueBuilder::new();
    register_catalogues(&mut builder);
    builder.build()
}
