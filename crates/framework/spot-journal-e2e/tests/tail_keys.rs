//! A remote-sync relay needs the tenant's tail keys: without them it is refused at open with the
//! relay's typed error, and the rig every scenario uses passes the shared test keys.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use spot_journal_e2e::events::{SOURCE, TENANT};
use spot_journal_e2e::keys::{Signer, manifest_keys};
use spot_journal_e2e::rig::relay_config;
use spot_journal_e2e::store::TestStore;
use spot_journal_relay::{Durability, Relay, RelayConfig};

#[tokio::test]
async fn a_remote_sync_relay_without_tail_keys_is_refused_at_open() {
    let config = RelayConfig::new(TENANT, SOURCE, Durability::RemoteSync);
    let error = Relay::open(
        TestStore::default(),
        Box::new(Signer),
        manifest_keys(),
        config,
        1_000,
    )
    .await
    .err()
    .expect("remote-sync without tail keys is refused");
    assert_eq!(error.code(), "journal_relay.config_invalid");
}

#[tokio::test]
async fn the_scenario_rig_config_carries_the_test_keys_and_opens() {
    let config = relay_config(TENANT, SOURCE);
    assert_eq!(config.durability, Durability::RemoteSync);
    assert!(config.tail_keys.is_some());
    Relay::open(
        TestStore::default(),
        Box::new(Signer),
        manifest_keys(),
        config,
        1_000,
    )
    .await
    .expect("opens with the shared test keys");
}
