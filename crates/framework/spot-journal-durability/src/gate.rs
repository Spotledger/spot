//! The acknowledgement gate: when may a commit be acknowledged (ADR-061 s4, s10).
//!
//! Input-driven: the caller feeds [`Event`]s with the current time in milliseconds and gets back
//! [`Output`]s. The gate holds no clock. Every key is a [`Position`] `(generation, seq)`, ordered
//! generation first, so the `seq` restart of a new generation after a fork is a normal increase.
//!
//! # A commit the gate does not accept
//!
//! Commits are expected in increasing position order. A commit the gate does not accept is
//! never dropped silently: the gate answers [`Output::Rejected`] with a typed
//! [`DurabilityError`]: [`DurabilityError::StaleGeneration`] for a lower generation than one
//! already seen (a stale writer), [`DurabilityError::OutOfOrder`] for a repeated or lower position
//! within the current generation, and [`DurabilityError::DuplicateCommand`] for a command id that
//! is already committed. A rejected commit changes no state.
//!
//! # A failed local commit (`LocalCommitFailed`)
//!
//! Under `remote-sync` the relay may make an entry durable before the database commit it
//! describes finishes. If that commit then fails or rolls back the caller feeds
//! [`Event::LocalCommitFailed`]. By the state of the sequence number:
//!
//! | State of the position | Effect |
//! | --- | --- |
//! | never seen (not yet reported locally durable) | nothing to forget; ignored and counted |
//! | waiting for its policy condition, or reported unknown | the wait is dropped, [`Output::Withdrawn`] is emitted, never an `Ack` |
//! | already acknowledged | a caller bug: an acknowledgement is never taken back; no output, counted |
//! | already withdrawn, or `command_id` does not match the entry | ignored and counted |
//!
//! Ignored events are counted in [`AckGate::rejected_events`] for the health surface.
//!
//! A withdrawn position is **consumed**: the gate has moved past it, so the retry of the
//! same `command_id` (for which [`AckGate::admit`] answers [`Admission::Proceed`]) commits
//! again with a **new, higher** position (in the same generation, or a later one).
//! Only the entry of that exact position is withdrawn; the same `seq` of another generation is a
//! different entry. The journal entry shipped for the withdrawn
//! position is voided by a compensating entry (WP-JRN-04.T4); this gate only reports
//! [`CommandOutcome::Withdrawn`]. The withdrawn record never holds entry bytes.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::error::DurabilityError;
use crate::policy::{OnUnreachable, Policy, StandingWarning, TenantDurability};
use crate::{BatchId, Position};

/// Tuning of the gate. Times are milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GateConfig {
    /// How long `degrade` may leave commits unshipped before an [`RpoBreach`] is raised.
    pub rpo_grace_millis: u64,
    /// How long a commit may wait for its acknowledgement condition before the outcome is
    /// reported unknown.
    pub ack_deadline_millis: u64,
    /// How many acknowledged and shipped commits to remember for [`AckGate::outcome`].
    pub retain_outcomes: usize,
}

impl Default for GateConfig {
    fn default() -> Self {
        Self {
            rpo_grace_millis: 60_000,
            ack_deadline_millis: 30_000,
            retain_outcomes: 10_000,
        }
    }
}

/// Why a chunk upload failed. Every reason marks the store unusable until it is reported
/// reachable again or a chunk becomes durable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureReason {
    /// The store could not be reached.
    Unreachable,
    /// The upload timed out.
    Timeout,
    /// The store refused the chunk.
    Rejected,
}

