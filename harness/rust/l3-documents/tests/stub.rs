//! The document behaviour suite runs on the stub host, so it stays green before the
//! document engine (WP-DOC-04, WP-DOC-10) exists and pins the engine when it does.

use spot_app_api::contract_tests::{Fixture, TestHost, run_all};
use spot_app_api::stub::StubHost;

use spot_l3_documents::run_all as run_document_suite;

/// Test-only fixture: every case gets a fresh `StubHost`.
struct StubFixture;

impl Fixture for StubFixture {
    fn fresh(&self) -> Box<dyn TestHost> {
        Box::new(StubHost::new())
    }
}

/// The suite has at least the 20 cases the task brief pins; the stub host passes all of them.
#[tokio::test]
async fn stub_host_passes_the_l3_document_suite() {
    let contract_report = run_all(&StubFixture).await;
    assert!(
        contract_report.outcomes.len() >= 24,
        "contract suite shrank: {} cases",
        contract_report.outcomes.len()
    );
    contract_report.assert_all_passed();

    let report = run_document_suite(&StubFixture).await;
    assert!(
        report.outcomes.len() >= 20,
        "document suite shrank: {} cases",
        report.outcomes.len()
    );
    report.assert_all_passed();
}
