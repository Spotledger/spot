//! The relay: stream in, per-destination bundles out.

use crate::deliver::{Ctx, DestState, flush_releases, group_by_txn, run_groups};
use crate::error::HubError;
use crate::model::{DestReport, RelayConfig, ReleaseReport, StreamEntry, TickReport};
use crate::state::{DestSnapshot, RelayState};
use crate::stream_contract::StreamContract;
use crate::traits::{
    AckBook, AckObserver, DestinationScope, HubAudit, HubStream, NoAudit, NoObserver, SendQueue,
};
use spot_sync_acks::{AckError, StreamHead};
use spot_sync_closure::{ExtrasSet, ImageSource, RecordKey, Redactor, References};
use spot_sync_freshness::Bounds;
use spot_sync_skew::{DEFAULT_HOLD_LIMIT, HoldSet, NodeReport, release_for_upgrade};
use std::collections::{BTreeMap, BTreeSet};

/// One destination node as the hub knows it, from its registry entry.
pub struct Destination {
    /// The node id.
    pub id: String,
    /// What it is entitled to.
    pub scope: Box<dyn DestinationScope>,
    /// Its `excluded_fields.down`, applied before anything is queued.
    pub redactor: Box<dyn Redactor>,
    /// Its last handshake report.
    pub report: NodeReport,
    /// Most entries the hub holds back for it (version skew).
    pub hold_limit: usize,
}

impl Destination {
    /// A destination with the default hold limit.
    #[must_use]
    pub fn new(
        id: impl Into<String>,
        scope: Box<dyn DestinationScope>,
        redactor: Box<dyn Redactor>,
        report: NodeReport,
    ) -> Self {
        Self {
            id: id.into(),
            scope,
            redactor,
            report,
            hold_limit: DEFAULT_HOLD_LIMIT,
        }
    }
}

/// The hub relay (ADR-062 section 3, ADR-063 section 5).
///
/// It reads the hub stream, decides per destination which entries that destination is entitled
/// to, builds the closure bundles, withholds what the node cannot decode yet, orders by lane and
/// pushes to the destination's own queue. A destination that cannot take more (full queue, fairness
/// cap, a failing adapter call) is marked *behind* and caught up on later ticks from the stream;
/// the others carry on. The relay owns no clock, randomness, network or database.
pub struct HubRelay<Q: SendQueue, S: HubStream, I: ImageSource> {
    pub(crate) cfg: RelayConfig,
    pub(crate) stream: S,
    queue: Q,
    images: I,
    refs: Box<dyn References>,
    observer: Box<dyn AckObserver>,
    pub(crate) dests: BTreeMap<String, DestState>,
    cursor: u64,
    /// What the relay has seen of the stream's contract: `last_txn` and `lane_of_entity`.
    pub(crate) contract: StreamContract,
    pub(crate) ledger: Option<Box<dyn AckBook>>,
    pub(crate) audit: Box<dyn HubAudit>,
    pub(crate) bounds: Option<Bounds>,
    pub(crate) last_now: i64,
    pub(crate) suspect: BTreeSet<String>,
    pub(crate) suspect_events: u64,
    pub(crate) audit_dropped: usize,
}

impl<Q: SendQueue, S: HubStream, I: ImageSource> HubRelay<Q, S, I> {
    /// A relay with no destinations.
    ///
    /// # Errors
    /// `InvalidConfig` or the closure limit error for unusable settings.
    pub fn new(
        cfg: RelayConfig,
        stream: S,
        queue: Q,
        images: I,
        refs: Box<dyn References>,
    ) -> Result<Self, HubError> {
        cfg.validate()?;
        Ok(Self {
            cfg,
            stream,
            queue,
            images,
            refs,
            observer: Box::new(NoObserver),
            dests: BTreeMap::new(),
            cursor: 0,
            contract: StreamContract::default(),
            ledger: None,
            audit: Box::new(NoAudit),
            bounds: None,
            last_now: 0,
            suspect: BTreeSet::new(),
            suspect_events: 0,
            audit_dropped: 0,
        })
    }

    /// Replace the ack observer (the obligation ledger update, WP-SYN-05.T2).
    pub fn set_observer(&mut self, observer: Box<dyn AckObserver>) {
        self.observer = observer;
    }

    /// Attach the acknowledgement ledger. With a ledger every ack is also recorded there, bounded
    /// by the hub's own stream head (see [`on_ack_at`](Self::on_ack_at)).
    pub fn set_ledger(&mut self, ledger: Box<dyn AckBook>) {
        self.ledger = Some(ledger);
    }

