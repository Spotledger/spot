//! The node intake: receive, apply, park, retry, acknowledge (ADR-064 sections 6 and 7).

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use spot_sync_closure::{Bundle, BundleRecord, Position, RecordKey};
use spot_sync_freshness::FreshnessTracker;
use spot_sync_skew::{HoldSet, NodeReport, RecordRef, triage};

use crate::error::NodeError;
use crate::model::{
    Applied, AppliedSet, ApplyRefusal, Counters, Delivery, GoneOutcome, MAX_GONE_PER_TENANT,
    NodeConfig, ParkedBundle, Receipt, RepairCause, RepairItem,
};
use crate::prefix::PrefixTracker;
use crate::restore::{RestoreError, RestoreState};
use crate::serve::{ServeStamp, ServeTracker};
use crate::traits::{BundleApply, NodeCatalog, ParkingStore};

/// What evaluating one bundle against the node's state produced.
enum Eval {
    Applied {
        set: AppliedSet,
        duplicates: usize,
        older_ignored: usize,
    },
    Missing(BTreeSet<RecordKey>),
    Refused(ApplyRefusal),
}

/// What the node keeps per tenant. Positions of two tenants overlap freely, so one tracker for
/// the node would let one tenant's settled positions advance another's acknowledged prefix.
struct TenantState {
    prefix: PrefixTracker,
    /// The serve-sequence prefix: what the hub is acknowledged by.
    serve: ServeTracker,
    freshness: FreshnessTracker,
    holds: HoldSet,
    /// Keys the hub answered `Gone` for: they exist no more, so a reference to one is not a
    /// miss. Bounded by `MAX_GONE_PER_TENANT`.
    gone: BTreeSet<RecordKey>,
}

impl TenantState {
    fn new() -> Self {
        Self {
            prefix: PrefixTracker::new(),
            serve: ServeTracker::new(),
            freshness: FreshnessTracker::new(),
            holds: HoldSet::new(),
            gone: BTreeSet::new(),
        }
    }
}

/// A bundle that left parking or was refused: its records, and the serve stamps of the bundles
/// that stand for it, which count into the serve prefix once every record is applied.
struct Unsettled {
    records: Vec<(RecordKey, Position)>,
    serves: Vec<ServeStamp>,
    /// Restored from durable state: the records are not known until the hub redelivers a bundle
    /// with this first position.
    restored: bool,
}

/// The receiving side of sync for one node and one hub.
pub struct NodeIntake<A: BundleApply, P: ParkingStore, C: NodeCatalog> {
    config: NodeConfig,
    apply: A,
    parking: P,
    catalog: C,
    report: NodeReport,
    /// Positions, freshness and skew holds, one set per tenant: each tenant has its own
    /// journal and position space (ADR-061 section 1), so nothing here is shared.
    tenants: BTreeMap<String, TenantState>,
    repair: VecDeque<RepairItem>,
    /// Bundles that left parking or were refused, by tenant and first position, with their
    /// records. They keep holding the prefix until every one of their records is applied (by a
    /// redelivery of the bundle or by any other bundle carrying them).
    unsettled: BTreeMap<(String, Position), Unsettled>,
    counters: Counters,
    next_seq: u64,
}

impl<A: BundleApply, P: ParkingStore, C: NodeCatalog> NodeIntake<A, P, C> {
    /// Build an intake.
    ///
    /// # Errors
    /// `InvalidLimits` when a parking limit or the tenant cap is zero.
    pub fn new(
        config: NodeConfig,
        apply: A,
        parking: P,
        catalog: C,
        report: NodeReport,
    ) -> Result<Self, NodeError> {
        config.limits.validate()?;
        if config.max_tenants == 0 {
            return Err(NodeError::InvalidLimits {
                reason: "the tenant cap must be above zero",
            });
        }
        Ok(Self {
            config,
            apply,
            parking,
            catalog,
            report,
            tenants: BTreeMap::new(),
            repair: VecDeque::new(),
            unsettled: BTreeMap::new(),
            counters: Counters::default(),
            next_seq: 0,
        })
    }

