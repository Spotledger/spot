//! Period totals: the combined view of one tenant's meter for one window.

use std::collections::BTreeMap;

use crate::meter::{MemberDigest, Meter};

/// A store's combined view of one tenant-meter-period.
///
/// `sum` is the counter total, `max` the gauge reading, `distinct` the distinct-set
/// population; exactly the dimension the meter's [`crate::meter::MeterKind`] names is
/// filled, the others stay at their zero values. Keeping all three in one struct lets
/// a quota evaluate any meter without a kind dispatch on the read path.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PeriodTotals {
    /// The counter sum (counters only).
    pub sum: u64,
    /// The maximum observed reading (gauges only).
    pub max: u64,
    /// The distinct member count (distinct sets only).
    pub distinct: u64,
}

impl PeriodTotals {
    /// The total in the dimension the meter's kind measures.
    #[must_use]
    pub fn for_meter(&self, meter: &Meter) -> u64 {
        match meter.kind() {
            crate::meter::MeterKind::Counter => self.sum,
            crate::meter::MeterKind::Gauge => self.max,
            crate::meter::MeterKind::DistinctSet => self.distinct,
        }
    }
}

/// The per-meter totals of one tenant-period: the shape `MeterStore::period_totals`
/// answers with, and what a [`crate::policy::QuotaPolicy`] reads.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TenantPeriodUsage {
    /// One entry per meter the store has seen events for.
    pub by_meter: BTreeMap<Meter, PeriodTotals>,
    /// One sorted digest list per distinct-set meter. Reconciliation needs the
    /// population, not just the count, to name *which* members differ; a store that
    /// cannot cheaply enumerate members may leave a meter out, which reconciliation
    /// then reports as "population unknown" rather than pretending agreement.
    pub member_sets: BTreeMap<Meter, Vec<MemberDigest>>,
}

impl TenantPeriodUsage {
    /// The total for one meter, zero when the store has seen nothing for it.
    #[must_use]
    pub fn total(&self, meter: &Meter) -> u64 {
        self.by_meter
            .get(meter)
            .map_or(0, |totals| totals.for_meter(meter))
    }

    /// The member digests of one distinct-set meter, sorted; empty otherwise.
    #[must_use]
    pub fn members_of(&self, meter: &Meter) -> Vec<MemberDigest> {
        self.member_sets.get(meter).cloned().unwrap_or_default()
    }

    /// Records one meter's totals; builders keep chaining.
    #[must_use]
    pub fn with_totals(mut self, meter: Meter, totals: PeriodTotals) -> Self {
        self.by_meter.insert(meter, totals);
        self
    }

    /// Records one distinct-set meter's member population (and its count).
    #[must_use]
    pub fn with_members(mut self, meter: Meter, mut members: Vec<MemberDigest>) -> Self {
        members.sort();
        members.dedup();
        let distinct = members.len() as u64;
        self.member_sets.insert(meter.clone(), members);
        self.by_meter.entry(meter).or_default().distinct = distinct;
        self
    }
}
