//! The rate limit as a property. Test code may unwrap what it built itself.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use proptest::prelude::*;
use spot_sync_healing::{HealingDriver, HealingSettings, MemoryJournal, RATE_WINDOW};

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// In any one-minute window, no more repairs start than the global limit, and none more
    /// than the per-node limit against one node, whatever the findings and the step times.
    #[test]
    fn executed_repairs_in_any_minute_never_exceed_the_limits(
        global in 1_u32..40,
        per_node in 1_u32..8,
        gaps in proptest::collection::vec(0_u64..30_000, 1..40),
        findings_per_node in 1_usize..30,
        nodes in 1_usize..8,
        fail in any::<bool>(),
    ) {
        let settings = HealingSettings {
            max_repairs_per_minute: global,
            max_repairs_per_node_per_minute: per_node,
            max_in_flight: 100_000,
            ..HealingSettings::default()
        };
        let script = if fail { Script::failing("down") } else { Script::default() };
        let mut driver = HealingDriver::new(script, MemoryJournal::new(), settings).unwrap();
        let observations: Vec<_> = (0..nodes)
            .map(|n| obs(&format!("n{n}"), (0..findings_per_node).map(|i| missing(&format!("R{i}"))).collect()))
            .collect();

        let mut now = 0_u64;
        let mut starts: Vec<(u64, String)> = Vec::new();
        for gap in gaps {
            now += gap;
            for attempt in driver.step(&observations, ms(now)) {
                starts.push((attempt.at.as_u64(), attempt.node));
            }
        }
        let window = RATE_WINDOW.as_u64();
        for (t, _) in &starts {
            let in_window = |t0: u64, u: u64| u >= t0 && u < t0 + window;
            let all = starts.iter().filter(|(u, _)| in_window(*t, *u)).count();
            prop_assert!(all <= global as usize, "{all} > {global}");
            for n in 0..nodes {
                let node = format!("n{n}");
                let c = starts.iter().filter(|(u, name)| in_window(*t, *u) && *name == node).count();
                prop_assert!(c <= per_node as usize, "{c} > {per_node}");
            }
        }
    }
}
