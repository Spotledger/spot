//! The document audit's chain appenders (WP-PRD-01.T22): one append handler per lane on the job
//! worker, and a required service that keeps one append job per lane queued. Registered after
//! `jobs` (it takes the `JobSubmit` port) and before the worker is built (the worker is built
//! in a finaliser, after every module registered).
//!
//! The registration moved here from `host.rs` when the host's table moved into modules.

use std::sync::Arc;

use super::SiteCtx;
use crate::Error;

/// This module's start order: after `jobs` (600), whose `JobSubmit` port it takes.
pub const ORDER: u32 = 650;

/// Registers the document audit's appenders.
pub async fn register(ctx: &mut SiteCtx<'_>) -> crate::Result<()> {
    if !ctx.assembling() {
        return Ok(());
    }
    let env = *ctx.env()?;
    let Some(host) = env.live.doc_audit() else {
        return Err(Error(format!(
            "site `{}`: the document audit was not built for this site",
            env.site.site
        )));
    };
    let queue = env
        .loaded
        .common
        .services
        .jobs
        .queues
        .first()
        .cloned()
        .ok_or_else(|| {
            Error(
                "the document audit needs a job queue; `[services.jobs] queues` is empty"
                    .to_owned(),
            )
        })?;
    host.register_handlers(ctx.job_handlers());
    let submit = ctx.require_port::<dyn super::ports::JobSubmit>()?;
    ctx.add(crate::audit::doc::DocAuditService::new(
        host,
        submit,
        queue,
        Arc::clone(ctx.clock()),
    ));
    Ok(())
}
