//! Acceptance and takeover regressions, independent of implementation folding code.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use proptest::prelude::*;
use spot_license::model::ExtraEntitlement;
use spot_license::{Entitlements, Limit, Status};
use spot_metering::adapters::{DocumentOperation, TransactionsFromDocuments};
use spot_metering::*;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

fn periods() -> TenantPeriods {
    TenantPeriods::new(PeriodSpec::calendar_month("UTC").unwrap())
}
fn period() -> PeriodKey {
    periods().resolve("acme", 0).unwrap()
}
fn fact(index: usize, kind: u8, value: u64) -> MeterEvent {
    let id = format!("digest-{index}");
    match kind % 4 {
        0 => MeterEvent::amount(id, "acme", Meter::Transactions, value, 0),
        1 => MeterEvent::amount(id, "acme", Meter::StorageBytes, value, 0),
        2 => MeterEvent::amount(id, "acme", Meter::AiTokens, value, 0),
        _ => MeterEvent::member(
            id,
            "acme",
            Meter::ActiveUsers,
            MemberDigest::new(format!("digest-user-{}", value % 101)),
            0,
        ),
    }
}
fn ground_truth(events: &[MeterEvent]) -> TallyTruth {
    let mut sums = BTreeMap::<Meter, u64>::new();
    let mut members = BTreeSet::new();
    let mut seen = BTreeSet::new();
    for event in events {
        if !seen.insert((&event.tenant, &event.id)) {
            continue;
        }
        if event.meter == Meter::ActiveUsers {
            members.insert(event.member.clone().unwrap());
        } else if event.meter == Meter::StorageBytes {
            let n = sums.entry(event.meter.clone()).or_default();
            *n = (*n).max(event.amount);
        } else {
            *sums.entry(event.meter.clone()).or_default() += event.amount;
        }
    }
    let mut truth = TallyTruth::new();
    for (meter, n) in sums {
        truth = truth.with_total("acme", &period(), meter, n);
    }
    truth.with_members(
        "acme",
        &period(),
        Meter::ActiveUsers,
        members.into_iter().collect(),
    )
}
// Seeded integer PRNG; test generation never calls a production clock or environment.
fn random(seed: &mut u64) -> u64 {
    *seed = seed
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    *seed
}
#[tokio::test]
async fn ten_thousand_facts_reconcile_after_reordering_duplicates_and_drop_retry() {
    let mut seed = 0xC7105;
    let original: Vec<_> = (0..10_000)
        .map(|i| {
            let kind = (random(&mut seed) >> 32) as u8;
            fact(i, kind, random(&mut seed) % 10_000)
        })
        .collect();
    let truth = ground_truth(&original);
    let mut shuffled: Vec<_> = original
        .iter()
        .map(|e| (random(&mut seed), e.clone()))
        .collect();
    shuffled.sort_by_key(|(key, _)| *key);
    let sink = BoundedSink::new(37);
    let store = MemoryMeterStore::new("acme");
    let resolver = periods();
    let mut retry = Vec::new();
    for (i, (_, event)) in shuffled.iter().enumerate() {
        for _ in 0..(1 + i % 3) {
            if !sink.report(event.clone()).unwrap().is_queued() {
                retry.push(event.clone());
            }
        }
        if i % 91 == 0 {
            store.add_events(sink.drain(37), &resolver).await.unwrap();
        }
    }
    store.add_events(sink.drain(37), &resolver).await.unwrap();
    assert!(sink.dropped() > 0);
    let before = reconcile(&store, &truth, "acme", &period(), &resolver, sink.dropped())
        .await
        .unwrap();
    assert!(!before.is_clean());
    assert!(!before.disagreements().is_empty());
    for batch in retry.chunks(113) {
        store.add_events(batch.to_vec(), &resolver).await.unwrap();
    }
    let after = reconcile(&store, &truth, "acme", &period(), &resolver, sink.dropped())
        .await
        .unwrap();
    assert!(after.disagreements().is_empty(), "{after:?}");
    assert!(after.rows.iter().all(ReconcileRow::agrees));
    assert_eq!(after.dropped, sink.dropped()); // repaired counts do not hide historical loss
    assert_eq!(store.event_count(), 10_000);
}
proptest! {
    #[test]
    fn permutation_and_duplicate_batch_preserve_every_meter(
        values in prop::collection::vec((any::<u8>(), 0u64..100_000, any::<u64>(), 1usize..5), 0..120)
    ) {
        let original: Vec<_> = values.iter().enumerate().map(|(i, (kind, n, _, _))| fact(i, *kind, *n)).collect();
        let truth = ground_truth(&original);
        let mut reordered: Vec<_> = values.iter().enumerate().flat_map(|(i, (_, _, rank, copies))| {
            let event = original[i].clone();
            (0..*copies).map(move |_| (*rank, event.clone()))
        }).collect();
        reordered.sort_by_key(|(rank, _)| *rank);
        let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
        runtime.block_on(async {
            let store = MemoryMeterStore::new("acme");
            let resolver = periods();
            for chunk in reordered.chunks(17) {
                store.add_events(chunk.iter().map(|(_, e)| e.clone()).collect(), &resolver).await.unwrap();
            }
            let report = reconcile(&store, &truth, "acme", &period(), &resolver, 0).await.unwrap();
            assert!(report.is_clean(), "{report:?}");
        });
    }
}
#[tokio::test]
async fn distinct_users_across_days_months_tenants_and_compacted_retry() {
    let resolver = periods();
    let store: Arc<dyn MeterStore> = Arc::new(MemoryMeterStore::new("acme"));
    // A second customer is a second store: one instance serves one tenant, so
    // "acme" cannot write into "other"'s totals.
    let other: Arc<dyn MeterStore> = Arc::new(MemoryMeterStore::new("other"));
    let jan = period();
    let feb = resolver.resolve("acme", jan.end_exclusive).unwrap();
    let user = |id, tenant, at| {
        MeterEvent::member(
            id,
            tenant,
            Meter::ActiveUsers,
            MemberDigest::new("digest-user"),
            at,
        )
    };
    let first = user("same", "acme", 0);
    store
        .add_events(
            vec![
                first.clone(),
                user("next-day", "acme", 86_400),
                user("next-month", "acme", feb.start),
            ],
            &resolver,
        )
        .await
        .unwrap();
    // The same event id under another tenant is a separate fact in a separate store.
    other
        .add_events(vec![user("same", "other", 0)], &resolver)
        .await
        .unwrap();
    for p in [&jan, &feb] {
        assert_eq!(
            store
                .period_totals("acme", p, &resolver)
                .await
                .unwrap()
                .total(&Meter::ActiveUsers),
            1
        );
    }
    assert_eq!(
        other
            .period_totals("other", &jan, &resolver)
            .await
            .unwrap()
            .total(&Meter::ActiveUsers),
        1
    );
    // Neither store accepts the other tenant's event.
    assert_eq!(
        store
            .add_events(vec![user("smuggled", "other", 0)], &resolver)
            .await
            .unwrap_err(),
        MeteringError::TenantMismatch
    );
    store
        .compact(&feb, &RetentionPolicy::default(), &resolver)
        .await
        .unwrap();
    assert_eq!(store.add_events(vec![first], &resolver).await.unwrap(), 0);
    assert_eq!(
        store
            .period_totals("acme", &jan, &resolver)
            .await
            .unwrap()
            .total(&Meter::ActiveUsers),
        1
    );
}
#[tokio::test]
async fn full_period_and_enum_identity_are_store_keys() {
    let resolver = periods();
    let shifted = TenantPeriods::new(PeriodSpec::calendar_month("Asia/Kolkata").unwrap());
    let p = period();
    let other = shifted.resolve("acme", 0).unwrap();
    assert_eq!(p.label, other.label);
    let store = MemoryMeterStore::new("acme");
    store
        .add_events(
            vec![
                MeterEvent::amount("utc", "acme", Meter::Transactions, 3, 0),
                MeterEvent::amount("custom", "acme", Meter::Custom("transactions".into()), 7, 0),
            ],
            &resolver,
        )
        .await
        .unwrap();
    store
        .add_events(
            vec![MeterEvent::amount(
                "zone",
                "acme",
                Meter::Transactions,
                11,
                0,
            )],
            &shifted,
        )
        .await
        .unwrap();
    let usage = store.period_totals("acme", &p, &resolver).await.unwrap();
    assert_eq!(usage.total(&Meter::Transactions), 3);
    assert_eq!(usage.total(&Meter::Custom("transactions".into())), 7);
    assert_eq!(
        store
            .period_totals("acme", &other, &shifted)
            .await
            .unwrap()
            .total(&Meter::Transactions),
        11
    );
}
#[tokio::test]
async fn malformed_batch_does_not_partially_write() {
    let store = MemoryMeterStore::new("acme");
    let valid = MeterEvent::amount("good", "acme", Meter::Transactions, 1, 0);
    let bad = MeterEvent::amount("bad", "acme", Meter::ActiveUsers, 1, 0);
    assert!(
        store
            .add_events(vec![valid, bad], &periods())
            .await
            .is_err()
    );
    assert_eq!(store.event_count(), 0);
}
async fn refresh(guard: &QuotaGuard, store: &dyn MeterStore, policy: &QuotaPolicy, now: i64) {
    guard
        .refresh(
            store,
            RefreshContext {
                tenant: "acme",
                period: &period(),
                resolver: &periods(),
                policy,
                licence: Status::Active,
                unknown_limits: vec![],
                dropped: 0,
                now,
            },
        )
        .await
        .unwrap();
}
#[tokio::test]
async fn soft_grace_does_not_slide_on_refresh_and_expires_on_hot_path() {
    let store = MemoryMeterStore::new("acme");
    let guard = QuotaGuard::new(
        "acme",
        QuotaSnapshot::empty(period(), Status::Active, 0),
        Duration::from_secs(10_000),
    );
    let policy = QuotaPolicy::new()
        .with_rule(
            Meter::Transactions,
            QuotaRule::new(Limit::UpTo(100)).soft_blocking(),
        )
        .with_grace(Duration::from_secs(10));
    store
        .add_events(
            vec![MeterEvent::amount(
                "digest",
                "acme",
                Meter::Transactions,
                105,
                0,
            )],
            &periods(),
        )
        .await
        .unwrap();
    refresh(&guard, &store, &policy, 1).await;
    refresh(&guard, &store, &policy, 5).await;
    assert_eq!(
        guard.allow(QuotaAction::CreateMeteredDocument, "acme", 10),
        ActionDecision::Allow
    );
    assert!(matches!(
        guard.allow(QuotaAction::CreateMeteredDocument, "acme", 11),
        ActionDecision::Deny { .. }
    ));
    refresh(&guard, &store, &policy, 11).await;
    refresh(&guard, &store, &policy, 12).await;
    assert!(matches!(
        guard.allow(QuotaAction::CreateMeteredDocument, "acme", 12),
        ActionDecision::Deny { .. }
    ));
    // Stale SoftBlock fails open; hard modes alone are fail-closed.
    assert_eq!(
        guard.allow(QuotaAction::CreateMeteredDocument, "acme", 20_000),
        ActionDecision::Allow
    );
}
#[tokio::test]
async fn tenant_bound_guard_stale_boundary_unlimited_and_refresh() {
    let store = MemoryMeterStore::new("acme");
    let guard = QuotaGuard::new(
        "acme",
        QuotaSnapshot::empty(period(), Status::Active, 0),
        Duration::from_secs(60),
    );
    let policy = QuotaPolicy::new().with_rule(
        Meter::Transactions,
        QuotaRule::new(Limit::UpTo(10)).hard_blocking(),
    );
    refresh(&guard, &store, &policy, 0).await;
    assert_eq!(
        guard.allow(QuotaAction::CreateMeteredDocument, "acme", 60),
        ActionDecision::Allow
    );
    assert_eq!(
        guard
            .allow(QuotaAction::CreateMeteredDocument, "acme", 61)
            .reason(),
        Some("metering.quota_snapshot_stale")
    );
    assert_eq!(
        guard
            .allow(QuotaAction::CreateMeteredDocument, "other", 0)
            .reason(),
        Some("metering.tenant_mismatch")
    );
    let unlimited = QuotaPolicy::new().with_rule(
        Meter::Transactions,
        QuotaRule::new(Limit::Unlimited).hard_blocking(),
    );
    refresh(&guard, &store, &unlimited, 0).await;
    assert_eq!(
        guard.allow(QuotaAction::CreateMeteredDocument, "acme", 61),
        ActionDecision::Allow
    );
    store
        .add_events(
            vec![MeterEvent::amount(
                "posted",
                "acme",
                Meter::Transactions,
                10,
                0,
            )],
            &periods(),
        )
        .await
        .unwrap();
    refresh(&guard, &store, &policy, 2).await;
    assert!(
        guard
            .allow(QuotaAction::CreateMeteredDocument, "acme", 2)
            .reason()
            .is_some()
    );
}
#[test]
fn large_extra_limits_stay_bounded_and_unknown_limits_are_visible() {
    let mut entitlements = Entitlements::default();
    entitlements.extra.insert(
        "max_storage_bytes".into(),
        ExtraEntitlement::Number(10_000_000_000),
    );
    entitlements
        .extra
        .insert("unknown_limit".into(), ExtraEntitlement::Number(1));
    let (policy, unknown) = QuotaPolicy::from_entitlements(&entitlements);
    assert_eq!(
        policy.rule_for(&Meter::StorageBytes).unwrap().limit,
        MeterLimit::UpTo(10_000_000_000)
    );
    assert_eq!(unknown, ["unknown_limit"]);
}
#[test]
fn only_posted_allowlisted_documents_are_transactions() {
    let adapter = TransactionsFromDocuments::new(["ExampleDocument".into()]);
    assert!(
        adapter
            .event(
                "digest",
                "acme",
                "ExampleDocument",
                DocumentOperation::Posted,
                0
            )
            .is_some()
    );
    assert!(
        adapter
            .event("digest", "acme", "Unmetered", DocumentOperation::Posted, 0)
            .is_none()
    );
    for operation in [
        DocumentOperation::Read,
        DocumentOperation::Reversal,
        DocumentOperation::Cancellation,
    ] {
        assert!(
            adapter
                .event("digest", "acme", "ExampleDocument", operation, 0)
                .is_none()
        );
    }
}
#[test]
fn ai_partial_day_query_refused_but_record_range_is_exact() {
    use spot_llm_router::{MemoryLedger, SumQuery, UsageRecord};
    let p = period_of(
        1_709_231_400,
        &PeriodSpec::calendar_month("Asia/Kolkata").unwrap(),
    )
    .unwrap();
    let mk = |id: &str, second: i64, tokens| UsageRecord {
        at: second as u64 * 1_000,
        tenant: "acme".into(),
        request_id: id.into(),
        task: "task".into(),
        provider: "provider".into(),
        model: "model".into(),
        input: tokens,
        output: 0,
        cache_read: 0,
        cache_write: 0,
    };
    let records = vec![
        mk("before", p.start - 1, 999),
        mk("inside", p.start, 17),
        mk("inside", p.start, 17),
        mk("after", p.end_exclusive, 999),
    ];
    assert!(
        AiTokensFromLedger::total_for_period(&MemoryLedger::new(), &SumQuery::tenant("acme"), &p)
            .is_err()
    );
    assert_eq!(
        AiTokensFromLedger::total_for_records(&records, &SumQuery::tenant("acme"), &p).unwrap(),
        17
    );
    assert_eq!(
        AiTokensFromLedger::event_for_record(&records[1])
            .unwrap()
            .id,
        "inside"
    );
    let mut overflow = records[1].clone();
    overflow.input = u64::MAX;
    overflow.output = 1;
    assert_eq!(
        AiTokensFromLedger::event_for_record(&overflow).unwrap_err(),
        MeteringError::CounterOverflow
    );
}
#[test]
fn table_driven_dst_fold_gap_leap_and_half_hour_boundaries() {
    for (zone, instant) in [
        ("America/New_York", "2026-03-08T07:00:00Z"),
        ("America/New_York", "2026-11-01T06:00:00Z"),
        ("Australia/Lord_Howe", "2026-04-04T15:00:00Z"),
        ("Asia/Kolkata", "2024-02-29T18:30:00Z"),
        ("UTC", "2024-02-29T00:00:00Z"),
        ("America/Havana", "2015-11-01T04:30:00Z"),
        ("Pacific/Apia", "2011-12-30T10:00:00Z"),
    ] {
        let at = instant.parse::<jiff::Timestamp>().unwrap().as_second();
        for spec in [
            PeriodSpec::calendar_month(zone).unwrap(),
            PeriodSpec::anniversary(zone, 31).unwrap(),
        ] {
            for t in [at - 1, at, at + 1] {
                let p = period_of(t, &spec).unwrap();
                assert!(p.contains(t));
                let next = period_of(p.end_exclusive, &spec).unwrap();
                assert_eq!(p.end_exclusive, next.start);
                assert!(!p.contains(next.start));
            }
        }
    }
}

