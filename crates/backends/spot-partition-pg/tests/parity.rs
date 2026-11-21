//! The in-memory stores are the contract: the same table of cases runs against both them and
//! PostgreSQL (`SPOT_TEST_PG_URL`; skipped with a message when unset, a failure when
//! `SPOT_REQUIRE_PG=1`).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, missing_docs)]

mod common;

use common::{Case, node, request, run_on_both, scope, t};
use spot_partition::{PartitionError, PartitionRegistry, RangeAllocator};
use spot_storage_contract::SeriesKey;

fn series(key: &str) -> SeriesKey {
    SeriesKey::new(key).unwrap()
}

fn map_at_before_inside_and_after_an_interval(
    registry: &dyn PartitionRegistry,
    _: &mut dyn RangeAllocator,
) {
    let version = registry
        .assign(request("W7", "edge-khi#branch_07", 100, Some(200)))
        .unwrap();
    assert_eq!(version, 1);
    let w7 = scope("W7");
    assert_eq!(registry.map_at(t(99)).owner(&w7), None);
    assert_eq!(
        registry.map_at(t(100)).owner(&w7),
        Some(&node("edge-khi#branch_07"))
    );
    assert_eq!(registry.map_at(t(199)).version(&w7), Some(1));
    assert_eq!(
        registry.map_at(t(200)).owner(&w7),
        None,
        "the end is exclusive"
    );
    assert_eq!(registry.map_at(t(5_000)).owner(&w7), None);
}

fn an_open_ended_assignment_never_stops(
    registry: &dyn PartitionRegistry,
    _: &mut dyn RangeAllocator,
) {
    registry.assign(request("W1", "hub", 0, None)).unwrap();
    assert_eq!(
        registry.map_at(t(10_000_000)).owner(&scope("W1")),
        Some(&node("hub"))
    );
    assert_eq!(registry.map_at(t(-1)).owner(&scope("W1")), None);
}

fn an_overlap_with_another_node_is_refused_and_leaves_nothing_behind(
    registry: &dyn PartitionRegistry,
    _: &mut dyn RangeAllocator,
) {
    registry
        .assign(request("W1", "edge-a", 0, Some(100)))
        .unwrap();
    let refused = registry
        .assign(request("W1", "edge-b", 50, Some(150)))
        .unwrap_err();
    assert_eq!(refused.code(), "partition.ownership_conflict");
    match refused {
        PartitionError::OwnershipConflict {
            owner, challenger, ..
        } => {
            assert_eq!(owner, "edge-a");
            assert_eq!(challenger, "edge-b");
        }
        other => panic!("expected a conflict, got {other:?}"),
    }
    assert_eq!(
        registry.history().len(),
        1,
        "a refusal writes no history row"
    );
    assert_eq!(
        registry.map_at(t(60)).owner(&scope("W1")),
        Some(&node("edge-a"))
    );
    // An open-ended challenger overlaps too.
    let open = registry
        .assign(request("W1", "edge-b", 90, None))
        .unwrap_err();
    assert_eq!(open.code(), "partition.ownership_conflict");
    // The refused attempt did not consume a version.
    assert_eq!(
        registry
            .assign(request("W1", "edge-b", 100, Some(200)))
            .unwrap(),
        2
    );
}

fn the_same_node_may_extend_and_adjacent_intervals_do_not_overlap(
    registry: &dyn PartitionRegistry,
    _: &mut dyn RangeAllocator,
) {
    registry
        .assign(request("W1", "edge-a", 0, Some(100)))
        .unwrap();
    // Same node, overlapping: allowed.
    assert_eq!(
        registry
            .assign(request("W1", "edge-a", 50, Some(150)))
            .unwrap(),
        2
    );
    // Another node starting exactly where the last one ends: allowed.
    assert_eq!(
        registry.assign(request("W1", "edge-b", 150, None)).unwrap(),
        3
    );
    assert_eq!(
        registry.map_at(t(149)).owner(&scope("W1")),
        Some(&node("edge-a"))
    );
    assert_eq!(
        registry.map_at(t(150)).owner(&scope("W1")),
        Some(&node("edge-b"))
    );
    // Other scope keys are independent.
    assert_eq!(
        registry.assign(request("W2", "edge-b", 0, None)).unwrap(),
        1
    );
}