    /// Receive one bundle: apply it if its closure is complete, otherwise park it whole.
    ///
    /// `now_ms` is a monotonic millisecond count the caller reads from its own clock.
    ///
    /// # Errors
    /// `ForeignSource` when the delivery is not from the configured hub (nothing is parked),
    /// `TooManyTenants` when the delivery names a new tenant beyond the configured cap (nothing
    /// is stored), or `ReferenceUnreadable` when a record's references cannot be read.
    pub fn receive(&mut self, delivery: Delivery, now_ms: u64) -> Result<Receipt, NodeError> {
        self.receive_stamped(delivery, None, now_ms)
    }

    /// As [`receive`](Self::receive) for a bundle that carries the serve stamp the hub's queue
    /// gave it. The stamp counts into [`served_prefix`](Self::served_prefix) when the bundle is
    /// settled (applied, replayed or held for skew), immediately or later when a parked bundle is
    /// released; a parked, refused or repaired bundle holds the prefix back until then.
    ///
    /// # Errors
    /// As [`receive`](Self::receive).
    pub fn receive_served(
        &mut self,
        delivery: Delivery,
        stamp: ServeStamp,
        now_ms: u64,
    ) -> Result<Receipt, NodeError> {
        self.receive_stamped(delivery, Some(stamp), now_ms)
    }

    fn receive_stamped(
        &mut self,
        delivery: Delivery,
        serve: Option<ServeStamp>,
        now_ms: u64,
    ) -> Result<Receipt, NodeError> {
        if delivery.source != self.config.hub_source {
            return Err(NodeError::ForeignSource {
                source_id: delivery.source.as_str().to_owned(),
            });
        }
        let Delivery { tenant, bundle, .. } = delivery;
        if !self.tenants.contains_key(&tenant) {
            if self.tenants.len() >= self.config.max_tenants {
                return Err(NodeError::TooManyTenants {
                    limit: self.config.max_tenants,
                });
            }
            self.tenants.insert(tenant.clone(), TenantState::new());
        }
        let Some((first, last)) = span(&bundle) else {
            self.settle_serve(&tenant, serve);
            return Ok(Receipt::Applied(Applied::default()));
        };

        // Version skew: a record the node cannot decode is held and the bundle goes no further.
        // A tombstone has no image to decode, so it is never held for skew.
        let batch: Vec<_> = bundle
            .records
            .iter()
            .filter(|r| !r.deleted)
            .map(|r| self.catalog.classify(r))
            .collect();
        let verdict = triage(&batch, &self.report);
        if let Some(first_hold) = verdict.held.first() {
            let reason = first_hold.reason.clone();
            // A restored repair hold at this position is settled by a bundle held for skew too:
            // a held bundle never blocks the prefix.
            let restored = self.release_restored(&tenant, first);
            let state = self.state_mut(&tenant);
            for hold in verdict.held {
                state.holds.insert(hold);
            }
            if restored {
                state.prefix.release(first);
            }
            state.prefix.settle(last);
            if let Some(stamp) = serve {
                state.serve.settle(stamp);
            }
            return Ok(Receipt::Held(reason));
        }

        self.adopt_restored(&tenant, first, &bundle);
        match self.evaluate(&tenant, &bundle)? {
            Eval::Applied {
                set,
                duplicates,
                older_ignored,
            } => {
                self.state_mut(&tenant).prefix.settle(last);
                self.settle_serve(&tenant, serve);
                let released = self.wake(&tenant, &live_keys(&set, &bundle))?;
                Ok(Receipt::Applied(Applied {
                    applied: set,
                    duplicates,
                    older_ignored,
                    released,
                }))
            }
            Eval::Missing(missing) => Ok(self.park(tenant, bundle, missing, first, serve, now_ms)),
            Eval::Refused(refusal) => {
                self.track_unsettled(&tenant, first, &bundle, serve, false);
                Ok(Receipt::Refused(refusal))
            }
        }
    }

