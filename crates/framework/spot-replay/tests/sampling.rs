//! The sample policy: window, stratification and determinism.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use spot_app_api::prelude::OffsetDateTime;
use spot_replay::{JournalEntry, ReplayPolicy, Sensitivity, Window, select};
use support::{ToyRelease, journal_entry, reference_at};

/// An instant `days` before the reference instant.
fn days_before(days: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(reference_at().unix_timestamp() - days * 86_400)
        .expect("instant")
}

/// `count` normal `party` entries, all with base 1010.
fn parties(count: u64) -> Vec<JournalEntry> {
    let old = ToyRelease::old();
    (1..=count)
        .map(|seq| journal_entry(seq, 1010, &old))
        .collect()
}

#[test]
fn the_day_window_drops_entries_before_the_cutoff() {
    let old = ToyRelease::old();
    // One entry per day, from 40 days before the reference instant.
    let entries: Vec<JournalEntry> = (0..40)
        .map(|day| {
            let mut entry = journal_entry(day + 1, 1010, &old);
            entry.occurred_at = days_before(i64::try_from(day).expect("day fits i64"));
            entry
        })
        .collect();
    let policy = ReplayPolicy::new(Window::LastDays { days: 7 }, 100);
    let sample = select(&policy, reference_at(), entries);
    // Days 0..=7 are inside the window (day 7 is exactly the cutoff); the rest drop.
    assert_eq!(sample.len(), 8, "{} selected", sample.len());
    assert!(
        sample
            .entries
            .iter()
            .all(|e| e.occurred_at >= days_before(7))
    );
}

#[test]
fn the_document_window_keeps_the_most_recent() {
    let entries = parties(50);
    let policy = ReplayPolicy::new(Window::LastDocuments { documents: 5 }, 100);
    let sample = select(&policy, reference_at(), entries);
    assert_eq!(sample.len(), 5);
    // The five highest sequences survive.
    let mut sequences: Vec<u64> = sample.entries.iter().map(|e| e.sequence).collect();
    sequences.sort();
    assert_eq!(sequences, vec![46, 47, 48, 49, 50]);
}

#[test]
fn stratification_caps_each_stratum_and_reports_coverage() {
    let old = ToyRelease::old();
    let mut entries = parties(10);
    // Ten high-sensitivity entries of another DocType.
    let highs: Vec<JournalEntry> = (1..=10)
        .map(|seq| {
            let mut entry = journal_entry(seq + 100, 1010, &old);
            entry.doctype = "gl_entry".to_owned();
            entry.source = "gl_entry".to_owned();
            entry.sensitivity = Sensitivity::High;
            entry
        })
        .collect();
    entries.extend(highs);

    let policy = ReplayPolicy::new(Window::LastDocuments { documents: 100 }, 3);
    let sample = select(&policy, reference_at(), entries);
    assert_eq!(sample.len(), 6, "three from each of two strata");
    let party = sample
        .strata
        .iter()
        .find(|s| s.doctype == "party")
        .expect("party stratum");
    assert_eq!((party.available, party.selected), (10, 3));
    let gl = sample
        .strata
        .iter()
        .find(|s| s.doctype == "gl_entry")
        .expect("gl stratum");
    assert_eq!(gl.sensitivity, Sensitivity::High);
    assert_eq!((gl.available, gl.selected), (10, 3));
}

#[test]
fn the_selection_is_deterministic_for_a_seed() {
    let entries = parties(200);
    let policy = ReplayPolicy::new(Window::LastDocuments { documents: 200 }, 20).with_seed(99);
    let first = select(&policy, reference_at(), entries.clone());
    let second = select(&policy, reference_at(), entries);
    let keys = |sample: &spot_replay::Sample| -> Vec<(String, u64)> {
        sample
            .entries
            .iter()
            .map(|e| (e.source.clone(), e.sequence))
            .collect()
    };
    assert_eq!(keys(&first), keys(&second));
    assert_eq!(keys(&first).len(), 20);
}

#[test]
fn a_different_seed_draws_a_different_sample() {
    let entries = parties(200);
    let a = select(
        &ReplayPolicy::new(Window::LastDocuments { documents: 200 }, 20).with_seed(1),
        reference_at(),
        entries.clone(),
    );
    let b = select(
        &ReplayPolicy::new(Window::LastDocuments { documents: 200 }, 20).with_seed(2),
        reference_at(),
        entries,
    );
    let keys = |sample: &spot_replay::Sample| -> Vec<u64> {
        sample.entries.iter().map(|e| e.sequence).collect()
    };
    assert_ne!(keys(&a), keys(&b));
}

#[test]
fn the_selection_is_in_causal_order() {
    let mut entries = parties(20);
    for seq in 1..=20u64 {
        let mut entry = journal_entry(seq + 100, 1010, &ToyRelease::old());
        entry.source = "payments".to_owned();
        entries.push(entry);
    }
    let policy = ReplayPolicy::new(Window::LastDocuments { documents: 40 }, 40).with_seed(3);
    let sample = select(&policy, reference_at(), entries);
    let keys: Vec<(String, u64)> = sample
        .entries
        .iter()
        .map(|e| (e.source.clone(), e.sequence))
        .collect();
    let mut sorted = keys.clone();
    sorted.sort();
    assert_eq!(keys, sorted);
}
