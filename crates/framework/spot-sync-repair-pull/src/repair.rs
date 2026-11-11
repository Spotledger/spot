//! The deterministic state machine.

use std::collections::BTreeMap;

use spot_sync_closure::{Position, RecordKey};

use crate::config::RepairConfig;
use crate::error::RepairError;
use crate::model::{
    CLOSURE_UNRESOLVED, ConditionRaise, Counters, Input, Output, PullRequest, ScopeCheck,
    ScopedResend,
};

#[derive(Debug, Clone, Copy)]
struct Tracked {
    seq: u64,
    position: Position,
    first_missed_ms: u64,
    asks: u32,
    next_ask_ms: u64,
}

#[derive(Debug, Clone, Copy)]
struct Resend {
    count: u32,
    next_ms: u64,
}

/// The repair loop of one node.
#[derive(Debug)]
pub struct RepairLoop {
    subject: String,
    cfg: RepairConfig,
    tracked: BTreeMap<RecordKey, Tracked>,
    order: BTreeMap<u64, RecordKey>,
    next_seq: u64,
    raised: bool,
    resend: Option<Resend>,
    force_resend: bool,
    counters: Counters,
}

impl RepairLoop {
    /// A loop for `node_id`.
    ///
    /// # Errors
    /// `InvalidConfig` when the settings are unusable.
    pub fn new(node_id: &str, cfg: RepairConfig) -> Result<Self, RepairError> {
        cfg.validate()?;
        Ok(Self {
            subject: format!("node:{node_id}"),
            cfg,
            tracked: BTreeMap::new(),
            order: BTreeMap::new(),
            next_seq: 0,
            raised: false,
            resend: None,
            force_resend: false,
            counters: Counters::default(),
        })
    }

    /// Keys currently missing and tracked.
    #[must_use]
    pub fn tracked_len(&self) -> usize {
        self.tracked.len()
    }

    /// Whether a scoped re-send is outstanding (single asks are suppressed).
    #[must_use]
    pub fn resend_pending(&self) -> bool {
        self.resend.is_some()
    }

    /// Whether `CLOSURE_UNRESOLVED` is currently raised.
    #[must_use]
    pub fn condition_raised(&self) -> bool {
        self.raised
    }

    /// The counters.
    #[must_use]
    pub fn counters(&self) -> Counters {
        self.counters
    }

    /// Feeds one input; returns what to do (a single `Nothing` when there is nothing).
    pub fn step(&mut self, input: Input, scope: &dyn ScopeCheck) -> Vec<Output> {
        let mut out = match input {
            Input::Missed {
                keys,
                bundle_position,
                now_ms,
            } => self.missed(keys, bundle_position, now_ms, scope),
            Input::Arrived(keys) => self.arrived(&keys),
            Input::Tick(now_ms) => self.tick(now_ms, scope),
        };
        if out.is_empty() {
            out.push(Output::Nothing);
        }
        out
    }

    fn missed(
        &mut self,
        keys: Vec<RecordKey>,
        position: Position,
        now_ms: u64,
        scope: &dyn ScopeCheck,
    ) -> Vec<Output> {
        let mut foreign = Vec::new();
        for key in keys {
            if let Some(t) = self.tracked.get_mut(&key) {
                t.position = t.position.min(position);
                self.counters.duplicate_misses += 1;
            } else if scope.in_scope(&key) {
                self.insert(key, position, now_ms);
            } else {
                self.counters.out_of_scope += 1;
                foreign.push(key);
            }
        }
        if foreign.is_empty() {
            Vec::new()
        } else {
            vec![Output::OutOfScope(foreign)]
        }
    }

    fn insert(&mut self, key: RecordKey, position: Position, now_ms: u64) {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.order.insert(seq, key.clone());
        self.tracked.insert(
            key,
            Tracked {
                seq,
                position,
                first_missed_ms: now_ms,
                asks: 0,
                next_ask_ms: now_ms,
            },
        );
        while self.tracked.len() > self.cfg.max_tracked {
            let Some((_, oldest)) = self.order.pop_first() else {
                break;
            };
            self.tracked.remove(&oldest);
            self.counters.dropped_by_bound += 1;
            self.force_resend = true;
        }
    }

    fn arrived(&mut self, keys: &[RecordKey]) -> Vec<Output> {
        for key in keys {
            if let Some(t) = self.tracked.remove(key) {
                self.order.remove(&t.seq);
            }
        }
        self.settle_if_empty()
    }

