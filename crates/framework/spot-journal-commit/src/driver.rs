//! The driver: queue, group commit, gate feeding, probing and ticket resolution.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::future::{Future, poll_fn};
use std::pin::pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::Poll;

use spot_journal_durability::{
    AckGate, Admission, BatchId, CommandOutcome, Event, FailureReason, Output, Position, RpoBreach,
    Seq,
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
    /// Requests withdrawn (`local_commit_failed`) whose journal entry is, or will be, in the
    /// journal anyway: shipped already, in flight, or queued with a later entry behind it (the
    /// relay takes strictly consecutive positions, so it cannot be dropped). The journal format
    /// has no way to void such an entry; see the crate docs.
    pub withdrawn_shipped: u64,
    /// The lowest sequence number counted in `withdrawn_shipped`.
    pub withdrawn_shipped_lowest_seq: Option<Seq>,
    /// Requests whose `local_commit_durable` did not arrive within
    /// `local_commit_deadline_millis` and were withdrawn.
    pub expired_local_commits: u64,
    /// Local commit confirmations the gate refused (a stale generation, an out-of-order position
    /// or a reused command id; the driver feeds them in order, so this signals a caller bug). The
    /// ticket resolved `Withdrawn`; nothing was acknowledged.
    pub rejected_commits: u64,
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
    /// The generation the relay writes into: every gate position of this driver carries it.
    generation: u64,
    queue: VecDeque<Queued>,
    /// The next sequence number `submit` accepts.
    next_seq: Seq,
    /// Submitted, not yet confirmed locally: command id to seq, and seq to command id.
    by_command: HashMap<String, Seq>,
    command_of: BTreeMap<Seq, String>,
    /// When each submitted, not yet confirmed request was submitted (for the local-commit
    /// deadline); same keys as `command_of`.
    submitted_at: BTreeMap<Seq, u64>,
    /// Command ids of commits confirmed locally and not yet acknowledged or withdrawn (needed
    /// to withdraw one: the gate checks the id).
    confirmed: BTreeMap<Seq, String>,
    withdrawn_shipped: u64,
    withdrawn_shipped_lowest: Option<Seq>,
    expired_local_commits: u64,
    rejected_commits: u64,
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
    /// The acknowledgement deadline, read from the gate's configuration when the driver is built.
    ack_deadline_millis: u64,
    generation: u64,
    wake: Notify,
    stop: AtomicBool,
    observer: Option<Arc<dyn Observer>>,
}

fn lock(inner: &Mutex<Inner>) -> MutexGuard<'_, Inner> {
    inner.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Feeds `event` to the gate and turns its outputs into resolutions.