fn an_interval_that_ends_before_it_starts_is_refused(
    registry: &dyn PartitionRegistry,
    _: &mut dyn RangeAllocator,
) {
    for to in [100, 50] {
        let error = registry
            .assign(request("W1", "edge-a", 100, Some(to)))
            .unwrap_err();
        assert_eq!(error.code(), "partition.invalid_argument");
    }
    assert!(registry.history().is_empty());
}

fn history_is_oldest_first_and_keeps_who_and_why(
    registry: &dyn PartitionRegistry,
    _: &mut dyn RangeAllocator,
) {
    let mut first = request("W1", "edge-a", 0, Some(10));
    first.actor = "alice".to_owned();
    first.reason = "branch opens".to_owned();
    first.at = t(1);
    registry.assign(first).unwrap();
    let mut second = request("W2", "edge-b", 5, None);
    second.actor = "bob".to_owned();
    second.reason = "move".to_owned();
    second.at = t(2);
    registry.assign(second).unwrap();
    let history = registry.history();
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].actor, "alice");
    assert_eq!(history[0].reason, "branch opens");
    assert_eq!(history[0].scope, scope("W1"));
    assert_eq!(history[0].effective_to, Some(t(10)));
    assert_eq!(history[0].at, t(1));
    assert_eq!(history[1].actor, "bob");
    assert_eq!(history[1].effective_to, None);
    assert_eq!(history[1].version, 1);
}

fn ranges_are_consecutive_and_disjoint_per_series(
    _: &dyn PartitionRegistry,
    allocator: &mut dyn RangeAllocator,
) {
    let a = allocator
        .lease(&series("SINV-"), &node("edge-a"), 100, t(1))
        .unwrap();
    let b = allocator
        .lease(&series("SINV-"), &node("edge-b"), 50, t(2))
        .unwrap();
    let other = allocator
        .lease(&series("PINV-"), &node("edge-a"), 10, t(3))
        .unwrap();
    assert_eq!((a.from, a.to), (1, 100));
    assert_eq!((b.from, b.to), (101, 150));
    assert_eq!((other.from, other.to), (1, 10), "series are independent");
    assert_eq!(b.node, node("edge-b"));
    let grants = allocator.grants();
    assert_eq!(grants.len(), 3);
    assert_eq!(grants[0].lease.from, 1);
    assert_eq!(grants[1].lease.from, 101);
    assert_eq!(grants[2].lease.series, series("PINV-"));
    assert_eq!(grants[1].granted_at, t(2));
}

fn a_zero_size_and_a_range_reaching_the_top_are_refused(
    _: &dyn PartitionRegistry,
    allocator: &mut dyn RangeAllocator,
) {
    let zero = allocator
        .lease(&series("S"), &node("edge-a"), 0, t(1))
        .unwrap_err();
    assert_eq!(zero.code(), "partition.invalid_argument");
    let big = allocator
        .lease(&series("S"), &node("edge-a"), u64::MAX - 1, t(2))
        .unwrap();
    assert_eq!((big.from, big.to), (1, u64::MAX - 1));
    let top = allocator
        .lease(&series("S"), &node("edge-a"), 1, t(3))
        .unwrap_err();
    assert_eq!(top.code(), "partition.invalid_range");
    let overflow = allocator
        .lease(&series("S"), &node("edge-a"), u64::MAX, t(4))
        .unwrap_err();
    assert_eq!(overflow.code(), "partition.invalid_range");
    assert_eq!(
        allocator.grants().len(),
        1,
        "a refused grant records nothing"
    );
}

#[test]
fn the_same_table_of_cases_passes_on_the_memory_store_and_on_postgres() {
    let cases: &[(&str, Case)] = &[
        (
            "map_at before, inside and after",
            map_at_before_inside_and_after_an_interval,
        ),
        (
            "open-ended assignment",
            an_open_ended_assignment_never_stops,
        ),
        (
            "overlap refused",
            an_overlap_with_another_node_is_refused_and_leaves_nothing_behind,
        ),
        (
            "same node and adjacent",
            the_same_node_may_extend_and_adjacent_intervals_do_not_overlap,
        ),
        (
            "invalid interval",
            an_interval_that_ends_before_it_starts_is_refused,
        ),
        ("history", history_is_oldest_first_and_keeps_who_and_why),
        (
            "consecutive ranges",
            ranges_are_consecutive_and_disjoint_per_series,
        ),
        (
            "zero size and the top",
            a_zero_size_and_a_range_reaching_the_top_are_refused,
        ),
    ];
    run_on_both(cases);
}
