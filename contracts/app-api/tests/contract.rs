//! The stub host passes the whole app API contract suite.

use spot_app_api::contract_tests::{Fixture, TestHost, run_all};
use spot_app_api::stub::StubHost;

struct Stub;

impl Fixture for Stub {
    fn fresh(&self) -> Box<dyn TestHost> {
        Box::new(StubHost::new())
    }
}

#[tokio::test]
async fn stub_host_passes_the_contract_suite() {
    let report = run_all(&Stub).await;
    assert!(
        report.outcomes.len() >= 24,
        "suite shrank: {} cases",
        report.outcomes.len()
    );
    report.assert_all_passed();
}
