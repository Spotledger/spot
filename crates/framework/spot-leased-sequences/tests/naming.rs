//! Naming over a leased allocator: the full pattern renders offline, two branches with disjoint
//! leases never share a name, a yearly pattern resets per series key, and exhaustion surfaces as
//! a retryable storage error.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::BTreeSet;

use common::*;
use spot_app_api::prelude::Month;
use spot_leased_sequences::LeasedSequences;
use spot_naming::{GapPolicy, NamingError};
use spot_partition::{MemoryRangeAllocator, RangeAllocator};
use spot_storage_contract::ErrorKind;

/// Names one document on `seq` and returns the rendered name.
fn name_once(
    namer: &spot_naming::Namer,
    ctx: &spot_naming::NamingContext,
    seq: &mut LeasedSequences,
) -> String {
    let doctype = name(SERIES_DOCTYPE);
    block_on(namer.name_for(
        &doctype,
        &doc(SERIES_DOCTYPE),
        ctx,
        GapPolicy::Tolerant,
        seq,
    ))
    .unwrap()
    .as_str()
    .to_owned()
}

#[test]
fn a_leased_number_renders_the_full_hub_pattern() {
    let namer = namer();
    let doctype = name(SERIES_DOCTYPE);
    let now = at(2026, Month::June, 1, 12, 0);
    let ctx = ctx(now);
    let key = namer.series_key(&doctype, &ctx).unwrap();
    assert_eq!(key.as_str(), "INV-2026-");

    let mut hub = MemoryRangeAllocator::new();
    let lease = hub
        .lease(&key, &node("edge-khi#branch_07"), 5, now)
        .unwrap();
    let mut seq = LeasedSequences::new().with_lease(branch(lease)).unwrap();

    assert_eq!(name_once(&namer, &ctx, &mut seq), "INV-2026-00001");
    assert_eq!(name_once(&namer, &ctx, &mut seq), "INV-2026-00002");
    assert_eq!(seq.remaining(&key), 3);
}

#[test]
fn two_branches_with_disjoint_leases_never_share_a_name() {
    let namer = namer();
    let doctype = name(SERIES_DOCTYPE);
    let now = at(2026, Month::June, 1, 12, 0);
    let ctx = ctx(now);
    let key = namer.series_key(&doctype, &ctx).unwrap();

    let mut hub = MemoryRangeAllocator::new();
    let seven = hub
        .lease(&key, &node("edge-khi#branch_07"), 3, now)
        .unwrap();
    let nine = hub
        .lease(&key, &node("edge-khi#branch_09"), 3, now)
        .unwrap();
    assert_eq!((seven.from, seven.to), (1, 3));
    assert_eq!(
        (nine.from, nine.to),
        (4, 6),
        "the hub never overlaps ranges"
    );

    let mut branch_seven = LeasedSequences::new().with_lease(branch(seven)).unwrap();
    let mut branch_nine = LeasedSequences::new().with_lease(branch(nine)).unwrap();

    let mut names = BTreeSet::new();
    for _ in 0..3 {
        assert!(names.insert(name_once(&namer, &ctx, &mut branch_seven)));
    }
    for _ in 0..3 {
        assert!(names.insert(name_once(&namer, &ctx, &mut branch_nine)));
    }

    assert_eq!(
        names.into_iter().collect::<Vec<_>>(),
        (1..=6)
            .map(|n| format!("INV-2026-{n:05}"))
            .collect::<Vec<_>>(),
        "six distinct names across the two branches, with no gap and no duplicate"
    );
}

#[test]
fn a_yearly_pattern_resets_per_its_series_key() {
    let namer = namer();
    let doctype = name(SERIES_DOCTYPE);
    let june_2026 = ctx(at(2026, Month::June, 1, 12, 0));
    let january_2027 = ctx(at(2027, Month::January, 5, 9, 0));
    let key_2026 = namer.series_key(&doctype, &june_2026).unwrap();
    let key_2027 = namer.series_key(&doctype, &january_2027).unwrap();
    assert_eq!(key_2026.as_str(), "INV-2026-");
    assert_eq!(key_2027.as_str(), "INV-2027-");

    // The hub's cursor is per series, so each year's lease starts at 1 — the reset the hub-side
    // naming tests prove for the transactional allocator.
    let mut hub = MemoryRangeAllocator::new();
    let lease_2026 = hub
        .lease(&key_2026, &node("edge-khi#branch_07"), 2, june_2026.now)
        .unwrap();
    let lease_2027 = hub
        .lease(&key_2027, &node("edge-khi#branch_07"), 2, january_2027.now)
        .unwrap();
    assert_eq!((lease_2026.from, lease_2027.from), (1, 1));

    let mut seq = LeasedSequences::new()
        .with_lease(branch(lease_2026))
        .unwrap()
        .with_lease(branch(lease_2027))
        .unwrap();

    assert_eq!(name_once(&namer, &june_2026, &mut seq), "INV-2026-00001");
    assert_eq!(
        name_once(&namer, &january_2027, &mut seq),
        "INV-2027-00001",
        "a new year starts at one again"
    );
    assert_eq!(
        name_once(&namer, &june_2026, &mut seq),
        "INV-2026-00002",
        "the 2026 counter kept going independently of 2027"
    );
    assert_eq!(seq.remaining(&key_2026), 0);
    assert_eq!(seq.remaining(&key_2027), 1);
}

