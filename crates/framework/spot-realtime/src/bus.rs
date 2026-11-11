//! The in-process bus: rooms, subscriptions, bounded non-blocking delivery, replay.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

use spot_core::{Clock, SiteId};
use tokio::sync::mpsc;

use crate::auth::RoomAuthorizer;
use crate::error::{Closed, JoinError, JoinOutcome};
use crate::event::{Event, Principal};
use crate::room::Room;

/// Publishes between amortised global-ageing sweeps — modelled on `spot-cache`'s
/// `SWEEP_INTERVAL_WRITES`. A publish still does its own room's pruning inline; every
/// 64th publish additionally sweeps a bounded slice of *other* rooms so a bus that
/// publishes forever without anyone calling [`RealtimeBus::stats`] still frees aged,
/// empty rooms.
pub const SWEEP_INTERVAL_PUBLISHES: u64 = 64;

/// Rooms examined per amortised sweep, chosen by a cursor rotating through the room
/// order (wrapping) — modelled on `spot-cache`'s `SWEEP_SCAN_BUDGET`. A sweep is bounded
/// work, never a full pass over the bus; the rotating window still reaches every room
/// over time.
pub const SWEEP_SCAN_BUDGET: usize = 32;

/// Queue capacity, replay length and replay age limits for a [`RealtimeBus`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BusConfig {
    /// Per-subscription bounded queue size. A subscriber whose queue fills is closed as a
    /// slow consumer instead of making the publisher wait. Clamped to at least 1.
    pub queue_capacity: usize,
    /// How many recent events per room stay replayable for reconnecting clients. Clamped to
    /// at least 1.
    pub replay_len: usize,
    /// How long an event stays replayable, measured on the bus clock's monotonic time.
    pub replay_max_age: Duration,
}

impl Default for BusConfig {
    fn default() -> Self {
        Self {
            queue_capacity: 256,
            replay_len: 1_000,
            replay_max_age: Duration::from_secs(5 * 60),
        }
    }
}

/// Counters describing live bus state, for tests and observability.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BusStats {
    /// Rooms currently held by the bus (subscribed, or replay not yet aged out).
    pub rooms: usize,
    /// Subscriptions currently registered.
    pub subscriptions: usize,
}

/// The `(site, room)` pair a room state is keyed by: the same room name on two sites is two
/// unrelated rooms.
type RoomKey = (SiteId, Room);

const OPEN: u8 = 0;
const CLOSED_SLOW: u8 = 1;

/// One subscriber's bus-side state. The queue's sender lives in the bus's `senders` map,
/// never in here: then dropping the bus — or closing the subscription — drops the sender
/// and ends the client's [`Subscription::recv`], while the client-side `Subscription` can
/// hold only a `Weak<Inner>`.
struct Slot {
    id: u64,
    closed: AtomicU8,
    /// Rooms this subscriber is currently in — pruned on close and on drop.
    rooms: Mutex<HashSet<RoomKey>>,
    /// The verified principal this subscriber was registered for; the only thing
    /// [`RealtimeBus::evict_room`] matches against (exact equality).
    principal: Principal,
    /// Bumped (under the state lock) each time the bus removes this slot from a room from
    /// the outside. The connection task compares it with the value it last saw and, when it
    /// differs, reconciles its mirrored room set from `rooms` (the source of truth).
    generation: AtomicU64,
}

impl Slot {
    fn close_reason(&self) -> Option<Closed> {
        match self.closed.load(Ordering::SeqCst) {
            OPEN => None,
            CLOSED_SLOW => Some(Closed::SlowConsumer),
            _ => Some(Closed::ChannelClosed),
        }
    }