    /// Seed `tenant` from durable state after a restart; see [`RestoreState`]. Call it before the
    /// first delivery of the tenant, once.
    ///
    /// Restoring then receiving the hub's redelivery gives the same state as never having
    /// restarted: parked bundles hold the prefix at their first positions again, repair items hold
    /// it until a bundle with that first position applies, the saved holds are back and the
    /// serve prefix starts at the stored value.
    ///
    /// # Errors
    /// [`RestoreError::AlreadyStarted`] when the tenant has state in this intake,
    /// [`RestoreError::TooManyTenants`] past the cap, [`RestoreError::Invalid`] when a repair
    /// item names another tenant. The intake is unchanged then.
    pub fn restore(&mut self, tenant: &str, state: RestoreState) -> Result<(), RestoreError> {
        if self.tenants.contains_key(tenant) {
            return Err(RestoreError::AlreadyStarted);
        }
        if self.tenants.len() >= self.config.max_tenants {
            return Err(RestoreError::TooManyTenants);
        }
        if state.repair.iter().any(|item| item.tenant != tenant) {
            return Err(RestoreError::Invalid);
        }
        let mut tenant_state = TenantState::new();
        tenant_state.prefix = PrefixTracker::restored(state.applied_prefix);
        tenant_state.serve = ServeTracker::restored(state.served_prefix.unwrap_or(0));
        if let Some(holds) = state.holds {
            tenant_state.holds = holds;
        }
        for first in state.parked_first_positions {
            tenant_state.prefix.hold(first);
        }
        for item in state.repair {
            let id = (tenant.to_owned(), item.first_position);
            if self.unsettled.contains_key(&id) {
                continue;
            }
            tenant_state.prefix.hold(item.first_position);
            self.unsettled.insert(
                id,
                Unsettled {
                    records: Vec::new(),
                    serves: Vec::new(),
                    restored: true,
                },
            );
        }
        self.tenants.insert(tenant.to_owned(), tenant_state);
        Ok(())
    }

    /// A bundle received for a position a restored repair item holds: its records are what must
    /// be applied for the hold to go.
    fn adopt_restored(&mut self, tenant: &str, first: Position, bundle: &Bundle) {
        if let Some(entry) = self.unsettled.get_mut(&(tenant.to_owned(), first))
            && entry.restored
        {
            entry.records = bundle
                .records
                .iter()
                .map(|r| (r.key.clone(), r.position))
                .collect();
            entry.restored = false;
        }
    }

    /// Drops a restored hold entry at `first`; whether there was one (the caller releases it).
    fn release_restored(&mut self, tenant: &str, first: Position) -> bool {
        let id = (tenant.to_owned(), first);
        if self.unsettled.get(&id).is_some_and(|entry| entry.restored) {
            self.unsettled.remove(&id);
            return true;
        }
        false
    }

    /// Move every bundle parked longer than `max_age_ms` to the repair list.
    pub fn expire(&mut self, now_ms: u64) {
        let max_age = self.config.limits.max_age_ms;
        for parked in self.parking.expire(now_ms, max_age) {
            self.send_to_repair(parked, RepairCause::Expired);
        }
    }

    /// The highest position of the tenant's journal such that every bundle up to it is applied
    /// (or held for skew, which never blocks the prefix). Only this is acknowledged to the hub
    /// (ADR-065 section 1). `None` for a tenant that has settled nothing or is unknown.
    #[must_use]
    pub fn applied_prefix(&self, tenant: &str) -> Option<Position> {
        self.tenants.get(tenant)?.prefix.prefix()
    }

    /// The serve sequence the node acknowledges to the hub for `tenant`: the highest one such that
    /// every bundle the hub's queue held up to it is settled here (WP-SYN-11.T2). Only bundles
    /// received with [`receive_served`](Self::receive_served) count. `None` when nothing counts
    /// yet or the tenant is unknown.
    ///
    /// Unlike [`applied_prefix`](Self::applied_prefix) it cannot leap over a bundle the node was
    /// never sent: the stamps name what lies below each bundle. It is volatile (a restart starts
    /// again from 0 and the hub re-serves what it still queues).
    #[must_use]
    pub fn served_prefix(&self, tenant: &str) -> Option<u64> {
        self.tenants.get(tenant)?.serve.prefix()
    }

