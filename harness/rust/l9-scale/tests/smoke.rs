//! The `smoke` profile against a real `spot serve` on a real PostgreSQL server (WP-TST-15.T1).
//! It needs `SPOT_TEST_PG_URL`; without it the test prints why and returns, it is never faked.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::Path;

use l9_scale::budget::Budgets;
use l9_scale::launcher::{self, LaunchOptions};
use l9_scale::profile::{OPS, Op, Profile};
use l9_scale::{Mode, RunOptions, execute};

fn crate_dir() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_smoke_profile_passes_against_a_real_spot_serve() {
    let profile = Profile::load(&crate_dir().join("profiles/smoke.toml")).unwrap();
    let budgets = Budgets::load(&crate_dir().join("budgets.toml")).unwrap();
    let out = std::env::temp_dir().join(format!("l9_smoke_report_{}", launcher::random_hex(8)));
    let run_id = launcher::random_hex(12);
    let options = RunOptions {
        profile,
        budgets,
        mode: Mode::Launch(LaunchOptions {
            repo: launcher::default_repo(),
            spot_bin: None,
            users: 5,
            run_id: run_id.clone(),
            keep: false,
        }),
        run_id,
        out_dir: Some(out.clone()),
    };
    let Some(finished) = execute(options).await.unwrap() else {
        println!("SKIPPED: the l9-scale smoke run needs SPOT_TEST_PG_URL");
        return;
    };
    let report = finished.report;
    assert!(report.passed, "budgets breached:\n{}", report.to_markdown());
    assert_eq!(
        report.shed_arrivals, 0,
        "the profile has enough sessions for its rate"
    );
    for op in OPS.iter().filter(|op| **op != Op::StaleUpdate) {
        let section = report.operations.iter().find(|o| o.name == op.name());
        assert!(
            section.is_some_and(|s| s.count > 0),
            "{op} never ran:\n{}",
            report.to_markdown()
        );
    }
    assert!(
        !report.resources.is_empty(),
        "no server resource sample was taken"
    );
    assert!(
        report
            .resources
            .iter()
            .any(|r| r.pg_connections.is_some_and(|n| n > 0))
    );
    // The report files exist, and no password or token is in them.
    let json = std::fs::read_to_string(out.join("report.json")).unwrap();
    let markdown = std::fs::read_to_string(out.join("report.md")).unwrap();
    assert!(markdown.starts_with("# L9 scale report: smoke (PASS)"));
    for text in [&json, &markdown] {
        let lower = text.to_lowercase();
        assert!(!lower.contains("password") && !lower.contains("bearer"));
    }
    let _ = std::fs::remove_dir_all(&out);
}