    /// Locks the membership set. Taken while the state lock is held, or standalone for a
    /// short update that does not overlap any other lock — but never held while taking the
    /// state lock: the bus's lock order is state before slot (see [`State`]).
    fn lock_rooms(&self) -> MutexGuard<'_, HashSet<RoomKey>> {
        self.rooms
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

struct RoomState {
    /// The incarnation's last handed-out seq, seeded at the bus's current high-water mark
    /// and advanced one per publish ([`RoomState::next_seq`]): seqs within a room are
    /// strictly increasing and never skip a value — never going backwards either, not
    /// even across a free (see [`ensure_room`]).
    seq: u64,
    /// Lowest seq such that a resume point strictly below it has lost events that cannot
    /// be replayed: such a point is `ResyncRequired`, so missed events are never silently
    /// skipped. A created room starts it at its first handed-out seq (see
    /// [`ensure_room`]), so every `resume_after` point an earlier incarnation could have
    /// handed out lands strictly below it; prune advances it to the seqs it drops.
    lost_before: u64,
    /// Recent events with the monotonic time they were published at, oldest first.
    replay: VecDeque<(Duration, Event)>,
    /// Ids of subscriptions currently in this room.
    subs: HashSet<u64>,
    /// Position of this room's key in [`State::room_keys`], kept in lockstep by
    /// `ensure_room`/`remove_room` so the amortised sweep can reach every room in
    /// constant time (the same device `spot-cache` uses for its expiry sweep).
    slot: usize,
}

impl RoomState {
    /// A room seeded at `base`: it has handed out nothing yet, so its first seq will be
    /// `base + 1` — strictly above the bus's high-water mark — and every resume point the
    /// room's earlier incarnations could hold is strictly below that first seq.
    fn with_base(base: u64, slot: usize) -> Self {
        Self {
            seq: base,
            lost_before: base.checked_add(1).unwrap_or_else(|| {
                panic!("realtime bus exhausted its `u64` sequence space; refusing a new room")
            }),
            replay: VecDeque::new(),
            subs: HashSet::new(),
            slot,
        }
    }

