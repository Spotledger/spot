//! The streaming bound: a million records a side compare in constant memory, and each record is
//! read exactly once.
//!
//! Both sides are an [`Iterator`] that generates the next summary on demand, so the test never
//! holds a record set and a collected side would be the only way the compare could count it. On
//! Linux the peak resident set of the process proves the bound; elsewhere the one-pass bound does.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::cell::Cell;
use std::rc::Rc;

use spot_journal_audit::{DerivedEntities, RecordSummary, compare};

/// How many records each side holds: a million, as ADR-061's audit scale assumes.
const TOTAL: u64 = 1_000_000;

/// How much resident memory the comparison may grow the process by, in KiB. A collected million
/// records need more than 100 MiB, so this bound separates streaming from collecting.
const BOUND_KIB: u64 = 32 * 1024;

/// The entities the generated records span.
const ENTITIES: [&str; 4] = ["account", "invoice", "journal_entry", "warehouse"];

/// A generated, sorted stream of summaries that counts what was read from it.
///
/// Records are grouped by entity and their ids ascend within an entity, so the stream is in the
/// order [`compare`] requires. Nothing but a counter is held between calls.
struct Feed {
    /// The next record index.
    next: u64,
    /// How many records the stream holds.
    total: u64,
    /// How many records were read.
    reads: Rc<Cell<u64>>,
}

impl Feed {
    /// A stream of `total` records.
    fn new(total: u64, reads: Rc<Cell<u64>>) -> Self {
        Self {
            next: 0,
            total,
            reads,
        }
    }
}

impl Iterator for Feed {
    type Item = RecordSummary;

    fn next(&mut self) -> Option<RecordSummary> {
        if self.next >= self.total {
            return None;
        }
        let per_entity = self.total / ENTITIES.len() as u64;
        let index = self.next;
        self.next += 1;
        self.reads.set(self.reads.get() + 1);
        let entity = ENTITIES[usize::try_from(index / per_entity).unwrap() % ENTITIES.len()];
        let id = index % per_entity;
        Some(RecordSummary::new(entity, format!("{id:08}"), 1, [0; 32]))
    }
}

/// The process's peak resident set in KiB, when the host reports it.
#[cfg(target_os = "linux")]
fn peak_resident_kib() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status
        .lines()
        .find(|line| line.starts_with("VmHWM:"))?
        .split_whitespace()
        .nth(1)?;
    line.parse().ok()
}

/// No peak resident set is available on this host.
#[cfg(not(target_os = "linux"))]
fn peak_resident_kib() -> Option<u64> {
    None
}

#[test]
fn a_million_records_a_side_stream_in_constant_memory() {
    let live_reads = Rc::new(Cell::new(0));
    let rebuilt_reads = Rc::new(Cell::new(0));
    let before = peak_resident_kib();

    let result = compare(
        Feed::new(TOTAL, Rc::clone(&live_reads)),
        Feed::new(TOTAL, Rc::clone(&rebuilt_reads)),
        &DerivedEntities::none(),
    )
    .expect("a sorted stream");

    let after = peak_resident_kib();

    assert!(result.is_match());
    assert_eq!(result.totals.live, TOTAL);
    assert_eq!(result.totals.rebuilt, TOTAL);
    assert_eq!(result.totals.differences(), 0);
    assert_eq!(result.per_entity.len(), ENTITIES.len());
    for entity in ENTITIES {
        let counted = result.entity(entity).expect("a counted entity");
        assert_eq!(counted.live, TOTAL / ENTITIES.len() as u64);
        assert_eq!(counted.rebuilt, TOTAL / ENTITIES.len() as u64);
    }

    // Each side is read exactly once, to the end: the compare keeps no copy to go back to.
    assert_eq!(live_reads.get(), TOTAL);
    assert_eq!(rebuilt_reads.get(), TOTAL);

    // On a host that reports its peak resident set, the bound is proven, not skipped: a collected
    // million records a side cost more than 100 MiB, a streamed one costs kilobytes.
    #[cfg(target_os = "linux")]
    {
        let before = before.expect("/proc/self/status must report VmHWM on Linux");
        let after = after.expect("/proc/self/status must report VmHWM on Linux");
        let grown = after.saturating_sub(before);
        assert!(
            grown < BOUND_KIB,
            "the compare grew the resident set by {grown} KiB: a million records a side must stream"
        );
    }
    #[cfg(not(target_os = "linux"))]
    {
        // No peak resident set here: the one-pass bound above is what this host can prove.
        let _ = (before, after);
    }
}
