//! The ranges and the naming path over them: restart without reuse, exhaustion with the
//! documented code, a stale or overlapping grant refused, series routing.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

mod common;

use std::future::Future;

use common::*;
use spot_edge_offline_host::carrier::LeaseAnswerMsg;
use spot_edge_offline_host::ranges::{Installed, RANGE_REFUSED, request_id};
use spot_edge_offline_host::sequences::SwitchedSequences;
use spot_edge_offline_host::series::{RangeView, SERIES_EXHAUSTED};
use spot_storage_contract::{ErrorKind, Sequences, SeriesKey, StorageResult};

fn answer(from: u64, to: u64, lease_id: u64) -> LeaseAnswerMsg {
    LeaseAnswerMsg::Granted {
        request_id: "r".to_owned(),
        series: "INV".to_owned(),
        from,
        to,
        lease_id,
    }
}

fn key() -> SeriesKey {
    SeriesKey::new("INV").unwrap()
}

/// The site's own allocator, for the series that are not leased.
#[derive(Default)]
struct Own(u64);

impl Sequences for Own {
    fn allocate(
        &mut self,
        _series: &SeriesKey,
        count: u32,
    ) -> impl Future<Output = StorageResult<u64>> + Send {
        let first = self.0 + 1;
        self.0 += u64::from(count);
        std::future::ready(Ok(first))
    }

    fn set_next(
        &mut self,
        _series: &SeriesKey,
        next: u64,
    ) -> impl Future<Output = StorageResult<()>> + Send {
        self.0 = next.saturating_sub(1);
        std::future::ready(Ok(()))
    }
}

#[test]
fn the_request_id_is_stable_until_a_range_is_installed() {
    let ranges = book(1_000, memory_store());
    assert_eq!(
        ranges.request_for("INV").unwrap().request_id,
        request_id("INV", 0)
    );
    assert_eq!(
        ranges.request_for("INV").unwrap().request_id,
        request_id("INV", 0)
    );
    assert!(ranges.request_for("PO").is_none());
    assert_eq!(
        ranges.install(&answer(1, 1_000, 1)).unwrap(),
        Installed::New
    );
    assert_eq!(
        ranges.request_for("INV").unwrap().request_id,
        request_id("INV", 1_000)
    );
    // The same answer again (a resend answered twice) changes nothing.
    assert_eq!(
        ranges.install(&answer(1, 1_000, 1)).unwrap(),
        Installed::AlreadyHeld
    );
}

#[test]
fn bad_answers_install_nothing() {
    let ranges = book(1_000, memory_store());
    ranges.install(&answer(1, 1_000, 1)).unwrap();
    for bad in [
        answer(500, 1_500, 2),
        answer(1_000, 2_000, 3),
        answer(1, 1_000, 9),
    ] {
        let error = ranges.install(&bad).unwrap_err();
        assert_eq!(error.code, RANGE_REFUSED);
    }
    let other_series = LeaseAnswerMsg::Granted {
        request_id: "r".to_owned(),
        series: "PO".to_owned(),
        from: 1,
        to: 10,
        lease_id: 4,
    };
    assert!(ranges.install(&other_series).is_err());
    assert!(
        ranges
            .install(&LeaseAnswerMsg::Refused {
                request_id: "r".to_owned(),
                code: "x".to_owned()
            })
            .is_err()
    );
    assert_eq!(ranges.last_held("INV"), 1_000);
}

#[tokio::test]
async fn naming_numbers_come_from_the_lease_and_a_restart_never_reuses_one() {
    let store = memory_store();
    let ranges = book(1_000, std::sync::Arc::clone(&store));
    ranges.install(&answer(1_001, 2_000, 2)).unwrap();
    let mut names = SwitchedSequences::new(Own::default(), ranges.sequences(), ["INV".to_owned()]);
    let mut taken = Vec::new();
    for _ in 0..500 {
        taken.push(names.allocate(&key(), 1).await.unwrap());
    }
    assert_eq!(taken.first(), Some(&1_001));
    assert_eq!(taken.last(), Some(&1_500));
    assert_eq!(ranges.remaining("INV"), 500);

    // The process restarts: the book is reopened from the store and continues after 1_500.
    let reopened = book(1_000, store);
    let mut names =
        SwitchedSequences::new(Own::default(), reopened.sequences(), ["INV".to_owned()]);
    let next = names.allocate(&key(), 1).await.unwrap();
    assert_eq!(next, 1_501);
    assert!(!taken.contains(&next));
}

#[tokio::test]
async fn an_exhausted_series_fails_retryably_with_the_code_and_never_reuses() {
    let ranges = book(3, memory_store());
    ranges.install(&answer(1, 3, 1)).unwrap();
    let mut names = SwitchedSequences::new(Own::default(), ranges.sequences(), ["INV".to_owned()]);
    for expected in 1..=3 {
        assert_eq!(names.allocate(&key(), 1).await.unwrap(), expected);
    }
    for _ in 0..3 {
        let error = names.allocate(&key(), 1).await.unwrap_err();
        assert_eq!(error.kind, ErrorKind::Unavailable);
        assert!(error.explanation.contains(SERIES_EXHAUSTED));
        assert!(error.explanation.contains("INV"));
    }
    assert_eq!(ranges.exhausted(), vec!["INV".to_owned()]);
    // A refill continues above, never from a used number.
    ranges.install(&answer(4, 6, 2)).unwrap();
    assert_eq!(names.allocate(&key(), 1).await.unwrap(), 4);
}

#[tokio::test]
async fn other_series_use_the_sites_own_allocator_and_set_next_is_refused_on_a_lease() {
    let ranges = book(1_000, memory_store());
    ranges.install(&answer(1, 1_000, 1)).unwrap();
    let mut names = SwitchedSequences::new(Own::default(), ranges.sequences(), ["INV".to_owned()]);
    let po = SeriesKey::new("PO").unwrap();
    assert!(!names.is_leased("PO"));
    assert_eq!(names.allocate(&po, 1).await.unwrap(), 1);
    assert_eq!(names.allocate(&po, 1).await.unwrap(), 2);
    assert!(names.set_next(&po, 10).await.is_ok());
    let refused = names.set_next(&key(), 10).await.unwrap_err();
    assert_eq!(refused.kind, ErrorKind::Unsupported);
}