    /// Hands out this room's next seq: strictly increasing, never skipping, and never
    /// wrapping. `u64::MAX` is the scheme's hard ceiling ([`State::high_water`]): a bus
    /// that has handed out every seq panics instead of reusing one.
    fn next_seq(&mut self) -> u64 {
        let seq = self.seq.checked_add(1).unwrap_or_else(|| {
            panic!("realtime bus exhausted its `u64` sequence space; refusing to reuse a seq")
        });
        self.seq = seq;
        seq
    }
}

/// The bus's guarded data, all behind one `Mutex` on [`Inner`].
///
/// **Lock order:** this state is always locked *before* a [`Slot`]'s rooms mutex
/// ([`Slot::lock_rooms`]) — publish, join, close and `Subscription::drop` all nest it that
/// way, and a slot's set is otherwise only ever locked on its own (`leave`), never held
/// while the state lock is taken; inverting the order would deadlock. The authorizer's
/// `.await` runs outside every `std::sync` lock.
struct State {
    rooms: HashMap<RoomKey, RoomState>,
    /// The room keys in no particular order — the vector the amortised sweep walks a
    /// bounded window of, exactly like `spot-cache`'s per-site key vector. Invariant:
    /// `room_keys[i]` is the key of the room whose `slot` is `i`, so
    /// `rooms.len() == room_keys.len()`. Kept in lockstep by `ensure_room`/`remove_room`.
    room_keys: Vec<RoomKey>,
    subs: HashMap<u64, Arc<Slot>>,
    senders: HashMap<u64, mpsc::Sender<Event>>,
    next_sub: u64,
    /// Publishes since the last amortised sweep (ticks up in `publish`, resets on sweep).
    publishes: u64,
    /// Rotates which window of `room_keys` the next amortised sweep examines.
    sweep_cursor: u64,
    /// The highest seq this bus has ever handed out, across every room. A new room's
    /// counter is seeded at it (see [`ensure_room`]), so a freed `(site, room)`'s next
    /// incarnation hands out nothing — and lets no `resume_after` point pass — that any
    /// earlier incarnation handed out, and the bus's very first room counts from seq 1.
    /// This is the scheme's one exhaustible resource: handing out `u64::MAX` is allowed
    /// exactly once, and the bus panics rather than wraps (see [`RoomState::next_seq`]).
    high_water: u64,
}

/// Shared bus internals; a `RealtimeBus` is a cheap `Clone` handle to one.
struct Inner {
    state: Mutex<State>,
    clock: Arc<dyn Clock>,
    authorizer: Arc<dyn RoomAuthorizer>,
    config: BusConfig,
}

impl Inner {
    /// Locks the state. The guarded data is plain maps; a panic elsewhere cannot leave them
    /// logically inconsistent, and no `.await` ever happens under this lock. It is the
    /// outer lock of the bus: a slot's rooms mutex is only ever taken under it (or alone) —
    /// see [`State`]'s lock order.
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// The in-process realtime event bus (ADR-25).
///
/// Publish is synchronous and never awaits a subscriber; delivery into each subscriber's
/// bounded queue is a non-blocking `try_send`, and a subscriber whose queue overflows is
/// closed ([`Closed::SlowConsumer`]) while publishers and other subscribers carry on.
///
/// Cross-node fan-out plugs in later at [`RealtimeBus::publish`] — the single funnel for
/// new events on this node; see the crate docs.
#[derive(Clone)]
pub struct RealtimeBus {
    inner: Arc<Inner>,
}

impl RealtimeBus {
    /// Builds a bus with the given limits, time source and permission seam.
    pub fn new(
        config: BusConfig,
        clock: Arc<dyn Clock>,
        authorizer: Arc<dyn RoomAuthorizer>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                state: Mutex::new(State {
                    rooms: HashMap::new(),
                    subs: HashMap::new(),
                    senders: HashMap::new(),
                    next_sub: 0,
                    high_water: 0,
                    room_keys: Vec::new(),
                    publishes: 0,
                    sweep_cursor: 0,
                }),
                clock,
                authorizer,
                config,
            }),
        }
    }

    /// Publishes a small notification to `(site, room)` and returns its `seq`.
    ///
    /// Never blocks and never awaits a subscriber, and it is O(subscribers of this room)
    /// plus one amortised step: the published room is aged inline, other rooms are only
    /// visited by the bounded sweep every [`SWEEP_INTERVAL_PUBLISHES`] publishes, and a
    /// full ageing pass lives in [`RealtimeBus::stats`]. Call this **after** the database
    /// transaction committed (directly, or via
    /// [`PendingEvents::flush`](crate::PendingEvents::flush)). A later cross-node
    /// fan-out consumer will feed remote events through this same funnel.
    pub fn publish(
        &self,
        site: &SiteId,
        room: &Room,
        kind: impl Into<String>,
        payload: serde_json::Value,
    ) -> u64 {
        let now = self.inner.clock.monotonic();
        let mut state = self.inner.lock();
        let key = (site.clone(), room.clone());
        let room_state = ensure_room(&mut state, key);
        let seq = room_state.next_seq();
        let event = Event {
            room: room.clone(),
            seq,
            kind: kind.into(),
            payload: Arc::new(payload),
            published_at: self.inner.clock.now_utc(),
        };
        room_state.replay.push_back((now, event.clone()));
        prune_room(room_state, &self.inner.config, now);
        let ids: Vec<u64> = room_state.subs.iter().copied().collect();
        // This handout moves the bus's high-water mark, and the mark only ever moves
        // up: rooms hold independent counters, so a publish to a room that trails
        // another hands out a seq still below the mark, and assigning `seq` unchecked
        // would pull the mark down. A re-created room seeds at the mark, so a regressed
        // mark would seed it below a seq an earlier incarnation already handed out — a
        // stale `resume_after` could then pass as gap-free.
        state.high_water = state.high_water.max(seq);

        // Deliver synchronously: one try_send per subscriber, never awaited. A full queue
        // means the subscriber is a slow consumer: close it, never wait for it. Ids are
        // collected first so the room and the sender map are not borrowed simultaneously.
        let mut overflowed = Vec::new();
        for id in ids {
            if let Some(tx) = state.senders.get(&id)
                && tx.try_send(event.clone()).is_err()
            {
                overflowed.push(id);
            }
        }
        for id in overflowed {
            tracing::warn!(subscription = id, room = %room, "closing slow consumer");
            if let Some(slot) = state.subs.get(&id).cloned() {
                close_slot(&mut state, &slot, CLOSED_SLOW);
            }
        }
        // Amortised global ageing: every SWEEP_INTERVAL_PUBLISHES publishes, sweep one
        // bounded window of *other* rooms under the same lock — no re-lock, no full pass.
        state.publishes += 1;
        if state.publishes >= SWEEP_INTERVAL_PUBLISHES {
            state.publishes = 0;
            state.sweep_rooms(&self.inner.config, now);
        }
        seq
    }

    /// Registers a new subscription for `principal` on `site`. The subscription starts in
    /// no rooms; join them with [`Subscription::join`].
    pub fn subscribe(&self, site: SiteId, principal: Principal) -> Subscription {
        let capacity = self.inner.config.queue_capacity.max(1);
        let (tx, rx) = mpsc::channel(capacity);
        let mut state = self.inner.lock();
        let id = state.next_sub;
        state.next_sub += 1;
        let slot = Arc::new(Slot {
            id,
            closed: AtomicU8::new(OPEN),
            rooms: Mutex::new(HashSet::new()),
            principal: principal.clone(),
            generation: AtomicU64::new(0),
        });
        state.subs.insert(id, Arc::clone(&slot));
        state.senders.insert(id, tx);
        Subscription {
            inner: Arc::downgrade(&self.inner),
            slot,
            rx,
            site,
            principal,
            seen_generation: 0,
        }
    }

    /// Makes every subscription of `principal` on `site` leave `room`, silently: the
    /// connections stay open and in their other rooms, and no message is sent to them. This
    /// is the revocation path for a permission change (the caller revalidates the rooms it
    /// cares about and calls this for each one the principal may no longer read).
    ///
    /// Returns how many subscriptions left. When it returns, no further event of `room` is
    /// delivered to those subscriptions, **including events already buffered in their
    /// queues** (dropped on receive; see [`Subscription::recv`]). Idempotent: an unknown
    /// site, room or principal, or a principal not in the room, returns 0. The same room
    /// name on another site is a different room and is never touched; the principal is
    /// matched exactly (id and guest flag). The owning connection learns of it through the
    /// slot's generation and drops the room from its own set before its next command.
    pub fn evict_room(&self, site: &SiteId, principal: &Principal, room: &Room) -> usize {
        let key = (site.clone(), room.clone());
        let mut state = self.inner.lock();
        let Some(room_state) = state.rooms.get(&key) else {
            return 0;
        };
        let victims: Vec<Arc<Slot>> = room_state
            .subs
            .iter()
            .filter_map(|id| state.subs.get(id))
            .filter(|slot| slot.principal == *principal)
            .cloned()
            .collect();
        let mut left = 0;
        for slot in victims {
            // Lock order: state (held) before the slot's set. Bump the generation after
            // the membership is gone so a reader that sees the new value sees the removal.
            if slot.lock_rooms().remove(&key) {
                if let Some(room_state) = state.rooms.get_mut(&key) {
                    room_state.subs.remove(&slot.id);
                }
                slot.generation.fetch_add(1, Ordering::SeqCst);
                left += 1;
            }
        }
        free_room_if_empty(&mut state, &key);
        left
    }

    /// Live bus state: room and subscription counts. `stats()` is the bus's full ageing
    /// step: every held room is pruned by age and length, and rooms with no subscribers
    /// and no replayable events are freed, before counting. The hot paths only ever age
    /// their own room, plus a bounded amortised sweep every
    /// [`SWEEP_INTERVAL_PUBLISHES`] publishes (see [`State::sweep_rooms`]), keeping
    /// `publish` O(subscribers of the published room).
    pub fn stats(&self) -> BusStats {
        self.prune();
        let state = self.inner.lock();
        BusStats {
            rooms: state.rooms.len(),
            subscriptions: state.subs.len(),
        }
    }

    /// Ages every room and frees the ones nobody is in and nothing is replayable from.
    fn prune(&self) {
        let now = self.inner.clock.monotonic();
        let mut state = self.inner.lock();
        for room_state in state.rooms.values_mut() {
            prune_room(room_state, &self.inner.config, now);
        }
        free_empty_rooms(&mut state);
    }
}

