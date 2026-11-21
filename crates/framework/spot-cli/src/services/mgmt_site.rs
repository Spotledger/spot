//! The site operations of the management API (WP-ADM-01.T6): hosts the handlers of
//! [`spot_mgmt_site`] on the aggregator [`MgmtHandlers`] port of the management API module.
//! Guide: `docs/running-spot/mgmt_site.md`.
//!
//! - The module adds the eighteen `site.*` operations (status, backup run/list/verify, restore
//!   preflight/apply, maintenance enter/leave and the ten migration verbs) when the management API
//!   module is present. Whether they are served is decided by `[services.mgmt_api] enabled`; this
//!   module has no listener of its own.
//! - Every operation runs the same `spot` command the operator would type: [`ChildCli`] starts this
//!   very executable with `--root` and the command's arguments (no shell, one `--flag=value` per
//!   argument), so the API and the CLI cannot drift and a refusal of the command is a refusal of the
//!   API. A command that outlives its timeout is killed.
//! - `[services.mgmt_site]` holds the two timeouts (`short_timeout_seconds`, default 600;
//!   `long_timeout_seconds`, default 21600). Unknown keys are refused.
//! - The migration verbs need the lifecycle context of this site ([`MigrationContexts`], a port
//!   another module provides); without it they refuse with the management code of
//!   `migration.port_unavailable` and change nothing.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use spot_mgmt_site::ports::BoxFuture;
use spot_mgmt_site::{
    CliError, CliOutput, CliRequest, Maintenance, MigrationContexts, SiteCli, SiteContext,
    SiteFacts, SiteOperation, Timeouts, handlers as site_handlers,
};

use crate::services::SiteCtx;
use crate::services::mgmt_api::MgmtHandlers;
use crate::{Error, Result};

/// Start order: after the management API module (100) it adds its operations to.
pub const ORDER: u32 = 1060;

/// The most bytes kept of one stream of a command (the handler caps the lines again).
const MAX_STREAM_BYTES: usize = 256 * 1024;

/// `[services.mgmt_site]`. Unknown keys are refused.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// The timeout of reads, checks, a preflight and `maintenance leave`, in seconds.
    pub short_timeout_seconds: u64,
    /// The timeout of a backup, a restore and `maintenance enter`, in seconds.
    pub long_timeout_seconds: u64,
}

impl Default for Settings {
    fn default() -> Self {
        let defaults = Timeouts::default();
        Self {
            short_timeout_seconds: defaults.short_secs,
            long_timeout_seconds: defaults.long_secs,
        }
    }
}

impl Settings {
    /// Checks the bounds: 10 seconds to a day for the short timeout, up to a week for the long one,
    /// and the long one not shorter than the short one.
    ///
    /// # Errors
    /// The setting out of bounds, in words.
    pub fn validate(&self) -> std::result::Result<(), String> {
        if !(10..=86_400).contains(&self.short_timeout_seconds) {
            return Err("short_timeout_seconds is 10 to 86400".to_owned());
        }
        if !(self.short_timeout_seconds..=604_800).contains(&self.long_timeout_seconds) {
            return Err(
                "long_timeout_seconds is at least short_timeout_seconds and at most 604800"
                    .to_owned(),
            );
        }
        Ok(())
    }

    fn timeouts(&self) -> Timeouts {
        Timeouts {
            short_secs: self.short_timeout_seconds,
            long_secs: self.long_timeout_seconds,
        }
    }
}

/// Runs `spot` commands as child processes of this executable.
#[derive(Clone, Debug)]
pub struct ChildCli {
    program: PathBuf,
    leading: Vec<String>,
    root: PathBuf,
}

impl ChildCli {
    /// A runner of `program` over the host `root` (`--root`).
    #[must_use]
    pub fn new(program: PathBuf, root: PathBuf) -> Self {
        Self {
            program,
            leading: Vec::new(),
            root,
        }
    }

    /// Arguments placed before `--root` (a wrapper program, or `-c SCRIPT NAME` for a shell in
    /// the tests). Empty in production.
    #[must_use]
    pub fn with_leading_args(mut self, leading: Vec<String>) -> Self {
        self.leading = leading;
        self
    }