    /// The ledger, for reading its state.
    #[must_use]
    pub fn ledger(&self) -> Option<&dyn AckBook> {
        self.ledger.as_deref()
    }

    /// Replace the audit seam that receives a note for each refused stamp.
    pub fn set_audit(&mut self, audit: Box<dyn HubAudit>) {
        self.audit = audit;
    }

    /// Set the window a node's stamp must fall in (part of the relay's configuration). Without
    /// it `on_stamped_document` refuses with `InvalidConfig`.
    pub fn set_bounds(&mut self, bounds: Bounds) {
        self.bounds = Some(bounds);
    }

    /// Register a destination. It starts at the current cursor: older stream positions reach it
    /// only through the node's catch-up, never through this relay.
    ///
    /// # Errors
    /// `DuplicateDestination` when the id is registered already.
    pub fn add_destination(&mut self, dest: Destination) -> Result<(), HubError> {
        if self.dests.contains_key(&dest.id) {
            return Err(HubError::DuplicateDestination { dest: dest.id });
        }
        let holds = HoldSet::with_limit(dest.hold_limit);
        self.dests
            .insert(dest.id.clone(), DestState::new(dest, holds));
        Ok(())
    }

    /// The highest stream position processed.
    #[must_use]
    pub fn cursor(&self) -> u64 {
        self.cursor
    }

    /// The queue (read access, for the transport and for tests).
    #[must_use]
    pub fn queue(&self) -> &Q {
        &self.queue
    }

    /// The queue (write access, for the transport's own bookkeeping).
    pub fn queue_mut(&mut self) -> &mut Q {
        &mut self.queue
    }

    /// The stream (write access, for the writer side of a double).
    pub fn stream_mut(&mut self) -> &mut S {
        &mut self.stream
    }

    /// The extras delivered to a destination.
    #[must_use]
    pub fn extras(&self, dest: &str) -> Option<&ExtrasSet> {
        self.dests.get(dest).map(|d| &d.extras)
    }

    /// The position a destination is caught up from, when it is behind the stream.
    #[must_use]
    pub fn behind_from(&self, dest: &str) -> Option<u64> {
        self.dests.get(dest).and_then(|d| d.catchup_from)
    }

    /// The entries currently withheld from a destination by a version-skew hold.
    #[must_use]
    pub fn held_positions(&self, dest: &str) -> Vec<u64> {
        self.dests
            .get(dest)
            .map(|d| d.held_entries.keys().copied().collect())
            .unwrap_or_default()
    }

