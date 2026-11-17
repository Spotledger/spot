//! The handlers of the eight site operations. Each one checks the site, builds the CLI command
//! the operator would type, runs it through the [`SiteCli`](crate::ports::SiteCli) port and
//! returns its output as capped, redacted JSON with the equivalent command line attached. There
//! is no second implementation of a backup, a restore or the maintenance gate here: if the CLI
//! refuses, so does the API, with the same exit code mapped to a management code.

use serde_json::{Value, json};
use spot_mgmt_api::handlers::{HandlerContext, HandlerError, HandlerRequest, HandlerResult};
use spot_mgmt_api::ports::BoxFuture;

use crate::args::{self, CliLine};
use crate::output::{self, Keep};
use crate::ports::{CliError, CliOutput, CliRequest, Maintenance, SiteContext};

/// What a handler needs from its running command: progress lines and the cancel request. The
/// server's `HandlerContext` is one; tests supply their own.
pub trait Control: Send + Sync {
    /// Records a progress line (advisory).
    fn progress<'a>(&'a self, step: &'a str, percent: u8) -> BoxFuture<'a, ()>;
    /// Completes when cancel is requested.
    fn cancelled(&self) -> BoxFuture<'_, ()>;
}

impl Control for HandlerContext {
    fn progress<'a>(&'a self, step: &'a str, percent: u8) -> BoxFuture<'a, ()> {
        Box::pin(HandlerContext::progress(self, step, percent))
    }

    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(HandlerContext::cancelled(self))
    }
}

/// How a command may be run.
#[derive(Clone, Copy)]
pub(crate) struct Mode {
    /// Whether the command may take the long timeout.
    pub long: bool,
    /// Whether a cancel request may stop it (only reads and checks: a backup, a restore and the
    /// gate are never torn down half way).
    pub cancellable: bool,
    /// What a result may keep verbatim.
    pub keep: Keep,
}

impl Mode {
    pub(crate) const READ: Mode = Mode {
        long: false,
        cancellable: true,
        keep: Keep::Nothing,
    };
    pub(crate) const CHECK: Mode = Mode {
        long: true,
        cancellable: true,
        keep: Keep::Nothing,
    };
    pub(crate) const SHORT: Mode = Mode {
        long: false,
        cancellable: false,
        keep: Keep::Nothing,
    };
    pub(crate) const LONG: Mode = Mode {
        long: true,
        cancellable: false,
        keep: Keep::Nothing,
    };
}

/// The error a CLI exit code stands for. 0 is success; 1 (usage) is a parameter the catalogue
/// let through that the command does not accept; 2 is a refusal with nothing changed; anything
/// else is a failure.
#[must_use]
pub fn code_of_exit(exit: u8) -> Option<&'static str> {
    match exit {
        0 => None,
        1 => Some("mgmt.invalid_parameter"),
        2 => Some("mgmt.refused_by_policy"),
        _ => Some("mgmt.internal"),
    }
}

/// A `site` parameter must be this host's site: a command for another site is `mgmt.forbidden`.
pub(crate) fn site_of<'a>(
    ctx: &SiteContext,
    request: &'a HandlerRequest,
) -> Result<&'a str, HandlerError> {
    let Some(site) = args::text(&request.params, "site") else {
        return Err(HandlerError::failed("mgmt.invalid_parameter"));
    };
    if site == ctx.site {
        Ok(site)
    } else {
        Err(HandlerError::failed("mgmt.forbidden"))
    }
}

fn param<'a>(request: &'a HandlerRequest, name: &str) -> Result<&'a str, HandlerError> {
    args::text(&request.params, name).ok_or_else(|| HandlerError::failed("mgmt.invalid_parameter"))
}

async fn execute(
    ctx: &SiteContext,
    ctl: &dyn Control,
    line: &CliLine,
    mode: Mode,
) -> Result<CliOutput, HandlerError> {
    let timeout_secs = if mode.long {
        ctx.timeouts.long_secs
    } else {
        ctx.timeouts.short_secs
    };
    let running = ctx.cli.run(CliRequest {
        argv: line.argv.clone(),
        timeout_secs,
    });
    let ended = if mode.cancellable {
        tokio::select! {
            ended = running => ended,
            () = ctl.cancelled() => return Err(HandlerError::cancelled()),
        }
    } else {
        running.await
    };
    match ended {
        Ok(output) => Ok(output),
        Err(CliError::Timeout | CliError::Spawn | CliError::Io) => {
            Err(HandlerError::failed("mgmt.internal"))
        }
    }
}

/// Runs one command and turns its end into a handler result.
async fn run_command(
    ctx: &SiteContext,
    ctl: &dyn Control,
    line: &CliLine,
    mode: Mode,
) -> HandlerResult {
    let output = execute(ctx, ctl, line, mode).await?;
    if let Some(code) = code_of_exit(output.exit) {
        return Err(HandlerError::failed(code));
    }
    Ok(output::result(&output, &line.display, mode.keep))
}

pub(crate) async fn backup_run(
    ctx: &SiteContext,
    ctl: &dyn Control,
    request: &HandlerRequest,
) -> HandlerResult {
    let site = site_of(ctx, request)?;
    let line = args::backup_run(site, param(request, "target")?);
    ctl.progress("backup run", 0).await;
    let result = run_command(ctx, ctl, &line, Mode::LONG).await;
    ctl.progress("backup run finished", 100).await;
    result
}

