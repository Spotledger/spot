//! The seam property: history pages followed by outbox pages are the whole stream, with no gap
//! and no duplicate, for random horizons, segment layouts and page sizes.
#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use std::sync::Arc;

use common::*;
use proptest::prelude::*;
use spot_event_stream_journal::{JournalHistory, Selector, check_seam};
use spot_journal_manifest::Manifest;
use spot_journal_store::MemoryStore;

/// Splits `1..=n` into consecutive ranges of the given sizes (the last takes the remainder).
fn ranges(n: u64, sizes: &[u64]) -> Vec<(u64, u64)> {
    let mut out = Vec::new();
    let mut next = 1;
    for size in sizes {
        if next > n {
            break;
        }
        let last = (next + size.max(&1) - 1).min(n);
        out.push((next, last));
        next = last + 1;
    }
    if next <= n {
        out.push((next, n));
    }
    out
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    #[test]
    fn history_then_outbox_is_the_whole_stream(
        n in 1u64..40,
        sizes in proptest::collection::vec(1u64..9, 1..6),
        horizon in 1u64..42,
        sealed_to in 0u64..40,
        history_page in 1usize..12,
        outbox_page in 1usize..12,
    ) {
        // The journal holds 1..=n: the first `sealed_to` positions sealed, the rest as chunks.
        let sealed_to = sealed_to.min(n);
        let store = Arc::new(MemoryStore::new());
        let built = build(
            Manifest::first(SOURCE, 1).expect("manifest"),
            &ranges(sealed_to, &sizes),
        );
        put_built(&store, &built);
        let mut next = sealed_to + 1;
        let mut batch = 1;
        while next <= n {
            let last = (next + 2).min(n);
            put_chunk(&store, 1, batch, next, last);
            next = last + 1;
            batch += 1;
        }

        // The outbox retains horizon..=n; history serves everything before it.
        let horizon = horizon.min(n + 1);
        let history_end = horizon - 1;
        let select: &Selector = &|_| true;
        let reader = reader(&store);
        let mut stream: Vec<u64> = Vec::new();
        let mut after = pos(0, 0);
        if history_end >= 1 {
            let through = Some(pos(1, history_end));
            loop {
                let page = block(reader.read_after(after, history_page, through, select))
                    .expect("history page");
                stream.extend(page.envelopes.iter().map(|e| e.position.seq));
                after = page.next_after;
                if page.reached_outbox {
                    break;
                }
            }
            prop_assert_eq!(after, pos(1, history_end));
            prop_assert!(check_seam(after, pos(1, horizon)).is_ok());
        }
        // The outbox serves from the position after the seam.
        let mut seq = history_end + 1;
        while seq <= n {
            let last = (seq + outbox_page as u64 - 1).min(n);
            stream.extend(seq..=last);
            seq = last + 1;
        }
        let want: Vec<u64> = (1..=n).collect();
        prop_assert_eq!(stream, want);
    }

    #[test]
    fn a_trim_during_the_call_fails_closed(history_end in 1u64..30, skipped in 2u64..10) {
        let first_retained = pos(1, history_end + skipped);
        prop_assert!(check_seam(pos(1, history_end), first_retained).is_err());
    }
}
