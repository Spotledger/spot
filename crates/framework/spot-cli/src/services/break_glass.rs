//! Break-glass, the customer's side (WP-ADM-11.T2): the tenant-side registry of break-glass
//! sessions, the append-only recording archive with its retention, the customer's list, summary,
//! recording read and revoke, and the automatic kill at expiry.
//!
//! `spot-break-glass` models one session (request, approvals, window, recording, events) and is
//! pure. The interactive terminal lives on the host agent (cluster E); this module is what the
//! *tenant server* holds: the evidence the customer sees, and the one place that can say "this
//! session may no longer act".
//!
//! # What is held where
//!
//! - **The customer's chain is the truth.** Every transition is an event on the tenant's audit
//!   chain ([`EventLog`]); [`CustomerView`] is rebuilt from it on every read, so nothing the
//!   operator side holds can change what the customer sees. A restart rebuilds the registry from
//!   the chain and the window objects (see below).
//! - **Windows.** When a session is activated the support side calls [`BreakGlassPort::register`]
//!   with its window; it is written once as an object (`registry/<session>.json`, never
//!   overwritten) so another node, or a restart, knows the bounds.
//! - **Recordings** are customer data containing secrets. [`BreakGlassPort::archive_recording`]
//!   verifies the hash chain of the bytes, seals them ([`Sealer`], the secrets service) and writes
//!   them once (`recordings/<session>.rec`, with a plain manifest holding only the digest). The
//!   digest goes to the audit chain as the `recording` event. There is no overwrite and no
//!   delete on the port; only the retention sweep of this service removes an object, once the
//!   retention from the data policy has passed and the session is over.
//! - **Reads** need the `security_administrator` duty (fail closed: an error from the duty check,
//!   a missing check or a missing duty is a refusal). A recording read is itself an event on the
//!   chain, written *before* the content is returned; if it cannot be written nothing is
//!   returned. The content is never put in an event, a log line or an error.
//!
//! # Revoke and expiry
//!
//! A revoke marks the session over in the registry *first*, so [`BreakGlassPort::authorise`]
//! refuses it from that instant whatever else fails (a lost message cannot extend a session). It
//! then writes the `revoked` event and delivers the kill through the in-process [`AgentBridge`].
//! A kill or an event that cannot be delivered is retried by the sweep; the answer says which of
//! the two did not happen yet. The sweep (default every 15 s) also ends every session past its
//! window, kills it and writes `expired`, even if the operator side never reports an end.
//!
//! # Level
//!
//! Optional. When a part is not wired (no event log, object store, sealer, duty check) the
//! start-up check fails, readiness reports the site degraded naming the part, every read answers
//! `break_glass_host.not_wired`, and [`BreakGlassPort::authorise`] refuses: a break-glass session
//! can never be authorised by a host that cannot show it to the customer. The rest of the site is
//! unaffected. Setting `[services.break_glass]` (all optional, unknown keys refused): `enabled`,
//! `sweep_secs`, `retention_days` (the fallback when the data policy names none),
//! `max_recording_bytes`, `page_max`.

#[path = "break_glass/host.rs"]
mod host;
#[path = "break_glass/ports.rs"]
mod ports;
#[cfg(test)]
#[path = "break_glass/tests.rs"]
mod tests;

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;

pub use host::BreakGlassHost;
pub use ports::*;

use crate::Result;
use crate::services::{CheckFuture, Level, RunFuture, Service, SiteCtx};

/// Start order: after the services it reads ports from (files, secrets, the support-access
/// service).
pub const ORDER: u32 = 1170;

struct BreakGlassService {
    host: Arc<BreakGlassHost>,
    every: Duration,
}

impl Service for BreakGlassService {
    fn name(&self) -> &'static str {
        "break_glass"
    }

    fn level(&self) -> Level {
        Level::Optional
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async move {
            let missing = self.host.missing();
            if !missing.is_empty() {
                return Err(format!(
                    "break-glass is not wired (missing: {}); no break-glass session can be authorised until it is",
                    missing.join(", ")
                ));
            }
            self.host
                .load()
                .await
                .map_err(|e| format!("the break-glass registry could not be loaded: {e}"))
        })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let mut stop = stop;
            loop {
                if *stop.borrow() {
                    break;
                }
                tokio::select! {
                    changed = stop.changed() => {
                        if changed.is_err() {
                            break;
                        }
                    }
                    () = tokio::time::sleep(self.every) => {
                        // What cannot be done now stays queued for the next sweep.
                        let _ = self.host.sweep().await;
                    }
                }
            }
            Ok(())
        })
    }
}

/// Registers the customer side of break-glass.
///
/// The parts come from the ports earlier modules provide; a part nobody provides leaves the
/// service degraded (see the module's "Level"), never silently permissive.
///
/// # Errors
///
/// A setting that does not decode or is out of bounds, or the port provided twice.
pub fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("break_glass")?;
    settings.validate()?;
    if !settings.enabled {
        return Ok(());
    }
    let parts = Parts {
        log: ctx.port::<dyn EventLog>(),
        store: ctx.port::<dyn ObjectStore>(),
        retire: ctx.port::<dyn Retire>(),
        sealer: ctx.port::<dyn Sealer>(),
        duties: ctx.port::<dyn Duties>(),
        bridge: ctx.port::<dyn AgentBridge>(),
        retention: ctx.port::<dyn Retention>(),
    };
    let every = Duration::from_secs(settings.sweep_secs);
    let host = Arc::new(BreakGlassHost::new(
        ctx.label(),
        Arc::clone(ctx.clock()),
        parts,
        settings,
    ));
    let port: Arc<dyn BreakGlassPort> = Arc::clone(&host) as Arc<dyn BreakGlassPort>;
    ctx.provide::<dyn BreakGlassPort>(port)?;
    ctx.add(Arc::new(BreakGlassService { host, every }));
    Ok(())
}
