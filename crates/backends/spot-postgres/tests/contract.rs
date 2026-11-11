//! The complete storage contract suite (`contract_tests::run_all`, 43 cases: the 30 of v0.1 and
//! the 13 construct cases of ADR-087), run against PostgreSQL.
//!
//! Every scratch database gets `btree_gist` (the exclusion case needs it) and, when the server
//! offers it, `postgis`, standing in for the audited `extension.install`. A server without
//! PostGIS skips the cases that prepare a geo catalog, printing which; the CI image that
//! installs it is WP-STO-10.T4's.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Mutex;

use spot_postgres::PgBackend;
use spot_postgres::testing::TestDb;
use spot_storage_contract::contract_tests::{Fixture, run_matching};

/// Cases that prepare the geo catalog and so need PostGIS.
const NEEDS_POSTGIS: &[&str] = &[
    "geo_values_roundtrip_exactly",
    "geo_values_are_validated",
    "geo_filters_are_validated",
    "geo_filters_follow_capabilities",
    "constructs_are_validated_by_the_catalog",
];

/// Hands out a fresh scratch database per case and keeps it alive until the run ends.
struct Postgres {
    databases: Mutex<Vec<TestDb>>,
    postgis: Mutex<bool>,
}

impl Fixture for Postgres {
    type Backend = PgBackend;

    async fn fresh(&self) -> PgBackend {
        let db = TestDb::create().await.expect("a test database");
        let missing = db
            .create_extensions(&["btree_gist", "pg_trgm", "unaccent", "postgis"])
            .await;
        assert!(
            missing.iter().all(|m| m == "postgis"),
            "the server lacks {missing:?}"
        );
        *self.postgis.lock().unwrap() = missing.is_empty();
        let backend = PgBackend::connect(db.config()).await.expect("connect");
        self.databases.lock().unwrap().push(db);
        backend
    }
}

#[tokio::test]
async fn the_whole_storage_contract_passes_on_postgres() {
    // Skips (printing why) when no server is configured, like every database test here.
    let Some(probe) = TestDb::create().await else {
        return;
    };
    let postgis = probe.create_extensions(&["postgis"]).await.is_empty();
    drop(probe);
    let fixture = Postgres {
        databases: Mutex::new(Vec::new()),
        postgis: Mutex::new(postgis),
    };
    let report = run_matching(&fixture, |case| postgis || !NEEDS_POSTGIS.contains(&case)).await;
    let expected = if postgis {
        43
    } else {
        43 - NEEDS_POSTGIS.len()
    };
    if !postgis {
        println!("SKIPPED (no postgis on this server): {NEEDS_POSTGIS:?}");
    }
    assert_eq!(report.outcomes.len(), expected, "every case must run");
    report.assert_all_passed();
}