#[tokio::test]
async fn overflowing_batch_is_atomic_and_retryable() {
    let store = MemoryMeterStore::new("acme");
    store
        .add_events(
            vec![MeterEvent::amount(
                "first",
                "acme",
                Meter::Transactions,
                u64::MAX - 1,
                0,
            )],
            &periods(),
        )
        .await
        .unwrap();
    let small = MeterEvent::amount("small", "acme", Meter::Transactions, 1, 0);
    let overflow = MeterEvent::amount("too-big", "acme", Meter::Transactions, 1, 0);
    assert_eq!(
        store
            .add_events(vec![small.clone(), overflow], &periods())
            .await
            .unwrap_err(),
        MeteringError::CounterOverflow
    );
    assert_eq!(store.event_count(), 1);
    assert_eq!(store.add_events(vec![small], &periods()).await.unwrap(), 1);
}
#[test]
fn alias_extra_cannot_loosen_typed_licence_limit() {
    let mut entitlements = Entitlements::from_lists(Some(Limit::UpTo(5)), vec![], vec![]);
    entitlements
        .extra
        .insert("max_users".into(), ExtraEntitlement::Number(50));
    let (policy, _) = QuotaPolicy::from_entitlements(&entitlements);
    assert_eq!(
        policy.rule_for(&Meter::ActiveUsers).unwrap().limit,
        MeterLimit::UpTo(5)
    );
}
#[test]
fn policy_keeps_custom_meter_identity() {
    let policy = QuotaPolicy::new()
        .with_rule(Meter::Transactions, QuotaRule::new(Limit::UpTo(5)))
        .with_rule(
            Meter::Custom("transactions".into()),
            QuotaRule::new(Limit::UpTo(50)),
        );
    assert_eq!(
        policy.rule_for(&Meter::Transactions).unwrap().limit,
        MeterLimit::UpTo(5)
    );
}

