//! The acknowledgement gate: when may a commit be acknowledged (ADR-061 s4, s10).
//!
//! Input-driven: the caller feeds [`Event`]s with the current time in milliseconds and gets back
//! [`Output`]s. The gate holds no clock. Commits are expected in increasing `seq` order; a
//! repeated or lower `seq` is ignored.
//!
//! # A failed local commit (`LocalCommitFailed`)
//!
//! Under `remote-sync` the relay may make an entry durable before the database commit it
//! describes finishes. If that commit then fails or rolls back the caller feeds
//! [`Event::LocalCommitFailed`]. By the state of the sequence number:
//!
//! | State of `seq` | Effect |
//! | --- | --- |
//! | never seen (not yet reported locally durable) | nothing to forget; ignored and counted |
//! | waiting for its policy condition, or reported unknown | the wait is dropped, [`Output::Withdrawn`] is emitted, never an `Ack` |
//! | already acknowledged | a caller bug: an acknowledgement is never taken back; no output, counted |
//! | already withdrawn, or `command_id` does not match the entry | ignored and counted |
//!
//! Ignored events are counted in [`AckGate::rejected_events`] for the health surface.
//!
//! A withdrawn sequence number is **consumed**: the gate has moved past it, so the retry of the
//! same `command_id` (for which [`AckGate::admit`] answers [`Admission::Proceed`]) commits
//! again with a **new, higher** sequence number. The journal entry shipped for the withdrawn
//! number is voided by a compensating entry (WP-JRN-04.T4); this gate only reports
//! [`CommandOutcome::Withdrawn`]. The withdrawn record never holds entry bytes.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::policy::{OnUnreachable, Policy, StandingWarning, TenantDurability};
use crate::{BatchId, Seq};

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
    /// The local database commit of `seq` (command `command_id`) is durable.
    LocalCommitDurable {
        /// Sequence number of the commit.
        seq: Seq,
        /// The caller's idempotency key (ADR-056).
        command_id: String,
    },
    /// The synchronous standby has everything up to and including `seq`.
    StandbyConfirmed {
        /// Highest sequence number the standby holds.
        seq: Seq,
    },
    /// One object-store round trip made every entry up to and including `up_to_seq` durable.
    ChunkDurable {
        /// The batch that was written.
        batch: BatchId,
        /// Highest sequence number the chunk covers.
        up_to_seq: Seq,
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
    /// The caller's database commit of `seq` failed or rolled back. A waiting commit is
    /// withdrawn; an acknowledged one is never taken back (see the module docs).
    LocalCommitFailed {
        /// Sequence number of the commit.
        seq: Seq,
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
        /// Sequence number.
        seq: Seq,
        /// What justified it.
        basis: AckBasis,
    },
    /// The commit is durable locally and waits for its policy condition.
    Wait {
        /// Sequence number.
        seq: Seq,
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
        /// Sequence number.
        seq: Seq,
    },
    /// The RPO breach alarm is raised.
    RpoBreachRaised(RpoBreach),
    /// The backlog has shipped; the alarm clears (once).
    RpoBreachCleared,
    /// The commit's local database commit failed; it will never be acknowledged. Its sequence
    /// number is consumed and the command may be retried with a new one.
    Withdrawn {
        /// Sequence number.
        seq: Seq,
    },
}

/// Commits were acknowledged locally and have been unshipped longer than the grace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RpoBreach {
    /// Time the oldest unshipped, acknowledged commit was acknowledged.
    pub since: u64,
    /// The lowest sequence number acknowledged but not yet in the object store.
    pub unshipped_from_seq: Seq,
}

