//! The storage contract suite against the in-memory backend: every case, queries included.

use spot_storage_contract::contract_tests;
use spot_storage_fake::FakeBackend;

struct Fixture;

impl contract_tests::Fixture for Fixture {
    type Backend = FakeBackend;

    async fn fresh(&self) -> FakeBackend {
        FakeBackend::new()
    }
}

#[tokio::test]
async fn storage_contract() {
    contract_tests::run_all(&Fixture).await.assert_all_passed();
}