impl State {
    /// The amortised global-ageing sweep: walks at most [`SWEEP_SCAN_BUDGET`] positions of
    /// [`State::room_keys`] starting at the rotating cursor (wrapping at the length),
    /// ages each visited room's replay buffer, and frees the ones that are now empty.
    /// Bounded work per call — never a full pass — yet successive calls advance the
    /// cursor over the whole map, so an idle room is aged out and freed by ordinary
    /// publish traffic alone, exactly as `spot-cache`'s expiry sweep reaches every entry.
    fn sweep_rooms(&mut self, config: &BusConfig, now: Duration) {
        let len = self.room_keys.len();
        if len == 0 {
            self.sweep_cursor = 0;
            return;
        }
        let start = (self.sweep_cursor % len as u64) as usize;
        let examined = SWEEP_SCAN_BUDGET.min(len);
        // Resolve positions to keys before removing anything: freeing a room swap-removes
        // into the freed slot and would shift the window out from under the walk.
        let window: Vec<RoomKey> = (start..start + examined)
            .map(|position| self.room_keys[position % len].clone())
            .collect();
        for key in &window {
            if let Some(room_state) = self.rooms.get_mut(key) {
                prune_room(room_state, config, now);
            }
            free_room_if_empty(self, key);
        }
        self.sweep_cursor = self.sweep_cursor.wrapping_add(examined as u64);
    }
}

