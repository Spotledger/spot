//! The production invariants of a rebuild (WP-RTB-01.T14): row counts, content digests and the
//! journal sequence each fail the rebuild when seeded with a corruption, a clean rebuild passes
//! all three, and no failure carries row content. No database: the tables are in memory (the
//! PostgreSQL source is the same facts read with SQL, proved by the real-database rebuild run).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use spot_journal_rebuild_host::invariants::{
    AppliedPositions, MemoryPositions, MemoryTables, ProductionInvariants, facts_of,
};
use spot_journal_rebuild_host::{HostError, InvariantResult, Invariants};

/// A value that must never appear in a result.
const MARKER: &str = "SECRET-MARKER-9f3a";

fn rows(count: u64) -> Vec<String> {
    (0..count)
        .map(|n| format!("(item-{n:04},{MARKER}-{n})"))
        .collect()
}

fn tables(item: Vec<String>, invoice: Vec<String>) -> MemoryTables {
    MemoryTables::new()
        .with_table("item", item)
        .with_table("invoice", invoice)
}

fn clean_positions() -> AppliedPositions {
    AppliedPositions {
        base: Some((1, 40)),
        head: (1, 45),
        applied: (41..=45).map(|seq| (1, seq)).collect(),
    }
}

async fn run(
    rebuilt: MemoryTables,
    expected: MemoryTables,
    positions: AppliedPositions,
) -> Vec<InvariantResult> {
    ProductionInvariants::new(
        Arc::new(rebuilt),
        Arc::new(expected),
        Arc::new(MemoryPositions(positions)),
    )
    .run()
    .await
    .unwrap()
}

fn failed(results: &[InvariantResult]) -> Vec<&str> {
    results
        .iter()
        .filter(|r| !r.held)
        .map(|r| r.name.as_str())
        .collect()
}

#[tokio::test]
async fn a_clean_rebuild_passes_all_three_checks() {
    let results = run(
        tables(rows(5), rows(3)),
        tables(rows(5), rows(3)),
        clean_positions(),
    )
    .await;
    assert_eq!(
        results.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
        ["row_counts", "content_digest", "journal_sequence"]
    );
    assert!(results.iter().all(|r| r.held && r.violations == 0));
    // The table checks compared both tables; the sequence check saw five positions.
    assert_eq!(results[0].checked, 2);
    assert_eq!(results[2].checked, 5);
}

#[tokio::test]
async fn row_order_does_not_matter() {
    let mut reversed = rows(5);
    reversed.reverse();
    let results = run(
        tables(reversed, rows(3)),
        tables(rows(5), rows(3)),
        clean_positions(),
    )
    .await;
    assert!(results.iter().all(|r| r.held));
}

#[tokio::test]
async fn a_missing_row_is_caught_by_the_row_count_check_and_names_the_table() {
    let results = run(
        tables(rows(4), rows(3)),
        tables(rows(5), rows(3)),
        clean_positions(),
    )
    .await;
    let bad = failed(&results);
    assert!(bad.contains(&"row_counts:item"), "{bad:?}");
    assert!(!bad.iter().any(|name| name.contains("invoice")));
}

#[tokio::test]
async fn a_changed_row_is_caught_by_the_digest_check_and_not_by_the_count() {
    let mut changed = rows(5);
    changed[2] = format!("(item-0002,{MARKER}-TAMPERED)");
    let results = run(
        tables(changed, rows(3)),
        tables(rows(5), rows(3)),
        clean_positions(),
    )
    .await;
    let bad = failed(&results);
    assert_eq!(bad, ["content_digest:item"]);
    assert!(results[0].held, "the count is the same");
}

#[tokio::test]
async fn a_table_on_one_side_only_is_a_difference() {
    let rebuilt = tables(rows(5), rows(3)).with_table("stray", rows(1));
    let results = run(rebuilt, tables(rows(5), rows(3)), clean_positions()).await;
    let bad = failed(&results);
    assert!(bad.contains(&"row_counts:stray"));
    assert!(bad.contains(&"content_digest:stray"));
}

