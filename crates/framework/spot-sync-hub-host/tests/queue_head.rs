//! The queue head the serve cache publishes (WP-AI-07.T16): the newest serve sequence the relay
//! assigned for a destination, deeper than the published window, so a reader's "expected position"
//! is exact. A core that cannot say publishes no head and the reader falls back to the window.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;

use spot_core::SystemClock;
use spot_sync_hub_host::ServeCache;
use spot_sync_hub_host::driver::{DriveSettings, HubCore, round};
use spot_sync_protocol::{Record, RecordName};

fn record(seq: u64) -> Record {
    Record::new(
        RecordName::new(&format!("bundle-{seq}")).unwrap(),
        1,
        seq,
        "{}",
    )
    .unwrap()
}

/// A core with one queue per destination: `queued` items numbered from `first`, of which the
/// window shows the oldest ones.
struct Core {
    queues: BTreeMap<String, (u64, u64)>,
    reports_head: bool,
}

impl HubCore for Core {
    fn tick(&mut self, _now_ms: i64) -> Result<(), String> {
        Ok(())
    }

    fn window(&mut self, dest: &str, max: usize) -> Result<Vec<Record>, String> {
        let Some((first, last)) = self.queues.get(dest).copied() else {
            return Ok(Vec::new());
        };
        Ok((first..=last).take(max).map(record).collect())
    }

    fn ack_served(&mut self, _dest: &str, _through: u64, _now_ms: i64) -> Result<(), String> {
        Ok(())
    }

    fn destinations(&self) -> Vec<String> {
        self.queues.keys().cloned().collect()
    }

    fn head_serve(&mut self, dest: &str) -> Option<u64> {
        if !self.reports_head {
            return None;
        }
        self.queues.get(dest).map(|(_, last)| *last)
    }
}

fn settings(window: usize) -> DriveSettings {
    DriveSettings {
        window,
        ..DriveSettings::default()
    }
}

#[test]
fn the_cache_holds_a_head_per_destination_only_once_one_was_published() {
    let (cache, _acks) = ServeCache::new();
    assert_eq!(cache.head("branch_01"), None);
    cache.publish_head("branch_01", 40);
    cache.publish_head("branch_02", 7);
    assert_eq!(cache.head("branch_01"), Some(40));
    assert_eq!(cache.head("branch_02"), Some(7));
    // A later round replaces it, even with a lower number (a rebuilt queue).
    cache.publish_head("branch_01", 12);
    assert_eq!(cache.head("branch_01"), Some(12));
    assert_eq!(cache.head("branch_03"), None);
}

#[test]
fn a_round_publishes_the_head_beyond_the_window() {
    let (cache, mut acks) = ServeCache::new();
    let mut core = Core {
        queues: BTreeMap::from([("branch_01".to_owned(), (11, 60))]),
        reports_head: true,
    };
    let clock = SystemClock::new();
    round(&mut core, &cache, &mut acks, &clock, settings(5), &|_| {});
    // The window shows five items; the queue holds fifty.
    assert_eq!(cache.depth("branch_01"), 5);
    assert_eq!(cache.head("branch_01"), Some(60));
}

#[test]
fn a_core_that_cannot_say_publishes_no_head() {
    let (cache, mut acks) = ServeCache::new();
    let mut core = Core {
        queues: BTreeMap::from([("branch_01".to_owned(), (1, 9))]),
        reports_head: false,
    };
    let clock = SystemClock::new();
    round(&mut core, &cache, &mut acks, &clock, settings(5), &|_| {});
    assert_eq!(cache.depth("branch_01"), 5);
    assert_eq!(cache.head("branch_01"), None);
}

#[test]
fn a_boxed_core_forwards_the_head() {
    let (cache, mut acks) = ServeCache::new();
    let core = Core {
        queues: BTreeMap::from([("branch_01".to_owned(), (1, 30))]),
        reports_head: true,
    };
    let mut boxed: Box<dyn HubCore> = Box::new(core);
    let clock = SystemClock::new();
    round(&mut boxed, &cache, &mut acks, &clock, settings(5), &|_| {});
    assert_eq!(cache.head("branch_01"), Some(30));
}
