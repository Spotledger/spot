//! The machinery itself: registry rules, violation output, retries, indeterminate commits, replay,
//! history format and the report schema.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use spot_l7_invariants::corrupt::burn_sequence_number;
use spot_l7_invariants::driver::DriverConfig;
use spot_l7_invariants::faulty::{FaultPlan, Faulty};
use spot_l7_invariants::framework_registry;
use spot_l7_invariants::history::{History, OpKind, Outcome};
use spot_l7_invariants::invariant::{
    BoxFuture, CheckContext, Invariant, InvariantRegistry, RegistryError, Report, RunMeta,
    Violation,
};
use spot_l7_invariants::model::Model;
use spot_l7_invariants::runner::{check, run_and_check, run_workload};
use spot_l7_invariants::schedule::{Profile, Schedule, Weights};
use spot_storage_contract::ErrorKind;
use spot_storage_fake::FakeBackend;

fn small(seed: u64, workers: u32, operations: u64) -> DriverConfig {
    let mut c = DriverConfig::for_profile(Profile::Smoke, seed);
    c.workers = workers;
    c.operations = operations;
    c
}

struct Fixed {
    id: &'static str,
    violations: usize,
}

impl Invariant for Fixed {
    fn id(&self) -> &str {
        self.id
    }
    fn describe(&self) -> &str {
        "a test invariant"
    }
    fn check<'a>(&'a self, _ctx: &'a CheckContext) -> BoxFuture<'a, Vec<Violation>> {
        let n = self.violations;
        let id = self.id;
        Box::pin(async move {
            (0..n)
                .map(|i| Violation::new(id, "thing", format!("id-{i}"), "broken"))
                .collect()
        })
    }
}

