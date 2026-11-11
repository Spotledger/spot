//! Reconciliation: does the meter agree with an independently computed truth?
//!
//! Metering is only trustworthy if it can be checked against something the metering
//! path did not produce. [`TruthSource`] is that something: in tests a ground-truth
//! tally built from the same facts by a different route, in production a count of
//! real rows (a later task points it at the database). [`reconcile`] compares each
//! meter's store total with the truth and lists every difference — never a silent
//! "all good".
//!
//! The acceptance property lives in the crate's tests: 10,000 randomly generated
//! events with duplicates, reorderings and dropped-then-retried batches produce totals
//! equal to the ground truth exactly.

use std::collections::BTreeMap;

use crate::error::MeteringError;
use crate::meter::{MemberDigest, Meter};
use crate::period::{PeriodKey, PeriodResolver};
use crate::store::MeterStore;
use crate::totals::TenantPeriodUsage;

/// The independently computed usage a reconciliation compares against.
///
/// Implementations must not read the same store the metering path wrote: the whole
/// value of a reconciliation is that the two numbers were produced by different
/// routes. A test's ground truth folds a batch directly; production counts rows.
pub trait TruthSource: Send + Sync {
    /// The truth for one tenant-period, per meter.
    fn truth<'a>(
        &'a self,
        tenant: &'a str,
        period: &'a PeriodKey,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<TenantPeriodUsage, MeteringError>> + Send + 'a>,
    >;
}

/// One meter's comparison: expected (truth), observed (store), and the difference.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReconcileRow {
    /// The meter compared.
    pub meter: Meter,
    /// The truth's total.
    pub expected: u64,
    /// The store's total.
    pub observed: u64,
    /// Members the truth has but the store does not (distinct-set meters only).
    pub missing_members: Vec<MemberDigest>,
    /// Members the store has but the truth does not (distinct-set meters only).
    pub extra_members: Vec<MemberDigest>,
}

impl ReconcileRow {
    /// Whether this row agrees: same total and, for a distinct set, same population.
    #[must_use]
    pub fn agrees(&self) -> bool {
        self.expected == self.observed
            && self.missing_members.is_empty()
            && self.extra_members.is_empty()
    }

    /// The signed difference `observed - expected`, for reports.
    #[must_use]
    pub fn delta(&self) -> i128 {
        i128::from(self.observed) - i128::from(self.expected)
    }
}

/// The result of one reconciliation: every meter, and the dropped-event count.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReconcileReport {
    /// The period reconciled.
    pub period: PeriodKey,
    /// One row per meter either side named.
    pub rows: Vec<ReconcileRow>,
    /// Events the sink dropped since the last reconciliation; never silently lost —
    /// a non-zero value explains a shortfall the rows may show.
    pub dropped: u64,
}

impl ReconcileReport {
    /// Whether every row agrees and nothing was dropped.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.dropped == 0 && self.rows.iter().all(ReconcileRow::agrees)
    }

    /// The rows that do not agree.
    #[must_use]
    pub fn disagreements(&self) -> Vec<&ReconcileRow> {
        self.rows.iter().filter(|row| !row.agrees()).collect()
    }
}

/// Compares `store`'s totals with `truth` for one tenant-period.
///
/// Every meter either side names is compared (a meter only one side knows is still a
/// difference: expected 0 or observed 0). For distinct-set meters the populations are
/// compared member by member when both sides report them; a side that reports no
/// population (a store that cannot enumerate members) yields no member rows, and the
/// count comparison still stands.
///
/// # Errors
/// Any store or truth error.
pub async fn reconcile(
    store: &dyn MeterStore,
    truth: &dyn TruthSource,
    tenant: &str,
    period: &PeriodKey,
    resolver: &dyn PeriodResolver,
    dropped: u64,
) -> Result<ReconcileReport, MeteringError> {
    let mut observed = store.period_totals(tenant, period, resolver).await?;
    let expected = truth.truth(tenant, period).await?;
    // The store contract exposes members separately; a backend need not inline them
    // into its totals query. Count-only truth still compares counts, not populations.
    for meter in expected.member_sets.keys() {
        if meter.kind().keeps_members() {
            let members = store
                .distinct_members(tenant, period, meter, resolver)
                .await?;
            observed.member_sets.insert(meter.clone(), members);
        }
    }

    let mut meters: Vec<Meter> = observed.by_meter.keys().cloned().collect();
    for meter in expected.by_meter.keys() {
        if !meters.contains(meter) {
            meters.push(meter.clone());
        }
    }
    meters.sort();

    let mut rows = Vec::with_capacity(meters.len());
    for meter in meters {
        let observed_total = observed.total(&meter);
        let expected_total = expected.total(&meter);
        let (missing_members, extra_members) = member_diff(&observed, &expected, &meter);
        rows.push(ReconcileRow {
            meter,
            expected: expected_total,
            observed: observed_total,
            missing_members,
            extra_members,
        });
    }

    Ok(ReconcileReport {
        period: period.clone(),
        rows,
        dropped,
    })
}

/// The member population difference for one meter, both directions sorted.
fn member_diff(
    observed: &TenantPeriodUsage,
    expected: &TenantPeriodUsage,
    meter: &Meter,
) -> (Vec<MemberDigest>, Vec<MemberDigest>) {
    if !meter.kind().keeps_members() || !expected.member_sets.contains_key(meter) {
        return (Vec::new(), Vec::new());
    }
    let observed_members = observed.members_of(meter);
    let expected_members = expected.members_of(meter);
    if observed_members.is_empty() && expected_members.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let missing = expected_members
        .iter()
        .filter(|member| !observed_members.contains(member))
        .cloned()
        .collect();
    let extra = observed_members
        .iter()
        .filter(|member| !expected_members.contains(member))
        .cloned()
        .collect();
    (missing, extra)
}