    /// A runner of the running executable.
    ///
    /// # Errors
    /// The executable's own path cannot be read.
    pub fn current(root: &Path) -> Result<Self> {
        let program = std::env::current_exe().map_err(|_| {
            Error("services.mgmt_site: the running executable cannot be found".to_owned())
        })?;
        Ok(Self::new(program, root.to_path_buf()))
    }
}

fn bounded(mut bytes: Vec<u8>) -> String {
    bytes.truncate(MAX_STREAM_BYTES);
    String::from_utf8_lossy(&bytes).into_owned()
}

impl SiteCli for ChildCli {
    fn run(&self, request: CliRequest) -> BoxFuture<'_, std::result::Result<CliOutput, CliError>> {
        Box::pin(async move {
            let mut command = tokio::process::Command::new(&self.program);
            command
                .args(&self.leading)
                .arg(format!("--root={}", self.root.display()))
                .args(&request.argv)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                // Dropping this future (a timeout, a cancel) kills the command.
                .kill_on_drop(true);
            let child = command.spawn().map_err(|_| CliError::Spawn)?;
            let waited = tokio::time::timeout(
                Duration::from_secs(request.timeout_secs),
                child.wait_with_output(),
            )
            .await;
            match waited {
                Err(_elapsed) => Err(CliError::Timeout),
                Ok(Err(_)) => Err(CliError::Io),
                Ok(Ok(output)) => Ok(CliOutput {
                    // A command killed by a signal has no exit code: that is a failure (3).
                    exit: output
                        .status
                        .code()
                        .and_then(|code| u8::try_from(code).ok())
                        .unwrap_or(3),
                    stdout: bounded(output.stdout),
                    stderr: bounded(output.stderr),
                }),
            }
        })
    }
}

/// What `site.status` reads without running a command. The gate of the running server is not
/// readable from this module yet, so the state is reported as `unknown` rather than guessed.
#[derive(Clone, Copy, Debug, Default)]
pub struct HostFacts;

impl SiteFacts for HostFacts {
    fn version(&self) -> String {
        env!("CARGO_PKG_VERSION").to_owned()
    }

    fn maintenance(&self) -> Maintenance {
        Maintenance::Unknown
    }
}

/// The operations of one site over `cli`.
#[must_use]
pub fn operations_for(
    site: &str,
    cli: Arc<dyn SiteCli>,
    migrations: Option<Arc<dyn MigrationContexts>>,
    settings: &Settings,
) -> Vec<SiteOperation> {
    site_handlers(SiteContext {
        site: site.to_owned(),
        cli,
        facts: Arc::new(HostFacts),
        migrations,
        timeouts: settings.timeouts(),
    })
}

fn refuse(message: impl std::fmt::Display) -> Error {
    Error(format!("services.mgmt_site: {message}"))
}

/// Registers the operations. Nothing is added on a reload build (the aggregator is read once, when
/// the site is assembled) or when the management API module is not part of the assembly.
///
/// # Errors
/// A setting that does not decode or validate, or the executable's path cannot be read.
pub async fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("mgmt_site")?;
    settings.validate().map_err(refuse)?;
    if !ctx.assembling() {
        return Ok(());
    }
    let Some(aggregator) = ctx.port::<MgmtHandlers>() else {
        return Ok(());
    };
    let (site, root) = {
        let env = ctx.env()?;
        (env.site.site.clone(), env.root.to_path_buf())
    };
    let cli: Arc<dyn SiteCli> = Arc::new(ChildCli::current(&root)?);
    let migrations = ctx.port::<dyn MigrationContexts>();
    for operation in operations_for(&site, cli, migrations, &settings) {
        let module = if operation.is_migration() {
            spot_migration_ops::MIGRATION_OPS_MODULE
        } else {
            spot_mgmt_site::SITE_OPS_MODULE
        };
        aggregator.add(
            module,
            operation.spec,
            operation.handler,
            operation.cancellable,
        );
    }
    Ok(())
}