    /// How many tenants the node has taken a delivery for.
    #[must_use]
    pub fn tenant_count(&self) -> usize {
        self.tenants.len()
    }

    /// Feed the tenant's freshness tracker from the hub's class horizons: every class whose
    /// horizon the tenant's applied prefix covers is confirmed at `now_monotonic_ms`. Returns
    /// the classes confirmed. An unknown tenant confirms nothing and is not created.
    pub fn on_horizons(
        &mut self,
        tenant: &str,
        class_horizons: &BTreeMap<String, Position>,
        now_monotonic_ms: u64,
    ) -> Vec<String> {
        let Some(state) = self.tenants.get_mut(tenant) else {
            return Vec::new();
        };
        let Some(prefix) = state.prefix.prefix() else {
            return Vec::new();
        };
        class_horizons
            .iter()
            .filter(|(class, horizon)| {
                state
                    .freshness
                    .confirm_if_covered(class, prefix, **horizon, now_monotonic_ms)
            })
            .map(|(class, _)| class.clone())
            .collect()
    }

    /// Hand the repair list to the closure repair loop (WP-SYN-04.T2). The bundles stay
    /// unsettled, and keep holding the prefix, until a bundle with the same first position
    /// applies.
    pub fn take_repair(&mut self) -> Vec<RepairItem> {
        self.repair.drain(..).collect()
    }

    /// Put items back at the front of the repair list: a host that took them with
    /// [`take_repair`](Self::take_repair) and could not store them gives them back, so none is
    /// lost. The bundles they stand for are still unsettled; nothing else changes.
    pub fn requeue_repair(&mut self, items: Vec<RepairItem>) {
        for item in items.into_iter().rev() {
            self.repair.push_front(item);
        }
    }

    /// Entries waiting on the repair list.
    #[must_use]
    pub fn repair_len(&self) -> usize {
        self.repair.len()
    }

    /// The tenant's freshness tracker (read for staleness decisions and stamps), if the tenant
    /// is known.
    #[must_use]
    pub fn freshness(&self, tenant: &str) -> Option<&FreshnessTracker> {
        self.tenants.get(tenant).map(|t| &t.freshness)
    }

    /// The tenant's records held for version skew, one per record, if the tenant is known.
    #[must_use]
    pub fn holds(&self, tenant: &str) -> Option<&HoldSet> {
        self.tenants.get(tenant).map(|t| &t.holds)
    }

    /// Replace the node's own report (after an upgrade) so held records can be released.
    pub fn set_report(&mut self, report: NodeReport) {
        self.report = report;
    }

    /// The counters for the health surface.
    #[must_use]
    pub fn counters(&self) -> Counters {
        self.counters
    }

    /// The apply path (read access, for tests and status).
    #[must_use]
    pub fn applier(&self) -> &A {
        &self.apply
    }

    /// The apply path (mutable access, for tests that script the sink).
    pub fn applier_mut(&mut self) -> &mut A {
        &mut self.apply
    }

    /// The parking store (read access, for tests and status).
    #[must_use]
    pub fn parking(&self) -> &P {
        &self.parking
    }

