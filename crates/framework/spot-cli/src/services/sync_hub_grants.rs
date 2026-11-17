//! The hub half of the offline edge, hosted: lease and permission snapshot requests answered
//! (WP-RTB-03.T8; ADR-062 section 7, ADR-070 rule 12.13). The logic is `spot_sync_hub_host::grants`;
//! this module reads `[services.sync_hub_grants]`, opens the tenant's [`Grants`] from the inputs
//! other modules provide, and offers the handler to the hub's connection layer.
//!
//! ```toml
//! [services.sync_hub_grants]
//! default_max = 1000                 # largest range of a series without its own maximum
//! min_interval_secs = 60             # least time between two grants of a series to a node
//! max_series = 64                    # distinct series the hub tracks
//! series_max = { INV = 5000 }        # per-series maximum range
//! entitle = [{ node = "edge-khi#branch_07", series = "INV" }]   # a node leases nothing by default
//! ```
//!
//! # Inputs are ports
//!
//! The node registry (standing), the audit chain, the durable grant log
//! ([`PgGrantLog`](spot_sync_hub_host::grants::PgGrantLog), table `spot_lease_grant_log` from
//! `spot migrate`) and, when the hub serves snapshots, the snapshot issuer come from the modules
//! that own them as one [`GrantsInputs`] port. Without it the module says so in its check and
//! answers nothing: a lease is never granted on a guess. The handler it offers is the
//! `dyn CarrierHandler` port, for the hub's connection layer to call with the authenticated
//! node (nothing in a payload names the node).
//!
//! Off unless `[services.sync_hub_grants]` is configured.

use std::sync::Arc;

use spot_sync_hub_host::grants::{CarrierHandler, Grants, GrantsInputs, PolicySettings};

use crate::services::{CheckFuture, Idle, Level, SiteCtx};
use crate::{Error, Result};

/// The module's name and its `[services.<name>]` table.
pub const NAME: &str = "sync_hub_grants";
/// After the hub service (1300).
pub const ORDER: u32 = 1310;

/// The settings table.
pub type Settings = PolicySettings;

/// Registers the module when `[services.sync_hub_grants]` is configured; absent, it is off.
///
/// # Errors
///
/// [`Error`] for an illegal series key or node id in the settings.
pub fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let Some(settings) = ctx.settings::<Settings>(NAME).ok() else {
        return Ok(());
    };
    let policy = settings
        .build()
        .map_err(|why| Error(format!("{NAME}: {why}")))?;
    if !ctx.assembling() {
        return Ok(());
    }
    let grants = ctx
        .port::<GrantsInputs>()
        .and_then(|inputs| inputs.open(policy))
        .map(Arc::new);
    let unwired = match grants {
        Some(grants) => {
            ctx.provide::<Grants>(Arc::clone(&grants))?;
            let handler: Arc<dyn CarrierHandler> = grants;
            ctx.provide::<dyn CarrierHandler>(handler)?;
            None
        }
        None => Some(
            "sync_hub_grants: no module provides the grant inputs (node registry, audit, grant \
             log); no range or snapshot is granted"
                .to_owned(),
        ),
    };
    ctx.add(Idle::new(
        NAME,
        Level::Optional,
        move || -> CheckFuture<'static> {
            let unwired = unwired.clone();
            Box::pin(async move { unwired.map_or(Ok(()), Err) })
        },
    ));
    Ok(())
}
