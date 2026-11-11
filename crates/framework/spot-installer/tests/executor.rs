//! The executor's acceptance line (WP-STR-04.T2): a failed upgrade rolls back cleanly and other
//! sites are untouched.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use common::*;
use spot_coordinator::Coordinator;
use spot_installer::testing::FailKind;
use spot_installer::{
    AppRegistryStore, Approval, ApprovedPlan, Catalogue, InstallPlan, LoadedComponent, Options,
    Request, SiteLifecycle, SiteState,
};

/// A reversible upgrade from `tax` 1.0.0 to 2.0.0: two patches and one fixture.
fn upgrade_plan() -> InstallPlan {
    let spec = Spec::new("tax", "2.0.0")
        .patch("p1", "data", true)
        .patch("p2", "metadata", true)
        .fixture("Role", "fixtures/roles.json");
    build_upgrade(spec)
}

/// An upgrade whose only patch cannot be undone, so the plan must snapshot first.
fn irreversible_plan() -> InstallPlan {
    build_upgrade(Spec::new("tax", "2.0.0").patch("p1", "data", false))
}

/// Builds the upgrade plan for `spec` against a site with `tax` 1.0.0 installed.
fn build_upgrade(spec: Spec) -> InstallPlan {
    let catalogue = Catalogue::new(vec![spec.candidate(2)]);
    let site = SiteState {
        apps: vec![installed("tax", "1.0.0", 1)],
    };
    spot_installer::plan(
        &site,
        &catalogue,
        &host("0.1.0"),
        &Request::Upgrade {
            app: "tax".to_owned(),
            to: None,
        },
        &Approval::new(),
    )
    .expect("the upgrade plan builds")
}

/// Builds an uninstall plan for `tax`, purging its data when `purge` is set.
fn uninstall_plan(purge: bool) -> InstallPlan {
    let site = SiteState {
        apps: vec![installed("tax", "1.0.0", 1)],
    };
    spot_installer::plan(
        &site,
        &Catalogue::new(Vec::new()),
        &host("0.1.0"),
        &Request::Uninstall {
            app: "tax".to_owned(),
            purge,
        },
        &Approval::new(),
    )
    .expect("the uninstall plan builds")
}

/// The site state before an upgrade: `tax` 1.0.0 installed.
fn before_state() -> SiteState {
    SiteState {
        apps: vec![installed("tax", "1.0.0", 1)],
    }
}

/// Seeds a harness with the site's app and its serving component.
async fn seeded(site: &spot_core::SiteId) -> Harness {
    let harness = Harness::new().await;
    harness.registry.seed(site, before_state());
    harness
        .components
        .seed(site, "tax", LoadedComponent::new(ver("1.0.0"), digest(1)));
    harness
}

#[tokio::test]
async fn a_reversible_upgrade_applies_every_step() {
    let site = Harness::site("shop");
    let h = seeded(&site).await;
    let plan = upgrade_plan();
    let outcome = h
        .executor
        .run(&site, &plan, plan.hash(), &Options::new("one"))
        .await
        .expect("the run succeeds");

    assert!(outcome.is_applied(), "{outcome:?}");
    assert_eq!(
        h.registry.state(&site).get("tax").unwrap().version,
        ver("2.0.0")
    );
    assert_eq!(
        h.components.serving(&site, "tax").unwrap().version,
        ver("2.0.0")
    );
    assert_eq!(
        h.patches.applied(&site, "tax"),
        BTreeSet::from(["p1".to_owned(), "p2".to_owned()])
    );
    assert!(
        h.fixtures
            .loaded(&site, "tax")
            .contains("fixtures/roles.json")
    );
    assert_eq!(h.registry.lifecycle_of(&site), Some(SiteLifecycle::Idle));
    assert!(h.registry.rows(&site).is_empty(), "the journal is cleared");
    // A plan whose every patch is reversible never snapshots.
    assert_eq!(h.snapshots.taken_count(&site), 0);
}

