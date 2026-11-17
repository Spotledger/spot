//! The serialised form of a hold set: sequence-of-entries maps, a golden file, round trips and
//! the three refusals on load.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]
mod common;

use common::{decodes, report};
use serde_json::{Value, json};
use spot_sync_skew::{
    HoldReason, HoldSet, Outgoing, RecordRef, STATE_DISORDER, STATE_DUPLICATE_KEY,
    STATE_OVER_LIMIT, filter_outgoing, on_undecodable, release_for_upgrade,
};

const GOLDEN: &str = include_str!("golden/hold_set.json");

fn rec(id: &str, version: u64) -> RecordRef {
    RecordRef {
        entity: "Invoice".into(),
        id: id.into(),
        version,
    }
}

fn out(position: u64, id: &str, version: u64, major: u32) -> Outgoing {
    Outgoing {
        position,
        record: rec(id, version),
        topic: "invoice".into(),
        major,
    }
}

/// Two receiver-side holds (`c`, `d`) and two hub-side queues (`a` with two entries, `b` with one).
fn sample() -> HoldSet {
    let node = decodes(report(5), "invoice", &[1]);
    let mut set = HoldSet::with_limit(1000);
    set.insert(on_undecodable(
        rec("d", 4),
        HoldReason::MetadataTooLow { required: 12 },
    ));
    set.insert(on_undecodable(
        rec("c", 1),
        HoldReason::MetadataTooLow { required: 11 },
    ));
    let batch = [out(1, "a", 1, 2), out(2, "b", 1, 2), out(3, "a", 2, 1)];
    let result = filter_outgoing(&batch, &node, &mut set);
    assert!(result.send.is_empty());
    set
}

fn golden() -> Value {
    serde_json::from_str(GOLDEN).unwrap()
}

#[test]
fn the_json_form_is_pinned_by_the_golden_file() {
    let written = serde_json::to_value(sample()).unwrap();
    assert_eq!(written, golden());
}

#[test]
fn the_golden_file_loads_back_to_the_same_set() {
    let loaded: HoldSet = serde_json::from_str(GOLDEN).unwrap();
    assert_eq!(loaded, sample());
    assert_eq!(loaded.limit(), 1000);
    assert_eq!((loaded.holds().len(), loaded.pending_len()), (2, 3));
}

#[test]
fn entries_are_sorted_by_key_whatever_the_insertion_order() {
    let value = serde_json::to_value(sample()).unwrap();
    let ids = |field: &str| -> Vec<String> {
        value[field]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry[1].as_str().unwrap().to_owned())
            .collect()
    };
    assert_eq!(ids("holds"), ["c", "d"]);
    assert_eq!(ids("pending"), ["a", "b"]);
}

#[test]
fn a_text_round_trip_keeps_behaviour() {
    let node = decodes(report(6), "invoice", &[1, 2]);
    let mut original = sample();
    let text = serde_json::to_string(&original).unwrap();
    let mut restored: HoldSet = serde_json::from_str(&text).unwrap();
    assert_eq!(restored, original);
    // The restored set releases exactly what the original does.
    assert_eq!(
        release_for_upgrade(&node, &mut restored),
        release_for_upgrade(&node, &mut original)
    );
    assert_eq!(restored, original);
}

#[test]
fn an_empty_set_round_trips() {
    let text = serde_json::to_string(&HoldSet::new()).unwrap();
    let back: HoldSet = serde_json::from_str(&text).unwrap();
    assert_eq!(back, HoldSet::new());
}

#[test]
fn a_duplicate_key_is_refused_not_overwritten() {
    let mut doc = golden();
    let again = doc["holds"][0].clone();
    doc["holds"].as_array_mut().unwrap().push(again);
    let err = serde_json::from_value::<HoldSet>(doc)
        .unwrap_err()
        .to_string();
    assert!(err.contains(STATE_DUPLICATE_KEY), "{err}");

    let mut doc = golden();
    let again = doc["pending"][1].clone();
    doc["pending"].as_array_mut().unwrap().push(again);
    let err = serde_json::from_value::<HoldSet>(doc)
        .unwrap_err()
        .to_string();
    assert!(err.contains(STATE_DUPLICATE_KEY), "{err}");
}

#[test]
fn a_queue_out_of_position_order_is_refused() {
    let mut doc = golden();
    doc["pending"][0][2].as_array_mut().unwrap().reverse();
    let err = serde_json::from_value::<HoldSet>(doc)
        .unwrap_err()
        .to_string();
    assert!(err.contains(STATE_DISORDER), "{err}");

    // Equal positions are not strictly ascending either.
    let mut doc = golden();
    let first = doc["pending"][0][2][0].clone();
    doc["pending"][0][2][1] = first;
    let err = serde_json::from_value::<HoldSet>(doc)
        .unwrap_err()
        .to_string();
    assert!(err.contains(STATE_DISORDER), "{err}");
}

#[test]
fn a_set_over_its_limit_is_refused() {
    let mut doc = golden();
    doc["limit"] = json!(2);
    let err = serde_json::from_value::<HoldSet>(doc)
        .unwrap_err()
        .to_string();
    assert!(err.contains(STATE_OVER_LIMIT), "{err}");
    // At the limit it loads.
    let mut doc = golden();
    doc["limit"] = json!(3);
    assert!(serde_json::from_value::<HoldSet>(doc).is_ok());
}

#[test]
fn the_old_map_form_is_not_accepted() {
    let doc = json!({ "holds": {}, "pending": {}, "limit": 10 });
    assert!(serde_json::from_value::<HoldSet>(doc).is_err());
}
