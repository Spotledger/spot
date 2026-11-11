//! The fail-closed rule of WP-MIG-08.T2: a ledger that cannot be read is an error, never "the
//! site is open" and never "there are no effects".
//!
//! A hold that answered `Open` — or an empty list — because the database was gone would let a
//! migration's mail out while the migration was unverified. These tests close the pool, so every
//! connection is gone exactly as after a database restart, and prove the error the hold is built
//! to treat as "stay held".
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use common::*;
use spot_effect_hold::{
    Admission, EffectClass, EffectRef, EffectState, Error, Gate, HoldSnapshot, HoldState,
    LedgerStore, MigrationId, PolicyTable,
};
use spot_effect_hold_pg::PgLedger;
use spot_postgres::testing::TestDb;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_closed_pool_errors_instead_of_answering_open() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let pool = pool(&db, 2, "fail_closed");
    let ledger = PgLedger::new(pool.clone());
    ledger.prepare().await.unwrap();
    let site = site("hold-closed.example");

    // While the database answers, a site that was never held is known to be Open.
    assert_eq!(ledger.get_hold(&site).await.unwrap(), HoldState::Open);

    // Every connection is gone. The same read must now fail, not answer Open.
    pool.close();
    let error = ledger.get_hold(&site).await.unwrap_err();
    assert!(
        matches!(&error, Error::LedgerUnavailable { .. }),
        "a closed pool must be a ledger error, got {error}"
    );
    assert_eq!(error.code(), spot_effect_hold::codes::LEDGER_UNAVAILABLE);

    // And a failed read is never an empty effect list.
    assert!(
        ledger
            .list_effects(&site, &[EffectState::Held])
            .await
            .is_err(),
        "an unreadable ledger must not look like an empty ledger"
    );
    assert!(
        ledger
            .append_effect(effect(&site, "k1", 1, &mig("mig-1")))
            .await
            .is_err()
    );
    assert!(ledger.get_checkpoint(&site, &mig("mig-1")).await.is_err());
    assert!(
        ledger
            .compare_and_set_hold(&site, &HoldState::Open, held(&mig("mig-1"), 1))
            .await
            .is_err()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unreadable_ledger_leaves_the_gate_held() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let pool = pool(&db, 2, "fail_closed_gate");
    let prepared = PgLedger::new(pool.clone());
    prepared.prepare().await.unwrap();
    let ledger: Arc<dyn LedgerStore> = Arc::new(prepared);
    let site = site("hold-closed-gate.example");

    pool.close();
    let gate = Gate::new(
        site.clone(),
        Arc::clone(&ledger),
        Arc::new(PolicyTable::new()),
    );
    gate.load(0).await;
    assert_eq!(
        gate.snapshot(),
        HoldSnapshot::Unknown,
        "an unreadable ledger must leave the gate unknown, which it treats as held"
    );

    let effect = EffectRef::new(site, EffectClass::Mail, "invoice_mail", "k1", 1);
    match gate.admit(&effect, 0).await {
        Admission::Held(ticket) => {
            assert!(!ticket.durable, "the ledger could not record the effect");
            assert_eq!(ticket.effect_id, None);
            assert_eq!(ticket.migration_id, MigrationId::unverified());
        }
        Admission::Proceed => panic!("an unreadable ledger let a mail effect leave the site"),
    }
}