#[tokio::test]
async fn a_failed_step_rolls_the_upgrade_back() {
    // Step indices for the reversible plan: 0 p1, 1 p2, 2 fixture, 3 commit, 4 load, 5 smoke.
    let cases = [
        (0_u32, FailKind::Patch),
        (1, FailKind::Patch),
        (2, FailKind::Fixture),
        (3, FailKind::Commit),
        (4, FailKind::Load),
        (5, FailKind::Smoke),
    ];
    for (step, kind) in cases {
        let site = Harness::site("shop");
        let h = seeded(&site).await;
        h.schedule.fail_at(step, kind);
        let plan = upgrade_plan();
        let outcome = h
            .executor
            .run(&site, &plan, plan.hash(), &Options::new("one"))
            .await
            .expect("a step failure is not a refusal");

        assert!(
            outcome.is_rolled_back(),
            "step {step} {kind:?} gave {outcome:?}"
        );
        assert_eq!(h.registry.state(&site), before_state(), "step {step}");
        assert_eq!(
            h.components.serving(&site, "tax").unwrap().version,
            ver("1.0.0"),
            "step {step}: the previous component is serving"
        );
        assert!(
            h.patches.applied(&site, "tax").is_empty(),
            "step {step}: patches undone"
        );
        assert!(
            h.fixtures.loaded(&site, "tax").is_empty(),
            "step {step}: fixtures removed"
        );
        assert_eq!(h.registry.lifecycle_of(&site), Some(SiteLifecycle::Idle));
        assert!(
            h.registry.rows(&site).is_empty(),
            "step {step}: journal clear"
        );
    }
}

#[tokio::test]
async fn an_irreversible_step_snapshots_first_and_restores() {
    // Steps: 0 snapshot, 1 patch, 2 commit, 3 load, 4 smoke.
    let site = Harness::site("shop");
    let h = seeded(&site).await;
    h.schedule.fail_at(2, FailKind::Commit);
    let plan = irreversible_plan();
    let outcome = h
        .executor
        .run(&site, &plan, plan.hash(), &Options::new("one"))
        .await
        .expect("a step failure is not a refusal");

    assert!(outcome.is_rolled_back(), "{outcome:?}");
    assert_eq!(
        h.snapshots.taken_count(&site),
        1,
        "the plan snapshots first"
    );
    assert_eq!(
        h.snapshots.restored_count(&site),
        1,
        "the failure restores the snapshot"
    );
    assert_eq!(h.registry.state(&site), before_state());
    assert_eq!(
        h.components.serving(&site, "tax").unwrap().version,
        ver("1.0.0")
    );
}

#[tokio::test]
async fn a_failed_rollback_needs_attention() {
    let site = Harness::site("shop");
    let h = seeded(&site).await;
    h.schedule.fail_at(5, FailKind::Smoke);
    h.schedule.fail_at(0, FailKind::Undo);
    let plan = upgrade_plan();
    let outcome = h
        .executor
        .run(&site, &plan, plan.hash(), &Options::new("one"))
        .await
        .expect("a step failure is not a refusal");

    assert!(outcome.needs_attention(), "{outcome:?}");
    assert_eq!(
        h.registry.lifecycle_of(&site),
        Some(SiteLifecycle::Installing),
        "the site stays flagged installing"
    );
    assert!(
        !h.registry.rows(&site).is_empty(),
        "everything stays journalled"
    );
}

#[tokio::test]
async fn an_unapproved_plan_is_refused() {
    let site = Harness::site("shop");
    let h = seeded(&site).await;
    let plan = upgrade_plan();
    let error = h
        .executor
        .run(&site, &plan, "not-the-hash", &Options::new("one"))
        .await
        .expect_err("an unapproved plan is refused");
    assert_eq!(error.code, "installer.plan_not_approved");
    assert_eq!(h.registry.state(&site), before_state(), "nothing changed");
}

