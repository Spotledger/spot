use proptest::prelude::*;
use spot_image_upcast::Registry;

use super::block_on;
use super::fixture::{Ev, Journal, TENANT, delete, put};
use crate::memory::{MemoryScratch, SteppingClock};
use crate::run_audit;

fn arb_script() -> impl Strategy<Value = Vec<Ev>> {
    proptest::collection::vec((1_u64..5, any::<bool>()), 1..24).prop_map(|steps| {
        let mut versions = std::collections::BTreeMap::<u64, (u64, bool)>::new();
        let mut events = Vec::new();
        for (n, remove) in steps {
            let entry = versions.entry(n).or_insert((0, true));
            if remove && !entry.1 {
                entry.0 += 1;
                events.push(delete(n, entry.0));
                entry.1 = true;
            } else if entry.1 {
                *entry = (1, false);
                events.push(put(n, 1));
            } else {
                entry.0 += 1;
                events.push(put(n, entry.0));
            }
        }
        events
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]

    #[test]
    fn a_live_side_built_from_the_script_always_matches(events in arb_script(), per in 1_usize..6) {
        let journal = Journal::new(events, per);
        let live = journal.live();
        let mut scratch = MemoryScratch::new(TENANT);
        let run = block_on(run_audit(
            &journal.store,
            &journal.keys(),
            &Registry::new(),
            &live,
            &mut scratch,
            &journal.options(u64::MAX),
            &SteppingClock::new(0, 1),
        ))
        .unwrap();
        prop_assert!(run.is_clean());
        prop_assert_eq!(scratch.discard_count(), 1);
    }
}
