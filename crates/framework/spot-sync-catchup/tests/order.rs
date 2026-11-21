#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Dependency ordering (ADR-064 section 8): targets before referrers, cycles in one group, a
//! deterministic tie-break, and the regression that position order is the wrong answer.

mod common;

use common::{MapReferences, rid, state};
use spot_sync_catchup::{Image, order_by_dependency};

/// The image of a fixture record at `position`.
fn image(position: u64, id: &str) -> Image {
    state(position, id, 1).image().unwrap()
}

/// The ids of one group.
fn ids(group: &[spot_sync_catchup::RecordId]) -> Vec<&str> {
    group.iter().map(|record| record.id.as_str()).collect()
}

#[test]
fn a_record_comes_after_a_newer_record_it_references() {
    // The invoice's image is at 105 and references the party whose latest image is at 109. In
    // position order the invoice would be applied first and the deferred foreign key would fail;
    // dependency order applies the party first, even though its image is newer.
    let images = vec![image(105, "invoice"), image(109, "party")];
    let refs = MapReferences::new().link("invoice", "party");
    let groups = order_by_dependency(&images, &refs).unwrap();
    assert_eq!(groups.len(), 2);
    assert_eq!(ids(&groups[0]), vec!["party"]);
    assert_eq!(ids(&groups[1]), vec!["invoice"]);

    // The regression, stated as data: applying in position order would give the wrong answer.
    let mut by_position: Vec<(u64, &str)> = images
        .iter()
        .map(|image| (image.position, image.record.id.as_str()))
        .collect();
    by_position.sort_unstable();
    let by_position: Vec<&str> = by_position.into_iter().map(|(_, id)| id).collect();
    assert_eq!(by_position, vec!["invoice", "party"]);
    let ordered: Vec<&str> = groups.iter().flat_map(|group| ids(group)).collect();
    assert_eq!(ordered, vec!["party", "invoice"]);
    assert_ne!(ordered, by_position);
}

#[test]
fn a_chain_is_ordered_from_the_target_down() {
    let images = vec![image(101, "c"), image(102, "b"), image(103, "a")];
    let refs = MapReferences::new().link("a", "b").link("b", "c");
    let groups = order_by_dependency(&images, &refs).unwrap();
    assert_eq!(groups.len(), 3);
    assert_eq!(ids(&groups[0]), vec!["c"]);
    assert_eq!(ids(&groups[1]), vec!["b"]);
    assert_eq!(ids(&groups[2]), vec!["a"]);
}

#[test]
fn a_cycle_stays_in_one_group_and_its_referrers_follow() {
    let images = vec![image(101, "a"), image(102, "b"), image(103, "c")];
    let refs = MapReferences::new()
        .link("a", "b")
        .link("b", "a")
        .link("c", "a");
    let groups = order_by_dependency(&images, &refs).unwrap();
    assert_eq!(groups.len(), 2);
    assert_eq!(ids(&groups[0]), vec!["a", "b"]);
    assert_eq!(ids(&groups[1]), vec!["c"]);
}

#[test]
fn independent_records_share_a_group_in_position_order() {
    let images = vec![image(102, "b"), image(101, "a"), image(103, "c")];
    let groups = order_by_dependency(&images, &MapReferences::new()).unwrap();
    assert_eq!(groups.len(), 1);
    assert_eq!(ids(&groups[0]), vec!["a", "b", "c"]);
}

#[test]
fn a_reference_outside_the_image_set_is_ignored() {
    // The party is out of the node's scope (or already there): the invoice can go alone.
    let images = vec![image(101, "invoice")];
    let refs = MapReferences::new().link("invoice", "party");
    let groups = order_by_dependency(&images, &refs).unwrap();
    assert_eq!(groups.len(), 1);
    assert_eq!(ids(&groups[0]), vec!["invoice"]);
}

#[test]
fn a_self_reference_is_not_a_cycle() {
    let images = vec![image(101, "a"), image(102, "b")];
    let refs = MapReferences::new().link("a", "a").link("b", "a");
    let groups = order_by_dependency(&images, &refs).unwrap();
    assert_eq!(groups.len(), 2);
    assert_eq!(ids(&groups[0]), vec!["a"]);
    assert_eq!(ids(&groups[1]), vec!["b"]);
}

#[test]
fn the_same_record_twice_is_refused() {
    let images = vec![image(101, "a"), image(102, "a")];
    let error = order_by_dependency(&images, &MapReferences::new()).unwrap_err();
    assert_eq!(error.code(), "sync_catchup.duplicate_image");
    assert_eq!(rid("a").entity, "doc");
}

#[test]
fn an_empty_image_set_orders_to_nothing() {
    let groups = order_by_dependency(&[], &MapReferences::new()).unwrap();
    assert!(groups.is_empty());
}

#[test]
fn a_deep_chain_does_not_exhaust_the_stack() {
    // 20,000 records in one chain: the orderer is iterative on purpose.
    let count = 20_000u64;
    let mut images = Vec::new();
    let mut refs = MapReferences::new();
    for i in 0..count {
        images.push(image(i, &format!("r{i}")));
        if i > 0 {
            refs = refs.link(&format!("r{i}"), &format!("r{}", i - 1));
        }
    }
    let groups = order_by_dependency(&images, &refs).unwrap();
    assert_eq!(groups.len(), count as usize);
    assert_eq!(groups[0][0].id, "r0");
    assert_eq!(groups[groups.len() - 1][0].id, format!("r{}", count - 1));
}
