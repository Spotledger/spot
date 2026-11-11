#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Clock skew (ADR-065 section 7): node clocks ten minutes ahead or behind, and a clock that
//! jumps. The hub's stale-document check (`spot-sync-freshness`) must neither miss a finding nor
//! invent one, because the node corrects its stamp with an offset it estimated from the hub, and
//! the freshness tracker reads only the monotonic clock.

use spot_sync_chaos::{World, WorldConfig};
use spot_sync_freshness::{
    Bounds, ClockSample, FreshnessSettings, FreshnessTracker, MandatoryEvent, StampInput,
    SyncStamp, build_stamp, find_violations_clamped, hub_offset_estimate,
};

const TEN_MINUTES: i64 = 600_000;
const DAY: i64 = 86_400_000;

fn world(seed: u64) -> World {
    World::build(WorldConfig {
        nodes: 5,
        ..WorldConfig::seeded(seed)
    })
    .unwrap()
}

/// The node's NTP-style exchange with the hub over a link with a 5 ms one-way delay.
fn estimate(world: &mut World, node: usize) -> i64 {
    let node_sent = world.clock().node_wall_ms(node);
    world.clock_mut().advance_by(5);
    let hub_received = world.clock().hub_wall_ms();
    world.clock_mut().advance_by(1);
    let hub_sent = world.clock().hub_wall_ms();
    world.clock_mut().advance_by(5);
    let node_received = world.clock().node_wall_ms(node);
    hub_offset_estimate(&[ClockSample {
        node_sent,
        hub_received,
        hub_sent,
        node_received,
    }])
    .unwrap()
    .offset_ms
}

/// The stamp a node builds when it posts a document now, with the offset it believes.
fn stamp(world: &World, node: usize, offset_ms: i64, applied_prefix: u64) -> SyncStamp {
    let mut tracker = FreshnessTracker::new();
    tracker.confirm("tax", 0);
    build_stamp(
        &StampInput {
            node: world.node_id(node),
            stamped_at_millis: world.clock().node_wall_ms(node),
            hub_offset_ms: offset_ms,
            applied_prefix,
            classes: &["tax".to_owned()],
            now_monotonic_millis: world.clock().elapsed_ms(),
            metadata_version: 1,
            software_version: "1.0.0",
        },
        &tracker,
        &FreshnessSettings::default(),
        None,
        None,
    )
}

fn event(position: u64, published_at: i64) -> MandatoryEvent {
    MandatoryEvent {
        position,
        published_at,
        effective_from: None,
        record: format!("rate-{position}"),
    }
}

/// The mandatory events published at known hub times around "now" (the posting moment), then
/// the findings the hub reports and the ones that are true.
struct Outcome {
    found: Vec<u64>,
    truth: Vec<u64>,
    clamped: bool,
}

fn post(world: &mut World, node: usize, offset_ms: i64, applied_prefix: u64) -> Outcome {
    let now = world.clock().hub_wall_ms();
    // Published well before, a moment before, a moment after and well after the posting.
    let events = vec![
        event(10, now - DAY),
        event(11, now - 3 * 60_000),
        event(12, now - 2_000),
        event(13, now + 2_000),
        event(14, now + 3 * 60_000),
        event(15, now - 30_000),
    ];
    let doc = stamp(world, node, offset_ms, applied_prefix);
    let bounds = Bounds::with_max_age(7 * DAY).unwrap();
    let findings = find_violations_clamped("INV-1", &doc, now, &bounds, &events).unwrap();
    let truth: Vec<u64> = events
        .iter()
        .filter(|e| e.published_at <= now && e.position > applied_prefix)
        .map(|e| e.position)
        .collect();
    let mut found: Vec<u64> = findings
        .iter()
        .map(|f| {
            f.mandatory_record
                .trim_start_matches("rate-")
                .parse()
                .unwrap()
        })
        .collect();
    found.sort_unstable();
    let mut truth = truth;
    truth.sort_unstable();
    Outcome {
        found,
        truth,
        clamped: findings.iter().any(|f| f.clamped),
    }
}

#[test]
fn skewed_clocks_with_an_estimated_offset_miss_and_invent_nothing() {
    for (seed, skew) in [
        (1, 0),
        (2, TEN_MINUTES),
        (3, -TEN_MINUTES),
        (4, 1),
        (5, -1),
        (6, 7 * 60_000 + 123),
    ] {
        let mut world = world(seed);
        for node in 0..5 {
            world.skew(node, skew);
        }
        world.clock_mut().advance_by(3 * 60_000);
        let offset = estimate(&mut world, 0);
        assert_eq!(offset, -skew, "SEED={seed} the offset estimate");
        let out = post(&mut world, 0, offset, 12);
        assert_eq!(out.found, out.truth, "SEED={seed} skew={skew}");
        assert!(
            !out.clamped,
            "SEED={seed} a correct estimate needs no clamp"
        );
    }
}