#[tokio::test]
async fn a_gap_in_the_journal_sequence_names_the_range() {
    let mut positions = clean_positions();
    positions
        .applied
        .retain(|(_, seq)| *seq != 43 && *seq != 44);
    let results = run(
        tables(rows(5), rows(3)),
        tables(rows(5), rows(3)),
        positions,
    )
    .await;
    assert_eq!(failed(&results), ["journal_sequence:g1 gap 43..44"]);
}

#[tokio::test]
async fn a_gap_right_after_the_snapshot_a_duplicate_and_a_short_head_are_caught() {
    let after_snapshot = AppliedPositions {
        base: Some((1, 40)),
        head: (1, 45),
        applied: (43..=45).map(|seq| (1, seq)).collect(),
    };
    let results = run(
        tables(rows(1), rows(1)),
        tables(rows(1), rows(1)),
        after_snapshot,
    )
    .await;
    assert_eq!(failed(&results), ["journal_sequence:g1 gap 41..42"]);

    let mut duplicated = clean_positions();
    duplicated.applied.push((1, 42));
    let results = run(
        tables(rows(1), rows(1)),
        tables(rows(1), rows(1)),
        duplicated,
    )
    .await;
    assert_eq!(failed(&results), ["journal_sequence:g1 duplicate 42"]);

    let mut short = clean_positions();
    short.applied.truncate(3);
    let results = run(tables(rows(1), rows(1)), tables(rows(1), rows(1)), short).await;
    assert_eq!(
        failed(&results),
        ["journal_sequence:g1 short of head 44..45"]
    );
}

#[tokio::test]
async fn a_head_generation_that_applied_nothing_is_short_of_head() {
    let positions = AppliedPositions {
        base: Some((1, 40)),
        head: (2, 3),
        applied: (41..=45).map(|seq| (1, seq)).collect(),
    };
    let results = run(
        tables(rows(1), rows(1)),
        tables(rows(1), rows(1)),
        positions,
    )
    .await;
    assert_eq!(failed(&results), ["journal_sequence:g2 short of head 1..3"]);
}

#[tokio::test]
async fn no_result_carries_row_content_or_a_digest() {
    let mut changed = rows(5);
    changed[0] = format!("(item-0000,{MARKER}-TAMPERED)");
    let rebuilt = tables(changed, rows(2));
    let expected = tables(rows(5), rows(3));
    let digest = facts_of(&rows(5)).digest;
    let mut positions = clean_positions();
    positions.applied.pop();
    let results = run(rebuilt, expected, positions).await;
    assert!(failed(&results).len() >= 3, "every check failed");
    let text = format!("{results:?}");
    assert!(!text.contains(MARKER), "no row content in {text}");
    assert!(!text.contains(&digest), "no digest in {text}");
}

#[test]
fn the_digest_is_a_function_of_the_rows_and_not_of_their_order() {
    assert_eq!(facts_of(&rows(4)), facts_of(&rows(4)));
    assert_ne!(facts_of(&rows(4)).digest, facts_of(&rows(5)).digest);
    assert_eq!(facts_of(&[]).rows, 0);
}

#[tokio::test]
async fn an_unreadable_source_fails_the_rebuild_naming_only_the_source() {
    use spot_journal_rebuild_host::Fut;
    use spot_journal_rebuild_host::invariants::{ExpectedTables, TableFacts};
    use std::collections::BTreeMap;

    struct Broken;
    impl ExpectedTables for Broken {
        fn replay(&self) -> Fut<'_, BTreeMap<String, TableFacts>> {
            Box::pin(async move { Err(HostError::JournalFailed) })
        }
    }
    let error = ProductionInvariants::new(
        Arc::new(tables(rows(1), rows(1))),
        Arc::new(Broken),
        Arc::new(MemoryPositions(clean_positions())),
    )
    .run()
    .await
    .unwrap_err();
    assert_eq!(
        error,
        HostError::InvariantFailed("scratch_replay_unreadable".to_owned())
    );
}
