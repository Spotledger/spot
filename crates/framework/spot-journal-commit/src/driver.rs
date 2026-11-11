//! The driver: queue, group commit, gate feeding, probing and ticket resolution.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::future::{Future, poll_fn};
use std::pin::pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::Poll;

use spot_journal_durability::{
    AckGate, Admission, BatchId, CommandOutcome, Event, FailureReason, Output, RpoBreach, Seq,
};
use tokio::sync::{Notify, oneshot};

use crate::error::CommitError;
use crate::runtime::Runtime;
use crate::seam::{ChunkRelay, ShipEntry, ShipError, Shipped};
use crate::settings::CommitSettings;
use crate::ticket::{CommitAnswer, CommitRequest, CommitTicket, Observer};

/// What an operator or the control plane may read from the driver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DriverStatus {
    /// `degrade` is in effect: commits are acknowledged on the local commit, the store is down.
    pub degraded: bool,
    /// `halt` is in effect: new commits are refused until the store returns.
    pub halted: bool,
    /// The RPO breach alarm, when raised.
    pub rpo_breach: Option<RpoBreach>,
    /// Locally committed requests waiting for their acknowledgement.
    pub awaiting_ack: usize,
    /// Requests not yet shipped to the relay.
    pub queued: usize,
    /// The last shipment failed and the driver waits to probe the store again.
    pub store_down: bool,
    /// The relay error code that stopped shipping for good, if any.
    pub stopped: Option<&'static str>,
}

#[derive(Debug)]
struct Queued {
    seq: Seq,
    entry: Vec<u8>,
    queued_at: u64,
}

struct Resolution {
    seq: Seq,
    answer: CommitAnswer,
    senders: Vec<oneshot::Sender<CommitAnswer>>,
}

struct Inner {
    gate: AckGate,
    queue: VecDeque<Queued>,
    /// The next sequence number `submit` accepts.
    next_seq: Seq,
    /// Submitted, not yet confirmed locally: command id to seq, and seq to command id.
    by_command: HashMap<String, Seq>,
    command_of: BTreeMap<Seq, String>,
    /// When each locally confirmed commit was registered with the gate (for wake-ups).
    registered_at: BTreeMap<Seq, u64>,
    waiters: BTreeMap<Seq, Vec<oneshot::Sender<CommitAnswer>>>,
    next_batch_id: u64,
    store_down: bool,
    next_retry: u64,
    stopped: Option<&'static str>,
}

/// The commit driver of one tenant. See the crate docs.
pub struct CommitDriver<R: ChunkRelay, T: Runtime> {
    inner: Mutex<Inner>,
    relay: tokio::sync::Mutex<R>,
    runtime: T,
    settings: CommitSettings,
    generation: u64,
    wake: Notify,
    stop: AtomicBool,
    observer: Option<Arc<dyn Observer>>,
}

fn lock(inner: &Mutex<Inner>) -> MutexGuard<'_, Inner> {
    inner.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Feeds `event` to the gate and turns its outputs into resolutions.
fn apply(inner: &mut Inner, now: u64, event: Event) -> Vec<Resolution> {
    let mut out = Vec::new();
    for output in inner.gate.handle(now, event) {
        let (seq, answer) = match output {
            Output::Ack { seq, .. } => (seq, CommitAnswer::Acked(seq)),
            Output::CommitOutcomeUnknown { seq } => (seq, CommitAnswer::OutcomeUnknown { seq }),
            _ => continue,
        };
        inner.registered_at.remove(&seq);
        out.push(Resolution {
            seq,
            answer,
            senders: inner.waiters.remove(&seq).unwrap_or_default(),
        });
    }
    out
}