pub(crate) async fn backup_list(
    ctx: &SiteContext,
    ctl: &dyn Control,
    request: &HandlerRequest,
) -> HandlerResult {
    let site = site_of(ctx, request)?;
    let line = args::backup_list(site);
    let output = execute(ctx, ctl, &line, Mode::READ).await?;
    if let Some(code) = code_of_exit(output.exit) {
        return Err(HandlerError::failed(code));
    }
    let mut result = output::result(&output, &line.display, Keep::Nothing);
    let listed = output::lines(&output.stdout, Keep::Nothing, output::MAX_LINES).0;
    result["backups"] = Value::Array(output::backups(&listed));
    Ok(result)
}

pub(crate) async fn backup_verify(
    ctx: &SiteContext,
    ctl: &dyn Control,
    request: &HandlerRequest,
) -> HandlerResult {
    let site = site_of(ctx, request)?;
    let line = args::backup_verify(site, param(request, "backup_id")?);
    run_command(ctx, ctl, &line, Mode::CHECK).await
}

pub(crate) async fn restore_preflight(
    ctx: &SiteContext,
    ctl: &dyn Control,
    request: &HandlerRequest,
) -> HandlerResult {
    let site = site_of(ctx, request)?;
    let line = args::restore_preflight(
        site,
        param(request, "backup_id")?,
        param(request, "target")?,
    );
    // The token is the one thing this result must carry: the caller hands it to `restore.apply`.
    let mode = Mode {
        keep: Keep::ConfirmToken,
        ..Mode::SHORT
    };
    run_command(ctx, ctl, &line, mode).await
}

pub(crate) async fn restore_apply(
    ctx: &SiteContext,
    ctl: &dyn Control,
    request: &HandlerRequest,
) -> HandlerResult {
    let site = site_of(ctx, request)?;
    let confirm = param(request, "confirm")?;
    // A restore cannot start without the preflight's token. Whether it is the right one, unused
    // and unexpired is decided by the command itself (it stores only a digest of it).
    if !args::plausible_token(confirm) {
        return Err(HandlerError::failed("mgmt.invalid_parameter"));
    }
    let line = args::restore_apply(
        site,
        param(request, "backup_id")?,
        param(request, "target")?,
        confirm,
    );
    ctl.progress("restore apply", 0).await;
    let result = run_command(ctx, ctl, &line, Mode::LONG).await;
    ctl.progress("restore apply finished", 100).await;
    result
}

pub(crate) async fn maintenance_enter(
    ctx: &SiteContext,
    ctl: &dyn Control,
    request: &HandlerRequest,
) -> HandlerResult {
    let site = site_of(ctx, request)?;
    let line = args::maintenance_enter(site, param(request, "reason")?);
    run_command(ctx, ctl, &line, Mode::LONG).await
}

pub(crate) async fn maintenance_leave(
    ctx: &SiteContext,
    ctl: &dyn Control,
    request: &HandlerRequest,
) -> HandlerResult {
    let site = site_of(ctx, request)?;
    let line = args::maintenance_leave(site);
    run_command(ctx, ctl, &line, Mode::SHORT).await
}

/// The newest of the listed backups: by the taken-at value as a number when all of them are
/// numbers, otherwise as text.
fn newest(backups: &[Value]) -> Option<&Value> {
    let key = |backup: &Value| -> Option<String> {
        backup
            .get("taken_at")
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    let numeric = backups
        .iter()
        .all(|b| key(b).is_some_and(|k| k.parse::<i64>().is_ok()));
    if numeric {
        backups.iter().max_by_key(|b| {
            key(b)
                .and_then(|k| k.parse::<i64>().ok())
                .unwrap_or(i64::MIN)
        })
    } else {
        backups.iter().max_by_key(|b| key(b))
    }
}

/// `site.status`: version, maintenance gate, newest backup and whether a migration is held. A
/// part that cannot be read is reported as `null`; the status itself does not fail because of it.
pub(crate) async fn status(
    ctx: &SiteContext,
    ctl: &dyn Control,
    request: &HandlerRequest,
) -> HandlerResult {
    let site = site_of(ctx, request)?;
    let maintenance = match ctx.facts.maintenance() {
        Maintenance::Open => json!({"state": "open"}),
        Maintenance::Closed { reason } => json!({"state": "closed", "reason": reason}),
        Maintenance::Unknown => json!({"state": "unknown"}),
    };
    let listing = args::backup_list(site);
    let last_backup = match execute(ctx, ctl, &listing, Mode::READ).await {
        Ok(out) if out.exit == 0 => {
            let lines = output::lines(&out.stdout, Keep::Nothing, output::MAX_LINES).0;
            newest(&output::backups(&lines)).cloned()
        }
        Ok(_)
        | Err(HandlerError {
            cancelled: false, ..
        }) => None,
        Err(cancelled) => return Err(cancelled),
    };
    let hold_line = args::hold_status(site);
    let held = match execute(ctx, ctl, &hold_line, Mode::READ).await {
        Ok(out) if out.exit == 0 => {
            let lines = output::lines(&out.stdout, Keep::Nothing, 5).0;
            let open = lines.iter().any(|l| l.contains("open (no hold)"));
            json!({"held": !open, "detail": lines})
        }
        Ok(_)
        | Err(HandlerError {
            cancelled: false, ..
        }) => Value::Null,
        Err(cancelled) => return Err(cancelled),
    };
    Ok(json!({
        "site": site,
        "version": ctx.facts.version(),
        "maintenance": maintenance,
        "last_backup": last_backup,
        "migration_hold": held,
        "cli": format!("spot backup list --site {site} && spot migrate-hold status --site {site}"),
    }))
}