///
/// The gate keys on `(generation, seq)`; one driver writes one generation (the relay's), so the
/// driver's own tickets stay keyed by `seq` and the generation is added and removed here. A
/// position of another generation is never produced by this driver.
fn apply(inner: &mut Inner, now: u64, event: Event) -> Vec<Resolution> {
    let mut out = Vec::new();
    for output in inner.gate.handle(now, event) {
        let (seq, answer) = match output {
            Output::Ack { position, .. } => {
                let seq = position.seq;
                inner.confirmed.remove(&seq);
                (seq, CommitAnswer::Acked(seq))
            }
            Output::CommitOutcomeUnknown { position } => {
                let seq = position.seq;
                (seq, CommitAnswer::OutcomeUnknown { seq })
            }
            Output::Withdrawn { position } => {
                let seq = position.seq;
                inner.confirmed.remove(&seq);
                (seq, CommitAnswer::Withdrawn(seq))
            }
            // The gate refused a commit confirmation and changed no state. Never drop it
            // silently: its ticket resolves `Withdrawn` (it is never acknowledged), the entry is
            // counted like a withdrawn shipped one and the refusal is counted in the status.
            Output::Rejected { position, .. } => {
                let seq = position.seq;
                inner.confirmed.remove(&seq);
                inner.rejected_commits = inner.rejected_commits.saturating_add(1);
                note_withdrawn_in_journal(inner, seq);
                (seq, CommitAnswer::Withdrawn(seq))
            }
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

/// Records that the journal holds (or will hold) an entry of a withdrawn commit.
fn note_withdrawn_in_journal(inner: &mut Inner, seq: Seq) {
    inner.withdrawn_shipped = inner.withdrawn_shipped.saturating_add(1);
    inner.withdrawn_shipped_lowest =
        Some(inner.withdrawn_shipped_lowest.map_or(seq, |l| l.min(seq)));
}

/// Withdraws `seq`: its database commit failed or never arrived.
///
/// * Submitted, not yet confirmed locally (the normal case: the caller reports a failure
///   instead of a confirmation). The gate has never seen it. If its entry is the last one in the
///   queue (nothing later was submitted) it is removed and the sequence number is released for
///   the retry. Otherwise the entry stays: the relay takes strictly consecutive positions, so
///   dropping it would leave a gap that later entries could not be shipped across. It is then
///   counted in `withdrawn_shipped`, as is an entry that already left the queue.
/// * Confirmed locally and still waiting for its acknowledgement: the gate withdraws it; the
///   sequence number stays consumed and the entry is counted as above.
/// * Anything else (acknowledged, withdrawn already, never submitted): `unknown_seq`; an
///   acknowledgement is never taken back.
fn withdraw(inner: &mut Inner, now: u64, seq: Seq) -> Result<Vec<Resolution>, CommitError> {
    if let Some(command_id) = inner.command_of.remove(&seq) {
        inner.by_command.remove(&command_id);
        inner.submitted_at.remove(&seq);
        let is_tail = inner.queue.back().is_some_and(|q| q.seq == seq)
            && inner.next_seq == seq.saturating_add(1);
        if is_tail {
            inner.queue.pop_back();
            inner.next_seq = seq;
        } else {
            note_withdrawn_in_journal(inner, seq);
        }
        let senders = inner.waiters.remove(&seq).unwrap_or_default();
        return Ok(vec![Resolution {
            seq,
            answer: CommitAnswer::Withdrawn(seq),
            senders,
        }]);
    }
    let Some(command_id) = inner.confirmed.get(&seq).cloned() else {
        return Err(CommitError::UnknownSeq { seq });
    };
    let position = Position::new(inner.generation, seq);
    let resolutions = apply(
        inner,
        now,
        Event::LocalCommitFailed {
            position,
            command_id,
        },
    );
    if resolutions.is_empty() {
        // The gate refused (the commit was acknowledged silently after an unknown outcome).
        inner.confirmed.remove(&seq);
        return Err(CommitError::UnknownSeq { seq });
    }
    note_withdrawn_in_journal(inner, seq);
    Ok(resolutions)
}

impl<R: ChunkRelay, T: Runtime> CommitDriver<R, T> {
    /// A driver over `gate` (one tenant's acknowledgement gate), `relay` (positioned at the next
    /// entry to ship), a clock and settings. The acknowledgement deadline is read from the gate's
    /// configuration; there is no second setting for it.
    ///
    /// # Errors
    /// `invalid_setting` when `settings.local_commit_deadline_millis` is below the gate's
    /// `ack_deadline_millis`.
    pub fn new(
        gate: AckGate,
        relay: R,
        runtime: T,
        settings: CommitSettings,
    ) -> Result<Self, CommitError> {
        let ack_deadline_millis = gate.config().ack_deadline_millis;
        settings.validate(ack_deadline_millis)?;
        let settings = CommitSettings {
            max_batch: settings.max_batch.max(1),
            ..settings
        };
        let generation = relay.generation();
        let inner = Inner {
            gate,
            generation,
            queue: VecDeque::new(),
            next_seq: relay.next_seq(),
            by_command: HashMap::new(),
            command_of: BTreeMap::new(),
            submitted_at: BTreeMap::new(),
            confirmed: BTreeMap::new(),
            withdrawn_shipped: 0,
            withdrawn_shipped_lowest: None,
            expired_local_commits: 0,
            rejected_commits: 0,
            registered_at: BTreeMap::new(),
            waiters: BTreeMap::new(),
            next_batch_id: settings.first_batch_id,
            store_down: false,
            next_retry: 0,
            stopped: None,
        };
        Ok(Self {
            inner: Mutex::new(inner),
            generation,
            relay: tokio::sync::Mutex::new(relay),
            runtime,
            settings,
            ack_deadline_millis,
            wake: Notify::new(),
            stop: AtomicBool::new(false),
            observer: None,
        })
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
            Admission::Existing(CommandOutcome::Committed { position }) => {
                let seq = position.seq;
                let _ = tx.send(CommitAnswer::Acked(seq));
                seq
            }
            Admission::Existing(CommandOutcome::Unknown { position }) => {
                let seq = position.seq;
                let _ = tx.send(CommitAnswer::OutcomeUnknown { seq });
                seq
            }
            Admission::Existing(CommandOutcome::Pending { position }) => {
                let seq = position.seq;
                inner.waiters.entry(seq).or_default().push(tx);
                seq
            }
            // AckGate::admit never returns Withdrawn; if it ever does, drop the sender so the
            // ticket reports an unknown outcome and the caller is never acknowledged.
            Admission::Existing(CommandOutcome::Withdrawn { position }) => {
                drop(tx);
                position.seq
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
                inner.submitted_at.insert(request.seq, now);
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
        inner.submitted_at.remove(&seq);
        inner.confirmed.insert(seq, command_id.clone());
        inner.registered_at.insert(seq, now);
        let position = Position::new(inner.generation, seq);
        let resolutions = apply(
            &mut inner,
            now,
            Event::LocalCommitDurable {
                position,
                command_id,
            },
        );
        drop(inner);
        self.deliver(resolutions);
        self.wake.notify_one();
        Ok(())
    }

    /// The caller's database commit of `seq` failed or rolled back (also after its entry was
    /// shipped under `remote-sync`). The ticket of `seq` resolves with
    /// [`CommitAnswer::Withdrawn`] and the command may be retried.
    ///
    /// What happens to the entry: see the crate docs ("A failed local commit"). In short, a
    /// queued entry that is the newest one is dropped and its sequence number is released; any
    /// other entry cannot be unshipped (the journal format has no void entry), is counted in
    /// [`DriverStatus::withdrawn_shipped`], and the driver goes on with later sequence numbers in
    /// order.
    ///
    /// # Errors
    /// `unknown_seq` for a sequence number that is not waiting for its local commit or its
    /// acknowledgement (never submitted, acknowledged already, or withdrawn already).
    pub fn local_commit_failed(&self, seq: Seq) -> Result<(), CommitError> {
        let now = self.runtime.now_millis();
        let resolutions = {
            let mut inner = lock(&self.inner);
            withdraw(&mut inner, now, seq)?
        };
        self.deliver(resolutions);
        self.wake.notify_one();
        Ok(())
    }

    /// The synchronous standby holds everything up to and including `seq` (`replica-sync`).
    pub fn standby_confirmed(&self, seq: Seq) {
        let position = Position::new(self.generation, seq);
        self.feed(Event::StandbyConfirmed { position });
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
            withdrawn_shipped: inner.withdrawn_shipped,
            withdrawn_shipped_lowest_seq: inner.withdrawn_shipped_lowest,
            expired_local_commits: inner.expired_local_commits,
            rejected_commits: inner.rejected_commits,
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
            consider(at.saturating_add(self.ack_deadline_millis));
        }
        if let Some((_, &at)) = inner.submitted_at.first_key_value() {
            // A request expires once the time is past its deadline, hence one more millisecond.
            consider(
                at.saturating_add(self.settings.local_commit_deadline_millis)
                    .saturating_add(1),
            );
        }
        wake
    }

    /// Does everything that is due at the current time: probes the store when it is down and the
    /// retry time has come, ships every due batch, and lets the gate expire overdue commits.
    /// Requests whose local commit never arrived within `local_commit_deadline_millis` are
    /// withdrawn first, so an expired request that is still queued is not shipped.
    pub async fn step(&self) {
        let mut relay = self.relay.lock().await;
        let now = self.runtime.now_millis();
        self.expire_local_commits(now);
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

    /// Withdraws every request that has waited for its local commit confirmation longer than
    /// `local_commit_deadline_millis`, newest first (so a run of expired newest entries is
    /// dropped from the queue), and counts each in `expired_local_commits`.
    fn expire_local_commits(&self, now: u64) {
        let deadline = self.settings.local_commit_deadline_millis;
        let resolutions = {
            let mut guard = lock(&self.inner);
            let inner = &mut *guard;
            let expired: Vec<Seq> = inner
                .submitted_at
                .iter()
                .filter(|&(_, &at)| at.saturating_add(deadline) < now)
                .map(|(&seq, _)| seq)
                .collect();
            let mut out = Vec::new();
            for seq in expired.into_iter().rev() {
                if let Ok(resolved) = withdraw(inner, now, seq) {
                    inner.expired_local_commits = inner.expired_local_commits.saturating_add(1);
                    out.extend(resolved);
                }
            }
            // Forget the ids of commits the gate has acknowledged silently (after an unknown
            // outcome): only a waiting or unknown commit can still be withdrawn.
            let gate = &inner.gate;
            inner.confirmed.retain(|_, id| {
                matches!(
                    gate.outcome(id),
                    Some(CommandOutcome::Pending { .. } | CommandOutcome::Unknown { .. })
                )
            });
            out
        };
        self.deliver(resolutions);
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
                    up_to: Position::new(self.generation, up_to_seq),
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
                        up_to: Position::new(self.generation, next.saturating_sub(1)),
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
