//! The node intake: receive, apply, park, retry, acknowledge (ADR-064 sections 6 and 7).

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use spot_sync_closure::{Bundle, Position, RecordKey};
use spot_sync_freshness::FreshnessTracker;
use spot_sync_skew::{HoldSet, NodeReport, RecordRef, triage};

use crate::error::NodeError;
use crate::model::{
    Applied, AppliedSet, ApplyRefusal, Counters, Delivery, NodeConfig, ParkedBundle, Receipt,
    RepairCause, RepairItem,
};
use crate::prefix::PrefixTracker;
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
    freshness: FreshnessTracker,
    holds: HoldSet,
}

impl TenantState {
    fn new() -> Self {
        Self {
            prefix: PrefixTracker::new(),
            freshness: FreshnessTracker::new(),
            holds: HoldSet::new(),
        }
    }
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
    unsettled: BTreeMap<(String, Position), Vec<(RecordKey, Position)>>,
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
            return Ok(Receipt::Applied(Applied::default()));
        };

        // Version skew: a record the node cannot decode is held and the bundle goes no further.
        let batch: Vec<_> = bundle
            .records
            .iter()
            .map(|r| self.catalog.classify(r))
            .collect();
        let verdict = triage(&batch, &self.report);
        if let Some(first_hold) = verdict.held.first() {
            let reason = first_hold.reason.clone();
            let state = self.state_mut(&tenant);
            for hold in verdict.held {
                state.holds.insert(hold);
            }
            state.prefix.settle(last);
            return Ok(Receipt::Held(reason));
        }

        match self.evaluate(&tenant, &bundle)? {
            Eval::Applied {
                set,
                duplicates,
                older_ignored,
            } => {
                self.state_mut(&tenant).prefix.settle(last);
                let released = self.wake(&tenant, &set.keys())?;
                Ok(Receipt::Applied(Applied {
                    applied: set,
                    duplicates,
                    older_ignored,
                    released,
                }))
            }
            Eval::Missing(missing) => Ok(self.park(tenant, bundle, missing, first, now_ms)),
            Eval::Refused(refusal) => {
                self.track_unsettled(&tenant, first, &bundle, false);
                Ok(Receipt::Refused(refusal))
            }
        }
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
            if self.apply.has(tenant, &record.key, record.position) {
                if self
                    .apply
                    .has(tenant, &record.key, record.position.saturating_add(1))
                {
                    older_ignored += 1;
                } else {
                    duplicates += 1;
                }
            } else {
                remaining.push(record.clone());
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

        let inside: BTreeSet<&RecordKey> = bundle.records.iter().map(|r| &r.key).collect();
        let mut missing = BTreeSet::new();
        for record in &remaining {
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
                if !inside.contains(&target) && !self.apply.has(tenant, &target, 0) {
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

    /// A bundle that is neither applied nor parked keeps (`held`: the prefix already counts it) holding the prefix at its first
    /// position until all its records are applied.
    fn track_unsettled(&mut self, tenant: &str, first: Position, bundle: &Bundle, held: bool) {
        let records = bundle
            .records
            .iter()
            .map(|r| (r.key.clone(), r.position))
            .collect();
        let fresh = self
            .unsettled
            .insert((tenant.to_owned(), first), records)
            .is_none();
        if fresh && !held {
            self.state_mut(tenant).prefix.hold(first);
        }
    }

    /// Release the hold of every repaired or refused bundle of `tenant` whose records are all
    /// applied for that tenant now. Another tenant's holds are never looked at.
    fn resolve_unsettled(&mut self, tenant: &str) {
        let done: Vec<(String, Position)> = self
            .unsettled
            .iter()
            .filter(|((t, _), records)| {
                t == tenant && records.iter().all(|(k, v)| self.apply.has(tenant, k, *v))
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in done {
            self.unsettled.remove(&id);
            self.repair
                .retain(|r| !(r.tenant == id.0 && r.first_position == id.1));
            self.state_mut(tenant).prefix.release(id.1);
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
        self.track_unsettled(&parked.tenant, parked.first_position, &parked.bundle, true);
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
                        work.extend(set.keys());
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

/// The lowest and highest record position in a bundle, or `None` when it is empty.
fn span(bundle: &Bundle) -> Option<(Position, Position)> {
    let first = bundle.records.iter().map(|r| r.position).min()?;
    let last = bundle.records.iter().map(|r| r.position).max()?;
    Some((first, last))
}

fn image_len(record: &spot_sync_closure::BundleRecord) -> usize {
    serde_json::to_string(&record.image).map_or(0, |s| s.len())
}