/// A subscription: one bounded queue feeding one client connection, on one site.
///
/// Dropping it removes the subscriber from every room it had joined; the bus keeps no
/// per-subscription state afterwards. When the bus (all handles) is dropped first, the
/// subscription's queue drains and then ends with [`Closed::ChannelClosed`].
pub struct Subscription {
    inner: Weak<Inner>,
    slot: Arc<Slot>,
    rx: mpsc::Receiver<Event>,
    site: SiteId,
    principal: Principal,
    /// The slot generation this subscription last reconciled against.
    #[cfg_attr(not(feature = "ws"), allow(dead_code))]
    // read only by `sync_rooms`, used by the `ws` session
    seen_generation: u64,
}

impl Subscription {
    /// Joins `room`, asking the [`RoomAuthorizer`] first — on **every** join, including
    /// joins made while resuming a subscription after reconnect, because permissions can
    /// change between connections. `resume_after` is the last `seq` the client saw, or
    /// `None` for a fresh join (no replay).
    ///
    /// Returns [`JoinError::Denied`] without entering the room if the authorizer refuses;
    /// [`JoinError::Closed`] if this subscription was already closed as a slow consumer,
    /// if its bus is gone, — or if replaying the missed events does not fit its queue, in
    /// which case the subscription is closed here as a slow consumer rather than left
    /// half-joined.
    ///
    /// [`JoinOutcome::ResyncRequired`] covers every unrecoverable resume point: older than
    /// this incarnation's replay allows (`resume_after < lost_before`), or *beyond* the
    /// room's latest seq (`resume_after > seq`) — the room's counter never goes backwards,
    /// so a beyond-the-head resume point can only come from before the room was reset —
    /// and a freshly re-created room's counter starts exactly one below its mark, so even
    /// its predecessor's last seq resyncs. `resume_after` exactly at the head of a live
    /// room replays nothing and stays plain `Joined`. The room is joined for every
    /// outcome: the client refetches, then continues live.
    pub async fn join(
        &mut self,
        room: &Room,
        resume_after: Option<u64>,
    ) -> Result<JoinOutcome, JoinError> {
        if self.slot.close_reason().is_some() {
            return Err(JoinError::Closed);
        }
        let Some(inner) = self.inner.upgrade() else {
            return Err(JoinError::Closed);
        };
        // The authorizer is awaited *outside* the state lock — the concurrency rule.
        let auth = inner
            .authorizer
            .authorize(&self.site, &self.principal, room)
            .await;
        auth.map_err(JoinError::Denied)?;

        let now = inner.clock.monotonic();
        let mut state = inner.lock();
        let live = state
            .subs
            .get(&self.slot.id)
            .is_some_and(|s| Arc::ptr_eq(s, &self.slot));
        if !live || self.slot.close_reason().is_some() {
            // Closed as a slow consumer while the authorizer was running.
            return Err(JoinError::Closed);
        }
        let key = (self.site.clone(), room.clone());
        self.slot.lock_rooms().insert(key.clone());
        let room_state = ensure_room(&mut state, key);
        room_state.subs.insert(self.slot.id);
        prune_room(room_state, &inner.config, now);

        match resume_after {
            // The client is beyond this room's seq: it cannot hold a seq this room never
            // handed out, so the room was reset under the client — freed earlier, or a
            // fresh process. Resume points from before the reset are unrecoverable.
            Some(n) if n > room_state.seq => Ok(JoinOutcome::ResyncRequired),
            Some(n) if n < room_state.seq => {
                if n < room_state.lost_before {
                    // Events the client missed are no longer replayable — dropped replay,
                    // an older incarnation of the room, or a gap too wide: refetch.
                    Ok(JoinOutcome::ResyncRequired)
                } else {
                    let missed: Vec<Event> = room_state
                        .replay
                        .iter()
                        .filter(|(_, e)| e.seq > n)
                        .map(|(_, e)| e.clone())
                        .collect();
                    let count = missed.len();
                    let mut overflowed = false;
                    if let Some(tx) = state.senders.get(&self.slot.id) {
                        for event in missed {
                            if tx.try_send(event).is_err() {
                                overflowed = true;
                                break;
                            }
                        }
                    }
                    if overflowed {
                        // Even the replay did not fit: the client is too far behind for
                        // its own queue. The subscription is a slow consumer and dead —
                        // say so here, not via a later recv.
                        close_slot(&mut state, &self.slot, CLOSED_SLOW);
                        Err(JoinError::Closed)
                    } else if count == 0 {
                        Ok(JoinOutcome::Joined)
                    } else {
                        Ok(JoinOutcome::Replayed { count })
                    }
                }
            }
            // Exactly at the head: replays nothing. But a freshly seeded room's counter
            // sits exactly one below its `lost_before` mark — everything its predecessor
            // handed out is strictly below the mark — so even a resume point at the
            // seed's head must resync instead of silently joining.
            Some(n) if n < room_state.lost_before => Ok(JoinOutcome::ResyncRequired),
            _ => Ok(JoinOutcome::Joined),
        }
    }

