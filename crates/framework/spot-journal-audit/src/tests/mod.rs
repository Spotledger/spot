//! Tests of the journal audit: the streaming compare, the fingerprint compare, the recovery time
//! objective and the recovery report.
#![allow(clippy::expect_used, clippy::unwrap_used)]

mod compare_tests;
mod fingerprint_tests;
mod property_tests;
mod report_tests;
mod result_tests;
mod rto_tests;

use crate::{Amount, DerivedEntities, RecordSummary};

/// A digest that carries its seed, so a seeded divergence is easy to read.
pub(crate) fn digest(seed: u8) -> [u8; 32] {
    [seed; 32]
}

/// One record summary.
pub(crate) fn record(entity: &str, id: &str, version: u64, seed: u8) -> RecordSummary {
    RecordSummary::new(entity, id, version, digest(seed))
}

/// A stream of record summaries, in the order given (the tests give them in sort order).
pub(crate) fn stream(entries: &[(&str, &str, u64, u8)]) -> Vec<RecordSummary> {
    entries
        .iter()
        .map(|(entity, id, version, seed)| record(entity, id, *version, *seed))
        .collect()
}

/// A derived-entity set from names.
pub(crate) fn derived(names: &[&str]) -> DerivedEntities {
    DerivedEntities::from_names(names.iter().copied())
}

/// A fingerprint's values from exact decimal strings.
pub(crate) fn amounts(entries: &[(&str, &str)]) -> Vec<(String, Amount)> {
    entries
        .iter()
        .map(|(key, text)| {
            (
                key.to_string(),
                Amount::decimal(text).expect("an exact decimal"),
            )
        })
        .collect()
}
