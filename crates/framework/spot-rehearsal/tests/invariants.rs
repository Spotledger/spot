//! The `InvariantSet` seam: the runner runs every set, and an adapter wraps WP-TST-07's L7
//! `InvariantRegistry` so a rehearsal reuses the framework's invariants without the rehearsal
//! crate depending on the harness crate.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::sync::Arc;

use spot_l7_invariants::{
    CheckContext, History, InvariantRegistry, Model, Oracle, RunMeta, Violation, framework_registry,
};
use spot_rehearsal::{
    BoxFuture, Fingerprint, InvariantContext, InvariantReport, InvariantSet, InvariantViolation,
    StaticInvariantSet,
};
use spot_storage_contract::Catalog;
use spot_storage_fake::FakeBackend;
use support::{empty_plan, scratch_target};

/// Wraps an L7 `InvariantRegistry` in the rehearsal's `InvariantSet` seam.
struct L7Set {
    backend: Arc<FakeBackend>,
    catalog: Arc<Catalog>,
    history: Arc<History>,
    oracle: Arc<Oracle>,
    meta: RunMeta,
    registry: InvariantRegistry,
}

impl InvariantSet for L7Set {
    fn name(&self) -> &str {
        "l7"
    }

    fn check<'a>(
        &'a self,
        _ctx: &'a InvariantContext<'a>,
    ) -> BoxFuture<'a, Vec<InvariantViolation>> {
        Box::pin(async move {
            let context = CheckContext::for_backend(
                self.backend.clone(),
                self.catalog.clone(),
                self.history.clone(),
                self.oracle.clone(),
            );
            let report = self.registry.check_all(&context, self.meta.clone()).await;
            report
                .results
                .iter()
                .flat_map(|result| {
                    result.violations.iter().map(move |violation: &Violation| {
                        InvariantViolation::new(
                            "l7",
                            violation.invariant.clone(),
                            violation.entity.clone(),
                            violation.id.clone(),
                            violation.detail.clone(),
                        )
                    })
                })
                .collect()
        })
    }
}

#[tokio::test]
async fn the_l7_registry_runs_through_the_invariant_set_seam() {
    let model = Arc::new(Model::new().expect("model"));
    let backend = Arc::new(FakeBackend::new());
    let registry: InvariantRegistry = framework_registry(&model);
    let set = L7Set {
        backend,
        catalog: model.catalog.clone(),
        history: Arc::new(History::new(7, 8, "smoke")),
        oracle: Arc::new(Oracle::new()),
        meta: RunMeta {
            seed: 7,
            backend: "fake".to_owned(),
            profile: "smoke".to_owned(),
            workers: 8,
        },
        registry,
    };

    let plan = empty_plan();
    let clone = spot_rehearsal::CloneTarget::new("localhost", scratch_target());
    let fingerprint = Fingerprint::default();
    let context = InvariantContext {
        clone: &clone,
        plan: &plan,
        fingerprint: &fingerprint,
    };
    let sets: Vec<Arc<dyn InvariantSet>> = vec![
        Arc::new(set),
        Arc::new(StaticInvariantSet::passing("site-policy")),
    ];
    let report = InvariantReport::run(&sets, &context).await;

    assert_eq!(report.sets.len(), 2);
    assert_eq!(report.sets[0].set, "l7");
    assert!(report.sets[1].passed);
}
