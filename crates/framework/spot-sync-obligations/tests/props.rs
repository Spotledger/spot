//! `outstanding` is monotone as the applied prefix advances.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use proptest::prelude::*;
use spot_sync_obligations::{MandatoryEvent, outstanding};

proptest! {
    #[test]
    fn outstanding_shrinks_as_prefix_advances(
        positions in proptest::collection::vec(1u64..200, 0..40),
        p1 in 0u64..200,
        step in 0u64..100,
    ) {
        let events: Vec<MandatoryEvent> = positions
            .iter()
            .map(|p| MandatoryEvent { position: *p, published_at_millis: 0, effective_from_millis: None, record: format!("r{p}") })
            .collect();
        let p2 = p1 + step;
        let a: Vec<u64> = outstanding(&events, p1, &[]).unwrap().iter().map(|o| o.event.position).collect();
        let b: Vec<u64> = outstanding(&events, p2, &[]).unwrap().iter().map(|o| o.event.position).collect();
        prop_assert!(b.iter().all(|x| a.contains(x)));
        prop_assert!(b.len() <= a.len());
        prop_assert!(a.windows(2).all(|w| w[0] < w[1]));
    }
}
