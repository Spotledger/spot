//! [`NodeLoop`]: the edge-initiated client loop.
//!
//! ```text
//! run:   loop { session; on failure: offline, backoff (jittered, min interval), retry }
//! session: hello -> cursor -> rounds { pull+apply until idle ; push until idle ; idle wait }
//! ```
//!
//! # Guarantees
//!
//! * **Edge-initiated, offline-safe.** The loop only dials out. A hub that is down for hours or
//!   days ends the session with an error; the loop records the offline-since time, waits (the
//!   backoff is capped at five minutes by default) and tries again. Nothing about the site's
//!   readiness changes: an edge works without its hub (EDG-01).
//! * **Acknowledge only what is applied.** The next pull asks for `from_seq = prefix + 1`, where
//!   the prefix is the serve-sequence prefix the intake derived from settled bundles. Parked,
//!   refused, quarantined and undecodable bundles hold it back; the hub re-serves from there.
//! * **Resume after a cut at any step.** Before the hello nothing exists; in the middle of a
//!   pull nothing was applied; after an apply and before the acknowledgement the cursor is
//!   already durable and the re-served bundles are replays (idempotent, counted); in the middle
//!   of a push or after a push before its acknowledgement the push cursor is below the data, so
//!   the next offer re-reads and resends, and the hub's `(name, version)` idempotence absorbs it.
//! * **A stop never tears an apply.** The apply runs on the worker thread; the loop awaits it
//!   outside every `select!`, so a stop is looked at between steps and the bundle transaction
//!   in flight completes.
//! * **Reconnect storms.** The wait after a failure is the jittered backoff, never less than the
//!   minimum interval since the last attempt began.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use spot_core::Clock;
use spot_events_contract::SourceId;
use spot_sync_http::{JitterSource, Sleeper};
use spot_sync_protocol::{Action, Offer, Record, Sender, SenderConfig};
use tokio::sync::watch;

use crate::applier::{Intake, Received};
use crate::backoff::Backoff;
use crate::envelope::{self, Served};
use crate::error::{Error, Result};
use crate::link::HubLink;
use crate::push::PushSide;
use crate::settings::LoopSettings;
use crate::status::StatusHandle;
use crate::worker::Worker;

/// Everything the loop needs, built by the host.
pub struct LoopParts {
    /// The loop's timings and sizes.
    pub settings: LoopSettings,
    /// The link to the hub.
    pub link: HubLink,
    /// The hub's stream: what the node pulls.
    pub hub_source: SourceId,
    /// The node's receiving side, on its worker thread.
    pub intake: Arc<Worker<Box<dyn Intake>>>,
    /// The node's push side, when the node owns partitions it pushes.
    pub push: Option<Arc<Worker<Box<dyn PushSide>>>>,
    /// The clock (time is an argument).
    pub clock: Arc<dyn Clock>,
    /// Jitter for the backoff, injected.
    pub jitter: Box<dyn JitterSource>,
    /// Where waits happen, injected.
    pub sleeper: Arc<dyn Sleeper>,
    /// The status the service reads.
    pub status: StatusHandle,
}

/// The loop.
pub struct NodeLoop {
    parts: LoopParts,
    backoff: Backoff,
}

/// The widest pull window a stuck stream grows to.
const MAX_WINDOW: usize = 4_096;

/// How a session ended.
enum End {
    Stopped,
}

impl NodeLoop {
    /// A loop over `parts`.
    ///
    /// # Errors
    ///
    /// `sync_node_host.invalid_settings` when the settings are out of bounds.
    pub fn new(parts: LoopParts) -> Result<Self> {
        let settings = parts.settings.validated()?;
        let backoff = Backoff::new(settings.backoff_min, settings.backoff_max);
        Ok(Self { parts, backoff })
    }

