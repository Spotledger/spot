//! The deterministic state machine. State is kept per tenant: a key missing for two tenants is
//! two tracked keys, and nothing a tenant does (arrivals, misses, re-sends, conditions) touches
//! another tenant's state. Only the memory bound is shared, per node.

use std::collections::BTreeMap;

use spot_sync_node::{Position, RecordKey};

use crate::config::RepairConfig;
use crate::error::RepairError;
use crate::model::{
    CLOSURE_UNRESOLVED, ConditionClear, ConditionRaise, Counters, Input, Output, PullRequest,
    ScopeCheck, ScopedResend,
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

/// Everything the loop remembers about one tenant.
#[derive(Debug, Default)]
struct TenantState {
    tracked: BTreeMap<RecordKey, Tracked>,
    /// Tracked keys by arrival sequence (oldest first).
    order: BTreeMap<u64, RecordKey>,
    /// Out-of-scope keys already reported, bounded by `max_out_of_scope`.
    out_of_scope: BTreeMap<RecordKey, u64>,
    out_of_scope_order: BTreeMap<u64, RecordKey>,
    raised: bool,
    resend: Option<Resend>,
    /// A key was dropped by the bound since the last escalation: the next tick escalates.
    force_resend: bool,
}

impl TenantState {
    /// Nothing worth remembering: the state can be discarded.
    fn is_idle(&self) -> bool {
        self.tracked.is_empty()
            && self.out_of_scope.is_empty()
            && !self.raised
            && self.resend.is_none()
            && !self.force_resend
    }

    /// Stops tracking `key`; whether it was tracked.
    fn untrack(&mut self, key: &RecordKey) -> bool {
        match self.tracked.remove(key) {
            Some(t) => {
                self.order.remove(&t.seq);
                true
            }
            None => false,
        }
    }

    fn forget_out_of_scope(&mut self, key: &RecordKey) {
        if let Some(seq) = self.out_of_scope.remove(key) {
            self.out_of_scope_order.remove(&seq);
        }
    }
}

/// The repair loop of one node, for all the tenants it serves.
#[derive(Debug)]
pub struct RepairLoop {
    subject: String,
    cfg: RepairConfig,
    tenants: BTreeMap<String, TenantState>,
    /// Tracked keys over all tenants (bounded by `max_tracked`).
    total: usize,
    next_seq: u64,
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
            tenants: BTreeMap::new(),
            total: 0,
            next_seq: 0,
            counters: Counters::default(),
        })
    }

    /// Keys currently missing and tracked, over all tenants.
    #[must_use]
    pub fn tracked_len(&self) -> usize {
        self.total
    }

    /// Keys currently missing and tracked for `tenant`.
    #[must_use]
    pub fn tracked_len_for(&self, tenant: &str) -> usize {
        self.tenants.get(tenant).map_or(0, |s| s.tracked.len())
    }

    /// Out-of-scope keys remembered (already reported) for `tenant`.
    #[must_use]
    pub fn out_of_scope_len(&self, tenant: &str) -> usize {
        self.tenants.get(tenant).map_or(0, |s| s.out_of_scope.len())
    }

    /// Tenants the loop holds state for.
    #[must_use]
    pub fn tenant_count(&self) -> usize {
        self.tenants.len()
    }

    /// Whether a scoped re-send is outstanding for `tenant` (its single asks are suppressed).
    #[must_use]
    pub fn resend_pending(&self, tenant: &str) -> bool {
        self.tenants.get(tenant).is_some_and(|s| s.resend.is_some())
    }

    /// Whether `CLOSURE_UNRESOLVED` is currently raised for `tenant`.
    #[must_use]
    pub fn condition_raised(&self, tenant: &str) -> bool {
        self.tenants.get(tenant).is_some_and(|s| s.raised)
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
                tenant,
                keys,
                bundle_position,
                now_ms,
            } => self.missed(&tenant, keys, bundle_position, now_ms, scope),
            Input::Arrived { tenant, keys } => self.arrived(&tenant, &keys),
            Input::Gone { tenant, keys } => self.gone(&tenant, &keys),
            Input::ResendComplete {
                tenant,
                scope: name,
            } => self.resend_complete(&tenant, &name, scope),
            Input::Tick(now_ms) => self.tick(now_ms, scope),
        };
        if out.is_empty() {
            out.push(Output::Nothing);
        }
        out
    }

    fn missed(
        &mut self,
        tenant: &str,
        keys: Vec<RecordKey>,
        position: Position,
        now_ms: u64,
        scope: &dyn ScopeCheck,
    ) -> Vec<Output> {
        if keys.is_empty() {
            return Vec::new();
        }
        if !self.tenants.contains_key(tenant) && self.tenants.len() >= self.cfg.max_tenants {
            self.counters.tenants_refused += 1;
            return vec![Output::TenantRefused(tenant.to_owned())];
        }
        let mut st = self.tenants.remove(tenant).unwrap_or_default();
        let mut foreign = Vec::new();
        for key in keys {
            if let Some(t) = st.tracked.get_mut(&key) {
                t.position = t.position.min(position);
                self.counters.duplicate_misses += 1;
            } else if scope.in_scope(tenant, &key) {
                st.forget_out_of_scope(&key);
                self.insert(&mut st, key, position, now_ms);
            } else if self.note_out_of_scope(&mut st, &key) {
                foreign.push(key);
            }
        }
        self.keep(tenant, st);
        self.enforce_bound();
        if foreign.is_empty() {
            Vec::new()
        } else {
            vec![Output::OutOfScope {
                tenant: tenant.to_owned(),
                keys: foreign,
            }]
        }
    }

    /// Puts the tenant's state back unless there is nothing left to remember.
    fn keep(&mut self, tenant: &str, st: TenantState) {
        if !st.is_idle() {
            self.tenants.insert(tenant.to_owned(), st);
        }
    }

    fn insert(&mut self, st: &mut TenantState, key: RecordKey, position: Position, now_ms: u64) {
        let seq = self.next_seq;
        self.next_seq += 1;
        st.order.insert(seq, key.clone());
        st.tracked.insert(
            key,
            Tracked {
                seq,
                position,
                first_missed_ms: now_ms,
                asks: 0,
                next_ask_ms: now_ms,
            },
        );
        self.total += 1;
    }

    /// Remembers an out-of-scope key; whether it is new (to be reported). The set is bounded:
    /// beyond `max_out_of_scope` the oldest is forgotten (counted) and may be reported again.
    fn note_out_of_scope(&mut self, st: &mut TenantState, key: &RecordKey) -> bool {
        if st.out_of_scope.contains_key(key) {
            self.counters.out_of_scope_repeats += 1;
            return false;
        }
        let seq = self.next_seq;
        self.next_seq += 1;
        st.out_of_scope_order.insert(seq, key.clone());
        st.out_of_scope.insert(key.clone(), seq);
        while st.out_of_scope.len() > self.cfg.max_out_of_scope {
            let Some((_, oldest)) = st.out_of_scope_order.pop_first() else {
                break;
            };
            st.out_of_scope.remove(&oldest);
            self.counters.out_of_scope_forgotten += 1;
        }
        self.counters.out_of_scope += 1;
        true
    }

    /// While more than `max_tracked` keys are tracked: drops the oldest key of the tenant with
    /// the most tracked keys (ties: the first tenant by name), so a tenant that floods the node
    /// loses its own keys first and cannot starve another tenant's slots. That tenant escalates.
    fn enforce_bound(&mut self) {
        while self.total > self.cfg.max_tracked {
            let mut pick: Option<(&String, usize)> = None;
            for (name, st) in &self.tenants {
                let n = st.tracked.len();
                if n > 0 && pick.is_none_or(|(_, best)| n > best) {
                    pick = Some((name, n));
                }
            }
            let Some((name, _)) = pick else {
                break;
            };
            let name = name.clone();
            let Some(st) = self.tenants.get_mut(&name) else {
                break;
            };
            let Some((_, oldest)) = st.order.pop_first() else {
                break;
            };
            st.tracked.remove(&oldest);
            st.force_resend = true;
            self.total = self.total.saturating_sub(1);
            self.counters.dropped_by_bound += 1;
        }
    }

    fn arrived(&mut self, tenant: &str, keys: &[RecordKey]) -> Vec<Output> {
        let Some(mut st) = self.tenants.remove(tenant) else {
            return Vec::new();
        };
        for key in keys {
            if st.untrack(key) {
                self.total = self.total.saturating_sub(1);
            }
            st.forget_out_of_scope(key);
        }
        let out = self.settle_if_empty(tenant, &mut st);
        self.keep(tenant, st);
        out
    }

    /// The hub answered `Gone` for `keys`: the records exist no more, which resolves a miss as an
    /// arrival does. Only the keys that were tracked are reported (a key nobody missed is not).
    fn gone(&mut self, tenant: &str, keys: &[RecordKey]) -> Vec<Output> {
        let Some(mut st) = self.tenants.remove(tenant) else {
            return Vec::new();
        };
        let mut resolved = Vec::new();
        for key in keys {
            if st.untrack(key) {
                self.total = self.total.saturating_sub(1);
                self.counters.gone += 1;
                resolved.push(key.clone());
            }
            st.forget_out_of_scope(key);
        }
        let mut out = Vec::new();
        if !resolved.is_empty() {
            out.push(Output::Gone {
                tenant: tenant.to_owned(),
                keys: resolved,
            });
        }
        out.extend(self.settle_if_empty(tenant, &mut st));
        self.keep(tenant, st);
        out
    }

    /// The hub finished a scoped re-send of `name` for `tenant`. Keys still missing are asked
    /// for again one by one; the escalation state resets. A drop by the bound that happened
    /// after the re-send was emitted is not covered by it, so `force_resend` is left alone.
    fn resend_complete(&mut self, tenant: &str, name: &str, scope: &dyn ScopeCheck) -> Vec<Output> {
        let Some(mut st) = self.tenants.remove(tenant) else {
            self.counters.resends_ignored += 1;
            return Vec::new();
        };
        if st.resend.is_none() || scope.scope(tenant) != name {
            self.counters.resends_ignored += 1;
            self.keep(tenant, st);
            return Vec::new();
        }
        st.resend = None;
        for t in st.tracked.values_mut() {
            t.asks = 0;
            t.next_ask_ms = 0;
        }
        self.counters.resends_completed += 1;
        let out = self.settle_if_empty(tenant, &mut st);
        self.keep(tenant, st);
        out
    }

    /// Resets the resend and clears the condition once nothing is missing (and no dropped key
    /// still awaits its escalation).
    fn settle_if_empty(&self, tenant: &str, st: &mut TenantState) -> Vec<Output> {
        if !st.tracked.is_empty() || st.force_resend {
            return Vec::new();
        }
        st.resend = None;
        if st.raised {
            st.raised = false;
            return vec![Output::Clear(ConditionClear {
                subject: self.subject.clone(),
                tenant: tenant.to_owned(),
            })];
        }
        Vec::new()
    }

    fn tick(&mut self, now_ms: u64, scope: &dyn ScopeCheck) -> Vec<Output> {
        let names: Vec<String> = self.tenants.keys().cloned().collect();
        let mut out = Vec::new();
        for name in names {
            let Some(mut st) = self.tenants.remove(&name) else {
                continue;
            };
            self.tick_tenant(&name, &mut st, now_ms, scope, &mut out);
            self.keep(&name, st);
        }
        out
    }

    fn tick_tenant(
        &mut self,
        tenant: &str,
        st: &mut TenantState,
        now_ms: u64,
        scope: &dyn ScopeCheck,
        out: &mut Vec<Output>,
    ) {
        // A scope that narrowed since the miss: those keys can never be served.
        let gone: Vec<RecordKey> = st
            .tracked
            .keys()
            .filter(|k| !scope.in_scope(tenant, k))
            .cloned()
            .collect();
        let mut reported = Vec::new();
        for key in gone {
            if st.untrack(&key) {
                self.total = self.total.saturating_sub(1);
            }
            if self.note_out_of_scope(st, &key) {
                reported.push(key);
            }
        }
        if !reported.is_empty() {
            out.push(Output::OutOfScope {
                tenant: tenant.to_owned(),
                keys: reported,
            });
        }
        out.extend(self.settle_if_empty(tenant, st));
        if st.tracked.is_empty() && !st.force_resend {
            return;
        }

        let exhausted = st
            .tracked
            .values()
            .any(|t| t.asks >= self.cfg.max_asks_per_key && t.next_ask_ms <= now_ms);
        match st.resend {
            Some(r) => {
                if st.force_resend || now_ms >= r.next_ms {
                    st.force_resend = false;
                    let count = r.count.saturating_add(1);
                    st.resend = Some(Resend {
                        count,
                        next_ms: now_ms.saturating_add(self.backoff(count, "resend")),
                    });
                    out.push(self.escalation(tenant, scope));
                    if st.tracked.is_empty() {
                        st.resend = None;
                    }
                }
            }
            None => {
                if st.force_resend || st.tracked.len() > self.cfg.escalate_keys || exhausted {
                    st.force_resend = false;
                    st.resend = Some(Resend {
                        count: 1,
                        next_ms: now_ms.saturating_add(self.backoff(1, "resend")),
                    });
                    out.push(self.escalation(tenant, scope));
                    if st.tracked.is_empty() {
                        st.resend = None;
                    }
                } else {
                    out.extend(self.asks(tenant, st, now_ms));
                }
            }
        }

        if !st.raised
            && st
                .tracked
                .values()
                .any(|t| now_ms.saturating_sub(t.first_missed_ms) >= self.cfg.raise_after_ms)
        {
            st.raised = true;
            out.push(Output::Raise(ConditionRaise {
                code: CLOSURE_UNRESOLVED.to_owned(),
                subject: self.subject.clone(),
                tenant: tenant.to_owned(),
                missing: st.tracked.len(),
            }));
        }
    }

    fn escalation(&mut self, tenant: &str, scope: &dyn ScopeCheck) -> Output {
        self.counters.escalations += 1;
        Output::Escalate(ScopedResend {
            tenant: tenant.to_owned(),
            scope: scope.scope(tenant),
            from_position: scope.applied_prefix(tenant),
        })
    }

    fn asks(&mut self, tenant: &str, st: &mut TenantState, now_ms: u64) -> Vec<Output> {
        let mut due: Vec<(Position, u64, RecordKey)> = st
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
                let asks = st.tracked.get(key).map_or(0, |t| t.asks) + 1;
                let wait = self.backoff(asks, &format!("{}/{}", key.entity, key.id));
                if let Some(t) = st.tracked.get_mut(key) {
                    t.asks = asks;
                    t.next_ask_ms = now_ms.saturating_add(wait);
                }
                keys.push(key.clone());
            }
            self.counters.keys_asked += keys.len() as u64;
            out.push(Output::Ask(PullRequest {
                tenant: tenant.to_owned(),
                keys,
            }));
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