#[test]
fn an_exhausted_lease_fails_retryably_and_never_reuses_a_number() {
    let namer = namer();
    let doctype = name(SERIES_DOCTYPE);
    let now = at(2026, Month::June, 1, 12, 0);
    let ctx = ctx(now);
    let key = namer.series_key(&doctype, &ctx).unwrap();

    let mut hub = MemoryRangeAllocator::new();
    let lease = hub
        .lease(&key, &node("edge-khi#branch_07"), 2, now)
        .unwrap();
    let mut seq = LeasedSequences::new().with_lease(branch(lease)).unwrap();

    assert_eq!(name_once(&namer, &ctx, &mut seq), "INV-2026-00001");
    assert_eq!(name_once(&namer, &ctx, &mut seq), "INV-2026-00002");

    let error = block_on(namer.name_for(
        &doctype,
        &doc(SERIES_DOCTYPE),
        &ctx,
        GapPolicy::Tolerant,
        &mut seq,
    ))
    .unwrap_err();
    let NamingError::Storage(storage) = error else {
        panic!("an exhausted lease must surface as a storage error, got {error}");
    };
    assert_eq!(storage.kind, ErrorKind::Unavailable);
    assert!(
        storage.kind.is_retryable(),
        "retry once the lease is renewed"
    );
    assert!(
        storage.explanation.contains("INV-2026-"),
        "the error names the series: {}",
        storage.explanation
    );
    assert!(
        !storage.explanation.contains("edge-khi"),
        "the error never names the node: {}",
        storage.explanation
    );
    assert_eq!(seq.remaining(&key), 0);

    // A second attempt fails the same way instead of reissuing number 3 from nowhere.
    let again = block_on(namer.name_for(
        &doctype,
        &doc(SERIES_DOCTYPE),
        &ctx,
        GapPolicy::Tolerant,
        &mut seq,
    ))
    .unwrap_err();
    assert!(matches!(again, NamingError::Storage(ref e) if e.kind == ErrorKind::Unavailable));
    assert_eq!(seq.remaining(&key), 0);
}

#[test]
fn a_rolled_back_document_leaves_a_gap_and_the_number_is_not_reused() {
    let namer = namer();
    let doctype = name(SERIES_DOCTYPE);
    let now = at(2026, Month::June, 1, 12, 0);
    let ctx = ctx(now);
    let key = namer.series_key(&doctype, &ctx).unwrap();

    let mut hub = MemoryRangeAllocator::new();
    let lease = hub
        .lease(&key, &node("edge-khi#branch_07"), 3, now)
        .unwrap();
    let mut seq = LeasedSequences::new().with_lease(branch(lease)).unwrap();

    // The name is rendered and its document is written, then the transaction rolls back. The
    // leased allocator has no rollback hook: the number is spent, exactly as documented.
    let discarded = name_once(&namer, &ctx, &mut seq);
    assert_eq!(discarded, "INV-2026-00001");

    let kept = name_once(&namer, &ctx, &mut seq);
    assert_eq!(
        kept, "INV-2026-00002",
        "the rolled-back number is lost, never handed out again"
    );
    assert_ne!(kept, discarded);
    assert_eq!(seq.remaining(&key), 1);
}

#[test]
fn a_series_with_no_lease_fails_instead_of_inventing_a_number() {
    let namer = namer();
    let doctype = name(SERIES_DOCTYPE);
    let ctx = ctx(at(2026, Month::June, 1, 12, 0));
    let key = namer.series_key(&doctype, &ctx).unwrap();
    let mut seq = LeasedSequences::new();

    let error = block_on(namer.name_for(
        &doctype,
        &doc(SERIES_DOCTYPE),
        &ctx,
        GapPolicy::Tolerant,
        &mut seq,
    ))
    .unwrap_err();
    assert!(
        matches!(error, NamingError::Storage(ref e) if e.kind == ErrorKind::Unavailable),
        "an unleased series is unavailable, not fatal: {error}"
    );
    assert_eq!(seq.remaining(&key), 0);
    assert!(seq.running_low(&key), "no lease is as low as it gets");
}