#[test]
fn a_node_that_ignores_its_skew_would_be_wrong_so_the_test_can_tell() {
    // A ten-minutes-ahead node that trusts its own clock claims a time ten minutes late and is
    // charged with events that had not been published; one ten minutes behind misses them.
    let mut ahead = world(7);
    ahead.skew(0, TEN_MINUTES);
    ahead.clock_mut().advance_by(5 * 60_000);
    let naive = post(&mut ahead, 0, 0, 12);
    assert_ne!(naive.found, naive.truth);

    let mut behind = world(8);
    behind.skew(0, -TEN_MINUTES);
    behind.clock_mut().advance_by(5 * 60_000);
    let naive = post(&mut behind, 0, 0, 12);
    assert_ne!(naive.found, naive.truth);
}

#[test]
fn a_wall_clock_jump_is_noticed_from_the_monotonic_clock_and_the_offset_re_estimated() {
    for (seed, jump) in [(1, TEN_MINUTES), (2, -TEN_MINUTES), (3, 90_000)] {
        let mut world = world(seed);
        world.skew(1, 4_000);
        world.clock_mut().advance_by(60_000);
        let before_wall = world.clock().node_wall_ms(1);
        let before_mono = world.clock().elapsed_ms();
        let mut offset = estimate(&mut world, 1);
        world.clock_mut().advance_by(30_000);
        world.jump(1, jump);
        // The wall clock moved by more than the monotonic one did: the estimate is void.
        let wall_moved = world.clock().node_wall_ms(1) - before_wall;
        let mono_moved = i64::try_from(world.clock().elapsed_ms() - before_mono).unwrap();
        assert_ne!(wall_moved, mono_moved, "SEED={seed} the jump left no trace");
        offset = if wall_moved == mono_moved {
            offset
        } else {
            estimate(&mut world, 1)
        };
        let out = post(&mut world, 1, offset, 12);
        assert_eq!(out.found, out.truth, "SEED={seed} jump={jump}");
    }
}

#[test]
fn a_stale_estimate_after_a_forward_jump_is_clamped_by_the_hub() {
    let mut world = world(4);
    world.clock_mut().advance_by(60_000);
    let offset = estimate(&mut world, 2);
    world.jump(2, TEN_MINUTES);
    // The node does not notice: it claims a time ten minutes in the hub's future.
    let out = post(&mut world, 2, offset, 12);
    assert!(out.clamped, "the hub should have bounded the claim");
    // Bounded by the receipt time plus the two second tolerance, never ten minutes: the extra
    // finding, if any, is an event published inside that tolerance and nothing else.
    for position in &out.found {
        assert!(
            out.truth.contains(position) || *position == 13,
            "position {position} was invented outside the tolerance"
        );
    }
    for position in &out.truth {
        assert!(
            out.found.contains(position),
            "position {position} was missed"
        );
    }
}

#[test]
fn a_stale_estimate_after_a_backward_jump_misses_a_finding_so_a_node_must_re_estimate() {
    let mut world = world(5);
    world.clock_mut().advance_by(60_000);
    let offset = estimate(&mut world, 3);
    world.jump(3, -TEN_MINUTES);
    let out = post(&mut world, 3, offset, 12);
    assert!(
        out.found.len() < out.truth.len(),
        "this is why the node re-estimates after a jump: the claim moved into the past"
    );
}

#[test]
fn staleness_reads_only_the_monotonic_clock_whatever_the_wall_clock_does() {
    let settings = FreshnessSettings::default();
    for (seed, skew) in [(1, TEN_MINUTES), (2, -TEN_MINUTES)] {
        let mut world = world(seed);
        world.skew(0, skew);
        let mut tracker = FreshnessTracker::new();
        tracker.confirm("tax", world.clock().elapsed_ms());
        // 47 hours of virtual time, then a wall-clock jump of ten minutes either way.
        world.clock_mut().advance_by(47 * 3_600_000);
        world.jump(0, skew);
        assert!(
            !tracker.is_stale("tax", world.clock().elapsed_ms(), &settings),
            "SEED={seed} a wall-clock jump made a fresh node stale"
        );
        // Two more hours really do make it stale, whatever its wall clock says.
        world.clock_mut().advance_by(2 * 3_600_000);
        world.jump(0, -skew * 2);
        assert!(
            tracker.is_stale("tax", world.clock().elapsed_ms(), &settings),
            "SEED={seed} a wall-clock jump hid a stale node"
        );
    }
}