    /// Check a bundle's references and apply it when the closure is complete.
    fn evaluate(&mut self, tenant: &str, bundle: &Bundle) -> Result<Eval, NodeError> {
        let mut duplicates = 0;
        let mut older_ignored = 0;
        let mut remaining = Vec::with_capacity(bundle.records.len());
        for record in &bundle.records {
            match self.replay_of(tenant, record) {
                Some(Replay::Duplicate) => duplicates += 1,
                Some(Replay::Older) => older_ignored += 1,
                None => remaining.push(record.clone()),
            }
        }
        self.counters.duplicates += duplicates as u64;
        self.counters.older_ignored += older_ignored as u64;
        if remaining.is_empty() {
            return Ok(Eval::Applied {
                set: AppliedSet::default(),
                duplicates,
                older_ignored,
            });
        }

        // A tombstone in the bundle does not satisfy a reference to its key: the record is gone.
        let inside: BTreeSet<&RecordKey> = bundle
            .records
            .iter()
            .filter(|r| !r.deleted)
            .map(|r| &r.key)
            .collect();
        let mut missing = BTreeSet::new();
        for record in remaining.iter().filter(|r| !r.deleted) {
            let refs = self.catalog.references(record).map_err(|e| match e {
                spot_sync_closure::ClosureError::ReferenceUnreadable { entity, field } => {
                    NodeError::ReferenceUnreadable { entity, field }
                }
                _ => NodeError::ReferenceUnreadable {
                    entity: record.key.entity.clone(),
                    field: "unknown".to_owned(),
                },
            })?;
            for target in refs {
                // A key that was deleted is not present, so a reference to it is a miss until
                // the hub answers `Gone` (`resolve_gone`): park, then release.
                if !inside.contains(&target)
                    && !self.apply.has(tenant, &target, 0)
                    && !self.is_gone(tenant, &target)
                {
                    missing.insert(target);
                }
            }
        }
        if !missing.is_empty() {
            return Ok(Eval::Missing(missing));
        }

        let to_apply = if remaining.len() == bundle.records.len() {
            bundle.clone()
        } else {
            let bytes = remaining.iter().map(image_len).sum();
            Bundle {
                kind: bundle.kind,
                records: remaining,
                bytes,
            }
        };
        match self.apply.apply_bundle(tenant, &to_apply) {
            Ok(set) => Ok(Eval::Applied {
                set,
                duplicates,
                older_ignored,
            }),
            Err(refusal) => Ok(Eval::Refused(refusal)),
        }
    }

    /// Whether the hub answered `Gone` for the key of this tenant.
    fn is_gone(&self, tenant: &str, key: &RecordKey) -> bool {
        self.tenants
            .get(tenant)
            .is_some_and(|state| state.gone.contains(key))
    }

    /// Whether the sink has this record (or a later word on it) already: a replay of the same
    /// version, or an old one a newer version or a later delete has superseded.
    fn replay_of(&self, tenant: &str, record: &BundleRecord) -> Option<Replay> {
        let key = &record.key;
        let at = record.position;
        let next = at.saturating_add(1);
        if record.deleted {
            // A delete is a replay when the same or a later delete was applied, and outdated when
            // the record was re-created at or after it.
            if self.apply.has_tombstone(tenant, key, at) {
                return Some(if self.apply.has_tombstone(tenant, key, next) {
                    Replay::Older
                } else {
                    Replay::Duplicate
                });
            }
            return self.apply.has(tenant, key, at).then_some(Replay::Older);
        }
        if self.apply.has(tenant, key, at) {
            return Some(if self.apply.has(tenant, key, next) {
                Replay::Older
            } else {
                Replay::Duplicate
            });
        }
        // A later delete supersedes this image: applying it would bring the record back.
        self.apply
            .has_tombstone(tenant, key, at)
            .then_some(Replay::Older)
    }

    /// The hub answered `Gone` for these keys of `tenant`: they exist no more. A bundle that was
    /// parked for one of them is no longer missing it and is applied now (the reference dangles by
    /// the record's own business); bundles that already left parking are released when the hub
    /// re-sends them. A tenant the node has no state for is not created. The keys are remembered
    /// per tenant up to `MAX_GONE_PER_TENANT`; the ones that did not fit come back in
    /// `GoneOutcome::not_recorded` and stay missing.
    ///
    /// # Errors
    /// `ReferenceUnreadable` when a released bundle's references cannot be read.
    pub fn resolve_gone(
        &mut self,
        tenant: &str,
        keys: &[RecordKey],
    ) -> Result<GoneOutcome, NodeError> {
        let mut recorded = Vec::new();
        let mut not_recorded = Vec::new();
        let Some(state) = self.tenants.get_mut(tenant) else {
            return Ok(GoneOutcome::default());
        };
        for key in keys {
            if state.gone.contains(key) || state.gone.len() < MAX_GONE_PER_TENANT {
                state.gone.insert(key.clone());
                recorded.push(key.clone());
            } else {
                not_recorded.push(key.clone());
            }
        }
        let released = self.wake(tenant, &recorded)?;
        Ok(GoneOutcome {
            released,
            not_recorded,
        })
    }

