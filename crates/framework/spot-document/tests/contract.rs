//! The document engine passes the app API contract suite, over the in-memory backend.
//!
//! This task (WP-DOC-04.T2) covers the posted-document operations and methods: `post`, `cancel`,
//! `correct`, `rename`, `update` of a posted document and whitelisted methods (ADR-42, ADR-16),
//! on top of the create, update, delete, load and form events of WP-DOC-04.T1 — the 24 cases in
//! [`host::CASES`]. Cases the suite grew after this brief (the v0.2 field model and field
//! security) are other work packages' scope.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod host;

use spot_app_api::contract_tests::{Fixture, TestHost, run_matching};

use host::{CASES, Host};

struct Engine;

impl Fixture for Engine {
    fn fresh(&self) -> Box<dyn TestHost> {
        Box::new(Host::new())
    }
}

#[tokio::test]
async fn the_engine_passes_the_24_cases_of_this_task() {
    let report = run_matching(&Engine, |case| CASES.contains(&case)).await;
    assert_eq!(report.outcomes.len(), CASES.len(), "a case went missing");
    report.assert_all_passed();
}
