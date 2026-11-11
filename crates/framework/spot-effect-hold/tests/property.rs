//! Property test: over random effect mixes and a random terminal action, the hold's invariants
//! always hold.
//!
//! Invariants:
//! - no held-class effect is dispatched while the site is held;
//! - every admitted-and-held effect ends in exactly one terminal state (`Dispatched` or
//!   `Discarded`);
//! - a fiscal transmission always proceeds and is never in the ledger.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{NOW, effect, world};
use proptest::collection::vec;
use proptest::prelude::*;
use spot_effect_hold::{Admission, EffectClass, EffectState, LedgerStore, ReleasePlan};

fn class_for(tag: u8) -> EffectClass {
    match tag % 6 {
        0 => EffectClass::Mail,
        1 => EffectClass::BankApi,
        2 => EffectClass::Webhook,
        3 => EffectClass::JobQueue("queue".to_owned()),
        4 => EffectClass::EdgeSync,
        _ => EffectClass::FiscalTransmission,
    }
}

async fn run_case(tags: &[u8], discard: bool) {
    let world = world("property.example");
    world.service.load(NOW).await;
    world
        .service
        .begin("mig-prop", "verify", NOW, None)
        .await
        .unwrap();

    let mut fiscal = 0usize;
    let mut held = 0usize;
    for (index, tag) in tags.iter().enumerate() {
        let class = class_for(*tag);
        let reference = effect(
            &world.site,
            class.clone(),
            &format!("p{index}"),
            &class.key(),
        );
        match world.service.gate().admit(&reference, NOW).await {
            Admission::Proceed => {
                assert!(
                    class.is_fiscal(),
                    "a non-fiscal effect proceeded while held"
                );
                fiscal += 1;
            }
            Admission::Held(ticket) => {
                assert!(!class.is_fiscal(), "a fiscal effect was held");
                assert!(ticket.durable);
                held += 1;
            }
        }
    }

    // Nothing was dispatched while held.
    assert_eq!(world.dispatcher.unique_count(), 0);

    if discard {
        let report = world
            .service
            .discard("mig-prop", "rolled back", NOW + 1000)
            .await
            .unwrap();
        assert_eq!(report.discarded, held as u64);
        assert_eq!(world.dispatcher.unique_count(), 0);
    } else {
        let report = world
            .service
            .release("mig-prop", ReleasePlan::new(), NOW + 1000)
            .await
            .unwrap();
        assert!(report.completed);
        assert_eq!(report.dispatched.len(), held);
        assert_eq!(world.dispatcher.unique_count(), held);
    }

    // Every held effect ended in exactly one terminal state; the fiscal ones were never recorded.
    let planned = if discard {
        EffectState::Discarded
    } else {
        EffectState::Dispatched
    };
    let terminal = world
        .ledger
        .list_effects(&world.site, &[planned])
        .await
        .unwrap();
    assert_eq!(terminal.len(), held);
    let all = world
        .ledger
        .list_effects(
            &world.site,
            &[
                EffectState::Held,
                EffectState::Releasing,
                EffectState::NeedsConfirmation,
            ],
        )
        .await
        .unwrap();
    assert!(all.is_empty(), "a held effect has no terminal state");
    assert!(terminal.iter().all(|record| !record.class.is_fiscal()));
    let _ = fiscal;
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    #[test]
    fn hold_invariants(tags in vec(0u8..6, 0..40), discard in any::<bool>()) {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("a test runtime builds");
        runtime.block_on(run_case(&tags, discard));
    }
}
