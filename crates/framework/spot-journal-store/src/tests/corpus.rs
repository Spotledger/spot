//! The property run, as a deterministic corpus.
//!
//! The round-trip property (`parse(format(key)) == key`) and the ordering property (ascending
//! lexical order is ascending numeric order) hold for *every* value, not just the handful the
//! hand-written cases pick. `proptest` is not a dependency of this crate — the brief allows only
//! `thiserror`, `serde`, `sha2`, `futures` and `tempfile` — so the property is checked over a
//! generated corpus instead: a xorshift64 sequence with a fixed seed, so a failure is
//! reproducible byte for byte, and no clock or randomness source is read.
//!
//! Every case is built through [`Layout`], formatted, parsed back and compared. A failure prints
//! the case that broke.

use crate::layout::{DIGITS, Key, Layout};

/// How many cases each shape checks.
const CASES: u64 = 512;

/// xorshift64: a deterministic generator, no dependency, no randomness source.
struct Corpus(u64);

impl Corpus {
    fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    /// A value that mixes small numbers (where padding matters most) with the top of the range.
    fn spread(&mut self) -> u64 {
        match self.next() % 4 {
            0 => self.next() % 8,
            1 => self.next() % 100_000,
            2 => self.next() % 4_000_000_000,
            _ => self.next(),
        }
    }
}

/// The tenants the corpus uses: all valid, all one path component.
const TENANTS: [&str; 5] = ["acme", "acme-eu", "t1", "tenant.with.dot", "A_b-9"];

#[test]
fn every_generated_key_round_trips() {
    let mut corpus = Corpus::new(0x5eed_1234_abcd_0001);
    for _ in 0..CASES {
        let tenant = TENANTS[usize::try_from(corpus.next() % 5).unwrap_or(0)];
        let generation = corpus.spread();
        let first = corpus.spread();
        let last = first.saturating_add(corpus.spread() % 1_000);
        let batch = corpus.spread();
        let position = corpus.spread();
        let hash = (0..32)
            .map(|_| format!("{:02x}", corpus.next() % 256))
            .collect::<String>();

        let keys = [
            Layout::manifest(tenant, generation).unwrap(),
            Layout::segment(tenant, generation, first, last).unwrap(),
            Layout::tail_chunk(tenant, generation, batch).unwrap(),
            Layout::snapshot(tenant, generation, position).unwrap(),
            Layout::snapshot_object(tenant, generation, position, "base.json").unwrap(),
            Layout::blob(tenant, &hash).unwrap(),
        ];
        for key in keys {
            let raw = key.to_key();
            assert!(!raw.contains("//"), "no key has an empty component: {raw}");
            assert!(!raw.starts_with('/'), "no key is absolute: {raw}");
            assert_eq!(
                raw.split('/').filter(|part| !part.is_empty()).count(),
                match key {
                    Key::Manifest(_) => 3,
                    Key::Segment(_) => 4,
                    Key::Tail(_) => 4,
                    Key::Snapshot(ref snapshot) => {
                        if snapshot.name.is_some() { 5 } else { 4 }
                    }
                    Key::Blob(_) => 3,
                },
                "the number of components is stable: {raw}"
            );
            let parsed = Key::parse(&raw)
                .unwrap_or_else(|error| panic!("{raw} does not parse back: {error}"));
            assert_eq!(parsed, key, "parse(format(key)) is the identity: {raw}");
        }
    }
}

#[test]
fn generated_generations_sort_the_same_way_twice() {
    let mut corpus = Corpus::new(0x5eed_1234_abcd_0002);
    let mut generations: Vec<u64> = (0..CASES).map(|_| corpus.spread()).collect();
    generations.sort_unstable();
    generations.dedup();

    let keys: Vec<String> = generations
        .iter()
        .map(|generation| Layout::tail_chunk("acme", *generation, 1).unwrap().to_key())
        .collect();
    let mut lexical = keys.clone();
    lexical.sort();
    assert_eq!(
        lexical, keys,
        "listing order of zero-padded generations is numeric order"
    );
}

#[test]
fn every_generated_number_keeps_the_canonical_width() {
    let mut corpus = Corpus::new(0x5eed_1234_abcd_0003);
    for _ in 0..CASES {
        let n = corpus.spread();
        let padded = Layout::padded(n);
        assert_eq!(padded.len(), DIGITS, "padded({n}) is {DIGITS} digits");
        assert!(
            padded.parse::<u64>() == Ok(n),
            "padding preserves the number: {padded}"
        );
        // The manifest key's file stem is the padded number and nothing else.
        let raw = Layout::manifest("acme", n).unwrap().to_key();
        let stem = raw
            .rsplit('/')
            .next()
            .and_then(|file| file.strip_suffix(".json"))
            .unwrap_or("missing");
        assert_eq!(stem, padded);
        assert_eq!(
            Key::parse(&raw).unwrap().to_key(),
            raw,
            "a generated manifest key is canonical"
        );
    }
}
