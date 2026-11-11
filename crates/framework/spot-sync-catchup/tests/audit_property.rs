#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Property: removing out-of-scope audit events from the input never changes the plan or the
//! destination-facing count (WP-SYN-07.T2).

mod common;

use common::{MapReferences, MapScope, audit_of, domain, state, topics};
use proptest::prelude::*;
use spot_sync_catchup::{Addressed, AuditDelivery, Unaddressed};
use spot_sync_catchup::{ClassCounts, Event, GapSummary, Settings, plan_with_audit_options};

fn summary(count: u64) -> GapSummary {
    GapSummary {
        applied_prefix: 0,
        stream_end: 1_000,
        hot_retention_floor: 0,
        in_scope_event_count: count,
        class_counts: ClassCounts {
            state: count,
            domain: 0,
            audit: 0,
        },
        estimated_coalesced_records: 5,
        estimated_coalesced_bytes: 500,
        estimated_snapshot_bytes: 1_000_000,
        node_is_new_or_resync: false,
    }
}

proptest! {
    #[test]
    fn out_of_scope_audit_events_never_change_the_destination_facing_plan(
        specs in proptest::collection::vec((0u8..4, 0usize..6, any::<bool>()), 0..40),
        option in 0usize..4,
    ) {
        let delivery = AuditDelivery {
            unaddressed: if option & 1 == 0 { Unaddressed::Deliver } else { Unaddressed::Drop },
            addressed: if option & 2 == 0 { Addressed::ScopeChecked } else { Addressed::Always },
        };
        let ids = ["a", "b", "c", "x", "y", "z"];
        let scope = MapScope::holding(&["a", "b", "c"]);
        let mut full: Vec<Event> = Vec::new();
        let mut without: Vec<Event> = Vec::new();
        for (index, (kind, who, mandatory)) in specs.iter().enumerate() {
            let position = 1 + index as u64;
            let id = ids[*who];
            let mut event = match kind {
                0 => state(position, id, position),
                1 => domain(position, id, position),
                _ => audit_of(position, id),
            };
            if *mandatory {
                event = event.mandatory();
            }
            let hidden = *kind >= 2 && *who >= 3;
            full.push(event.clone());
            if !hidden {
                without.push(event);
            }
        }
        let in_scope_count = without.len();
        let run = |events: &[Event]| {
            plan_with_audit_options(
                events,
                &summary(in_scope_count as u64),
                &Settings::default(),
                &topics(),
                &MapReferences::new(),
                &scope,
                "n",
                delivery,
            )
            .unwrap()
        };
        let a = run(&full);
        let b = run(&without);
        prop_assert_eq!(a.plan, b.plan);
        prop_assert_eq!(a.counts.destination_facing(), b.counts.destination_facing());
    }
}