/// An input to the gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// The local database commit at `position` (command `command_id`) is durable.
    LocalCommitDurable {
        /// Position of the commit.
        position: Position,
        /// The caller's idempotency key (ADR-056).
        command_id: String,
    },
    /// The synchronous standby has everything up to and including `position` (across
    /// generations: positions of earlier generations are covered too).
    StandbyConfirmed {
        /// Highest position the standby holds.
        position: Position,
    },
    /// One object-store round trip made every entry up to and including `up_to` durable.
    ChunkDurable {
        /// The batch that was written.
        batch: BatchId,
        /// Highest position the chunk covers.
        up_to: Position,
    },
    /// A chunk upload failed.
    ChunkFailed {
        /// The batch that failed.
        batch: BatchId,
        /// Why.
        reason: FailureReason,
    },
    /// The relay's view of whether the object store can be reached.
    StoreReachable(bool),
    /// The caller's database commit at `position` failed or rolled back. A waiting commit is
    /// withdrawn; an acknowledged one is never taken back (see the module docs).
    LocalCommitFailed {
        /// Position of the commit.
        position: Position,
        /// The caller's idempotency key of that commit.
        command_id: String,
    },
    /// Time passes (deadlines and the RPO grace are checked on every call; this just advances).
    Tick,
}

/// What an acknowledgement was based on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AckBasis {
    /// The local commit was durable (`local`).
    Local,
    /// The standby confirmed it (`replica-sync`).
    Standby,
    /// A durable chunk covers it (`remote-sync`).
    Chunk,
    /// `remote-sync` with `degrade` while the store was unreachable: not yet off-site.
    Degraded,
}

/// A refusal to start a commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// `halt`: the store is unreachable.
    Halted,
}

/// What the gate says in response to an event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Output {
    /// The commit may be acknowledged to the caller.
    Ack {
        /// Position.
        position: Position,
        /// What justified it.
        basis: AckBasis,
    },
    /// The commit is durable locally and waits for its policy condition.
    Wait {
        /// Position.
        position: Position,
    },
    /// New commits are refused (`halt` entered).
    Refuse(Refusal),
    /// `degrade` entered: commits are acknowledged on the local commit and queued for shipping.
    Degraded {
        /// When the store became unreachable.
        since: u64,
    },
    /// The store is reachable again (ends `halt` refusal or `degrade`).
    StoreRecovered,
    /// The commit is durable locally but its acknowledgement did not arrive in time.
    CommitOutcomeUnknown {
        /// Position.
        position: Position,
    },
    /// The RPO breach alarm is raised.
    RpoBreachRaised(RpoBreach),
    /// The backlog has shipped; the alarm clears (once).
    RpoBreachCleared,
    /// The commit's local database commit failed; it will never be acknowledged. Its position
    /// is consumed and the command may be retried with a new one.
    Withdrawn {
        /// Position.
        position: Position,
    },
    /// A commit the gate does not accept (stale generation, out of order or a reused command
    /// id). It changes no state and is never acknowledged.
    Rejected {
        /// The refused commit.
        position: Position,
        /// Why, with its stable code.
        error: DurabilityError,
    },
}

/// Commits were acknowledged locally and have been unshipped longer than the grace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RpoBreach {
    /// Time the oldest unshipped, acknowledged commit was acknowledged.
    pub since: u64,
    /// The lowest position acknowledged but not yet in the object store.
    pub unshipped_from: Position,
}

/// What the gate says about a commit it has seen, for the retry of a `command_id`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandOutcome {
    /// Durable locally, still waiting for its policy condition.
    Pending {
        /// Position.
        position: Position,
    },
    /// The commit was acknowledged.
    Committed {
        /// Position.
        position: Position,
    },
    /// Durable locally; the acknowledgement did not arrive in time (and may still).
    Unknown {
        /// Position.
        position: Position,
    },
    /// The local commit failed and the commit was withdrawn. The position is consumed;
    /// the command may be retried (a new position).
    Withdrawn {
        /// The consumed position.
        position: Position,
    },
}

/// The answer to "may I start this command?".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// Run the command.
    Proceed,
    /// Do not run it.
    Refuse(Refusal),
    /// This `command_id` was already committed: answer from this, never commit again.
    Existing(CommandOutcome),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Pending,
    Unknown,
    Acked(AckBasis),
    Withdrawn,
}

#[derive(Debug, Clone)]
struct Entry {
    command_id: String,
    registered_at: u64,
    state: State,
    /// Acknowledged while the store was unreachable and not yet in a durable chunk.
    exposed: bool,
    acked_at: u64,
}

