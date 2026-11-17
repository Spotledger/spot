//! The driver: a plain thread that runs the relay and publishes the serve windows.
//!
//! [`HubCore`] is the narrow, synchronous port the thread drives; [`RelayCore`] implements it
//! over a real [`HubRelay`] (its queue, stream and image source are PostgreSQL-backed in
//! production and in-memory in tests). Policy (which node gets what) is the relay's
//! destinations, registered by the policy loader (WP-RTB-02.T8): [`RelayCore::add_destination`].

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use spot_core::Clock;
use spot_sync_closure::ImageSource;
use spot_sync_hub::{Destination, HubRelay, HubStream, SendQueue};
use spot_sync_protocol::{Record, RecordName};
use tokio::sync::mpsc;

use crate::cache::{AckNote, ServeCache};

/// What the driver thread needs from the hub.
pub trait HubCore: Send {
    /// Stages the tenant stream into the destination queues.
    ///
    /// # Errors
    /// A short reason (no payload).
    fn tick(&mut self, now_ms: i64) -> Result<(), String>;

    /// The up-to-`max` oldest queued items of `dest`, as wire records numbered by serve
    /// sequence.
    ///
    /// # Errors
    /// A short reason.
    fn window(&mut self, dest: &str, max: usize) -> Result<Vec<Record>, String>;

    /// Trims `dest`'s queue through the serve sequence `through`.
    ///
    /// # Errors
    /// A short reason (an ack above what was issued, an unknown destination).
    fn ack_served(&mut self, dest: &str, through: u64, now_ms: i64) -> Result<(), String>;

    /// The destinations the hub serves.
    fn destinations(&self) -> Vec<String>;
}

/// A boxed core drives like the core it holds (the service keeps it behind a trait object).
impl HubCore for Box<dyn HubCore> {
    fn tick(&mut self, now_ms: i64) -> Result<(), String> {
        (**self).tick(now_ms)
    }
    fn window(&mut self, dest: &str, max: usize) -> Result<Vec<Record>, String> {
        (**self).window(dest, max)
    }
    fn ack_served(&mut self, dest: &str, through: u64, now_ms: i64) -> Result<(), String> {
        (**self).ack_served(dest, through, now_ms)
    }
    fn destinations(&self) -> Vec<String> {
        (**self).destinations()
    }
}

/// A [`HubCore`] over a real relay.
pub struct RelayCore<Q: SendQueue, S: HubStream, I: ImageSource> {
    relay: HubRelay<Q, S, I>,
    dests: Vec<String>,
}

impl<Q: SendQueue, S: HubStream, I: ImageSource> RelayCore<Q, S, I> {
    /// A core over `relay`.
    #[must_use]
    pub fn new(relay: HubRelay<Q, S, I>) -> Self {
        Self {
            relay,
            dests: Vec::new(),
        }
    }

    /// Registers a destination node with the relay and remembers its name.
    ///
    /// # Errors
    /// The relay's refusal, as text.
    pub fn add_destination(&mut self, destination: Destination) -> Result<(), String> {
        let id = destination.id.clone();
        self.relay
            .add_destination(destination)
            .map_err(|error| error.to_string())?;
        if !self.dests.contains(&id) {
            self.dests.push(id);
        }
        Ok(())
    }

    /// The relay, for the loader that owns policy.
    pub fn relay_mut(&mut self) -> &mut HubRelay<Q, S, I> {
        &mut self.relay
    }
}

impl<Q: SendQueue + Send, S: HubStream + Send, I: ImageSource + Send> HubCore
    for RelayCore<Q, S, I>
{
    fn tick(&mut self, now_ms: i64) -> Result<(), String> {
        self.relay
            .tick(now_ms)
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    fn window(&mut self, dest: &str, max: usize) -> Result<Vec<Record>, String> {
        let pending = self
            .relay
            .queue()
            .peek(dest, max)
            .map_err(|error| error.to_string())?;
        let mut out = Vec::with_capacity(pending.len());
        for item in pending {
            let bundle = item.item;
            // A legacy row without a serve sequence cannot be served: stop the window there.
            if bundle.serve_seq == 0 {
                break;
            }
            let payload = serde_json::to_string(&bundle)
                .map_err(|_| "a queued bundle did not encode".to_owned())?;
            let name = RecordName::new(&format!("bundle-{}", bundle.serve_seq))
                .map_err(|_| "a bundle name was refused".to_owned())?;
            let record = Record::new(name, 1, bundle.serve_seq, payload)
                .map_err(|_| "a bundle is not a valid wire record".to_owned())?;
            out.push(record);
        }
        out.sort_by_key(|record| record.seq);
        Ok(out)
    }

    fn ack_served(&mut self, dest: &str, through: u64, now_ms: i64) -> Result<(), String> {
        self.relay
            .on_ack_served_at(dest, through, now_ms)
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    fn destinations(&self) -> Vec<String> {
        self.dests.clone()
    }
}

/// How the driver runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DriveSettings {
    /// The pause between rounds.
    pub interval: Duration,
    /// The most items published per destination.
    pub window: usize,
}

impl Default for DriveSettings {
    fn default() -> Self {
        Self {
            interval: Duration::from_millis(200),
            window: 4_096,
        }
    }
}

fn wall_ms(clock: &dyn Clock) -> i64 {
    i64::try_from(clock.now_utc().unix_timestamp_nanos() / 1_000_000).unwrap_or(0)
}

/// Runs rounds until `stop` is set: apply the acknowledgements heard since the last round, tick
/// the relay, publish every destination's window. Blocks: call it on a plain thread.
///
/// `on_error` hears one short reason per failure (never a payload); a failed round does not stop
/// the loop, so a store that comes back is picked up again.
pub fn drive<C: HubCore>(
    mut core: C,
    cache: &Arc<ServeCache>,
    mut acks: mpsc::UnboundedReceiver<AckNote>,
    clock: &dyn Clock,
    stop: &AtomicBool,
    settings: DriveSettings,
    on_error: &dyn Fn(&str),
) {
    while !stop.load(Ordering::SeqCst) {
        round(&mut core, cache, &mut acks, clock, settings, on_error);
        std::thread::sleep(settings.interval);
    }
}

/// One round of [`drive`]; public so a test can step it without a thread.
pub fn round<C: HubCore>(
    core: &mut C,
    cache: &Arc<ServeCache>,
    acks: &mut mpsc::UnboundedReceiver<AckNote>,
    clock: &dyn Clock,
    settings: DriveSettings,
    on_error: &dyn Fn(&str),
) {
    let now = wall_ms(clock);
    // The newest claim per destination is enough: a lower one is a repeat.
    let mut newest: std::collections::BTreeMap<String, u64> = std::collections::BTreeMap::new();
    while let Ok(note) = acks.try_recv() {
        let slot = newest.entry(note.dest).or_insert(0);
        *slot = (*slot).max(note.through);
    }
    for (dest, through) in newest {
        if let Err(reason) = core.ack_served(&dest, through, now) {
            on_error(&reason);
        }
    }
    if let Err(reason) = core.tick(now) {
        on_error(&reason);
    }
    for dest in core.destinations() {
        match core.window(&dest, settings.window) {
            Ok(records) => cache.publish(&dest, records),
            Err(reason) => on_error(&reason),
        }
    }
}
