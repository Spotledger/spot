//! Crash-resume (WP-STR-04.T2): a run cut at any point ends in the old state or the new one,
//! never between, and `resume` is idempotent.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use spot_installer::{
    Approval, Catalogue, InstallPlan, LoadedComponent, Options, Outcome, Request, SiteState,
};

/// A reversible upgrade from `tax` 1.0.0 to 2.0.0: two patches and one fixture.
fn upgrade_plan() -> InstallPlan {
    let spec = Spec::new("tax", "2.0.0")
        .patch("p1", "data", true)
        .patch("p2", "metadata", true)
        .fixture("Role", "fixtures/roles.json");
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

/// Seeds a harness with the site's app and its serving component.
async fn seeded(site: &spot_core::SiteId) -> Harness {
    let harness = Harness::new().await;
    harness.registry.seed(
        site,
        SiteState {
            apps: vec![installed("tax", "1.0.0", 1)],
        },
    );
    harness
        .components
        .seed(site, "tax", LoadedComponent::new(ver("1.0.0"), digest(1)));
    harness
}

/// Asserts the site is wholly old (1.0.0) or wholly new (2.0.0), never between.
fn assert_old_or_new(h: &Harness, site: &spot_core::SiteId, label: &str) {
    let state = h.registry.state(site);
    let version = state.get("tax").map(|app| app.version.clone());
    let serving = h.components.serving(site, "tax").map(|c| c.version);
    let old = version == Some(ver("1.0.0"));
    let new = version == Some(ver("2.0.0"));
    assert!(old || new, "{label}: registry has {version:?}");
    assert_eq!(version, serving, "{label}: registry and component disagree");
}

#[tokio::test]
async fn resume_after_a_lost_lease_at_every_write_is_never_between() {
    // `allowed` is the number of `check_fence` calls that succeed; the run stops at the next.
    for allowed in 0..12_usize {
        let site = Harness::site("shop");
        let h = seeded(&site).await;
        let breaker = Arc::new(FenceBreaker::new(h.coordinator.clone(), allowed));
        let executor = h.executor_with(breaker);
        let plan = upgrade_plan();
        // A run that is not cut returns Applied; one that is cut returns lease_lost. Either way
        // the site must be in a whole state after resume.
        let _ = executor
            .run(&site, &plan, plan.hash(), &Options::new("one"))
            .await;
        // A crashed holder's lease lapses; the next holder may take it.
        h.clock.advance(Duration::from_secs(120));

        let outcome = h.executor.resume(&site).await.expect("resume succeeds");
        assert!(
            matches!(
                outcome,
                Outcome::Applied { .. } | Outcome::RolledBack { .. } | Outcome::Nothing
            ),
            "allowed {allowed}: {outcome:?}"
        );
        assert_old_or_new(&h, &site, &format!("allowed {allowed}"));

        // Idempotent: a second resume finds nothing.
        let again = h
            .executor
            .resume(&site)
            .await
            .expect("the second resume succeeds");
        assert!(
            matches!(again, Outcome::Nothing),
            "allowed {allowed}: {again:?}"
        );
        assert_old_or_new(&h, &site, &format!("second resume, allowed {allowed}"));
    }
}

#[tokio::test]
async fn cancelling_at_each_await_then_resuming_is_never_between() {
    // Polling the run future `polls` times and dropping it cuts at the `polls`-th await, because
    // every fake call yields once. The number of awaits is small; 40 covers every cut point.
    for polls in 0..40_usize {
        let site = Harness::site("shop");
        let h = seeded(&site).await;
        let plan = upgrade_plan();
        let options = Options::new("one");
        let mut fut = Box::pin(h.executor.run(&site, &plan, plan.hash(), &options));
        for _ in 0..polls {
            // A zero timeout polls the inner future once and then elapses.
            if tokio::time::timeout(Duration::ZERO, &mut fut).await.is_ok() {
                break;
            }
        }
        drop(fut);
        h.clock.advance(Duration::from_secs(120));

        let outcome = h.executor.resume(&site).await.expect("resume succeeds");
        assert!(
            matches!(
                outcome,
                Outcome::Applied { .. } | Outcome::RolledBack { .. } | Outcome::Nothing
            ),
            "polls {polls}: {outcome:?}"
        );
        assert_old_or_new(&h, &site, &format!("polls {polls}"));

        let again = h
            .executor
            .resume(&site)
            .await
            .expect("the second resume succeeds");
        assert!(
            matches!(again, Outcome::Nothing),
            "polls {polls}: {again:?}"
        );
    }
}

#[tokio::test]
async fn resume_with_no_journal_is_a_no_op() {
    let site = Harness::site("shop");
    let h = seeded(&site).await;
    let outcome = h.executor.resume(&site).await.expect("resume succeeds");
    assert!(matches!(outcome, Outcome::Nothing));
    assert_eq!(
        h.registry.state(&site),
        SiteState {
            apps: vec![installed("tax", "1.0.0", 1)]
        }
    );
}

#[tokio::test]
async fn resume_completes_a_run_interrupted_after_the_commit() {
    let site = Harness::site("shop");
    let h = seeded(&site).await;
    // The reversible plan's fence calls: p1 started/done (1,2), p2 started/done (3,4), fixture
    // started/done (5,6), commit started (7), commit write (8), load started (9). Allow the
    // first eight, so the commit lands and the load's write is the one that loses the lease.
    let breaker = Arc::new(FenceBreaker::new(h.coordinator.clone(), 8));
    let executor = h.executor_with(breaker);
    let plan = upgrade_plan();
    let error = executor
        .run(&site, &plan, plan.hash(), &Options::new("one"))
        .await
        .expect_err("the lease is lost after the commit");
    assert_eq!(error.code, "installer.lease_lost");
    assert_eq!(
        h.registry.state(&site).get("tax").unwrap().version,
        ver("2.0.0"),
        "the registry committed before the interruption"
    );
    h.clock.advance(Duration::from_secs(120));

    let resumed = h.executor.resume(&site).await.expect("resume succeeds");
    assert!(resumed.is_applied(), "{resumed:?}");
    assert_eq!(
        h.components.serving(&site, "tax").unwrap().version,
        ver("2.0.0"),
        "resume completed the load"
    );
}

#[tokio::test]
async fn a_journal_with_an_idle_flag_is_refused_and_the_lease_is_given_back() {
    // `finish` clears the journal before it sets the flag, so rows with an idle flag cannot be
    // produced by the executor; resume refuses them fail closed and must not keep the lease.
    use spot_coordinator::Coordinator;
    use spot_installer::{AppRegistryStore, InstallJournal, StepId, StepKind, StepState};
    let site = Harness::site("shop");
    let h = seeded(&site).await;
    h.registry
        .record(
            &site,
            InstallJournal::new(
                "hash",
                StepId::new(0, StepKind::Patch, "p1"),
                StepState::Done,
                0,
                None,
            ),
        )
        .await
        .unwrap();
    let error = h.executor.resume(&site).await.expect_err("refused");
    assert_eq!(error.code, "installer.step_failed", "{error}");
    let lease = format!("installer:{site}");
    assert!(
        h.coordinator.current(&lease).await.unwrap().is_none(),
        "the refused resume gave the lease back"
    );
    assert!(
        !h.registry.rows(&site).is_empty(),
        "the journal is left alone"
    );
}