    /// A bundle that is neither applied nor parked keeps (`held`: the prefix already counts it) holding the prefix at its first
    /// position until all its records are applied. Its serve stamp, if it has one, is settled
    /// then too.
    fn track_unsettled(
        &mut self,
        tenant: &str,
        first: Position,
        bundle: &Bundle,
        serve: Option<ServeStamp>,
        held: bool,
    ) {
        let records: Vec<(RecordKey, Position)> = bundle
            .records
            .iter()
            .map(|r| (r.key.clone(), r.position))
            .collect();
        let id = (tenant.to_owned(), first);
        let fresh = match self.unsettled.get_mut(&id) {
            Some(entry) => {
                entry.records = records;
                entry.restored = false;
                if let Some(stamp) = serve
                    && !entry.serves.contains(&stamp)
                {
                    entry.serves.push(stamp);
                }
                false
            }
            None => {
                self.unsettled.insert(
                    id,
                    Unsettled {
                        records,
                        serves: serve.into_iter().collect(),
                        restored: false,
                    },
                );
                true
            }
        };
        if fresh && !held {
            self.state_mut(tenant).prefix.hold(first);
        }
    }

    /// Count a serve stamp into the tenant's serve prefix, if the bundle came with one.
    fn settle_serve(&mut self, tenant: &str, serve: Option<ServeStamp>) {
        if let Some(stamp) = serve {
            self.state_mut(tenant).serve.settle(stamp);
        }
    }