    /// The entries currently withheld from a destination, as `(position, record key)`: the records
    /// a hold keeps back and the dependents withheld behind them ("dependency held"), in position
    /// order.
    #[must_use]
    pub fn held_entries(&self, dest: &str) -> Vec<(u64, RecordKey)> {
        self.dests
            .get(dest)
            .map(|d| {
                d.held_entries
                    .iter()
                    .map(|(position, entry)| (*position, entry.key.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The state the caller must persist to survive a restart. Persist it **before** the
    /// transport removes anything the previous tick queued; on restore a replay is harmless
    /// because every queue push is idempotent.
    #[must_use]
    pub fn snapshot(&self) -> RelayState {
        RelayState {
            cursor: self.cursor,
            last_txn: self.contract.last_txn().map(str::to_owned),
            lane_of_entity: self.contract.lanes().clone(),
            destinations: self
                .dests
                .iter()
                .map(|(id, d)| (id.clone(), DestSnapshot::of(d)))
                .collect(),
        }
    }

    /// Restore a snapshot onto the registered destinations.
    ///
    /// What the relay remembered of the stream's contract (`last_txn`, `lane_of_entity`) comes
    /// back with the cursor, so a transaction that a restart splits and a lane that changes after
    /// it are still refused.
    ///
    /// # Errors
    /// `UnknownDestination` when the snapshot names a destination that is not registered.
    pub fn restore(&mut self, state: RelayState) -> Result<(), HubError> {
        for id in state.destinations.keys() {
            if !self.dests.contains_key(id) {
                return Err(HubError::UnknownDestination { dest: id.clone() });
            }
        }
        self.cursor = state.cursor;
        self.contract = StreamContract::restored(state.last_txn, state.lane_of_entity);
        for (id, snap) in state.destinations {
            if let Some(d) = self.dests.get_mut(&id) {
                snap.apply(d);
            }
        }
        Ok(())
    }

    /// One pass: retry pending releases, catch up destinations that are behind, then read the
    /// stream after the cursor and route it.
    ///
    /// The cursor moves past an entry once every destination has either been pushed its bundles
    /// or been marked behind from that entry; a destination marked behind is re-read from the
    /// stream by `catchup_from` (part of the snapshot), so nothing is lost and one slow
    /// destination never stops the others.
    ///
    /// Every read is checked against the [`HubStream`] contract first (whole source transactions,
    /// a lane that is a function of its entity, ascending positions). A read that breaks it is
    /// refused whole: nothing is queued, the cursor does not move, and the code is on
    /// [`TickReport::failed`] and on every destination's report, so the next tick asks again.
    ///
    /// # Errors
    /// `StreamFailed` or `StreamDisorder` when the stream cannot be read as promised; nothing has
    /// moved then. A failing destination is reported in `TickReport`, not returned.
    pub fn tick(&mut self, now_millis: i64) -> Result<TickReport, HubError> {
        let Self {
            cfg,
            stream,
            queue,
            images,
            refs,
            dests,
            cursor,
            contract,
            ..
        } = self;
        let mut ctx = Ctx {
            cfg,
            queue,
            images,
            refs: refs.as_ref(),
        };
        let mut report = TickReport {
            now_millis,
            ..TickReport::default()
        };
        let mut used: BTreeMap<String, usize> = BTreeMap::new();
        for id in dests.keys() {
            report
                .destinations
                .insert(id.clone(), DestReport::default());
        }

        // Phase 0 and 1: pending releases, then destinations that are behind.
        for (id, st) in dests.iter_mut() {
            let rep = report.destinations.entry(id.clone()).or_default();
            let u = used.entry(id.clone()).or_insert(0);
            flush_releases(&mut ctx, st, u, rep);
            if let Some(from) = st.catchup_from {
                let window = Window {
                    from,
                    through: *cursor,
                };
                catch_up(&mut ctx, stream, contract, st, window, u, rep);
            }
        }

        // Phase 2: the new part of the stream. Every read is checked against the stream contract
        // before anything is built from it.
        let fresh = stream.read_after(*cursor, ctx.cfg.read_batch)?;
        report.read = fresh.len();
        let passed = match contract.check(&fresh, ctx.cfg, Some(*cursor), true) {
            Ok(passed) => passed,
            // A disorder stays what it was: a stream fault the caller sees as an error, with
            // nothing moved.
            Err(e @ HubError::StreamDisorder { .. }) => return Err(e),
            Err(e) => {
                // A broken contract is refused whole: nothing is queued, the cursor does not move
                // and what the relay remembers of the stream does not change, so the next tick
                // asks again.
                let code = e.code();
                report.failed = Some(code);
                report.cursor = *cursor;
                for (id, rep) in report.destinations.iter_mut() {
                    rep.failed = Some(code);
                    if let Some(st) = dests.get(id) {
                        rep.behind = st.catchup_from.is_some();
                        rep.backpressured = st.backpressured;
                    }
                }
                return Ok(report);
            }
        };
        let last = fresh.last().map_or(*cursor, |e| e.position);
        contract.commit(passed);
        let (local, foreign): (Vec<StreamEntry>, Vec<StreamEntry>) = fresh
            .into_iter()
            .partition(|e| e.tenant == ctx.cfg.tenant && e.source == ctx.cfg.source);
        report.foreign_dropped = foreign.len();
        let groups = group_by_txn(&local);
        for (id, st) in dests.iter_mut() {
            if st.catchup_from.is_some() {
                continue;
            }
            let rep = report.destinations.entry(id.clone()).or_default();
            let u = used.entry(id.clone()).or_insert(0);
            if let Some(i) = run_groups(&mut ctx, st, &groups, u, rep) {
                st.catchup_from = groups.get(i).and_then(|g| g.first()).map(|e| e.position);
            }
        }
        *cursor = last;
        report.cursor = last;
        for (id, st) in dests.iter() {
            if let Some(rep) = report.destinations.get_mut(id) {
                rep.backpressured = st.backpressured;
                rep.behind = st.catchup_from.is_some();
            }
        }
        // Taken only on success, so a failed tick loses no evidence.
        report.suspect = std::mem::take(&mut self.suspect).into_iter().collect();
        report.suspect_events = std::mem::take(&mut self.suspect_events);
        report.audit_dropped = std::mem::take(&mut self.audit_dropped);
        self.last_now = now_millis;
        Ok(report)
    }

    /// The destination reports that it applied the stream through `applied_prefix`: as
    /// [`on_ack_at`](Self::on_ack_at) at the hub time of the last tick.
    ///
    /// # Errors
    /// As [`on_ack_at`](Self::on_ack_at).
    pub fn on_ack(&mut self, dest: &str, applied_prefix: u64) -> Result<usize, HubError> {
        let at = self.last_now;
        self.on_ack_at(dest, applied_prefix, at)
    }

    /// The destination reports that it applied the stream through `applied_prefix`, heard at hub
    /// time `at`: trim its queue and tell the observer and the ledger. An ack that goes
    /// backwards, past the cursor or past the hub's own stream head is refused and changes
    /// nothing. The head is read from the hub's [`HubStream`], never from the node; an ack
    /// beyond it marks the destination suspect in the next [`TickReport`].
    ///
    /// # Errors
    /// `UnknownDestination`, `PrefixRegressed`, `PrefixAhead`, `AckBeyondHead`, `StreamFailed`,
    /// `QueueFailed` or the ledger's own refusal.
    pub fn on_ack_at(
        &mut self,
        dest: &str,
        applied_prefix: u64,
        at: i64,
    ) -> Result<usize, HubError> {
        let cursor = self.cursor;
        let Some(st) = self.dests.get(dest) else {
            return Err(HubError::UnknownDestination {
                dest: dest.to_owned(),
            });
        };
        if let Some(before) = st.acked
            && applied_prefix < before
        {
            return Err(HubError::PrefixRegressed {
                dest: dest.to_owned(),
                before,
                after: applied_prefix,
            });
        }
        if applied_prefix > cursor {
            self.mark_suspect(dest);
            return Err(HubError::PrefixAhead {
                dest: dest.to_owned(),
                prefix: applied_prefix,
                cursor,
            });
        }
        // The hub's own head bounds the claim. The cursor trails it normally, but a restored
        // snapshot can put the cursor above a rebuilt stream, so the head is checked on its own.
        let head = self.stream.head()?;
        if applied_prefix > head {
            self.mark_suspect(dest);
            return Err(HubError::AckBeyondHead {
                dest: dest.to_owned(),
                claimed: applied_prefix,
                head,
            });
        }
        if let Some(ledger) = self.ledger.as_mut()
            && let Err(e) = ledger.on_ack(
                dest,
                &self.cfg.tenant,
                applied_prefix,
                StreamHead::new(head),
                at,
            )
        {
            return Err(self.ack_failure(dest, e));
        }
        let removed = self.queue.ack(dest, applied_prefix)?;
        if let Some(st) = self.dests.get_mut(dest) {
            st.acked = Some(applied_prefix);
        }
        self.observer.on_ack(dest, applied_prefix);
        Ok(removed)
    }

    /// Map a ledger refusal to the hub's error; an over-claim marks the destination suspect.
    pub(crate) fn ack_failure(&mut self, dest: &str, e: AckError) -> HubError {
        match e {
            AckError::PrefixBeyondHead { claimed, head, .. } => {
                self.mark_suspect(dest);
                HubError::AckBeyondHead {
                    dest: dest.to_owned(),
                    claimed,
                    head,
                }
            }
            AckError::HoldBeyondHead { .. } => {
                self.mark_suspect(dest);
                HubError::Ack(e)
            }
            AckError::PrefixRegressed { before, after, .. } => HubError::PrefixRegressed {
                dest: dest.to_owned(),
                before,
                after,
            },
            AckError::UnknownNode { .. } => HubError::UnknownDestination {
                dest: dest.to_owned(),
            },
            other => HubError::Ack(other),
        }
    }

    pub(crate) fn mark_suspect(&mut self, dest: &str) {
        self.suspect.insert(dest.to_owned());
        self.suspect_events += 1;
    }

    /// The destination applied redelivered records at these positions: drop them from its queue.
    ///
    /// # Errors
    /// `UnknownDestination` or `QueueFailed`.
    pub fn on_redelivery_ack(&mut self, dest: &str, positions: &[u64]) -> Result<usize, HubError> {
        if !self.dests.contains_key(dest) {
            return Err(HubError::UnknownDestination {
                dest: dest.to_owned(),
            });
        }
        let mut removed = 0;
        for p in positions {
            removed += self.queue.remove_redelivered(dest, *p)?;
        }
        Ok(removed)
    }

    /// The destination reports a new capability: release the holds it now satisfies and requeue
    /// them in position order, ahead of newer records of the same record key (those are still
    /// withheld behind them, so nothing newer is queued for such a key). A dependent withheld
    /// because a record its closure needed is held (reason "dependency held") is released with
    /// that record, transitively and in position order, so the queue carries the dependency
    /// before the record that needs it.
    ///
    /// # Errors
    /// `UnknownDestination`. A failure to build or push a released bundle leaves the entries
    /// pending (`ReleaseReport::pending`); the next tick retries.
    pub fn on_node_upgraded(
        &mut self,
        dest: &str,
        report: NodeReport,
    ) -> Result<ReleaseReport, HubError> {
        let Self {
            cfg,
            queue,
            images,
            refs,
            dests,
            ..
        } = self;
        let st = dests
            .get_mut(dest)
            .ok_or_else(|| HubError::UnknownDestination {
                dest: dest.to_owned(),
            })?;
        st.dest.report = report;
        let released = release_for_upgrade(&st.dest.report, &mut st.holds);
        let mut out = ReleaseReport {
            released: released.len(),
            ..ReleaseReport::default()
        };
        for o in &released {
            match st.held_entries.remove(&o.position) {
                Some(e) => {
                    st.release_backlog.insert(o.position, e);
                }
                None => out.missing.push(o.position),
            }
        }
        out.released_by_dependency = release_dependents(st);
        let mut ctx = Ctx {
            cfg,
            queue,
            images,
            refs: refs.as_ref(),
        };
        let mut rep = DestReport::default();
        let mut used = 0;
        flush_releases(&mut ctx, st, &mut used, &mut rep);
        out.bundles_queued = rep.bundles_queued;
        out.pending = st.release_backlog.len();
        Ok(out)
    }
}

/// Move the dependents that are no longer blocked into the release backlog, in position order.
/// A dependent waits for every held record its closure needs; one that is itself withheld is a
/// blocker in turn, so the loop runs until nothing more can go. Returns how many were moved.
fn release_dependents(st: &mut DestState) -> usize {
    let mut released = 0;
    loop {
        let held_holds: BTreeSet<RecordKey> = st
            .holds
            .pending_holds()
            .iter()
            .map(|h| RecordKey::new(h.record.entity.clone(), h.record.id.clone()))
            .collect();
        let withheld: BTreeSet<RecordKey> =
            st.held_entries.values().map(|e| e.key.clone()).collect();
        let ready: Vec<u64> = st
            .held_by_dependency
            .iter()
            .filter(|(_, blockers)| {
                blockers
                    .iter()
                    .all(|b| !held_holds.contains(b) && !withheld.contains(b))
            })
            .map(|(position, _)| *position)
            .collect();
        if ready.is_empty() {
            return released;
        }
        for position in ready {
            st.held_by_dependency.remove(&position);
            if let Some(entry) = st.held_entries.remove(&position) {
                st.release_backlog.insert(position, entry);
                released += 1;
            }
        }
    }
}

/// One catch-up read: the position a destination is caught up from and the cursor it must not read
/// past.
#[derive(Debug, Clone, Copy)]
struct Window {
    /// The first position the destination has not been given.
    from: u64,
    /// The highest position the relay has processed.
    through: u64,
}

fn catch_up<Q: SendQueue, S: HubStream, I: ImageSource>(
    ctx: &mut Ctx<'_, Q, I>,
    stream: &S,
    contract: &mut StreamContract,
    st: &mut DestState,
    window: Window,
    used: &mut usize,
    rep: &mut DestReport,
) {
    let Window { from, through } = window;
    match ctx.queue.queued_bytes(&st.dest.id) {
        Ok(bytes) if bytes >= ctx.cfg.max_queue_bytes => {
            st.backpressured = true;
            return;
        }
        Ok(_) => st.backpressured = false,
        Err(e) => {
            rep.failed = Some(e.code());
            return;
        }
    }
    let after = from.saturating_sub(1);
    let entries = match stream.read_after(after, ctx.cfg.read_batch) {
        Ok(entries) => entries,
        Err(e) => {
            rep.failed = Some(e.code());
            return;
        }
    };
    // The catch-up read is checked against the same contract. It re-reads positions the relay has
    // passed already, so a transaction at its start is the beginning of a whole one, not the
    // continuation of the last one processed; its own shape must still hold. A refused read
    // leaves `catchup_from` where it was, so the next tick asks again.
    match contract.check(&entries, ctx.cfg, Some(after), false) {
        Ok(passed) => contract.commit(passed),
        Err(e) => {
            rep.failed = Some(e.code());
            return;
        }
    }
    let entries: Vec<StreamEntry> = entries
        .into_iter()
        .filter(|e| e.position <= through)
        .collect();
    let done_through = entries.last().map_or(through, |e| e.position);
    let local: Vec<StreamEntry> = entries
        .into_iter()
        .filter(|e| e.tenant == ctx.cfg.tenant && e.source == ctx.cfg.source)
        .collect();
    let groups = group_by_txn(&local);
    match run_groups(ctx, st, &groups, used, rep) {
        Some(i) => {
            st.catchup_from = groups.get(i).and_then(|g| g.first()).map(|e| e.position);
        }
        None => {
            st.catchup_from = (done_through < through).then_some(done_through + 1);
        }
    }
}
