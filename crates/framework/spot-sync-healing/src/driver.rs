//! The healing driver: findings in, bounded and journaled repairs out.

use std::cmp::Reverse;
use std::collections::BTreeMap;

use spot_conditions::{ConditionParams, Millis};
use spot_sync_reconcile::Scope;

use crate::actions::{ActionOutcome, ActionRequest, RepairActions, run};
use crate::frozen::{FrozenScopes, NoFrozen};
use crate::journal::{HealingJournal, JournalEntry, JournalOutcome};
use crate::plan::{Observation, Planned, Priority, plan};
use crate::signal::{Cause, ConditionSignal};
use crate::window::RateWindow;
use crate::{ActionKind, HealingError, HealingSettings, Target};

/// One repair the driver ran in a `step`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attempt {
    /// The destination node.
    pub node: String,
    /// The scope.
    pub scope: Scope,
    /// The record or scope repaired.
    pub target: Target,
    /// The last action tried (the one that decided the outcome).
    pub kind: ActionKind,
    /// The outcome of that action; `NotSupported` only when every action of the ladder was.
    pub outcome: ActionOutcome,
    /// Which execution of this repair this was, counting from 1.
    pub number: u32,
    /// When, on the caller's clock.
    pub at: Millis,
}

/// Where one repair stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepairStatus {
    /// Waiting its turn (rate limit, in-flight bound or priority).
    Pending,
    /// Failed; waiting out the backoff.
    Backoff {
        /// The earliest next attempt.
        until: Millis,
    },
    /// Done; waiting for the finding to disappear.
    InFlight {
        /// When it counts as not having worked.
        verify_at: Millis,
    },
    /// The scope is on the freeze list: never repaired.
    Frozen,
    /// Gave up after `max_attempts`; the condition is raised.
    Stopped,
    /// No repair is made for the whole scope; the code says why.
    Refused {
        /// The `sync_healing.*` code.
        code: &'static str,
    },
}

/// One row of [`HealingDriver::status`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusEntry {
    /// The destination node.
    pub node: String,
    /// The scope.
    pub scope: Scope,
    /// The repair, or `None` for a refusal that covers the whole scope.
    pub target: Option<Target>,
    /// Where it stands.
    pub status: RepairStatus,
}

#[derive(Debug)]
struct KeyState {
    first_seen: Millis,
    attempts: u32,
    next_allowed: Millis,
    awaiting: Option<Millis>,
    stopped: bool,
    last_kind: ActionKind,
    version: Option<u64>,
    priority: Priority,
    ladder: &'static [ActionKind],
}

#[derive(Debug, Default)]
struct ScopeState {
    refused: Option<HealingError>,
    condition_open: bool,
    keys: BTreeMap<Target, KeyState>,
}

type ScopeId = (String, Scope);

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Candidate {
    rank: Reverse<Priority>,
    first_seen: Millis,
    id: ScopeId,
    target: Target,
}

/// Executes repairs safely: rate limited, backed off, journaled, bounded, and never against a
/// frozen scope or from a mirror to its owner.
///
/// The driver reads no clock and holds no thread: the caller passes `now` and a list of the
/// latest comparisons to every [`step`](Self::step), and applies the
/// [`take_signals`](Self::take_signals) it collects to its conditions engine.
pub struct HealingDriver<A: RepairActions, J: HealingJournal, F: FrozenScopes = NoFrozen> {
    actions: A,
    journal: J,
    frozen: F,
    settings: HealingSettings,
    scopes: BTreeMap<ScopeId, ScopeState>,
    window: RateWindow,
    signals: Vec<ConditionSignal>,
}

impl<A: RepairActions, J: HealingJournal> HealingDriver<A, J, NoFrozen> {
    /// A driver with nothing frozen.
    ///
    /// # Errors
    ///
    /// [`HealingError::InvalidSettings`] when the settings fail
    /// [`HealingSettings::validate`].
    pub fn new(actions: A, journal: J, settings: HealingSettings) -> Result<Self, HealingError> {
        settings.validate()?;
        Ok(Self {
            actions,
            journal,
            frozen: NoFrozen,
            settings,
            scopes: BTreeMap::new(),
            window: RateWindow::default(),
            signals: Vec::new(),
        })
    }
}

impl<A: RepairActions, J: HealingJournal, F: FrozenScopes> HealingDriver<A, J, F> {
    /// The same driver with `frozen` as its freeze list.
    pub fn with_frozen<G: FrozenScopes>(self, frozen: G) -> HealingDriver<A, J, G> {
        HealingDriver {
            actions: self.actions,
            journal: self.journal,
            frozen,
            settings: self.settings,
            scopes: self.scopes,
            window: self.window,
            signals: self.signals,
        }
    }

    /// The actions (for a test double to inspect).
    pub fn actions(&self) -> &A {
        &self.actions
    }

    /// The journal.
    pub fn journal(&self) -> &J {
        &self.journal
    }