/// The state machine. See the crate docs.
#[derive(Debug, Clone)]
pub struct AckGate {
    tenant: TenantDurability,
    config: GateConfig,
    entries: BTreeMap<Position, Entry>,
    by_command: HashMap<String, Position>,
    pending: BTreeSet<Position>,
    last: Option<Position>,
    standby_upto: Option<Position>,
    shipped_upto: Option<Position>,
    reachable: bool,
    degraded_since: Option<u64>,
    breach: Option<RpoBreach>,
    rejected_events: u64,
    now: u64,
}

impl AckGate {
    /// A gate for one tenant. The store starts out reachable.
    pub fn new(tenant: TenantDurability, config: GateConfig) -> Self {
        Self {
            tenant,
            config,
            entries: BTreeMap::new(),
            by_command: HashMap::new(),
            pending: BTreeSet::new(),
            last: None,
            standby_upto: None,
            shipped_upto: None,
            reachable: true,
            degraded_since: None,
            breach: None,
            rejected_events: 0,
            now: 0,
        }
    }

    /// A copy of the gate's configuration, so a caller keeps no second copy that can differ.
    pub fn config(&self) -> GateConfig {
        self.config
    }

    /// How many `LocalCommitFailed` events were ignored (unknown or already acknowledged
    /// position, or a mismatching command id). For the health surface.
    pub fn rejected_events(&self) -> u64 {
        self.rejected_events
    }

    /// The tenant setting this gate enforces.
    pub fn tenant(&self) -> &TenantDurability {
        &self.tenant
    }

    /// The standing warning of the setting, if any.
    pub fn standing_warning(&self) -> Option<StandingWarning> {
        self.tenant.standing_warning()
    }

    /// The RPO breach currently raised, if any.
    pub fn rpo_breach(&self) -> Option<RpoBreach> {
        self.breach
    }

    /// Whether `degrade` is in effect (store unreachable).
    pub fn is_degraded(&self) -> bool {
        self.degraded_since.is_some()
    }

    /// Whether new commits are refused (`halt` with the store unreachable).
    pub fn is_halted(&self) -> bool {
        self.halts() && !self.reachable
    }

    /// Commits durable locally that wait for their acknowledgement.
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// May this command start? A `command_id` already seen is answered from its outcome and
    /// never committed a second time (ADR-061 s4, ADR-056). A withdrawn command (its local
    /// commit failed) is a fresh command: it may commit again with a new sequence number.
    pub fn admit(&self, command_id: &str) -> Admission {
        if let Some(outcome) = self
            .outcome(command_id)
            .filter(|o| !matches!(o, CommandOutcome::Withdrawn { .. }))
        {
            return Admission::Existing(outcome);
        }
        if self.is_halted() {
            return Admission::Refuse(Refusal::Halted);
        }
        Admission::Proceed
    }

    /// The outcome of a commit by its `command_id`, if the gate has seen it.
    pub fn outcome(&self, command_id: &str) -> Option<CommandOutcome> {
        let position = *self.by_command.get(command_id)?;
        let entry = self.entries.get(&position)?;
        Some(match entry.state {
            State::Pending => CommandOutcome::Pending { position },
            State::Unknown => CommandOutcome::Unknown { position },
            State::Acked(_) => CommandOutcome::Committed { position },
            State::Withdrawn => CommandOutcome::Withdrawn { position },
        })
    }

    /// Feed one event at time `now_millis` (clamped to never run backwards).
    pub fn handle(&mut self, now_millis: u64, event: Event) -> Vec<Output> {
        self.now = self.now.max(now_millis);
        let now = self.now;
        let mut out = Vec::new();
        match event {
            Event::LocalCommitDurable {
                position,
                command_id,
            } => {
                self.on_commit(now, position, command_id, &mut out);
            }
            Event::LocalCommitFailed {
                position,
                command_id,
            } => {
                self.on_commit_failed(position, &command_id, &mut out);
            }
            Event::StandbyConfirmed { position } => self.on_standby(now, position, &mut out),
            Event::ChunkDurable { up_to, .. } => self.on_chunk(now, up_to, &mut out),
            Event::ChunkFailed { .. } => self.set_reachable(now, false, &mut out),
            Event::StoreReachable(up) => self.set_reachable(now, up, &mut out),
            Event::Tick => {}
        }
        self.expire(now, &mut out);
        self.check_breach(now, &mut out);
        self.prune();
        out
    }

