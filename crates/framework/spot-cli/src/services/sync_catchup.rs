//! Catch-up and bootstrap hosted (WP-RTB-02.T7, ADR-066,
//! `docs/running-spot/sync_catchup.md`): the hub-side director of a site.
//!
//! # What it does
//!
//! Builds the [`spot_sync_catchup_host::Director`] over the durable progress table
//! (`spot_sync_catchup_progress`, created by `spot migrate`), with the thresholds of
//! `[services.sync_catchup]` and a bounded number of parallel bootstraps, and provides it as a
//! port for the hub host (WP-RTB-02.T4): the hub asks it how to route a returning node, reports
//! each applied chunk and, when the node is live again, `finish`.
//!
//! # Configuration
//!
//! `[services.sync_catchup]` (the catch-up thresholds are documented there; unknown keys are
//! refused). The brief names `services.sync_hub.catchup.*`, a sub-table of another module's table,
//! which this module cannot claim without editing the hub host's settings: see the task report.
//!
//! # Roles
//!
//! Hub only. A standalone site and an edge have no returning nodes to route, so the module
//! registers only on a hub.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use spot_pg_bridge::{Bridge, BridgeConfig};
use spot_sync_catchup_host::{CatchupConfig, Director, PgProgress};

use crate::services::{CheckFuture, Level, RunFuture, Service, SiteCtx};
use crate::{Error, Result};

/// After the hub host.
pub const ORDER: u32 = 1150;

/// The roles the module runs in: only a hub routes returning nodes.
pub const ROLES: &[&str] = &["hub"];

/// The module's service: the start-up check that the progress table exists.
struct CatchupService {
    bridge: Arc<Bridge>,
}

impl Service for CatchupService {
    fn name(&self) -> &'static str {
        "sync_catchup"
    }

    fn level(&self) -> Level {
        // Losing it means no catch-up plan and no bootstrap, which the hub says in its own
        // conditions; a returning node stays on live relay or waits.
        Level::Optional
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async move {
            let bridge = Arc::clone(&self.bridge);
            tokio::task::spawn_blocking(move || {
                bridge
                    .run_default(|conn| {
                        Box::pin(async move {
                            conn.query_one(
                                "SELECT to_regclass('spot_sync_catchup_progress') IS NOT NULL",
                                &[],
                            )
                            .await
                            .map(|row| row.get::<_, bool>(0))
                        })
                    })
                    .map_err(|_| "the catch-up progress table could not be checked".to_owned())
                    .and_then(|present| {
                        if present {
                            Ok(())
                        } else {
                            Err("the catch-up progress table is missing; it is created by `spot migrate`".to_owned())
                        }
                    })
            })
            .await
            .map_err(|_| "the catch-up check task failed".to_owned())?
        })
    }

    fn run(&self, mut stop: tokio::sync::watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let _ = stop.wait_for(|stopped| *stopped).await;
            Ok(())
        })
    }
}

fn provided() -> &'static Mutex<HashMap<String, Arc<Director>>> {
    static PROVIDED: OnceLock<Mutex<HashMap<String, Arc<Director>>>> = OnceLock::new();
    PROVIDED.get_or_init(Mutex::default)
}

/// The director of a running hub (for a harness).
#[must_use]
pub fn hosted(site: &str) -> Option<Arc<Director>> {
    provided()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(site)
        .cloned()
}

fn build_bridge(backend: &spot_postgres::PgBackend, pool_size: usize) -> Result<Arc<Bridge>> {
    let largest = BridgeConfig::largest_in_flight(pool_size);
    if largest < 1 {
        return Err(Error(format!(
            "services.sync_catchup needs a PostgreSQL pool of at least 3 connections (postgres.pool_max_size is {pool_size})"
        )));
    }
    let config = BridgeConfig {
        name: "sync_catchup".to_owned(),
        max_in_flight: largest.min(4),
        workers: 2,
        queue_capacity: 8,
        default_deadline: Duration::from_secs(10),
        max_deadline: Duration::from_secs(30),
    };
    let backend = backend.clone();
    std::thread::spawn(move || {
        Bridge::for_backend(config, &backend).map_err(|e| Error(format!("{e}")))
    })
    .join()
    .map_err(|_| Error("the catch-up bridge thread failed".to_owned()))?
    .map(Arc::new)
}

/// Registers the director (see the module docs). A reload re-provides the running one.
pub fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let site = ctx.label().to_owned();
    if !ctx.assembling() {
        if let Some(director) = hosted(&site) {
            ctx.provide::<Director>(director)?;
        }
        return Ok(());
    }
    let config: CatchupConfig = ctx.settings_or_default("sync_catchup")?;
    config
        .validate()
        .map_err(|e| Error(format!("services.sync_catchup: {}: {e}", e.code())))?;
    let env = *ctx.env()?;
    let bridge = build_bridge(env.backend, env.loaded.common.postgres.pool_max_size)?;
    let store = Arc::new(PgProgress::new(Arc::clone(&bridge)));
    let director = Arc::new(
        Director::new(&config, store)
            .map_err(|e| Error(format!("services.sync_catchup: {}", e.code())))?,
    );
    ctx.add(Arc::new(CatchupService { bridge }));
    ctx.provide::<Director>(Arc::clone(&director))?;
    provided()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(site, director);
    Ok(())
}