    /// Release the hold of every repaired or refused bundle of `tenant` whose records are all
    /// applied for that tenant now. Another tenant's holds are never looked at.
    fn resolve_unsettled(&mut self, tenant: &str) {
        let done: Vec<(String, Position)> = self
            .unsettled
            .iter()
            .filter(|((t, _), entry)| {
                t == tenant
                    && !entry.restored
                    && entry
                        .records
                        .iter()
                        .all(|(k, v)| self.apply.has(tenant, k, *v))
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in done {
            let entry = self.unsettled.remove(&id);
            self.repair
                .retain(|r| !(r.tenant == id.0 && r.first_position == id.1));
            let state = self.state_mut(tenant);
            state.prefix.release(id.1);
            for stamp in entry.into_iter().flat_map(|e| e.serves) {
                state.serve.settle(stamp);
            }
        }
    }

    /// The tenant's state, created if absent. The tenant cap is enforced in `receive`, the only
    /// way a new tenant enters; everything else works on a tenant `receive` already admitted.
    fn state_mut(&mut self, tenant: &str) -> &mut TenantState {
        self.tenants
            .entry(tenant.to_owned())
            .or_insert_with(TenantState::new)
    }

    /// Park a bundle whole, making room first. A bundle too large to park goes to repair.
    fn park(
        &mut self,
        tenant: String,
        bundle: Bundle,
        missing: BTreeSet<RecordKey>,
        first: Position,
        serve: Option<ServeStamp>,
        now_ms: u64,
    ) -> Receipt {
        let limits = self.config.limits;
        let bytes = bundle.records.iter().map(image_len).sum::<usize>();
        let parked = ParkedBundle {
            seq: self.next_seq,
            tenant: tenant.clone(),
            bundle,
            missing: missing.clone(),
            parked_at_ms: now_ms,
            first_position: first,
            bytes,
            serve,
        };
        self.next_seq += 1;
        self.state_mut(&tenant).prefix.hold(first);
        let list: Vec<RecordKey> = missing.into_iter().collect();
        if bytes > limits.max_bytes {
            self.send_to_repair(parked, RepairCause::Oversize);
            return Receipt::Overflow { missing: list };
        }
        while self.parking.len(&tenant) >= limits.max_bundles
            || self.parking.bytes(&tenant).saturating_add(bytes) > limits.max_bytes
        {
            match self.parking.take_oldest(&tenant) {
                Some(old) => {
                    self.counters.dropped_by_bound += 1;
                    self.send_to_repair(old, RepairCause::Bound);
                }
                None => break,
            }
        }
        self.parking.park(parked);
        Receipt::Parked { missing: list }
    }

    /// A parked bundle leaves parking unapplied: it stays unsettled and goes to the repair list.
    fn send_to_repair(&mut self, parked: ParkedBundle, cause: RepairCause) {
        self.counters.to_repair += 1;
        self.track_unsettled(
            &parked.tenant,
            parked.first_position,
            &parked.bundle,
            parked.serve,
            true,
        );
        self.repair.push_back(RepairItem {
            tenant: parked.tenant,
            first_position: parked.first_position,
            missing: parked.missing.into_iter().collect(),
            cause,
        });
    }

    /// After records were applied, retry the tenant's parked bundles in arrival order until
    /// nothing more becomes ready.
    fn wake(&mut self, tenant: &str, applied: &[RecordKey]) -> Result<Vec<AppliedSet>, NodeError> {
        self.resolve_unsettled(tenant);
        let mut released = Vec::new();
        let mut work: Vec<RecordKey> = applied.to_vec();
        while !work.is_empty() {
            let ready = self.parking.ready(tenant, &work);
            work = Vec::new();
            for mut parked in ready {
                match self.evaluate(tenant, &parked.bundle)? {
                    Eval::Applied { set, .. } => {
                        let state = self.state_mut(tenant);
                        state.prefix.release(parked.first_position);
                        if let Some((_, last)) = span(&parked.bundle) {
                            state.prefix.settle(last);
                        }
                        if let Some(stamp) = parked.serve {
                            state.serve.settle(stamp);
                        }
                        work.extend(live_keys(&set, &parked.bundle));
                        released.push(set);
                        self.resolve_unsettled(tenant);
                    }
                    Eval::Missing(missing) => {
                        parked.missing = missing;
                        self.parking.park(parked);
                    }
                    Eval::Refused(_) => self.send_to_repair(parked, RepairCause::Refused),
                }
            }
        }
        Ok(released)
    }

    /// Release the tenant's holds for records the node can now decode; returns them for
    /// re-request by id. An unknown tenant has none.
    pub fn release_holds(&mut self, tenant: &str) -> Vec<RecordRef> {
        let Some(state) = self.tenants.get_mut(tenant) else {
            return Vec::new();
        };
        let holds = state.holds.holds();
        let outcome = spot_sync_skew::release_holds(&holds, &self.report);
        let mut records = Vec::new();
        for hold in outcome.released {
            state.holds.applied(&hold.record);
            records.push(hold.record);
        }
        records
    }
}

/// How a record the sink already knows about relates to what it holds.
enum Replay {
    /// The same version (or the same delete) again.
    Duplicate,
    /// A newer version or a later delete is applied already.
    Older,
}

/// The keys of an applied set that are present now: a tombstone satisfies no reference, so it
/// wakes nothing.
fn live_keys(set: &AppliedSet, bundle: &Bundle) -> Vec<RecordKey> {
    let deleted: BTreeSet<&RecordKey> = bundle
        .records
        .iter()
        .filter(|r| r.deleted)
        .map(|r| &r.key)
        .collect();
    set.records
        .iter()
        .filter(|r| !deleted.contains(&r.key))
        .map(|r| r.key.clone())
        .collect()
}

/// The lowest and highest record position in a bundle, or `None` when it is empty.
fn span(bundle: &Bundle) -> Option<(Position, Position)> {
    let first = bundle.records.iter().map(|r| r.position).min()?;
    let last = bundle.records.iter().map(|r| r.position).max()?;
    Some((first, last))
}

fn image_len(record: &BundleRecord) -> usize {
    if record.deleted {
        return 0;
    }
    serde_json::to_string(&record.image).map_or(0, |s| s.len())
}