    /// The freeze list, to change it.
    pub fn frozen_mut(&mut self) -> &mut F {
        &mut self.frozen
    }

    /// The settings in force.
    pub fn settings(&self) -> &HealingSettings {
        &self.settings
    }

    /// The condition changes since the last call, oldest first.
    pub fn take_signals(&mut self) -> Vec<ConditionSignal> {
        std::mem::take(&mut self.signals)
    }

    /// One pass: takes the latest comparisons, runs the repairs that are due and allowed, and
    /// returns what it ran.
    ///
    /// Level-triggered: only the findings given are wanted. A repair whose finding is gone
    /// has worked; a scope given with no findings is verified clean and its condition clears.
    /// Run twice with the same observations and the same `now`, the second pass does nothing.
    ///
    /// Order: most urgent first (aggregates, then records, then orphans), then the oldest
    /// finding first. At most `max_in_flight` repairs are executed and not yet verified, at
    /// most the global and per-node rate in any one minute.
    pub fn step(&mut self, observations: &[Observation], now: Millis) -> Vec<Attempt> {
        for observation in observations {
            self.refresh(observation, now);
        }
        self.expire_unverified(now);
        self.window.prune(now);

        let mut in_flight = self
            .scopes
            .values()
            .flat_map(|s| s.keys.values())
            .filter(|k| k.awaiting.is_some())
            .count();
        let mut candidates = self.candidates(now);
        candidates.sort();

        let mut attempts = Vec::new();
        for candidate in candidates {
            if in_flight >= self.settings.max_in_flight as usize
                || self
                    .window
                    .global_full(self.settings.max_repairs_per_minute)
            {
                break;
            }
            let node = &candidate.id.0;
            if self
                .window
                .node_full(node, self.settings.max_repairs_per_node_per_minute)
            {
                continue;
            }
            let attempt = self.execute(&candidate, now);
            if attempt.outcome == ActionOutcome::Done {
                in_flight += 1;
            }
            attempts.push(attempt);
        }
        attempts
    }

    /// Where every known repair stands at `now`.
    pub fn status(&self, now: Millis) -> Vec<StatusEntry> {
        let mut out = Vec::new();
        for (id, state) in &self.scopes {
            let (node, scope) = id;
            if let Some(error) = &state.refused {
                out.push(StatusEntry {
                    node: node.clone(),
                    scope: scope.clone(),
                    target: None,
                    status: RepairStatus::Refused { code: error.code() },
                });
                continue;
            }
            let frozen = self.frozen.is_frozen(node, scope);
            for (target, key) in &state.keys {
                let status = if frozen {
                    RepairStatus::Frozen
                } else if key.stopped {
                    RepairStatus::Stopped
                } else if let Some(verify_at) = key.awaiting {
                    RepairStatus::InFlight { verify_at }
                } else if key.next_allowed > now {
                    RepairStatus::Backoff {
                        until: key.next_allowed,
                    }
                } else {
                    RepairStatus::Pending
                };
                out.push(StatusEntry {
                    node: node.clone(),
                    scope: scope.clone(),
                    target: Some(target.clone()),
                    status,
                });
            }
        }
        out
    }

    /// Operator action: lets a stopped scope try again from the first attempt. The condition
    /// stays raised until a comparison shows the scope clean.
    pub fn reset(&mut self, node: &str, scope: &Scope, now: Millis) {
        if let Some(state) = self.scopes.get_mut(&(node.to_owned(), scope.clone())) {
            for key in state.keys.values_mut() {
                if key.stopped {
                    key.stopped = false;
                    key.attempts = 0;
                    key.next_allowed = now;
                }
            }
        }
    }

    fn refresh(&mut self, observation: &Observation, now: Millis) {
        let id: ScopeId = (observation.node.clone(), observation.report.scope.clone());
        match plan(observation) {
            Err(error) => {
                let state = self.scopes.entry(id.clone()).or_default();
                state.keys.clear();
                state.refused = Some(error);
            }
            Ok(planned) => {
                let state = self.scopes.entry(id.clone()).or_default();
                state.refused = None;
                let mut wanted: BTreeMap<Target, Planned> = BTreeMap::new();
                for item in planned {
                    wanted.entry(item.target.clone()).or_insert(item);
                }
                state.keys.retain(|target, _| wanted.contains_key(target));
                for (target, item) in wanted {
                    let key = state.keys.entry(target).or_insert_with(|| KeyState {
                        first_seen: now,
                        attempts: 0,
                        next_allowed: now,
                        awaiting: None,
                        stopped: false,
                        last_kind: item.ladder[0],
                        version: item.version,
                        priority: item.priority,
                        ladder: item.ladder,
                    });
                    key.version = item.version;
                }
            }
        }
        self.update_condition(&id);
        if let Some(state) = self.scopes.get(&id)
            && state.refused.is_none()
            && state.keys.is_empty()
            && !state.condition_open
        {
            self.scopes.remove(&id);
        }
    }

