//! The memory double against the shared cases, plus the rules that hold for every type.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::sync::Arc;

use common::conformance::*;
use spot_store_state::*;

#[test]
fn the_memory_double_passes_the_shared_cases() {
    run_all(&MemoryStoreState::new());
}

#[test]
fn a_fresh_store_per_case_also_passes() {
    for (_, case) in cases() {
        case(&MemoryStoreState::new());
    }
}

#[test]
fn debug_and_display_hide_key_hashes_and_summaries() {
    let secret = KeyHash::from_bytes([0xAB; 32]);
    let text = format!("{secret:?}");
    assert!(
        !text.contains("171") && !text.to_lowercase().contains("ab"),
        "{text}"
    );
    let record = KeyRecord {
        publisher_id: "p".into(),
        key_id: 1,
        key_hash: secret,
        revoked_at: None,
    };
    let text = format!("{record:?}");
    assert!(!text.contains("171") && !text.contains("0xab"), "{text}");

    let summary = ReviewSummary::new(r#"{"secret_finding":"SUMMARYTEXT"}"#.to_owned()).unwrap();
    let mut review = review("dbg_rv", "dbg_app", "1.0.0", 1);
    review.summary = Some(summary.clone());
    for text in [format!("{summary:?}"), format!("{review:?}")] {
        assert!(!text.contains("SUMMARYTEXT"), "{text}");
    }
    let list = IssuedList {
        generation: 1,
        issued_at: 1,
        next_update: 2,
        body_json: "{\"BODYTEXT\":1}".into(),
    };
    assert!(!format!("{list:?}").contains("BODYTEXT"));

    let store = MemoryStoreState::new();
    store
        .issue_publisher_key("dbg_pub", secret, "k", 1)
        .unwrap();
    let error = store
        .issue_publisher_key("dbg_pub", secret, "k", 1)
        .unwrap_err();
    for text in [format!("{error}"), format!("{error:?}")] {
        assert!(!text.contains("171") && !text.contains("0xab"), "{text}");
    }
}

#[test]
fn racing_threads_get_distinct_generations_and_one_listing_winner() {
    let store = Arc::new(MemoryStoreState::new());
    let name = app("race_app");
    let v = version("1.0.0");
    store
        .set_listing(&name, &v, ListingState::PendingReview, "a", 1)
        .unwrap();
    let mut handles = Vec::new();
    for i in 0..8u32 {
        let store = Arc::clone(&store);
        let (name, v) = (name.clone(), v.clone());
        handles.push(std::thread::spawn(move || {
            let generations: Vec<u64> =
                (0..50).map(|_| store.next_generation(1).unwrap()).collect();
            let target = if i % 2 == 0 {
                ListingState::Listed
            } else {
                ListingState::Rejected
            };
            (
                generations,
                store.set_listing(&name, &v, target, "t", 2).is_ok(),
            )
        }));
    }
    let mut all = Vec::new();
    let mut winners = 0;
    for handle in handles {
        let (generations, won) = handle.join().unwrap();
        assert!(generations.windows(2).all(|w| w[0] < w[1]));
        all.extend(generations);
        winners += usize::from(won);
    }
    all.sort_unstable();
    all.dedup();
    assert_eq!(all.len(), 400, "no generation was handed out twice");
    assert_eq!(winners, 1);
}