impl<R: ChunkRelay, T: Runtime> CommitDriver<R, T> {
    /// A driver over `gate` (one tenant's acknowledgement gate), `relay` (positioned at the next
    /// entry to ship), a clock and settings.
    pub fn new(gate: AckGate, relay: R, runtime: T, settings: CommitSettings) -> Self {
        let settings = CommitSettings {
            max_batch: settings.max_batch.max(1),
            ..settings
        };
        let inner = Inner {
            gate,
            queue: VecDeque::new(),
            next_seq: relay.next_seq(),
            by_command: HashMap::new(),
            command_of: BTreeMap::new(),
            registered_at: BTreeMap::new(),
            waiters: BTreeMap::new(),
            next_batch_id: settings.first_batch_id,
            store_down: false,
            next_retry: 0,
            stopped: None,
        };
        Self {
            inner: Mutex::new(inner),
            generation: relay.generation(),
            relay: tokio::sync::Mutex::new(relay),
            runtime,
            settings,
            wake: Notify::new(),
            stop: AtomicBool::new(false),
            observer: None,
        }
    }

    /// Reports every resolution, in order, to `observer`.
    #[must_use]
    pub fn with_observer(mut self, observer: Arc<dyn Observer>) -> Self {
        self.observer = Some(observer);
        self
    }

    /// The generation the relay writes into.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Takes a commit request and returns its ticket.
    ///
    /// A `command_id` already seen is never queued again: it is attached to the commit in flight
    /// or answered from the gate's outcome. Under `halt` with the store unreachable the ticket is
    /// already resolved `Refused`, and the sequence number is not consumed.
    ///
    /// # Errors
    /// `seq_out_of_order` when `seq` is not the next one; `relay_stopped` after a failure no
    /// retry cures.
    pub fn submit(&self, request: CommitRequest) -> Result<CommitTicket, CommitError> {
        let now = self.runtime.now_millis();
        let (tx, rx) = oneshot::channel();
        let mut inner = lock(&self.inner);
        if let Some(&seq) = inner.by_command.get(&request.command_id) {
            inner.waiters.entry(seq).or_default().push(tx);
            return Ok(CommitTicket::new(seq, rx));
        }
        let ticket_seq = match inner.gate.admit(&request.command_id) {
            Admission::Existing(CommandOutcome::Committed { seq }) => {
                let _ = tx.send(CommitAnswer::Acked(seq));
                seq
            }
            Admission::Existing(CommandOutcome::Unknown { seq }) => {
                let _ = tx.send(CommitAnswer::OutcomeUnknown { seq });
                seq
            }
            Admission::Existing(CommandOutcome::Pending { seq }) => {
                inner.waiters.entry(seq).or_default().push(tx);
                seq
            }
            // AckGate::admit never returns Withdrawn; if it ever does, drop the sender so the
            // ticket reports an unknown outcome and the caller is never acknowledged.
            Admission::Existing(CommandOutcome::Withdrawn { seq }) => {
                drop(tx);
                seq
            }
            Admission::Refuse(refusal) => {
                let _ = tx.send(CommitAnswer::Refused(refusal));
                request.seq
            }
            Admission::Proceed => {
                if let Some(cause) = inner.stopped {
                    return Err(CommitError::RelayStopped { cause });
                }
                if request.seq != inner.next_seq {
                    return Err(CommitError::SeqOutOfOrder {
                        expected: inner.next_seq,
                        offered: request.seq,
                    });
                }
                inner.next_seq = inner.next_seq.saturating_add(1);
                inner
                    .by_command
                    .insert(request.command_id.clone(), request.seq);
                inner.command_of.insert(request.seq, request.command_id);
                inner.waiters.entry(request.seq).or_default().push(tx);
                inner.queue.push_back(Queued {
                    seq: request.seq,
                    entry: request.entry,
                    queued_at: now,
                });
                request.seq
            }
        };
        drop(inner);
        self.wake.notify_one();
        Ok(CommitTicket::new(ticket_seq, rx))
    }