    /// Leaves `room`. Idempotent: leaving a room not joined is a no-op.
    pub fn leave(&mut self, room: &Room) {
        let key = (self.site.clone(), room.clone());
        if !self.slot.lock_rooms().remove(&key) {
            return;
        }
        if let Some(inner) = self.inner.upgrade() {
            let mut state = inner.lock();
            if let Some(room_state) = state.rooms.get_mut(&key) {
                room_state.subs.remove(&self.slot.id);
            }
            // Only the room this call emptied — never a scan of the whole map.
            free_room_if_empty(&mut state, &key);
        }
    }

    /// The next event across all joined rooms — `seq`-ordered within a room, no order
    /// across rooms. Returns `Err(Closed)` when the subscription ends:
    /// [`Closed::SlowConsumer`] if the bus disconnected it (remaining buffered events are
    /// discarded, matching ADR-25's "disconnect, never wait"), [`Closed::ChannelClosed`]
    /// when the subscription's queue ends (its bus was dropped).
    pub async fn recv(&mut self) -> Result<Event, Closed> {
        // The close flag wins over buffered events: a slow consumer's queue is
        // discarded, not drained.
        if let Some(reason) = self.slot.close_reason() {
            return Err(reason);
        }
        loop {
            match self.rx.recv().await {
                Some(event) => {
                    // An event queued before this subscription was evicted from (or left)
                    // its room must not be delivered: check membership at receive time.
                    let key = (self.site.clone(), event.room.clone());
                    if self.slot.lock_rooms().contains(&key) {
                        return Ok(event);
                    }
                    if let Some(reason) = self.slot.close_reason() {
                        return Err(reason);
                    }
                }
                None => return Err(self.slot.close_reason().unwrap_or(Closed::ChannelClosed)),
            }
        }
    }

    /// Reconciles a connection's mirrored room-name set with the bus's membership after
    /// the host evicted this subscription from rooms. O(rooms of this connection), and only
    /// when the slot's generation changed since the last call.
    #[cfg_attr(not(feature = "ws"), allow(dead_code))] // only the `ws` session calls it
    pub(crate) fn sync_rooms(&mut self, rooms: &mut HashSet<String>) {
        let generation = self.slot.generation.load(Ordering::SeqCst);
        if generation == self.seen_generation {
            return;
        }
        self.seen_generation = generation;
        let members = self.slot.lock_rooms();
        rooms.retain(|name| {
            Room::parse(name).is_ok_and(|room| members.contains(&(self.site.clone(), room)))
        });
    }

    /// The site this subscription belongs to.
    pub fn site(&self) -> &SiteId {
        &self.site
    }

    /// The principal this subscription belongs to.
    pub fn principal(&self) -> &Principal {
        &self.principal
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        // Remove from every joined room and drop the sender, so any other reader of the
        // queue ends; then let empty rooms free themselves. If the bus is already gone
        // there is nothing to clean.
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        let mut state = inner.lock();
        state.subs.remove(&self.slot.id);
        state.senders.remove(&self.slot.id);
        let rooms: Vec<RoomKey> = self.slot.lock_rooms().iter().cloned().collect();
        self.slot.lock_rooms().clear();
        for key in rooms {
            if let Some(room_state) = state.rooms.get_mut(&key) {
                room_state.subs.remove(&self.slot.id);
            }
            // Only a room this drop emptied — never a scan of the whole map.
            free_room_if_empty(&mut state, &key);
        }
    }
}