    /// Resets the resend and clears the condition once nothing is missing.
    fn settle_if_empty(&mut self) -> Vec<Output> {
        if !self.tracked.is_empty() {
            return Vec::new();
        }
        self.resend = None;
        if self.raised {
            self.raised = false;
            return vec![Output::Clear(self.subject.clone())];
        }
        Vec::new()
    }

    fn tick(&mut self, now_ms: u64, scope: &dyn ScopeCheck) -> Vec<Output> {
        let mut out = Vec::new();
        // A scope that narrowed since the miss: those keys can never be served.
        let gone: Vec<RecordKey> = self
            .tracked
            .keys()
            .filter(|k| !scope.in_scope(k))
            .cloned()
            .collect();
        if !gone.is_empty() {
            for key in &gone {
                if let Some(t) = self.tracked.remove(key) {
                    self.order.remove(&t.seq);
                }
            }
            self.counters.out_of_scope += gone.len() as u64;
            out.push(Output::OutOfScope(gone));
        }
        out.extend(self.settle_if_empty());
        if self.tracked.is_empty() && !self.force_resend {
            return out;
        }

        let exhausted = self
            .tracked
            .values()
            .any(|t| t.asks >= self.cfg.max_asks_per_key && t.next_ask_ms <= now_ms);
        match self.resend {
            Some(r) => {
                if now_ms >= r.next_ms {
                    let count = r.count.saturating_add(1);
                    self.resend = Some(Resend {
                        count,
                        next_ms: now_ms.saturating_add(self.backoff(count, "resend")),
                    });
                    out.push(self.escalation(scope));
                }
            }
            None => {
                if self.force_resend || self.tracked.len() > self.cfg.escalate_keys || exhausted {
                    self.force_resend = false;
                    self.resend = Some(Resend {
                        count: 1,
                        next_ms: now_ms.saturating_add(self.backoff(1, "resend")),
                    });
                    out.push(self.escalation(scope));
                    if self.tracked.is_empty() {
                        self.resend = None;
                    }
                } else {
                    out.extend(self.asks(now_ms));
                }
            }
        }

        if !self.raised
            && self
                .tracked
                .values()
                .any(|t| now_ms.saturating_sub(t.first_missed_ms) >= self.cfg.raise_after_ms)
        {
            self.raised = true;
            out.push(Output::Raise(ConditionRaise {
                code: CLOSURE_UNRESOLVED.to_owned(),
                subject: self.subject.clone(),
                missing: self.tracked.len(),
            }));
        }
        out
    }

    fn escalation(&mut self, scope: &dyn ScopeCheck) -> Output {
        self.counters.escalations += 1;
        Output::Escalate(ScopedResend {
            scope: scope.scope(),
            from_position: scope.applied_prefix(),
        })
    }

    fn asks(&mut self, now_ms: u64) -> Vec<Output> {
        let mut due: Vec<(Position, u64, RecordKey)> = self
            .tracked
            .iter()
            .filter(|(_, t)| t.next_ask_ms <= now_ms)
            .map(|(k, t)| (t.position, t.seq, k.clone()))
            .collect();
        due.sort();
        let mut out = Vec::new();
        for chunk in due.chunks(self.cfg.max_keys) {
            let mut keys = Vec::with_capacity(chunk.len());
            for (_, _, key) in chunk {
                let asks = self.tracked.get(key).map_or(0, |t| t.asks) + 1;
                let wait = self.backoff(asks, &format!("{}/{}", key.entity, key.id));
                if let Some(t) = self.tracked.get_mut(key) {
                    t.asks = asks;
                    t.next_ask_ms = now_ms.saturating_add(wait);
                }
                keys.push(key.clone());
            }
            self.counters.keys_asked += keys.len() as u64;
            out.push(Output::Ask(PullRequest { keys }));
        }
        out
    }

    /// Wait after the `n`th ask (1-based): `ask_interval * 2^(n-1)` capped at `max_interval`,
    /// plus up to a quarter of that as seeded jitter, capped again.
    fn backoff(&self, n: u32, salt: &str) -> u64 {
        let shift = n.saturating_sub(1).min(32);
        let base = self
            .cfg
            .ask_interval_ms
            .saturating_mul(1u64 << shift)
            .min(self.cfg.max_interval_ms);
        let jitter = mix(self.cfg.jitter_seed, salt, n) % (base / 4 + 1);
        base.saturating_add(jitter).min(self.cfg.max_interval_ms)
    }
}

/// FNV-1a over the salt, then a splitmix64 finaliser: stable, seedable, no randomness source.
fn mix(seed: u64, salt: &str, n: u32) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325 ^ seed;
    for b in salt.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    let mut z = h.wrapping_add(u64::from(n).wrapping_mul(0x9e37_79b9_7f4a_7c15));
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}