#[test]
fn duplicate_and_empty_ids_are_refused() {
    let mut registry = InvariantRegistry::new();
    assert!(
        registry
            .register(Box::new(Fixed {
                id: "a.one",
                violations: 0
            }))
            .is_ok()
    );
    assert_eq!(
        registry.register(Box::new(Fixed {
            id: "a.one",
            violations: 0
        })),
        Err(RegistryError::Duplicate("a.one".into()))
    );
    assert_eq!(
        registry.register(Box::new(Fixed {
            id: "",
            violations: 0
        })),
        Err(RegistryError::EmptyId)
    );
    assert_eq!(registry.ids(), vec!["a.one"]);
    // The framework's own ids are taken: an app cannot shadow one.
    let model = Arc::new(Model::new().unwrap());
    let mut framework = framework_registry(&model);
    assert_eq!(framework.len(), 9);
    assert!(matches!(
        framework.register(Box::new(Fixed {
            id: "seq-gapless",
            violations: 0
        })),
        Err(RegistryError::Duplicate(_))
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn check_all_runs_every_invariant_and_caps_the_list() {
    let model = Arc::new(Model::new().unwrap());
    let backend = Arc::new(FakeBackend::new());
    let done = run_workload(backend.clone(), model.clone(), small(1, 2, 20), "fake")
        .await
        .unwrap();
    let mut registry = InvariantRegistry::new();
    registry
        .register(Box::new(Fixed {
            id: "app.first",
            violations: 3,
        }))
        .unwrap();
    registry
        .register(Box::new(Fixed {
            id: "app.second",
            violations: 500,
        }))
        .unwrap();
    registry
        .register(Box::new(Fixed {
            id: "app.third",
            violations: 0,
        }))
        .unwrap();
    let report = check(backend, &model, &done, &registry).await;
    assert!(!report.ok);
    // The first failure did not stop the others.
    assert_eq!(report.failed_ids(), vec!["app.first", "app.second"]);
    let second = report
        .results
        .iter()
        .find(|r| r.id == "app.second")
        .unwrap();
    assert_eq!(second.total_violations, 500);
    assert_eq!(second.violations.len(), 200);
    assert!(report.to_text().contains("... and 300 more"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_app_invariant_registered_next_to_the_framework_runs_with_it() {
    let model = Arc::new(Model::new().unwrap());
    let (_, report) = run_and_check(
        Arc::new(FakeBackend::new()),
        model,
        small(2, 2, 50),
        "fake",
        |registry| {
            registry
                .register(Box::new(Fixed {
                    id: "app.rule",
                    violations: 1,
                }))
                .unwrap();
        },
    )
    .await
    .unwrap();
    assert_eq!(report.failed_ids(), vec!["app.rule"]);
    assert_eq!(report.results.len(), 10);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn violations_carry_ids_and_counts_but_no_document_content() {
    let model = Arc::new(Model::new().unwrap());
    let backend = Arc::new(FakeBackend::new());
    let done = run_workload(backend.clone(), model.clone(), small(3, 4, 300), "fake")
        .await
        .unwrap();
    burn_sequence_number(backend.as_ref(), &model, "l7-series-0")
        .await
        .unwrap();
    // Also lose track of a document: the oracle forgets one live order.
    let registry = framework_registry(&model);
    let report = check(backend, &model, &done, &registry).await;
    assert_eq!(report.failed_ids(), vec!["seq-gapless"]);
    let json = report.to_json();
    // Documents carry text like `party-<uuid>`, `item-1` and `v3`; none of it may leak.
    for leak in ["party-", "item-", "upd-", "counter-", "setup-"] {
        assert!(
            !json.contains(leak),
            "violation output contains `{leak}`: {json}"
        );
    }
    for v in report.results.iter().flat_map(|r| &r.violations) {
        assert!(
            v.detail
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || " _=.-".contains(c)),
            "detail is not machine-readable: {}",
            v.detail
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retry_exhaustion_is_recorded_as_failure_never_as_success() {
    let model = Arc::new(Model::new().unwrap());
    let inner = Arc::new(FakeBackend::new());
    let backend = Arc::new(Faulty::new(
        inner,
        FaultPlan {
            fail_commits_with: Some(ErrorKind::SerializationFailure),
            ..FaultPlan::default()
        },
    ));
    let (done, report) = run_and_check(backend, model, small(4, 4, 200), "fake+faulty", |_| {})
        .await
        .unwrap();
    let writes: Vec<_> = done
        .history
        .entries
        .iter()
        .filter(|e| !matches!(e.kind, OpKind::Read))
        .collect();
    assert!(!writes.is_empty());
    for e in &writes {
        match &e.outcome {
            Outcome::Failed { code, exhausted } => {
                assert!(*exhausted, "{e:?}");
                assert_eq!(code, "storage.serialization_failure");
                assert_eq!(e.attempts, 8);
            }
            // Operations that find nothing to work on never reach commit.
            Outcome::Rejected { .. } | Outcome::RolledBack => {}
            other => panic!("a write that could not commit was recorded as {other:?}"),
        }
    }
    // Nothing committed, so the oracle expects nothing and the store agrees.
    assert!(report.ok, "{}", report.to_text());
    assert!(done.history.counts().exhausted > 0);
}

async fn lost_ack_run(applies: bool) -> (spot_l7_invariants::runner::Completed, Report) {
    let model = Arc::new(Model::new().unwrap());
    let backend = Arc::new(Faulty::new(
        Arc::new(FakeBackend::new()),
        FaultPlan {
            lose_commit_ack_one_in: 7,
            lost_ack_applies: applies,
            ..FaultPlan::default()
        },
    ));
    run_and_check(backend, model, small(5, 4, 600), "fake+faulty", |_| {})
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn indeterminate_commits_are_resolved_when_the_write_was_applied() {
    let (done, report) = lost_ack_run(true).await;
    let counts = done.history.counts();
    assert!(counts.indeterminate > 20, "{counts:?}");
    assert_eq!(counts.indeterminate, counts.resolved);
    assert!(
        done.history
            .entries
            .iter()
            .filter_map(|e| match &e.outcome {
                Outcome::Indeterminate { resolved } => *resolved,
                _ => None,
            })
            .all(|applied| applied),
        "every lost acknowledgement hid an applied write"
    );
    assert!(report.ok, "{}", report.to_text());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn indeterminate_commits_are_resolved_when_the_write_was_not_applied() {
    let (done, report) = lost_ack_run(false).await;
    let counts = done.history.counts();
    assert!(counts.indeterminate > 20, "{counts:?}");
    assert_eq!(counts.indeterminate, counts.resolved);
    assert!(
        done.history
            .entries
            .iter()
            .filter_map(|e| match &e.outcome {
                Outcome::Indeterminate { resolved } => *resolved,
                _ => None,
            })
            .all(|applied| !applied)
    );
    assert!(report.ok, "{}", report.to_text());
}

#[test]
fn the_same_seed_gives_the_same_schedule() {
    let w = Weights::default();
    let a = Schedule::generate(99, 8, 2_000, &w);
    let b = Schedule::generate(99, 8, 2_000, &w);
    let c = Schedule::generate(100, 8, 2_000, &w);
    assert_eq!(a, b);
    assert_ne!(a, c);
    assert_eq!(a.total(), 2_000);
    assert_eq!(a.workers.len(), 8);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_replayed_seed_runs_the_same_operations_per_worker() {
    async fn kinds(seed: u64) -> Vec<(u32, u64, OpKind)> {
        let model = Arc::new(Model::new().unwrap());
        let done = run_workload(
            Arc::new(FakeBackend::new()),
            model,
            small(seed, 4, 300),
            "fake",
        )
        .await
        .unwrap();
        let mut v: Vec<_> = done
            .history
            .entries
            .iter()
            .map(|e| (e.worker, e.seq, e.kind.clone()))
            .collect();
        v.sort_by_key(|(w, s, _)| (*w, *s));
        v
    }
    assert_eq!(kinds(11).await, kinds(11).await);
    assert_ne!(kinds(11).await, kinds(12).await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_history_round_trips_through_json_lines() {
    let model = Arc::new(Model::new().unwrap());
    let done = run_workload(Arc::new(FakeBackend::new()), model, small(6, 3, 60), "fake")
        .await
        .unwrap();
    let mut bytes = Vec::new();
    done.history.write_jsonl(&mut bytes).unwrap();
    let text = String::from_utf8(bytes).unwrap();
    let first = text.lines().next().unwrap();
    assert_eq!(
        first,
        r#"{"format":"spot-l7-history","version":1,"seed":6,"workers":3,"profile":"smoke"}"#
    );
    assert_eq!(text.lines().count(), 61);
    let back = History::read_jsonl(&text).unwrap();
    assert_eq!(&back, done.history.as_ref());
    assert!(History::read_jsonl(&text.replace("version\":1", "version\":2")).is_err());
}

#[test]
fn the_report_json_schema_is_golden() {
    let report = Report {
        schema: "spot-l7-report/1".into(),
        ok: false,
        meta: RunMeta {
            seed: 42,
            backend: "fake".into(),
            profile: "smoke".into(),
            workers: 8,
        },
        operations: spot_l7_invariants::history::OutcomeCounts {
            total: 10,
            committed: 6,
            rolled_back: 1,
            rejected: 1,
            failed: 1,
            exhausted: 1,
            indeterminate: 1,
            resolved: 1,
        },
        results: vec![spot_l7_invariants::invariant::InvariantResult {
            id: "seq-gapless".into(),
            describe: "numbers are 1..=n".into(),
            passed: false,
            total_violations: 1,
            violations: vec![Violation::new(
                "seq-gapless",
                "sequence",
                "l7-series-0",
                "gap number=3",
            )],
        }],
    };
    let expected = r#"{
  "schema": "spot-l7-report/1",
  "ok": false,
  "meta": {
    "seed": 42,
    "backend": "fake",
    "profile": "smoke",
    "workers": 8
  },
  "operations": {
    "total": 10,
    "committed": 6,
    "rolled_back": 1,
    "rejected": 1,
    "failed": 1,
    "exhausted": 1,
    "indeterminate": 1,
    "resolved": 1
  },
  "results": [
    {
      "id": "seq-gapless",
      "describe": "numbers are 1..=n",
      "passed": false,
      "total_violations": 1,
      "violations": [
        {
          "invariant": "seq-gapless",
          "entity": "sequence",
          "id": "l7-series-0",
          "detail": "gap number=3"
        }
      ]
    }
  ]
}"#;
    assert_eq!(report.to_json(), expected);
    let back: Report = serde_json::from_str(expected).unwrap();
    assert_eq!(back, report);
    assert!(
        report
            .to_text()
            .starts_with("L7 invariants: VIOLATIONS on fake (profile smoke, 8 workers, seed 42)")
    );
}
