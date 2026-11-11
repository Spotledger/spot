//! The parity matrix on every available backend, and the completeness of the case set.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use spot_storage_parity::{FakeFactory, ParityRunner, PgFactory};

#[tokio::test(flavor = "multi_thread")]
async fn the_matrix_is_green_on_every_backend() {
    let runner = ParityRunner::from_repo().unwrap();
    let fake = runner.run_backend(&FakeFactory).await;
    let pg = runner.run_backend(&PgFactory).await;
    let report = runner.report(&[fake, pg], "postgres");
    println!("{report}");
    report.assert_green();
}

#[test]
fn every_decided_rule_has_a_case_and_every_case_a_rule() {
    let runner = ParityRunner::from_repo().unwrap();
    let problems = runner.completeness_problems();
    assert!(problems.is_empty(), "{}", problems.join("\n"));
    assert!(
        runner.cases.len() >= 150,
        "the matrix has {} cases",
        runner.cases.len()
    );
}