    fn remote(&self) -> Option<OnUnreachable> {
        match self.tenant.policy {
            Policy::RemoteSync { on_unreachable } => Some(on_unreachable),
            _ => None,
        }
    }

    fn halts(&self) -> bool {
        self.remote() == Some(OnUnreachable::Halt)
    }

    fn on_commit(
        &mut self,
        now: u64,
        position: Position,
        command_id: String,
        out: &mut Vec<Output>,
    ) {
        if let Some(error) = self.refusal_of(position, &command_id) {
            out.push(Output::Rejected { position, error });
            return;
        }
        self.last = Some(position);
        let mut entry = Entry {
            command_id: command_id.clone(),
            registered_at: now,
            state: State::Pending,
            exposed: false,
            acked_at: now,
        };
        self.by_command.insert(command_id, position);
        let basis = match self.tenant.policy {
            Policy::Local => Some(AckBasis::Local),
            Policy::ReplicaSync => self
                .standby_upto
                .is_some_and(|s| position <= s)
                .then_some(AckBasis::Standby),
            Policy::RemoteSync { on_unreachable } => {
                if self.shipped_upto.is_some_and(|s| position <= s) {
                    Some(AckBasis::Chunk)
                } else if !self.reachable && on_unreachable == OnUnreachable::Degrade {
                    entry.exposed = true;
                    Some(AckBasis::Degraded)
                } else {
                    None
                }
            }
        };
        match basis {
            Some(basis) => {
                entry.state = State::Acked(basis);
                self.entries.insert(position, entry);
                out.push(Output::Ack { position, basis });
            }
            None => {
                self.entries.insert(position, entry);
                self.pending.insert(position);
                out.push(Output::Wait { position });
            }
        }
    }

    /// Why a commit at `position` under `command_id` is not accepted, if it is not. Changes
    /// nothing.
    fn refusal_of(&self, position: Position, command_id: &str) -> Option<DurabilityError> {
        if let Some(last) = self.last {
            if position.generation < last.generation {
                return Some(DurabilityError::StaleGeneration {
                    position,
                    current: last.generation,
                });
            }
            if position <= last {
                return Some(DurabilityError::OutOfOrder { position, last });
            }
        }
        let withdrawn_before = self
            .by_command
            .get(command_id)
            .and_then(|p| self.entries.get(p))
            .is_some_and(|e| e.state == State::Withdrawn);
        if self.by_command.contains_key(command_id) && !withdrawn_before {
            return Some(DurabilityError::DuplicateCommand { position });
        }
        None
    }

    fn on_commit_failed(&mut self, position: Position, command_id: &str, out: &mut Vec<Output>) {
        let Some(entry) = self.entries.get_mut(&position) else {
            self.rejected_events = self.rejected_events.saturating_add(1);
            return;
        };
        if entry.command_id != command_id || !matches!(entry.state, State::Pending | State::Unknown)
        {
            self.rejected_events = self.rejected_events.saturating_add(1);
            return;
        }
        entry.state = State::Withdrawn;
        entry.exposed = false;
        self.pending.remove(&position);
        out.push(Output::Withdrawn { position });
    }

    fn on_standby(&mut self, now: u64, position: Position, out: &mut Vec<Output>) {
        if self.tenant.policy != Policy::ReplicaSync {
            return;
        }
        if self.standby_upto.is_some_and(|s| position <= s) {
            return;
        }
        self.standby_upto = Some(position);
        self.release_upto(now, position, AckBasis::Standby, false, out);
    }

    fn on_chunk(&mut self, now: u64, up_to: Position, out: &mut Vec<Output>) {
        if self.remote().is_none() {
            return;
        }
        // A durable chunk proves the store is there.
        self.set_reachable(now, true, out);
        if self.shipped_upto.is_some_and(|s| up_to <= s) {
            return;
        }
        self.shipped_upto = Some(up_to);
        self.release_upto(now, up_to, AckBasis::Chunk, true, out);
    }