    /// Runs until `stop` becomes `true`. Returns `Ok(())` on a clean stop; session failures are
    /// not errors here, they are the offline state (the loop retries forever).
    pub async fn run(mut self, mut stop: watch::Receiver<bool>) -> Result<()> {
        loop {
            if *stop.borrow() {
                return Ok(());
            }
            let began = self.parts.clock.monotonic();
            self.parts.status.connecting();
            match self.session(&mut stop).await {
                Ok(End::Stopped) => return Ok(()),
                Err(error) => {
                    let now = self.parts.clock.now_utc().unix_timestamp();
                    tracing::info!(code = error.code, "the hub link is down; backing off");
                    self.parts.status.offline(now, error.code);
                    self.refresh_queued().await;
                    let delay = self.backoff.next_delay(self.parts.jitter.as_mut());
                    let since = self.parts.clock.monotonic().saturating_sub(began);
                    let floor = self
                        .parts
                        .settings
                        .min_reconnect_interval
                        .saturating_sub(since);
                    if self.wait(delay.max(floor), &mut stop).await {
                        return Ok(());
                    }
                }
            }
        }
    }

    /// Sleeps for `duration`, or returns `true` early when the stop signal arrives.
    async fn wait(&self, duration: Duration, stop: &mut watch::Receiver<bool>) -> bool {
        tokio::select! {
            () = self.parts.sleeper.sleep(duration) => *stop.borrow(),
            changed = stop.changed() => changed.is_err() || *stop.borrow(),
        }
    }

    /// Best-effort refresh of the queued-changes number while offline.
    async fn refresh_queued(&self) {
        if let Some(push) = &self.parts.push
            && let Ok(queued) = push.call(|side| side.queued()).await
        {
            self.parts.status.set_queued(queued);
        }
    }