    /// The local database commit of `seq` is durable. Confirmations come in sequence order. The
    /// gate learns of the commit here, so no ticket resolves `Acked` before this call.
    ///
    /// # Errors
    /// `unknown_seq` for a sequence number not waiting for confirmation, `seq_out_of_order` when
    /// an earlier one is still unconfirmed.
    pub fn local_commit_durable(&self, seq: Seq) -> Result<(), CommitError> {
        let now = self.runtime.now_millis();
        let mut inner = lock(&self.inner);
        let Some((&first, _)) = inner.command_of.first_key_value() else {
            return Err(CommitError::UnknownSeq { seq });
        };
        if !inner.command_of.contains_key(&seq) {
            return Err(CommitError::UnknownSeq { seq });
        }
        if first != seq {
            return Err(CommitError::SeqOutOfOrder {
                expected: first,
                offered: seq,
            });
        }
        let command_id = inner.command_of.remove(&seq).unwrap_or_default();
        inner.by_command.remove(&command_id);
        inner.registered_at.insert(seq, now);
        let resolutions = apply(
            &mut inner,
            now,
            Event::LocalCommitDurable { seq, command_id },
        );
        drop(inner);
        self.deliver(resolutions);
        self.wake.notify_one();
        Ok(())
    }

    /// The synchronous standby holds everything up to and including `seq` (`replica-sync`).
    pub fn standby_confirmed(&self, seq: Seq) {
        self.feed(Event::StandbyConfirmed { seq });
    }

    /// The driver's state for the control plane and operators.
    #[must_use]
    pub fn status(&self) -> DriverStatus {
        let inner = lock(&self.inner);
        DriverStatus {
            degraded: inner.gate.is_degraded(),
            halted: inner.gate.is_halted(),
            rpo_breach: inner.gate.rpo_breach(),
            awaiting_ack: inner.gate.pending_count(),
            queued: inner.queue.len(),
            store_down: inner.store_down,
            stopped: inner.stopped,
        }
    }

    /// The earliest time something needs doing (a batch is due, a store probe is due, an
    /// acknowledgement deadline passes), or `None` when idle.
    #[must_use]
    pub fn next_wake_millis(&self) -> Option<u64> {
        let inner = lock(&self.inner);
        let mut wake: Option<u64> = None;
        let mut consider = |t: u64| wake = Some(wake.map_or(t, |w| w.min(t)));
        if inner.stopped.is_none() {
            if inner.store_down {
                consider(inner.next_retry);
            } else if let Some(front) = inner.queue.front() {
                if inner.queue.len() >= self.settings.max_batch {
                    consider(0);
                } else {
                    consider(
                        front
                            .queued_at
                            .saturating_add(self.settings.group_window_millis),
                    );
                }
            }
        }
        if let Some((_, &at)) = inner.registered_at.first_key_value() {
            consider(at.saturating_add(self.settings.ack_deadline_millis));
        }
        wake
    }

    /// Does everything that is due at the current time: probes the store when it is down and the
    /// retry time has come, ships every due batch, and lets the gate expire overdue commits.
    pub async fn step(&self) {
        let mut relay = self.relay.lock().await;
        let now = self.runtime.now_millis();
        let probe_due = {
            let inner = lock(&self.inner);
            inner.store_down && inner.stopped.is_none() && now >= inner.next_retry
        };
        if probe_due {
            self.probe(&mut relay, now).await;
        }
        while let Some((batch_id, batch)) = self.take_batch(now) {
            if !self.ship(&mut relay, batch_id, batch, now).await {
                break;
            }
        }
        self.feed(Event::Tick);
    }

    /// Runs [`CommitDriver::step`] whenever work arrives or becomes due, until
    /// [`CommitDriver::stop`] is called.
    pub async fn run(&self) {
        while !self.stop.load(Ordering::SeqCst) {
            let mut notified = pin!(self.wake.notified());
            self.step().await;
            if self.stop.load(Ordering::SeqCst) {
                return;
            }
            let mut sleep = self.next_wake_millis().map(|t| self.runtime.sleep_until(t));
            poll_fn(|cx| {
                if notified.as_mut().poll(cx).is_ready() {
                    return Poll::Ready(());
                }
                if let Some(s) = sleep.as_mut()
                    && s.as_mut().poll(cx).is_ready()
                {
                    return Poll::Ready(());
                }
                Poll::Pending
            })
            .await;
        }
    }

