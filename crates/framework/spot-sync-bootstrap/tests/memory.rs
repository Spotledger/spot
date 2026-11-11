#![allow(clippy::unwrap_used, clippy::expect_used)]
//! A large snapshot is streamed: one planning pass, then chunk-sized fetches.

mod common;

use common::*;
use spot_sync_bootstrap::{Bootstrap, Budget, StartOptions};
use spot_sync_closure::Limits;

#[test]
fn two_hundred_thousand_records_stream_with_chunk_sized_reads() {
    let records = (0..200_000)
        .map(|i| rec("Customer", &format!("c{i:06}"), 1, &[]))
        .collect();
    let src = source(records);
    let counting = Counting::new(&src);
    let limits = Limits::default();
    let mut s = Bootstrap::start(
        &counting,
        seams(&SCOPE),
        limits,
        &node("n1"),
        hub(),
        StartOptions::default(),
    )
    .unwrap();
    assert_eq!(counting.scans.get(), 1);
    assert_eq!(counting.scanned.get(), 200_000);

    let (mut chunks, mut records, mut last) = (0_usize, 0_usize, None);
    while let Some(c) = s.next_chunk(Budget::of(&limits)).unwrap() {
        assert!(c.bytes <= limits.max_bytes);
        let n: usize = c.bundles.iter().map(|b| b.records.len()).sum();
        assert!(n <= limits.max_records);
        records += n;
        chunks += 1;
        last = c.resume_from;
    }
    assert_eq!(records, 200_000);
    assert_eq!(chunks, 400);
    assert_eq!(last, Some(POSITION + 1));
    // The session never asked the snapshot for more than one chunk's records at a time, and
    // never scanned again.
    assert!(counting.max_fetch.get() <= limits.max_records);
    assert_eq!(counting.scans.get(), 1);
    assert_eq!(counting.fetched.get(), 200_000);
}