#[tokio::test]
async fn late_older_refresh_cannot_replace_newer_snapshot() {
    let guard = QuotaGuard::new(
        "acme",
        QuotaSnapshot::empty(period(), Status::Active, 0),
        Duration::from_secs(60),
    );
    let store = MemoryMeterStore::new("acme");
    refresh(&guard, &store, &QuotaPolicy::new(), 10).await;
    refresh(&guard, &store, &QuotaPolicy::new(), 5).await;
    assert_eq!(guard.last_refresh(), 10);
    assert_eq!(guard.snapshot().built_at, 10);
}
#[test]
fn storage_and_ai_quotas_gate_only_new_documents() {
    for meter in [Meter::StorageBytes, Meter::AiTokens] {
        let policy = QuotaPolicy::new().with_rule(
            meter.clone(),
            QuotaRule::new(Limit::UpTo(1)).hard_blocking(),
        );
        let usage = TenantPeriodUsage::default().with_totals(
            meter.clone(),
            PeriodTotals {
                sum: 2,
                max: 2,
                distinct: 0,
            },
        );
        let report = evaluate_quota(
            &policy,
            &usage,
            Status::Grace { until: 50 },
            period(),
            0,
            vec![],
            0,
        );
        assert!(
            report
                .allows(QuotaAction::CreateMeteredDocument)
                .reason()
                .is_some()
        );
        for action in [
            QuotaAction::Read,
            QuotaAction::ExportOwnData,
            QuotaAction::SignInExistingUser,
            QuotaAction::PostReversal,
            QuotaAction::CancelDocument,
        ] {
            assert_eq!(report.allows(action), ActionDecision::Allow);
        }
    }
}

#[test]
fn tenant_policy_cannot_raise_a_licensed_limit() {
    let entitlements = Entitlements::from_lists(Some(Limit::UpTo(5)), vec![], vec![]);
    let tenant = QuotaPolicy::new().with_rule(
        Meter::ActiveUsers,
        QuotaRule::new(Limit::UpTo(50)).hard_blocking(),
    );
    let (combined, _) = QuotaPolicy::from_entitlements_with_policy(&entitlements, &tenant);
    let rule = combined.rule_for(&Meter::ActiveUsers).unwrap();
    assert_eq!(rule.limit, MeterLimit::UpTo(5));
    assert_eq!(rule.mode, QuotaMode::HardBlock);
}