#[tokio::test]
async fn a_second_installer_is_refused_while_the_first_holds_the_lease() {
    let site = Harness::site("shop");
    let h = seeded(&site).await;
    let lease = format!("installer:{site}");
    h.coordinator
        .acquire(&lease, "other", Duration::from_secs(60))
        .await
        .expect("the other installer takes the lease");
    let plan = upgrade_plan();
    let error = h
        .executor
        .run(&site, &plan, plan.hash(), &Options::new("mine"))
        .await
        .expect_err("the lease is held");
    assert_eq!(error.code, "installer.lease_held");
}

#[tokio::test]
async fn a_stale_fence_stops_the_run_and_leaves_the_journal() {
    let site = Harness::site("shop");
    let h = seeded(&site).await;
    // Two `check_fence` calls succeed (p1 started, p1 done); the third (p2 started) is lost.
    let breaker = Arc::new(FenceBreaker::new(h.coordinator.clone(), 2));
    let executor = h.executor_with(breaker);
    let plan = upgrade_plan();
    let error = executor
        .run(&site, &plan, plan.hash(), &Options::new("one"))
        .await
        .expect_err("the lease is lost");
    assert_eq!(error.code, "installer.lease_lost");
    assert!(
        !h.registry.rows(&site).is_empty(),
        "the journal is left for the next holder"
    );
}

#[tokio::test]
async fn one_site_failing_leaves_the_others_applied() {
    let kinds = [
        FailKind::Patch,
        FailKind::Patch,
        FailKind::Fixture,
        FailKind::Commit,
        FailKind::Load,
        FailKind::Smoke,
    ];
    for (step, kind) in kinds.into_iter().enumerate() {
        let h = Harness::new().await;
        let sites: Vec<spot_core::SiteId> =
            (1..=5).map(|i| Harness::site(&format!("s{i}"))).collect();
        let mut plans: BTreeMap<spot_core::SiteId, ApprovedPlan> = BTreeMap::new();
        for site in &sites {
            h.registry.seed(site, before_state());
            h.components
                .seed(site, "tax", LoadedComponent::new(ver("1.0.0"), digest(1)));
            let plan = upgrade_plan();
            plans.insert(
                site.clone(),
                ApprovedPlan {
                    hash: plan.hash().to_owned(),
                    plan,
                },
            );
        }
        let site3 = sites[2].clone();
        h.schedule.fail_at_site(&site3, step as u32, kind);

        let results = h
            .executor
            .run_many(&sites, &plans, &Options::new("fleet"), 2)
            .await;
        assert_eq!(results.len(), 5, "one result per site");
        for (site, result) in &results {
            let outcome = result.as_ref().expect("a step failure is not a refusal");
            if site == &site3 {
                assert!(!outcome.is_applied(), "site 3 step {step} gave {outcome:?}");
                assert_eq!(
                    h.registry.state(&site3),
                    before_state(),
                    "site 3 step {step} rolled back"
                );
            } else {
                assert!(
                    outcome.is_applied(),
                    "site {site} step {step} gave {outcome:?}"
                );
            }
        }
        // Every call for one site named that site, never another's.
        for site in &sites {
            for call in h.schedule.calls_for(site) {
                assert_eq!(call.site, site.as_str());
            }
        }
        assert_eq!(h.schedule.sites_called().len(), 5);
    }
}

