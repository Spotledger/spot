//! spot-mgmt-site: the site operation handlers of the management API (WP-ADM-01.T6).
//!
//! The catalogue of contract 0.2.0 names eighteen site operations. Ten are the migration verbs of
//! `spot-migration-ops` (their handlers are used here as they are); the other eight are
//! `site.status`, `site.backup.run|list|verify`, `site.restore.preflight|apply` and
//! `site.maintenance.enter|leave`. [`handlers`] returns all eighteen, ready for the
//! `MgmtHandlers` aggregator of the host.
//!
//! # One implementation
//!
//! A handler never reimplements a backup, a restore or the maintenance gate. It builds the
//! `spot` command line an operator would type (typed parameters in, one `--flag=value` argument
//! each, no shell), runs it through the [`ports::SiteCli`] port and returns what it printed as
//! capped, redacted JSON with the equivalent line attached. So "every console action has an
//! equivalent CLI command producing the same result" holds by construction: the management
//! result and the CLI result come from the same code, and a refusal of the CLI (exit 2) is a
//! refusal of the API.
//!
//! # Rules every handler keeps
//!
//! - the `site` parameter must be this host's site, else `mgmt.forbidden`;
//! - a backup target is a configured name (the catalogue pattern forbids a path);
//! - `restore.apply` needs the single-use `confirm` token of `restore.preflight`: no token, no
//!   command (the command checks it is the right one, unused and unexpired);
//! - only reads and checks may be cancelled; a backup, a restore and the maintenance gate are
//!   never torn down half way;
//! - output is capped (lines, characters) and credentials in it are redacted.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use std::sync::Arc;

use spot_mgmt_api::handlers::{Handler, HandlerError, HandlerRequest, HandlerResult, handler_fn};
use spot_mgmt_ops::OperationSpec;

pub mod args;
pub mod catalogue;
mod handlers;
mod migrate;
pub mod output;
pub mod ports;

pub use catalogue::{SITE_OPERATIONS, SITE_OPS_MODULE, SiteOpsModule, requires, specs};
pub use handlers::{Control, code_of_exit};
pub use ports::{
    CliError, CliOutput, CliRequest, Maintenance, MigrationContexts, SiteCli, SiteContext,
    SiteFacts, Timeouts,
};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-mgmt-site";

/// One operation, ready for the host: its catalogue entry, its handler and whether cancel is
/// supported.
#[derive(Clone)]
pub struct SiteOperation {
    /// The catalogue entry (the host adds it to the catalogue under [`SITE_OPS_MODULE`] or the
    /// migration module).
    pub spec: OperationSpec,
    /// The handler.
    pub handler: Arc<dyn Handler>,
    /// Whether a cancel request stops it.
    pub cancellable: bool,
}

impl SiteOperation {
    /// The operation id.
    #[must_use]
    pub fn id(&self) -> &str {
        self.spec.id.as_str()
    }

    /// Whether the operation is one of the ten `site.migrate.*` verbs.
    #[must_use]
    pub fn is_migration(&self) -> bool {
        self.id().starts_with("site.migrate.")
    }
}

/// Runs one operation of the site for `request`: the whole behaviour of a handler, apart from the
/// plumbing of the server (`HandlerContext` implements [`Control`]).
///
/// # Errors
/// A [`HandlerError`](spot_mgmt_api::handlers::HandlerError) with the management code of the
/// refusal or failure (`mgmt.forbidden` for another site, `mgmt.invalid_parameter`,
/// `mgmt.refused_by_policy`, `mgmt.internal`, `mgmt.unknown_operation`, or cancelled).
pub async fn invoke(
    ctx: &SiteContext,
    control: &dyn Control,
    request: &HandlerRequest,
) -> HandlerResult {
    match request.operation.as_str() {
        "site.backup.run" => handlers::backup_run(ctx, control, request).await,
        "site.backup.list" => handlers::backup_list(ctx, control, request).await,
        "site.backup.verify" => handlers::backup_verify(ctx, control, request).await,
        "site.restore.preflight" => handlers::restore_preflight(ctx, control, request).await,
        "site.restore.apply" => handlers::restore_apply(ctx, control, request).await,
        "site.maintenance.enter" => handlers::maintenance_enter(ctx, control, request).await,
        "site.maintenance.leave" => handlers::maintenance_leave(ctx, control, request).await,
        "site.status" => handlers::status(ctx, control, request).await,
        other if other.starts_with("site.migrate.") => migrate::run(ctx, request).await,
        _ => Err(HandlerError::failed("mgmt.unknown_operation")),
    }
}

/// The handlers of the eighteen site operations for `ctx`.
#[must_use]
pub fn handlers(ctx: SiteContext) -> Vec<SiteOperation> {
    let ctx = Arc::new(ctx);
    // Reads and checks can be abandoned; a backup, a restore, the gate and every migration verb
    // that changes something finish or fail on their own terms.
    let cancellable = |id: &str| {
        matches!(
            id,
            "site.backup.list"
                | "site.backup.verify"
                | "site.status"
                | "site.migrate.show"
                | "site.migrate.audit"
        )
    };
    let mut specs = catalogue::specs();
    specs.extend(spot_migration_ops::specs());
    specs
        .into_iter()
        .map(|spec| {
            let ctx = Arc::clone(&ctx);
            let handler: Arc<dyn Handler> = handler_fn(move |handler_ctx, request| {
                let ctx = Arc::clone(&ctx);
                async move { invoke(&ctx, &handler_ctx, &request).await }
            });
            let cancellable = cancellable(spec.id.as_str());
            SiteOperation {
                spec,
                handler,
                cancellable,
            }
        })
        .collect()
}