/// A [`TruthSource`] that folds a fixed set of facts directly, the shape a test or a
/// row-counting production source fills.
///
/// It is deliberately independent of [`MeterStore`]: the same facts, combined by
/// different code, so agreement is evidence rather than tautology.
#[derive(Clone, Debug, Default)]
pub struct TallyTruth {
    by_period: BTreeMap<(String, PeriodKey), TenantPeriodUsage>,
}

impl TallyTruth {
    /// An empty truth.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records one meter's total for a tenant-period; builders keep chaining.
    #[must_use]
    pub fn with_total(
        mut self,
        tenant: impl Into<String>,
        period: &PeriodKey,
        meter: Meter,
        total: u64,
    ) -> Self {
        let key = (tenant.into(), period.clone());
        let entry = self.by_period.entry(key).or_default();
        let totals = entry.by_meter.entry(meter.clone()).or_default();
        match meter.kind() {
            crate::meter::MeterKind::Counter => totals.sum = total,
            crate::meter::MeterKind::Gauge => totals.max = total,
            crate::meter::MeterKind::DistinctSet => totals.distinct = total,
        }
        self
    }

    /// Records one distinct-set meter's members; builders keep chaining.
    #[must_use]
    pub fn with_members(
        mut self,
        tenant: impl Into<String>,
        period: &PeriodKey,
        meter: Meter,
        members: Vec<MemberDigest>,
    ) -> Self {
        let key = (tenant.into(), period.clone());
        let entry = self.by_period.entry(key).or_default();
        *entry = entry.clone().with_members(meter, members);
        self
    }
}

impl TruthSource for TallyTruth {
    fn truth<'a>(
        &'a self,
        tenant: &'a str,
        period: &'a PeriodKey,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<TenantPeriodUsage, MeteringError>> + Send + 'a>,
    > {
        let usage = self
            .by_period
            .get(&(tenant.to_owned(), period.clone()))
            .cloned()
            .unwrap_or_default();
        // Recompute distinct counts from members when present, so a member list and a
        // count cannot disagree in the truth.
        let mut usage = usage;
        for (meter, members) in &usage.member_sets {
            if let Some(totals) = usage.by_meter.get_mut(meter) {
                totals.distinct = members.len() as u64;
            }
        }
        Box::pin(async move { Ok(usage) })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::meter::MemberDigest;
    use crate::period::{PeriodSpec, period_of};

    fn a_period() -> PeriodKey {
        let spec = PeriodSpec::calendar_month("UTC").unwrap();
        period_of(0, &spec).unwrap()
    }

    #[tokio::test]
    async fn a_clean_reconciliation_reports_no_disagreement() {
        use crate::event::MeterEvent;
        use crate::period::TenantPeriods;
        use crate::store::{MemoryMeterStore, MeterStore as _};

        let store = MemoryMeterStore::new("acme");
        let resolver = TenantPeriods::new(PeriodSpec::calendar_month("UTC").unwrap());
        store
            .add_events(
                vec![MeterEvent::amount("d1", "acme", Meter::Transactions, 7, 0)],
                &resolver,
            )
            .await
            .unwrap();
        let truth = TallyTruth::new().with_total("acme", &a_period(), Meter::Transactions, 7);
        let report = reconcile(&store, &truth, "acme", &a_period(), &resolver, 0)
            .await
            .unwrap();
        assert!(report.is_clean());
        assert!(report.disagreements().is_empty());
    }

    #[tokio::test]
    async fn a_shortfall_is_reported_with_the_meter_and_the_delta() {
        use crate::event::MeterEvent;
        use crate::period::TenantPeriods;
        use crate::store::MemoryMeterStore;

        let store = MemoryMeterStore::new("acme");
        let resolver = TenantPeriods::new(PeriodSpec::calendar_month("UTC").unwrap());
        store
            .add_events(
                vec![MeterEvent::amount("d1", "acme", Meter::Transactions, 7, 0)],
                &resolver,
            )
            .await
            .unwrap();
        // The truth says 10; the store observed 7. A dropped batch explains it.
        let truth = TallyTruth::new().with_total("acme", &a_period(), Meter::Transactions, 10);
        let report = reconcile(&store, &truth, "acme", &a_period(), &resolver, 3)
            .await
            .unwrap();
        assert!(!report.is_clean());
        assert_eq!(report.dropped, 3);
        let row = &report.disagreements()[0];
        assert_eq!(row.meter, Meter::Transactions);
        assert_eq!(row.expected, 10);
        assert_eq!(row.observed, 7);
        assert_eq!(row.delta(), -3);
    }

    #[tokio::test]
    async fn member_differences_are_named_in_both_directions() {
        use crate::period::TenantPeriods;
        use crate::store::MemoryMeterStore;

        let store = MemoryMeterStore::new("acme");
        let resolver = TenantPeriods::new(PeriodSpec::calendar_month("UTC").unwrap());
        // The store has no events; the truth names two members. Both are missing.
        let truth = TallyTruth::new().with_members(
            "acme",
            &a_period(),
            Meter::ActiveUsers,
            vec![MemberDigest::new("u1"), MemberDigest::new("u2")],
        );
        let report = reconcile(&store, &truth, "acme", &a_period(), &resolver, 0)
            .await
            .unwrap();
        let row = &report.disagreements()[0];
        assert_eq!(row.meter, Meter::ActiveUsers);
        assert_eq!(row.missing_members.len(), 2);
        assert!(row.extra_members.is_empty());
    }
}