/// Ages the replay buffer by count and by `replay_max_age`, advancing the `lost_before`
/// watermark so a resume point into dropped events cannot pass as gap-free.
fn prune_room(room_state: &mut RoomState, config: &BusConfig, now: Duration) {
    while let Some((published, _)) = room_state.replay.front() {
        if now.saturating_sub(*published) > config.replay_max_age {
            if let Some((_, event)) = room_state.replay.pop_front() {
                room_state.lost_before = room_state.lost_before.max(event.seq);
            }
        } else {
            break;
        }
    }
    while room_state.replay.len() > config.replay_len.max(1) {
        if let Some((_, event)) = room_state.replay.pop_front() {
            room_state.lost_before = room_state.lost_before.max(event.seq);
        }
    }
}

/// Looks up `(site, room)`, creating its state when the bus does not hold it. The new
/// room is registered in [`State::room_keys`] at the `slot` its state carries.
///
/// A freed `(site, room)` is never resurrected at an old seq: the fresh incarnation's
/// counter is seeded at the bus's [`State::high_water`] — the highest seq handed out
/// anywhere on this bus — so its first handed-out seq is strictly above everything
/// earlier incarnations handed out, and `seq` never goes backwards for a key, not even
/// across a free. The fresh [`RoomState::lost_before`] mark is that first handed-out seq,
/// so every `resume_after` point an earlier incarnation could have handed out lands
/// strictly below it and reads as `ResyncRequired` — never a silent gap.
fn ensure_room(state: &mut State, key: RoomKey) -> &mut RoomState {
    let fresh = !state.rooms.contains_key(&key);
    let (base, slot) = (state.high_water, state.room_keys.len());
    if fresh {
        state.room_keys.push(key.clone());
    }
    state
        .rooms
        .entry(key)
        .or_insert_with(|| RoomState::with_base(base, slot))
}

/// Removes `key` if present and keeps [`State::room_keys`] in lockstep: the key that sat
/// last in the vector swaps into the freed position and that room's `slot` is fixed up —
/// the same swap-remove device `spot-cache` uses for its key vector.
fn remove_room(state: &mut State, key: &RoomKey) {
    let Some(room_state) = state.rooms.remove(key) else {
        return;
    };
    let slot = room_state.slot;
    if slot < state.room_keys.len() {
        // swap_remove returns the removed element (this key); the key that moved into
        // `slot` is what now sits there — unless the removed key was last, in which case
        // the vector shrank and nothing needs its slot fixed.
        state.room_keys.swap_remove(slot);
        if let Some(moved) = state.room_keys.get(slot) {
            let moved = moved.clone();
            if let Some(moved_state) = state.rooms.get_mut(&moved) {
                moved_state.slot = slot;
            }
        }
    }
}

/// Frees exactly `key` when the call that emptied it left it with no subscribers and no
/// replayable events. Bounded by the caller's own touched rooms — the whole-map ageing
/// lives in [`RealtimeBus::stats`] ([`free_empty_rooms`]) and the amortised sweep
/// ([`State::sweep_rooms`]).
fn free_room_if_empty(state: &mut State, key: &RoomKey) {
    let empty = state
        .rooms
        .get(key)
        .is_some_and(|room_state| room_state.subs.is_empty() && room_state.replay.is_empty());
    if empty {
        remove_room(state, key);
    }
}

/// Ages every room by count and `replay_max_age`, and frees the ones nobody is in and
/// nothing is replayable from across the whole map — the full sweep behind
/// [`RealtimeBus::stats`], not a per-call path. (A freed room's next incarnation is
/// seeded above everything the bus has handed out — see [`ensure_room`] — so
/// reconnecting clients hit `ResyncRequired` rather than a silent gap, and `seq` never
/// goes backwards for the key.)
fn free_empty_rooms(state: &mut State) {
    state
        .rooms
        .retain(|_, room_state| !room_state.subs.is_empty() || !room_state.replay.is_empty());
    // Rebuild the sweep vector and the slot stamps after the retain (cold path only, so
    // the O(n) rebuild keeps the lockstep invariant the cheapest way).
    state.room_keys.clear();
    state.room_keys.extend(state.rooms.keys().cloned());
    for (position, key) in state.room_keys.iter().enumerate() {
        if let Some(room_state) = state.rooms.get_mut(key) {
            room_state.slot = position;
        }
    }
    state.sweep_cursor = 0;
}

