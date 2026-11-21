//! The acceptance for WP-CTL-01.T2: five tenants provisioned concurrently on a real database,
//! isolation between their schemas, and the time one provisioning takes against the five-minute
//! target.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::time::{Duration, Instant};

use common::{fixture, request, runtime};
use spot_tenancy::{ControlStore, TenantState};

/// The internal test budget: generous so a loaded CI runner does not fail on a slow disk, while
/// the printed number is the real one to compare against the five-minute acceptance target.
const BUDGET: Duration = Duration::from_secs(120);

/// The product target from the work package.
const TARGET: Duration = Duration::from_secs(300);

#[test]
fn five_tenants_provision_concurrently_are_isolated_and_one_is_timed() {
    let rt = runtime();
    let Some(f) = rt.block_on(fixture(rt.clone())) else {
        return;
    };

    // The plan is registered first, as the control-plane API does; only `run` touches the data.
    let mut tenants = Vec::new();
    for (index, name) in ["alpha", "bravo", "charlie", "delta", "echo"]
        .into_iter()
        .enumerate()
    {
        let plan = f
            .plane()
            .provision(&request(name, "eu-west", index as u8 + 1), "ops", 1_000)
            .unwrap();
        assert!(
            plan.estimated_ms < TARGET.as_millis() as u64,
            "the {} estimate is {} ms",
            name,
            plan.estimated_ms
        );
        tenants.push(plan.tenant);
    }

    let started = Instant::now();
    let timings = std::thread::scope(|scope| {
        let fixture = &f;
        let mut handles = Vec::new();
        for (index, id) in tenants.iter().copied().enumerate() {
            handles.push(scope.spawn(move || {
                let at = Instant::now();
                // One control plane per thread: its ports are trait objects.
                let outcome = fixture.plane().run(&id, "ops", 1_001);
                (index, outcome, at.elapsed())
            }));
        }
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>()
    });
    let total = started.elapsed();

    let mut slowest = Duration::ZERO;
    for (index, outcome, elapsed) in &timings {
        let tenant = outcome
            .as_ref()
            .unwrap_or_else(|error| panic!("tenant {index} failed: {error}"));
        assert_eq!(tenant.state, TenantState::Active);
        slowest = slowest.max(*elapsed);
    }

    // The real number, printed: one provisioning under five concurrent runs.
    println!(
        "provisioning one tenant took {} ms (slowest of five concurrent runs; total {} ms; \
         budget {} ms; five-minute target {} ms)",
        slowest.as_millis(),
        total.as_millis(),
        BUDGET.as_millis(),
        TARGET.as_millis()
    );
    assert!(
        slowest < BUDGET,
        "one provisioning took {} ms, over the {} ms budget",
        slowest.as_millis(),
        BUDGET.as_millis()
    );

    // Every tenant is Active in the registry, and the placement load counts all five.
    let stored = f.store.list().unwrap();
    assert_eq!(stored.len(), 5);
    assert!(
        stored
            .iter()
            .all(|tenant| tenant.state == TenantState::Active)
    );
    assert_eq!(
        f.store
            .load()
            .unwrap()
            .get(&(f.cluster.clone(), f.database.clone()))
            .copied(),
        Some(5)
    );

    // Isolation: each tenant's schema holds exactly its own seed row, and a query made with one
    // tenant's schema first in `search_path` cannot see another tenant's row.
    let expected = ["alpha", "bravo", "charlie", "delta", "echo"];
    for (index, id) in tenants.iter().enumerate() {
        assert!(f.schema_exists(id), "tenant {index} has no schema");
        assert_eq!(f.seed_rows(id), 1, "tenant {index} seed rows");
        assert_eq!(f.seed_labels(id), vec![expected[index].to_owned()]);
        for (other, label) in expected.iter().enumerate() {
            if other != index {
                assert!(
                    !f.seed_labels(id).contains(&(*label).to_owned()),
                    "tenant {index} sees tenant {other}'s row"
                );
            }
        }
    }

    // The schemas are the per-tenant names derived from the ids, never the tenant names.
    for (index, id) in tenants.iter().enumerate() {
        assert_eq!(id.schema_name(), format!("t_{}", id.as_uuid().simple()));
        assert!(!id.schema_name().contains(expected[index]));
    }
}

#[test]
fn a_provisioned_tenant_is_not_provisioned_twice() {
    let rt = runtime();
    let Some(f) = rt.block_on(fixture(rt.clone())) else {
        return;
    };
    let id = f.provision("acme", 1, 1_000);
    assert_eq!(f.seed_rows(&id), 1);
    let error = f.plane().run(&id, "ops", 2_000).unwrap_err();
    assert_eq!(error.code(), spot_tenancy::codes::WRONG_STATE);
    assert_eq!(f.seed_rows(&id), 1, "a refused rerun changed the seed");
}