    /// Acknowledge every pending commit up to `upto` in position order; commits whose outcome was
    /// reported unknown become acknowledged silently.
    fn release_upto(
        &mut self,
        now: u64,
        upto: Position,
        basis: AckBasis,
        shipped: bool,
        out: &mut Vec<Output>,
    ) {
        for (position, entry) in self.entries.range_mut(..=upto) {
            match entry.state {
                State::Pending => {
                    entry.state = State::Acked(basis);
                    entry.acked_at = now;
                    self.pending.remove(position);
                    out.push(Output::Ack {
                        position: *position,
                        basis,
                    });
                }
                State::Unknown => {
                    entry.state = State::Acked(basis);
                    entry.acked_at = now;
                }
                State::Acked(_) | State::Withdrawn => {}
            }
            if shipped {
                entry.exposed = false;
            }
        }
    }

    fn set_reachable(&mut self, now: u64, up: bool, out: &mut Vec<Output>) {
        let Some(on_unreachable) = self.remote() else {
            return;
        };
        if up == self.reachable {
            return;
        }
        self.reachable = up;
        if up {
            self.degraded_since = None;
            out.push(Output::StoreRecovered);
            return;
        }
        match on_unreachable {
            OnUnreachable::Halt => out.push(Output::Refuse(Refusal::Halted)),
            OnUnreachable::Degrade => {
                self.degraded_since = Some(now);
                out.push(Output::Degraded { since: now });
                let waiting: Vec<Position> = self.pending.iter().copied().collect();
                for position in waiting {
                    if let Some(entry) = self.entries.get_mut(&position) {
                        entry.state = State::Acked(AckBasis::Degraded);
                        entry.exposed = true;
                        entry.acked_at = now;
                    }
                    self.pending.remove(&position);
                    out.push(Output::Ack {
                        position,
                        basis: AckBasis::Degraded,
                    });
                }
            }
        }
    }

    fn expire(&mut self, now: u64, out: &mut Vec<Output>) {
        let deadline = self.config.ack_deadline_millis;
        let due: Vec<Position> = self
            .pending
            .iter()
            .copied()
            .filter(|position| {
                self.entries
                    .get(position)
                    .is_some_and(|e| e.registered_at.saturating_add(deadline) <= now)
            })
            .collect();
        for position in due {
            if let Some(entry) = self.entries.get_mut(&position) {
                entry.state = State::Unknown;
            }
            self.pending.remove(&position);
            out.push(Output::CommitOutcomeUnknown { position });
        }
    }

    fn check_breach(&mut self, now: u64, out: &mut Vec<Output>) {
        let oldest = self
            .entries
            .iter()
            .find(|(_, e)| e.exposed)
            .map(|(position, e)| (*position, e.acked_at));
        match (oldest, self.breach) {
            (None, Some(_)) => {
                self.breach = None;
                out.push(Output::RpoBreachCleared);
            }
            (Some((pos, _)), Some(mut breach)) => {
                breach.unshipped_from = pos;
                self.breach = Some(breach);
            }
            (Some((pos, since)), None)
                if now >= since.saturating_add(self.config.rpo_grace_millis) =>
            {
                let breach = RpoBreach {
                    since,
                    unshipped_from: pos,
                };
                self.breach = Some(breach);
                out.push(Output::RpoBreachRaised(breach));
            }
            _ => {}
        }
    }

    /// Forget the oldest settled commits beyond the retention bound.
    fn prune(&mut self) {
        while self.entries.len() > self.config.retain_outcomes {
            let Some((&position, entry)) = self.entries.iter().next() else {
                break;
            };
            if !matches!(entry.state, State::Acked(_) | State::Withdrawn) || entry.exposed {
                break;
            }
            let command_id = entry.command_id.clone();
            self.entries.remove(&position);
            // A retried command id points at a newer position: leave that mapping.
            if self.by_command.get(&command_id) == Some(&position) {
                self.by_command.remove(&command_id);
            }
        }
    }
}