    fn expire_unverified(&mut self, now: Millis) {
        let mut touched = Vec::new();
        for (id, state) in &mut self.scopes {
            for (target, key) in &mut state.keys {
                let Some(verify_at) = key.awaiting else {
                    continue;
                };
                if now < verify_at {
                    continue;
                }
                key.awaiting = None;
                self.journal.append(JournalEntry {
                    node: id.0.clone(),
                    scope: id.1.clone(),
                    target: target.clone(),
                    version: key.version,
                    kind: key.last_kind,
                    outcome: JournalOutcome::NotVerified,
                    at: now,
                });
                if key.attempts >= self.settings.max_attempts {
                    key.stopped = true;
                } else {
                    key.next_allowed = add(now, self.settings.backoff(key.attempts));
                }
                touched.push(id.clone());
            }
        }
        for id in touched {
            self.update_condition(&id);
        }
    }

    fn candidates(&self, now: Millis) -> Vec<Candidate> {
        let mut out = Vec::new();
        for (id, state) in &self.scopes {
            if state.refused.is_some() || self.frozen.is_frozen(&id.0, &id.1) {
                continue;
            }
            for (target, key) in &state.keys {
                if key.stopped || key.awaiting.is_some() || key.next_allowed > now {
                    continue;
                }
                out.push(Candidate {
                    rank: Reverse(key.priority),
                    first_seen: key.first_seen,
                    id: id.clone(),
                    target: target.clone(),
                });
            }
        }
        out
    }

    fn execute(&mut self, candidate: &Candidate, now: Millis) -> Attempt {
        let (node, scope) = (&candidate.id.0, &candidate.id.1);
        let (ladder, version) = self
            .scopes
            .get(&candidate.id)
            .and_then(|s| s.keys.get(&candidate.target))
            .map_or((&[][..], None), |k| (k.ladder, k.version));
        self.window.record(node, now);

        let request = ActionRequest {
            node,
            scope,
            target: &candidate.target,
            version,
        };
        let mut kind = ladder
            .first()
            .copied()
            .unwrap_or(ActionKind::RequestSnapshot);
        let mut outcome = ActionOutcome::NotSupported;
        for rung in ladder {
            kind = *rung;
            outcome = run(&mut self.actions, kind, &request);
            self.journal.append(JournalEntry {
                node: node.clone(),
                scope: scope.clone(),
                target: candidate.target.clone(),
                version,
                kind,
                outcome: JournalOutcome::of(&outcome),
                at: now,
            });
            if outcome != ActionOutcome::NotSupported {
                break;
            }
        }

        let mut number = 0;
        if let Some(key) = self
            .scopes
            .get_mut(&candidate.id)
            .and_then(|s| s.keys.get_mut(&candidate.target))
        {
            key.attempts = key.attempts.saturating_add(1);
            key.last_kind = kind;
            number = key.attempts;
            if outcome == ActionOutcome::Done {
                key.awaiting = Some(add(now, self.settings.verify_after));
            } else if key.attempts >= self.settings.max_attempts {
                key.stopped = true;
            } else {
                key.next_allowed = add(now, self.settings.backoff(key.attempts));
            }
        }
        self.update_condition(&candidate.id);
        Attempt {
            node: candidate.id.0.clone(),
            scope: candidate.id.1.clone(),
            target: candidate.target.clone(),
            kind,
            outcome,
            number,
            at: now,
        }
    }

    /// Raises or clears the condition of one node and scope to match its state.
    fn update_condition(&mut self, id: &ScopeId) {
        let Some(state) = self.scopes.get(id) else {
            return;
        };
        let cause = if let Some(error) = &state.refused {
            Some(Cause::Refused(error.code()))
        } else if state.keys.values().any(|k| k.stopped) {
            Some(Cause::AttemptsExhausted)
        } else {
            None
        };
        let subject = format!("{}/{}", id.0, id.1.label());
        match (cause, state.condition_open) {
            (Some(cause), false) => {
                let params = ConditionParams::new()
                    .with("node", id.0.as_str())
                    .with("scope", id.1.label())
                    .with("cause", cause.as_str())
                    .with("attempts", self.settings.max_attempts.to_string());
                let evidence = self.journal.entries(&id.0, &id.1);
                self.signals.push(ConditionSignal::Raise {
                    subject,
                    params,
                    cause,
                    evidence,
                });
                if let Some(state) = self.scopes.get_mut(id) {
                    state.condition_open = true;
                }
            }
            (None, true) => {
                self.signals.push(ConditionSignal::Clear { subject });
                if let Some(state) = self.scopes.get_mut(id) {
                    state.condition_open = false;
                }
            }
            _ => {}
        }
    }
}

fn add(now: Millis, wait: Millis) -> Millis {
    Millis::new(now.as_u64().saturating_add(wait.as_u64()))
}
