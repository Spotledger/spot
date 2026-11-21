//! Without an ack, a node's level never decreases as the clock advances.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use proptest::prelude::*;
use spot_conditions::MemorySink;
use spot_sync_acks::{AckLedger, MemoryStore, NodeStateStore, StreamHead};
use spot_sync_obligations::{MandatoryEvent, ObligationSettings};

const T: &str = "t1";
const H: i64 = 3_600_000;

proptest! {
    #[test]
    fn levels_never_decrease_without_an_ack(
        published in proptest::collection::vec((0i64..48, proptest::option::of(0i64..96)), 1..8),
        steps in proptest::collection::vec(0i64..10, 1..30),
    ) {
        let mut l = AckLedger::new(
            MemoryStore::new(), MemorySink::new(), defs(), ObligationSettings::default(),
        ).unwrap();
        l.register_node("n1", T, "1", 0).unwrap();
        for (i, (at, eff)) in published.iter().enumerate() {
            l.register_mandatory(T, MandatoryEvent {
                position: i as u64 + 1,
                published_at_millis: at * H,
                effective_from_millis: eff.map(|e| e * H),
                record: format!("r{i}"),
            });
        }
        let mut now = 0i64;
        let mut last = l.level("n1", T).unwrap();
        for s in steps {
            now += s * H;
            l.tick(now).unwrap();
            let level = l.level("n1", T).unwrap();
            prop_assert!(level >= last, "level fell from {last:?} to {level:?} at {now}");
            last = level;
        }
    }

    #[test]
    fn no_recorded_prefix_exceeds_the_largest_accepted_head(
        steps in proptest::collection::vec((0u64..40, 0u64..60, prop_oneof![Just(u64::MAX), 0u64..60]), 1..30),
    ) {
        let mut l = AckLedger::new(
            MemoryStore::new(), MemorySink::new(), defs(), ObligationSettings::default(),
        ).unwrap();
        l.register_node("n1", T, "1", 0).unwrap();
        let mut head = 0u64;
        let mut largest_accepted_head = 0u64;
        for (i, (advance, _, claim)) in steps.iter().enumerate() {
            head += advance;
            let accepted = l
                .on_ack("n1", T, *claim, StreamHead::new(head), i as i64)
                .is_ok();
            if accepted {
                largest_accepted_head = largest_accepted_head.max(head);
            }
            let recorded = l.store().load("n1", T).unwrap().applied_prefix;
            prop_assert!(recorded <= largest_accepted_head, "{recorded} > {largest_accepted_head}");
        }
    }
}

/// The three obligation definitions, taken from the catalogue.
fn defs() -> Vec<spot_conditions::ConditionDef> {
    [
        "OBLIGATION_DUE_SOON",
        "OBLIGATION_OVERDUE",
        "OBLIGATION_RED",
    ]
    .into_iter()
    .map(|id| spot_sync_conditions::entry(id).unwrap().def)
    .collect()
}