#[tokio::test]
async fn an_uninstall_keeps_data_unless_purged() {
    // A keep-data uninstall removes the component, the fixtures and the registry row, but not
    // the app's data, and never snapshots.
    let site = Harness::site("shop");
    let h = seeded(&site).await;
    h.fixtures.seed(&site, "tax", &["fixtures/roles.json"]);
    h.registry
        .set_owned_doctypes(&site, "tax", &["Invoice", "Sales Order"]);
    let plan = uninstall_plan(false);
    let outcome = h
        .executor
        .run(&site, &plan, plan.hash(), &Options::new("one"))
        .await
        .expect("the uninstall succeeds");
    assert!(outcome.is_applied(), "{outcome:?}");
    assert!(h.registry.state(&site).get("tax").is_none());
    assert!(h.components.serving(&site, "tax").is_none());
    assert!(h.fixtures.loaded(&site, "tax").is_empty());
    assert!(!h.registry.was_purged(&site, "tax"), "data is kept");
    assert_eq!(h.snapshots.taken_count(&site), 0);

    // A purge deletes the app's data and, being irreversible, snapshots first.
    let site2 = Harness::site("shop2");
    let h2 = seeded(&site2).await;
    h2.fixtures.seed(&site2, "tax", &["fixtures/roles.json"]);
    h2.registry.set_owned_doctypes(&site2, "tax", &["Invoice"]);
    let plan2 = uninstall_plan(true);
    assert!(plan2.purges(), "the plan records the purge");
    let outcome2 = h2
        .executor
        .run(&site2, &plan2, plan2.hash(), &Options::new("one"))
        .await
        .expect("the purge succeeds");
    assert!(outcome2.is_applied(), "{outcome2:?}");
    assert!(h2.registry.was_purged(&site2, "tax"), "data is purged");
    assert_eq!(
        h2.snapshots.taken_count(&site2),
        1,
        "a purge snapshots first"
    );
}

/// A second node: its own coordinator, so the per-site lease cannot stop it, over the same
/// registry and fakes.
async fn second_node(h: &Harness) -> spot_installer::Executor {
    let coordinator = spot_coordinator::InProcessCoordinator::start(
        Arc::new(spot_coordinator::InMemoryPresence::new()),
        spot_coordinator::NodeId::new("installer-test-b").expect("a constant node id is valid"),
        h.clock.clone(),
    )
    .await
    .expect("the second coordinator starts alone");
    h.executor_with(coordinator)
}

#[tokio::test]
async fn a_run_on_a_site_another_node_is_installing_is_refused_before_any_work() {
    // Recorded security finding: node B already moved the site to installing and holds its own lease; node A's
    // coordinator knows nothing of it, so only the lifecycle compare can stop A.
    let site = Harness::site("shop");
    let h = seeded(&site).await;
    h.registry
        .set_lifecycle(&site, SiteLifecycle::Idle, SiteLifecycle::Installing)
        .await
        .expect("the other node starts its install");
    let plan = upgrade_plan();
    let calls_before = h.schedule.calls_for(&site).len();
    let error = h
        .executor
        .run(&site, &plan, plan.hash(), &Options::new("a"))
        .await
        .expect_err("the site is already installing");
    assert_eq!(error.code, "installer.lease_held", "{error}");
    let calls: Vec<_> = h.schedule.calls_for(&site)[calls_before..]
        .iter()
        .map(|call| call.method)
        .collect();
    assert_eq!(
        calls,
        ["set_lifecycle"],
        "the refused run made the compare-and-set and nothing else: no read, no step"
    );
    assert_eq!(h.registry.state(&site), before_state(), "nothing changed");
    assert_eq!(
        h.registry.lifecycle_of(&site),
        Some(SiteLifecycle::Installing),
        "the site is left as the other node set it"
    );
}

#[tokio::test]
async fn two_executors_racing_one_site_let_exactly_one_install() {
    let site = Harness::site("shop");
    let h = seeded(&site).await;
    let other = second_node(&h).await;
    let plan = upgrade_plan();
    let (options_a, options_b) = (Options::new("a"), Options::new("b"));
    let (first, second) = tokio::join!(
        h.executor.run(&site, &plan, plan.hash(), &options_a),
        other.run(&site, &plan, plan.hash(), &options_b),
    );
    let results = [first, second];
    let applied = results
        .iter()
        .filter(|result| matches!(result, Ok(outcome) if outcome.is_applied()))
        .count();
    assert_eq!(applied, 1, "exactly one node installs: {results:?}");
    let refused: Vec<_> = results.iter().filter_map(|r| r.as_ref().err()).collect();
    assert_eq!(refused.len(), 1, "{results:?}");
    assert_eq!(refused[0].code, "installer.lease_held");
    assert_eq!(
        h.registry.state(&site).get("tax").unwrap().version,
        ver("2.0.0")
    );
    assert_eq!(h.patches.applied(&site, "tax").len(), 2, "patches ran once");
}