/// Marks `slot` closed with `reason` and removes it from the bus's maps, every room and
/// its sender queue. Called with the state lock held; performs no I/O and no await. Frees
/// only the slot's rooms that this close emptied — no global map scan.
fn close_slot(state: &mut State, slot: &Arc<Slot>, reason: u8) {
    slot.closed.store(reason, Ordering::SeqCst);
    state.subs.remove(&slot.id);
    state.senders.remove(&slot.id);
    let rooms: Vec<RoomKey> = slot.lock_rooms().iter().cloned().collect();
    slot.lock_rooms().clear();
    for key in rooms {
        if let Some(room_state) = state.rooms.get_mut(&key) {
            room_state.subs.remove(&slot.id);
        }
        free_room_if_empty(state, &key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key_of(site: &str, room: &str) -> RoomKey {
        (
            SiteId::new(site).unwrap_or_else(|e| panic!("test site name should be valid: {e}")),
            Room::doctype(room).unwrap_or_else(|e| panic!("valid room: {e}")),
        )
    }

    /// A bare `State` for seq-scheme tests: no subscriptions, no senders, no slots.
    fn state_at(high_water: u64) -> State {
        State {
            rooms: HashMap::new(),
            room_keys: Vec::new(),
            subs: HashMap::new(),
            senders: HashMap::new(),
            next_sub: 0,
            high_water,
            publishes: 0,
            sweep_cursor: 0,
        }
    }

    /// One publish's handout, as `publish` performs it between locking and delivering.
    fn hand_out(state: &mut State, key: &RoomKey) -> u64 {
        let seq = ensure_room(state, key.clone()).next_seq();
        state.high_water = state.high_water.max(seq);
        seq
    }

    #[test]
    fn a_room_is_seeded_at_the_high_water_mark_and_counts_up_from_there() {
        let mut state = state_at(40);
        let key = key_of("acme.example", "Item");
        assert_eq!(
            hand_out(&mut state, &key),
            41,
            "first handout: one above the mark"
        );
        assert_eq!(hand_out(&mut state, &key), 42);
        // The room is freed (as ageing does) and re-created in a bus that has meanwhile
        // handed out far more: the fresh incarnation counts strictly above the mark,
        // never from 1 again.
        state.rooms.clear();
        state.room_keys.clear();
        state.high_water = 500;
        assert_eq!(
            hand_out(&mut state, &key),
            501,
            "re-created: still one above the mark"
        );
        assert_eq!(state.high_water, 501);
    }

    #[test]
    fn seqs_rise_to_the_limit_and_stop_instead_of_wrapping() {
        // Ten seqs of headroom: handouts must strictly increase to the very last seq,
        // and the next handout must panic, not wrap and reuse a seq.
        let mut state = state_at(u64::MAX - 10);
        let key = key_of("acme.example", "Item");
        let expected_start = u64::MAX - 10;
        for i in 1..=10u64 {
            let seq = hand_out(&mut state, &key);
            assert_eq!(
                seq,
                expected_start + i,
                "handed-out seqs must strictly increase"
            );
        }
        assert_eq!(state.high_water, u64::MAX);
    }

    #[test]
    #[should_panic(expected = "exhausted its `u64` sequence space")]
    fn a_bus_that_has_handed_out_every_seq_panics_instead_of_reusing() {
        let mut state = state_at(u64::MAX);
        hand_out(&mut state, &key_of("acme.example", "Item"));
    }

    /// The reviewer's probe, kept as a regression test: rooms hold independent counters,
    /// so one room may trail another, and a handout from the trailing room must leave the
    /// mark unharmed — the mark is the highest seq the bus has handed out, room by room.
    #[test]
    fn a_handout_from_a_trailing_room_never_lowers_the_high_water_mark() {
        let mut state = state_at(0);
        let (a, b) = (key_of("acme.example", "A"), key_of("acme.example", "B"));
        hand_out(&mut state, &a); // a=1
        for _ in 0..10 {
            hand_out(&mut state, &b); // b counts to 11; the mark reaches 11
        }
        hand_out(&mut state, &a); // a's counter trails the mark: its next seq is 2
        assert!(
            state.high_water >= 11,
            "handing out from the trailing room must never lower the mark below 11"
        );
    }
}