/// What the gate says about a commit it has seen, for the retry of a `command_id`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandOutcome {
    /// Durable locally, still waiting for its policy condition.
    Pending {
        /// Sequence number.
        seq: Seq,
    },
    /// The commit was acknowledged.
    Committed {
        /// Sequence number.
        seq: Seq,
    },
    /// Durable locally; the acknowledgement did not arrive in time (and may still).
    Unknown {
        /// Sequence number.
        seq: Seq,
    },
    /// The local commit failed and the commit was withdrawn. The sequence number is consumed;
    /// the command may be retried (a new sequence number).
    Withdrawn {
        /// The consumed sequence number.
        seq: Seq,
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
    entries: BTreeMap<Seq, Entry>,
    by_command: HashMap<String, Seq>,
    pending: BTreeSet<Seq>,
    last_seq: Option<Seq>,
    standby_upto: Option<Seq>,
    shipped_upto: Option<Seq>,
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
            last_seq: None,
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
    /// sequence number, or a mismatching command id). For the health surface.
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
        let seq = *self.by_command.get(command_id)?;
        let entry = self.entries.get(&seq)?;
        Some(match entry.state {
            State::Pending => CommandOutcome::Pending { seq },
            State::Unknown => CommandOutcome::Unknown { seq },
            State::Acked(_) => CommandOutcome::Committed { seq },
            State::Withdrawn => CommandOutcome::Withdrawn { seq },
        })
    }

    /// Feed one event at time `now_millis` (clamped to never run backwards).
    pub fn handle(&mut self, now_millis: u64, event: Event) -> Vec<Output> {
        self.now = self.now.max(now_millis);
        let now = self.now;
        let mut out = Vec::new();
        match event {
            Event::LocalCommitDurable { seq, command_id } => {
                self.on_commit(now, seq, command_id, &mut out);
            }
            Event::LocalCommitFailed { seq, command_id } => {
                self.on_commit_failed(seq, &command_id, &mut out);
            }
            Event::StandbyConfirmed { seq } => self.on_standby(now, seq, &mut out),
            Event::ChunkDurable { up_to_seq, .. } => self.on_chunk(now, up_to_seq, &mut out),
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

    fn on_commit(&mut self, now: u64, seq: Seq, command_id: String, out: &mut Vec<Output>) {
        let withdrawn_before = self
            .by_command
            .get(&command_id)
            .and_then(|s| self.entries.get(s))
            .is_some_and(|e| e.state == State::Withdrawn);
        if self.last_seq.is_some_and(|last| seq <= last)
            || (self.by_command.contains_key(&command_id) && !withdrawn_before)
        {
            return;
        }
        self.last_seq = Some(seq);
        let mut entry = Entry {
            command_id: command_id.clone(),
            registered_at: now,
            state: State::Pending,
            exposed: false,
            acked_at: now,
        };
        self.by_command.insert(command_id, seq);
        let basis = match self.tenant.policy {
            Policy::Local => Some(AckBasis::Local),
            Policy::ReplicaSync => self
                .standby_upto
                .is_some_and(|s| seq <= s)
                .then_some(AckBasis::Standby),
            Policy::RemoteSync { on_unreachable } => {
                if self.shipped_upto.is_some_and(|s| seq <= s) {
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
                self.entries.insert(seq, entry);
                out.push(Output::Ack { seq, basis });
            }
            None => {
                self.entries.insert(seq, entry);
                self.pending.insert(seq);
                out.push(Output::Wait { seq });
            }
        }
    }

    fn on_commit_failed(&mut self, seq: Seq, command_id: &str, out: &mut Vec<Output>) {
        let Some(entry) = self.entries.get_mut(&seq) else {
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
        self.pending.remove(&seq);
        out.push(Output::Withdrawn { seq });
    }

    fn on_standby(&mut self, now: u64, seq: Seq, out: &mut Vec<Output>) {
        if self.tenant.policy != Policy::ReplicaSync {
            return;
        }
        if self.standby_upto.is_some_and(|s| seq <= s) {
            return;
        }
        self.standby_upto = Some(seq);
        self.release_upto(now, seq, AckBasis::Standby, false, out);
    }

    fn on_chunk(&mut self, now: u64, up_to: Seq, out: &mut Vec<Output>) {
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

    /// Acknowledge every pending commit up to `upto` in seq order; commits whose outcome was
    /// reported unknown become acknowledged silently.
    fn release_upto(
        &mut self,
        now: u64,
        upto: Seq,
        basis: AckBasis,
        shipped: bool,
        out: &mut Vec<Output>,
    ) {
        for (seq, entry) in self.entries.range_mut(..=upto) {
            match entry.state {
                State::Pending => {
                    entry.state = State::Acked(basis);
                    entry.acked_at = now;
                    self.pending.remove(seq);
                    out.push(Output::Ack { seq: *seq, basis });
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
                let waiting: Vec<Seq> = self.pending.iter().copied().collect();
                for seq in waiting {
                    if let Some(entry) = self.entries.get_mut(&seq) {
                        entry.state = State::Acked(AckBasis::Degraded);
                        entry.exposed = true;
                        entry.acked_at = now;
                    }
                    self.pending.remove(&seq);
                    out.push(Output::Ack {
                        seq,
                        basis: AckBasis::Degraded,
                    });
                }
            }
        }
    }

    fn expire(&mut self, now: u64, out: &mut Vec<Output>) {
        let deadline = self.config.ack_deadline_millis;
        let due: Vec<Seq> = self
            .pending
            .iter()
            .copied()
            .filter(|seq| {
                self.entries
                    .get(seq)
                    .is_some_and(|e| e.registered_at.saturating_add(deadline) <= now)
            })
            .collect();
        for seq in due {
            if let Some(entry) = self.entries.get_mut(&seq) {
                entry.state = State::Unknown;
            }
            self.pending.remove(&seq);
            out.push(Output::CommitOutcomeUnknown { seq });
        }
    }

    fn check_breach(&mut self, now: u64, out: &mut Vec<Output>) {
        let oldest = self
            .entries
            .iter()
            .find(|(_, e)| e.exposed)
            .map(|(seq, e)| (*seq, e.acked_at));
        match (oldest, self.breach) {
            (None, Some(_)) => {
                self.breach = None;
                out.push(Output::RpoBreachCleared);
            }
            (Some((seq, _)), Some(mut breach)) => {
                breach.unshipped_from_seq = seq;
                self.breach = Some(breach);
            }
            (Some((seq, since)), None)
                if now >= since.saturating_add(self.config.rpo_grace_millis) =>
            {
                let breach = RpoBreach {
                    since,
                    unshipped_from_seq: seq,
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
            let Some((&seq, entry)) = self.entries.iter().next() else {
                break;
            };
            if !matches!(entry.state, State::Acked(_) | State::Withdrawn) || entry.exposed {
                break;
            }
            let command_id = entry.command_id.clone();
            self.entries.remove(&seq);
            // A retried command id points at a newer sequence number: leave that mapping.
            if self.by_command.get(&command_id) == Some(&seq) {
                self.by_command.remove(&command_id);
            }
        }
    }
}
