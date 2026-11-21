//! The retention sweep, a job for `spot-jobs`.
//!
//! One run, at an instant the caller passes in:
//!
//! 1. **Expire.** `Outbox::expire_due` marks every `Queued` or `Sent` message past its lifetime
//!    `Expired` and pushes the event the producer reads; the slot it held is filled by a
//!    tombstone so the receiver's conversation does not stall (ADR-058 rule 13).
//! 2. **Refuse stale parked messages.** `Inbox::sweep_expired` refuses every parked message that
//!    expired while waiting for a gap to close.
//! 3. **Purge payloads.** Settled outbox entries older than the grace period, and (when the
//!    host configures it) applied inbox entries older than the keep period, lose their payload;
//!    the row, state, ids and times stay for audit, and an inbox row keeps the digest and the
//!    signature of the payload it emptied.
//!
//! The configurable *lifetime* of a message (1 to 365 days, per topic) is the sender's
//! [`Lifetimes`](spot_link_exchange::Lifetimes); this sweep only enforces it.

use std::sync::Arc;

use spot_jobs::worker::{HandlerResult, JobHandlerRegistry};
use spot_link_exchange::{IdSource, Inbox, InboxStore, InputQuarantine, Lifetimes, Outbox, Result};
use spot_tenant_link::{AuditSink, IdentitySigner};

use crate::scope::PgExchange;

/// The job name the sweep registers under.
pub const SWEEP_JOB: &str = "link_exchange_retention_sweep";

/// What a run keeps and how much it does at once.
#[derive(Clone, Copy, Debug)]
pub struct SweepConfig {
    /// How long a settled outbox payload is kept after the message settled.
    pub outbox_grace_ms: u64,
    /// How long an applied inbox payload is kept after it was received; `None` keeps it.
    pub inbox_keep_ms: Option<u64>,
    /// The most rows purged per table per run.
    pub batch: usize,
}

impl Default for SweepConfig {
    fn default() -> Self {
        Self {
            outbox_grace_ms: 7 * 86_400_000,
            inbox_keep_ms: None,
            batch: 1_000,
        }
    }
}

/// The pieces the sweep builds an `Outbox` and an `Inbox` from.
#[derive(Clone)]
pub struct SweepHost {
    /// The audit trail.
    pub audit: Arc<dyn AuditSink>,
    /// This side's identity key (signs the tombstones and receipts).
    pub signer: Arc<dyn IdentitySigner + Send + Sync>,
    /// Message ids.
    pub ids: Arc<dyn IdSource>,
    /// Where refused inputs go.
    pub quarantine: Arc<dyn InputQuarantine>,
    /// Message lifetimes.
    pub lifetimes: Lifetimes,
}

/// What one run did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// Messages marked `Expired` (each with an event for its producer).
    pub expired: usize,
    /// Parked messages refused because they expired while waiting.
    pub parked_refused: usize,
    /// Outbox payloads purged.
    pub outbox_purged: u64,
    /// Inbox payloads purged.
    pub inbox_purged: u64,
}

/// The sweep of one tenant.
#[derive(Clone)]
pub struct RetentionSweep {
    exchange: PgExchange,
    host: SweepHost,
    config: SweepConfig,
}

impl RetentionSweep {
    /// A sweep over `exchange`.
    #[must_use]
    pub fn new(exchange: PgExchange, host: SweepHost, config: SweepConfig) -> Self {
        Self {
            exchange,
            host,
            config,
        }
    }

    /// Runs once at `now` (Unix milliseconds). Synchronous, like the stores it drives: call it
    /// from a plain thread or a multi-thread runtime's blocking section.
    ///
    /// # Errors
    /// An audit, quarantine or store failure.
    pub fn run(&self, now: u64) -> Result<SweepReport> {
        let outbox_store = self.exchange.outbox();
        let inbox_store = self.exchange.inbox();
        let outbox = Outbox::new(
            &outbox_store,
            self.host.audit.as_ref(),
            self.host.signer.as_ref(),
            self.host.ids.as_ref(),
            self.host.lifetimes.clone(),
        );
        let inbox = Inbox::new(
            &inbox_store,
            self.host.audit.as_ref(),
            self.host.quarantine.as_ref(),
            self.host.signer.as_ref(),
        );
        let expired = outbox.expire_due(now)?.len();
        let parked_refused = inbox.sweep_expired(now)?.len();
        let outbox_purged = outbox_store.purge_settled(
            now.saturating_sub(self.config.outbox_grace_ms),
            now,
            self.config.batch,
        )?;
        let inbox_purged = match self.config.inbox_keep_ms {
            Some(keep) => {
                inbox_store.purge_applied(now.saturating_sub(keep), now, self.config.batch)?
            }
            None => 0,
        };
        Ok(SweepReport {
            expired,
            parked_refused,
            outbox_purged,
            inbox_purged,
        })
    }

    /// Registers the sweep as the handler of [`SWEEP_JOB`]. A failed run asks to be retried; the
    /// failure itself is audited by the exchange where it applies and is never put in the job
    /// result (which carries a fixed sentence only).
    pub fn register(self: Arc<Self>, registry: &mut JobHandlerRegistry) {
        registry.register(SWEEP_JOB, move |context| {
            let sweep = Arc::clone(&self);
            Box::pin(async move {
                let nanos = context.clock.unix_timestamp_nanos();
                let Ok(now) = u64::try_from(nanos / 1_000_000) else {
                    return Ok(HandlerResult::Fail(
                        "the job clock is out of range".to_owned(),
                    ));
                };
                let outcome = tokio::task::spawn_blocking(move || sweep.run(now)).await;
                Ok(match outcome {
                    Ok(Ok(_)) => HandlerResult::Ok,
                    _ => HandlerResult::Retry("the retention sweep could not finish".to_owned()),
                })
            })
        });
    }
}
