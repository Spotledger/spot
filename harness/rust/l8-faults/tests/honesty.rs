//! The durability audit catches the two lies the brief names, offline on the in-memory fake.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use spot_l7_invariants::driver::DriverConfig;
use spot_l7_invariants::faulty::{FaultPlan, Faulty};
use spot_l7_invariants::history::{History, Level, OpKind, Outcome};
use spot_l7_invariants::invariant::{CheckContext, RunMeta};
use spot_l7_invariants::model::Model;
use spot_l7_invariants::oracle::Oracle;
use spot_l7_invariants::runner::Completed;
use spot_l7_invariants::schedule::Profile;
use spot_l7_invariants::{LoadDriver, framework_registry};
use spot_l8_faults::audit::DurabilityAudit;
use spot_l8_faults::honesty::{LossyClient, LossyPlan, indeterminate_as_success};
use spot_storage_fake::FakeBackend;

const SEED: u64 = 31_337;

fn driver_config() -> DriverConfig {
    DriverConfig::for_profile(Profile::Smoke, SEED)
}

/// Runs the L7 driver and checks it with the framework invariants plus the durability audit.
async fn run_and_audit<B: spot_storage_contract::StorageBackend>(
    backend: Arc<B>,
    model: Arc<Model>,
) -> (
    spot_l7_invariants::invariant::Report,
    spot_l8_faults::audit::AuditReport,
) {
    let driver = LoadDriver::new(backend.clone(), model.clone(), driver_config());
    let run = driver.run().await.expect("the workload runs");
    let done = Completed {
        meta: RunMeta {
            seed: run.seed,
            backend: "fake".to_owned(),
            profile: run.profile.clone(),
            workers: run.workers,
        },
        history: Arc::new(run.history),
        oracle: Arc::new(run.oracle),
    };
    let report = spot_l7_invariants::runner::check(
        backend.clone(),
        &model,
        &done,
        &framework_registry(&model),
    )
    .await;
    let ctx = CheckContext::for_backend(
        backend,
        model.catalog.clone(),
        done.history.clone(),
        done.oracle.clone(),
    );
    let audit = DurabilityAudit::run(&ctx).await;
    (report, audit)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_client_that_lies_about_commits_is_caught() {
    let model = Arc::new(Model::new().unwrap());
    let backend = Arc::new(LossyClient::new(
        Arc::new(FakeBackend::new()),
        LossyPlan { lie_one_in: 40 },
    ));
    let (report, audit) = run_and_audit(backend, model).await;
    println!("{}", report.to_text());
    println!("{}", audit.table());
    assert!(
        !report.ok,
        "the framework invariants must see the missing documents"
    );
    assert!(!audit.ok, "the durability audit must catch the lie");
    assert!(
        audit
            .violations
            .iter()
            .any(|v| v.detail.starts_with("acknowledged_effect_absent")),
        "expected acknowledged_effect_absent, got {:?}",
        audit.violations
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_driver_that_treats_indeterminate_as_success_is_caught() {
    // The backend loses a commit acknowledgement and the transaction really did not commit; the
    // driver resolves each to absent. A driver that treated indeterminate as success would record
    // them as committed — so rewrite the history that way and the audit must catch it.
    let model = Arc::new(Model::new().unwrap());
    let backend = Arc::new(Faulty::new(
        Arc::new(FakeBackend::new()),
        FaultPlan {
            lose_commit_ack_one_in: 20,
            lost_ack_applies: false,
            ..FaultPlan::default()
        },
    ));
    let driver = LoadDriver::new(backend.clone(), model.clone(), driver_config());
    let run = driver.run().await.expect("the workload runs");
    let resolved_absent = run
        .history
        .entries
        .iter()
        .filter(|e| {
            matches!(
                e.outcome,
                Outcome::Indeterminate {
                    resolved: Some(false)
                }
            )
        })
        .count();
    assert!(
        resolved_absent > 0,
        "the faulty backend must have produced indeterminate commits"
    );

    let lied = indeterminate_as_success(&run.history);
    let oracle = Arc::new(run.oracle);
    let ctx = CheckContext::for_backend(backend, model.catalog.clone(), Arc::new(lied), oracle);
    let audit = DurabilityAudit::run(&ctx).await;
    println!("{}", audit.table());
    assert!(
        !audit.ok,
        "the audit must catch the treated-as-success commit"
    );
    assert!(
        audit
            .violations
            .iter()
            .any(|v| v.detail.starts_with("acknowledged_effect_absent")),
        "expected acknowledged_effect_absent, got {:?}",
        audit.violations
    );
}

#[test]
fn the_classification_counts_every_outcome() {
    let mut history = History::new(1, 1, "smoke");
    let entry = |outcome: Outcome| spot_l7_invariants::history::HistoryEntry {
        worker: 0,
        seq: 0,
        start_us: 0,
        end_us: 0,
        kind: OpKind::Read,
        isolation: Level::ReadCommitted,
        outcome,
        attempts: 1,
        touched: Vec::new(),
        values: BTreeMap::new(),
    };
    history.entries = vec![
        entry(Outcome::Committed),
        entry(Outcome::RolledBack),
        entry(Outcome::Rejected {
            code: "storage.not_found".to_owned(),
        }),
        entry(Outcome::Failed {
            code: "storage.unavailable".to_owned(),
            exhausted: true,
        }),
        entry(Outcome::Indeterminate {
            resolved: Some(true),
        }),
        entry(Outcome::Indeterminate {
            resolved: Some(false),
        }),
        entry(Outcome::Indeterminate { resolved: None }),
    ];
    let counts = DurabilityAudit::classify(&history);
    assert_eq!(
        counts.get(&spot_l8_faults::audit::CommitClass::Acknowledged),
        Some(&1)
    );
    assert_eq!(
        counts.get(&spot_l8_faults::audit::CommitClass::Refused),
        Some(&2)
    );
    assert_eq!(
        counts.get(&spot_l8_faults::audit::CommitClass::Failed),
        Some(&1)
    );
    assert_eq!(
        counts.get(&spot_l8_faults::audit::CommitClass::IndeterminateCommitted),
        Some(&1)
    );
    assert_eq!(
        counts.get(&spot_l8_faults::audit::CommitClass::IndeterminateAbsent),
        Some(&1)
    );
    assert_eq!(
        counts.get(&spot_l8_faults::audit::CommitClass::Unresolved),
        Some(&1)
    );
    let _ = Oracle::new();
}