    async fn session(&mut self, stop: &mut watch::Receiver<bool>) -> Result<End> {
        let settings = self.parts.settings;
        let hello = tokio::select! {
            reply = self.parts.link.hello() => reply,
            _ = stop.changed() => return Ok(End::Stopped),
        };
        let _ = hello.map_err(|error| match error.code {
            spot_sync_http::error::VERSION_MISMATCH | spot_sync_http::error::METADATA_EPOCH => {
                Error::hub_refused(format!("the hub refused the handshake: {}", error.code))
            }
            _ => error,
        })?;
        self.parts.status.connected();

        let mut prefix = self.parts.intake.call(|intake| intake.cursor()).await?;
        let mut sender = Sender::new(
            SenderConfig::new(
                settings.batch_records,
                u64::try_from(settings.offer_records).unwrap_or(u64::MAX),
            )
            .map_err(|error| Error::invalid_settings(error.to_string()))?,
        )
        .map_err(|error| Error::invalid_settings(error.to_string()))?;
        let mut node_source: Option<SourceId> = None;
        if let Some(push) = &self.parts.push {
            let acked = push.call(|side| side.acked_seq()).await?;
            let source = push.call(|side| Ok(side.source())).await?;
            // The sender resumes after the stored acknowledgement: a cut at any push step leaves
            // the cursor below the data, so the next offer resends what the hub may not have.
            sender.resume(&source, acked.saturating_add(1));
            node_source = Some(source);
        }
        let mut unsettled: BTreeSet<u64> = BTreeSet::new();
        let mut caught_up_at = self.parts.clock.monotonic();
        let mut window = settings.batch_records;

        loop {
            if *stop.borrow() {
                return Ok(End::Stopped);
            }
            let mut moved = false;

            // Pull and apply.
            for _ in 0..settings.steps_per_round {
                if *stop.borrow() {
                    return Ok(End::Stopped);
                }
                let from = prefix.saturating_add(1);
                let pulled = tokio::select! {
                    batch = self.parts.link.pull(&self.parts.hub_source, from, window) => batch?,
                    _ = stop.changed() => return Ok(End::Stopped),
                };
                let Some(batch) = pulled else { break };
                if batch.source != self.parts.hub_source {
                    return Err(Error::foreign_record(
                        "the hub answered a pull with a batch of another source",
                    ));
                }
                let (served, bad) = decode_new(&batch.records, &unsettled, prefix);
                if served.is_empty() {
                    if bad > 0 {
                        self.parts.status.add_received(0, bad as u64);
                    }
                    // Everything in the batch is already with the intake (parked, held or
                    // refused) or undecodable. When the batch filled the window the bundle the
                    // parked ones wait for may lie beyond it: widen the window and ask again from
                    // the same acknowledged prefix. Otherwise wait for the hub to move.
                    if batch.records.len() >= window && window < MAX_WINDOW {
                        window = window.saturating_mul(2).min(MAX_WINDOW);
                        continue;
                    }
                    break;
                }
                for item in &served {
                    unsettled.insert(item.stamp.seq);
                }
                let now_ms =
                    u64::try_from(self.parts.clock.monotonic().as_millis()).unwrap_or(u64::MAX);
                let received: Received = self
                    .parts
                    .intake
                    .call(move |intake| intake.receive(served, bad, now_ms))
                    .await?;
                moved = true;
                if received.prefix > prefix {
                    window = settings.batch_records;
                }
                prefix = prefix.max(received.prefix);
                unsettled.retain(|seq| *seq > prefix);
                if unsettled.len() > settings.max_unsettled {
                    return Err(Error::apply_failed(
                        "too many served bundles are waiting unsettled; reconnecting",
                    ));
                }
                self.parts.status.add_received(
                    received.applied as u64,
                    (received.parked + received.held + received.refused + received.quarantined)
                        as u64,
                );
            }

            // Push.
            if let (Some(push), Some(node_source)) = (&self.parts.push, &node_source) {
                for _ in 0..settings.steps_per_round {
                    if *stop.borrow() {
                        return Ok(End::Stopped);
                    }
                    let max = settings.offer_records;
                    let offer: Offer = push.call(move |side| side.offer(max)).await?;
                    if offer.records.is_empty() {
                        break;
                    }
                    let Action::Send(batch) = sender.next(&offer) else {
                        break;
                    };
                    let sent = u64::try_from(batch.records.len()).unwrap_or(0);
                    let reply = tokio::select! {
                        reply = self.parts.link.push(&batch) => reply,
                        _ = stop.changed() => return Ok(End::Stopped),
                    };
                    let reply = match reply {
                        Ok(reply) => reply,
                        Err(error) => {
                            sender.on_timeout(node_source);
                            return Err(error);
                        }
                    };
                    moved = true;
                    match reply {
                        spot_sync_http::wire::PushReply::Ack(ack) => {
                            sender.on_ack(&ack);
                            let through = ack.through_seq;
                            push.call(move |side| side.acked(through)).await?;
                            self.parts.status.add_pushed(sent);
                        }
                        spot_sync_http::wire::PushReply::Nack(nack) => {
                            sender.on_nack(&nack);
                        }
                    }
                }
                if let Ok(queued) = push.call(|side| side.queued()).await {
                    self.parts.status.set_queued(queued);
                }
            }

            // A whole round went through without an error: the hub is really there, so the next
            // failure starts from the minimum again (a hub that answers the hello and nothing
            // else keeps backing off).
            self.backoff.reset();
            if moved {
                let behind = self.parts.clock.monotonic().saturating_sub(caught_up_at);
                self.parts
                    .status
                    .set_lag_ms(Some(u64::try_from(behind.as_millis()).unwrap_or(u64::MAX)));
                continue;
            }
            // Idle in both directions: caught up.
            caught_up_at = self.parts.clock.monotonic();
            self.parts.status.set_lag_ms(Some(0));
            if self.wait(settings.poll_idle, stop).await {
                return Ok(End::Stopped);
            }
        }
    }
}

/// Decodes the records of a batch that the intake has not seen yet: above the acknowledged
/// prefix and not already delivered. Returns the bundles and the number of undecodable records.
fn decode_new(records: &[Record], unsettled: &BTreeSet<u64>, prefix: u64) -> (Vec<Served>, usize) {
    let mut served = Vec::new();
    let mut bad = 0;
    for record in records {
        if record.seq <= prefix || unsettled.contains(&record.seq) {
            continue;
        }
        match envelope::decode(record) {
            Ok(item) => served.push(item),
            Err(error) => {
                tracing::warn!(
                    seq = record.seq,
                    code = error.code,
                    "a served record was refused"
                );
                bad += 1;
            }
        }
    }
    (served, bad)
}