    /// Asks [`CommitDriver::run`] to return.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        self.wake.notify_one();
    }

    fn feed(&self, event: Event) {
        let now = self.runtime.now_millis();
        let resolutions = apply(&mut lock(&self.inner), now, event);
        self.deliver(resolutions);
    }

    fn deliver(&self, resolutions: Vec<Resolution>) {
        for r in resolutions {
            for sender in r.senders {
                let _ = sender.send(r.answer);
            }
            if let Some(observer) = &self.observer {
                observer.resolved(r.seq, &r.answer);
            }
        }
    }

    /// The next due batch, taken off the queue; `None` when nothing is due or shipping waits.
    fn take_batch(&self, now: u64) -> Option<(u64, Vec<Queued>)> {
        let mut inner = lock(&self.inner);
        if inner.store_down || inner.stopped.is_some() {
            return None;
        }
        let front = inner.queue.front()?;
        let due = inner.queue.len() >= self.settings.max_batch
            || front
                .queued_at
                .saturating_add(self.settings.group_window_millis)
                <= now;
        if !due {
            return None;
        }
        let take = inner.queue.len().min(self.settings.max_batch);
        let batch: Vec<Queued> = inner.queue.drain(..take).collect();
        let batch_id = inner.next_batch_id;
        inner.next_batch_id = inner.next_batch_id.saturating_add(1);
        Some((batch_id, batch))
    }

    /// Ships one batch. Returns whether shipping may go on.
    async fn ship(&self, relay: &mut R, batch_id: u64, batch: Vec<Queued>, now: u64) -> bool {
        let entries: Vec<ShipEntry> = batch
            .iter()
            .map(|q| ShipEntry {
                seq: q.seq,
                line: q.entry.clone(),
            })
            .collect();
        let batch_ref = BatchId(batch_id);
        match relay.ship(batch_id, &entries, now).await {
            Ok(Shipped::Durable { up_to_seq }) => {
                self.feed(Event::ChunkDurable {
                    batch: batch_ref,
                    up_to_seq,
                });
                true
            }
            Ok(Shipped::Buffered) => true,
            Err(error) => {
                let reason = {
                    let mut inner = lock(&self.inner);
                    for queued in batch.into_iter().rev() {
                        inner.queue.push_front(queued);
                    }
                    match error {
                        ShipError::StoreDown { .. } => {
                            inner.store_down = true;
                            inner.next_retry = now.saturating_add(self.settings.retry_millis);
                            FailureReason::Unreachable
                        }
                        ShipError::Stopped { code } => {
                            inner.stopped = Some(code);
                            FailureReason::Rejected
                        }
                    }
                };
                self.feed(Event::ChunkFailed {
                    batch: batch_ref,
                    reason,
                });
                false
            }
        }
    }

    /// Resumes the relay. A success means the store is reachable; entries the relay already holds
    /// (a chunk written although the call reported a failure) are dropped from the queue and
    /// reported durable.
    async fn probe(&self, relay: &mut R, now: u64) {
        match relay.resume().await {
            Ok(()) => {
                let next = relay.next_seq();
                let held = {
                    let mut inner = lock(&self.inner);
                    inner.store_down = false;
                    let mut held = false;
                    while inner.queue.front().is_some_and(|q| q.seq < next) {
                        inner.queue.pop_front();
                        held = true;
                    }
                    held
                };
                if held {
                    let batch = BatchId(lock(&self.inner).next_batch_id.saturating_sub(1));
                    self.feed(Event::ChunkDurable {
                        batch,
                        up_to_seq: next.saturating_sub(1),
                    });
                }
                self.feed(Event::StoreReachable(true));
            }
            Err(ShipError::StoreDown { .. }) => {
                lock(&self.inner).next_retry = now.saturating_add(self.settings.retry_millis);
            }
            Err(ShipError::Stopped { code }) => {
                lock(&self.inner).stopped = Some(code);
            }
        }
    }
}
