//! The matrix: the fake and PostgreSQL at the three levels, checked against the expected table.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

mod common;

use std::sync::Arc;

use spot_l7_invariants::history::Level;
use spot_l8_isolation::anomaly::AnomalyClass;
use spot_l8_isolation::backends::run_matrix;
use spot_l8_isolation::history::Workload;
use spot_l8_isolation::model::Model;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_backend_matches_the_expected_table() {
    for workload in [Workload::ListAppend, Workload::RwRegister] {
        let config = common::conflict_heavy(workload, 3);
        let m = run_matrix(Arc::new(Model::default()), &config).await;
        assert!(m.meets_expectation(), "{}", m.to_text());

        let fake = m.backend("fake").expect("the fake is always run");
        for level in [Level::ReadCommitted, Level::RepeatableRead] {
            let report = fake.at(level).expect("all three levels run");
            assert!(
                report.ok(),
                "fake {workload:?} at {level:?}: {:?}",
                report.violations
            );
        }
        // The fake's serializable level is expected to show only write skew.
        if let Some(ser) = fake.at(Level::Serializable) {
            assert!(
                ser.violations.iter().all(|c| *c == AnomalyClass::G2),
                "fake serializable must only ever show write skew: {:?}",
                ser.violations
            );
        }

        if let Some(pg) = m.backend("pg") {
            for level in [
                Level::ReadCommitted,
                Level::RepeatableRead,
                Level::Serializable,
            ] {
                let report = pg.at(level).expect("all three levels run on pg");
                assert!(
                    report.ok(),
                    "PostgreSQL {workload:?} at {level:?}: {:?}",
                    report.violations
                );
            }
        } else {
            println!("SKIPPED: PostgreSQL matrix not run (no SPOT_TEST_PG_URL)");
        }
    }
}
