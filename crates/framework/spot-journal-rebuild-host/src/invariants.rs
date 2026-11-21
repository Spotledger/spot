//! The production invariants of a rebuild (WP-RTB-01.T14, ADR-061 section 6).
//!
//! [`crate::Invariants`] is the suite the host runs after the projectors; until now nothing
//! production implemented it, so a rebuild passed whatever was rebuilt (or, through the CLI's
//! suite, failed for want of checks). [`ProductionInvariants`] is the default suite: three
//! checks, each of which fails the rebuild.
//!
//! 1. **`row_counts`**: per table, the rebuilt database holds as many rows as the journal replayed
//!    into a scratch store ([`ExpectedTables`]).
//! 2. **`content_digest`**: per table, an order-independent digest of the rows equals the digest
//!    of the replayed scratch store. A row that changed, appeared or vanished changes it.
//! 3. **`journal_sequence`**: the positions applied between the snapshot and the head have no gap
//!    and no duplicate ([`JournalPositions`]).
//!
//! # What a failure says
//!
//! A failing check is its own [`InvariantResult`] whose name carries the table or the position
//! range: `row_counts:invoice`, `content_digest:invoice`, `journal_sequence:g1 gap 41..43`. The
//! host reports that name and nothing more. Names, positions and counts only: **never a row,
//! a value or a digest** (a digest of a small table can be guessed against).
//!
//! # The two sides
//!
//! [`PgTables`] reads a PostgreSQL database: `count(*)` and a sum of `hashtextextended` of each
//! row's text, so memory is constant and the answer does not depend on row order. The rebuilt
//! database and the scratch database must be on the **same server version** (the hash function
//! is stable within one). [`MemoryTables`] is the same facts over rows in memory, for tests and
//! for a caller whose scratch store is not a database.

use std::collections::BTreeMap;
use std::sync::Arc;

use sha2::{Digest, Sha256};

use crate::checkpoint::Fut;
use crate::error::HostError;
use crate::run::{InvariantResult, Invariants};

/// What is known of one table: how many rows, and a digest of their content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableFacts {
    /// The row count.
    pub rows: u64,
    /// An order-independent digest of the rows (opaque text: compared, never shown).
    pub digest: String,
}

/// A source of per-table facts.
pub trait TableSource: Send + Sync {
    /// The facts of every table this source covers, by table name.
    fn tables(&self) -> Fut<'_, BTreeMap<String, TableFacts>>;
}

/// The facts of the journal replayed into a scratch store: what the rebuilt tables must equal.
///
/// The implementer replays the journal (the same executor and apply path as the rebuild) into a
/// scratch store, reads its facts with the same digest as the rebuilt side and drops the scratch
/// store. It never touches the rebuilt database.
pub trait ExpectedTables: Send + Sync {
    /// The facts of the replay, by table name.
    fn replay(&self) -> Fut<'_, BTreeMap<String, TableFacts>>;
}

/// A journal position: `(generation, seq)`.
pub type Position = (u64, u64);

/// The positions the rebuild applied, from the snapshot to the head.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppliedPositions {
    /// The snapshot the rebuild started from (exclusive), or `None` for a rebuild from the
    /// beginning of the journal.
    pub base: Option<Position>,
    /// The journal's head: the last position the rebuild must have reached.
    pub head: Position,
    /// Every position applied, in the order applied.
    pub applied: Vec<Position>,
}

/// Where the applied positions come from (the executor's report or the checkpointing sink).
pub trait JournalPositions: Send + Sync {
    /// The positions applied.
    fn applied(&self) -> Fut<'_, AppliedPositions>;
}

/// One thing a check found: a table or a position range, and counts. No content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finding(pub String);

/// The table names that differ in their row count (names only), plus the number compared.
#[must_use]
pub fn count_findings(
    rebuilt: &BTreeMap<String, TableFacts>,
    expected: &BTreeMap<String, TableFacts>,
) -> (u64, Vec<Finding>) {
    compare(rebuilt, expected, |a, b| a.rows == b.rows)
}

/// The table names whose content digest differs (names only), plus the number compared.
#[must_use]
pub fn digest_findings(
    rebuilt: &BTreeMap<String, TableFacts>,
    expected: &BTreeMap<String, TableFacts>,
) -> (u64, Vec<Finding>) {
    compare(rebuilt, expected, |a, b| a.digest == b.digest)
}

fn compare(
    rebuilt: &BTreeMap<String, TableFacts>,
    expected: &BTreeMap<String, TableFacts>,
    same: impl Fn(&TableFacts, &TableFacts) -> bool,
) -> (u64, Vec<Finding>) {
    let mut names: Vec<&String> = rebuilt.keys().chain(expected.keys()).collect();
    names.sort();
    names.dedup();
    let mut findings = Vec::new();
    for name in &names {
        let held = match (rebuilt.get(*name), expected.get(*name)) {
            (Some(a), Some(b)) => same(a, b),
            // A table on one side only is a difference.
            _ => false,
        };
        if !held {
            findings.push(Finding((*name).clone()));
        }
    }
    (u64::try_from(names.len()).unwrap_or(u64::MAX), findings)
}

/// The gaps and duplicates in the applied positions, as ranges (numbers only), plus how many
/// positions were checked.
///
/// Within a generation the sequence must be contiguous, starting right after the snapshot (for
/// the snapshot's own generation) and ending at the head (for the head's generation); a later
/// generation may start anywhere but must then be contiguous.
#[must_use]
pub fn sequence_findings(positions: &AppliedPositions) -> (u64, Vec<Finding>) {
    let mut findings = Vec::new();
    let mut by_generation: BTreeMap<u64, Vec<u64>> = BTreeMap::new();
    for (generation, seq) in &positions.applied {
        by_generation.entry(*generation).or_default().push(*seq);
    }
    let checked = u64::try_from(positions.applied.len()).unwrap_or(u64::MAX);
    for (generation, seqs) in &by_generation {
        // Duplicates, in the order seen.
        let mut seen = std::collections::BTreeSet::new();
        for seq in seqs {
            if !seen.insert(*seq) {
                findings.push(Finding(format!("g{generation} duplicate {seq}")));
            }
        }
        let first = seen.iter().next().copied().unwrap_or(0);
        let last = seen.iter().next_back().copied().unwrap_or(0);
        let start = match positions.base {
            Some((base_generation, base_seq)) if base_generation == *generation => {
                base_seq.saturating_add(1)
            }
            _ => first,
        };
        if first > start {
            findings.push(Finding(format!(
                "g{generation} gap {start}..{}",
                first.saturating_sub(1)
            )));
        }
        let mut previous = first;
        for seq in seen.iter().skip(1) {
            if *seq > previous.saturating_add(1) {
                findings.push(Finding(format!(
                    "g{generation} gap {}..{}",
                    previous.saturating_add(1),
                    seq.saturating_sub(1)
                )));
            }
            previous = *seq;
        }
        let (head_generation, head_seq) = positions.head;
        if head_generation == *generation && last < head_seq {
            findings.push(Finding(format!(
                "g{generation} short of head {}..{head_seq}",
                last.saturating_add(1)
            )));
        }
    }
    // A head generation that applied nothing at all is a gap of its whole range.
    let (head_generation, head_seq) = positions.head;
    if !by_generation.contains_key(&head_generation) && head_seq > 0 {
        let from = match positions.base {
            Some((g, s)) if g == head_generation => s.saturating_add(1),
            _ => 1,
        };
        if from <= head_seq {
            findings.push(Finding(format!(
                "g{head_generation} short of head {from}..{head_seq}"
            )));
        }
    }
    (checked, findings)
}

fn results(name: &str, checked: u64, findings: &[Finding]) -> Vec<InvariantResult> {
    if findings.is_empty() {
        return vec![InvariantResult {
            name: name.to_owned(),
            held: true,
            checked,
            violations: 0,
        }];
    }
    findings
        .iter()
        .map(|finding| InvariantResult {
            name: format!("{name}:{}", finding.0),
            held: false,
            checked,
            violations: 1,
        })
        .collect()
}

/// The default suite of a rebuild: row counts, content digests and the journal sequence.
pub struct ProductionInvariants {
    rebuilt: Arc<dyn TableSource>,
    expected: Arc<dyn ExpectedTables>,
    positions: Arc<dyn JournalPositions>,
}

impl ProductionInvariants {
    /// A suite over the rebuilt database, the replay into a scratch store and the applied
    /// positions.
    #[must_use]
    pub fn new(
        rebuilt: Arc<dyn TableSource>,
        expected: Arc<dyn ExpectedTables>,
        positions: Arc<dyn JournalPositions>,
    ) -> Self {
        Self {
            rebuilt,
            expected,
            positions,
        }
    }
}

/// A source that could not be read fails the rebuild, naming the source and nothing else.
fn unreadable(source: &str) -> HostError {
    HostError::InvariantFailed(format!("{source}_unreadable"))
}

impl Invariants for ProductionInvariants {
    fn run(&self) -> Fut<'_, Vec<InvariantResult>> {
        Box::pin(async move {
            let rebuilt = self
                .rebuilt
                .tables()
                .await
                .map_err(|_| unreadable("rebuilt_database"))?;
            let expected = self
                .expected
                .replay()
                .await
                .map_err(|_| unreadable("scratch_replay"))?;
            let positions = self
                .positions
                .applied()
                .await
                .map_err(|_| unreadable("journal_positions"))?;
            let (counted, count_bad) = count_findings(&rebuilt, &expected);
            let (hashed, digest_bad) = digest_findings(&rebuilt, &expected);
            let (sequenced, sequence_bad) = sequence_findings(&positions);
            let mut out = results("row_counts", counted, &count_bad);
            out.extend(results("content_digest", hashed, &digest_bad));
            out.extend(results("journal_sequence", sequenced, &sequence_bad));
            Ok(out)
        })
    }
}

// ---- the two table sources ---------------------------------------------------------------

fn row_hash(row: &str) -> u64 {
    let digest = Sha256::digest(row.as_bytes());
    let mut first = [0_u8; 8];
    first.copy_from_slice(&digest[..8]);
    u64::from_be_bytes(first)
}

/// The facts of `rows` (each row as its canonical text): the count and the sum of the row
/// hashes, so the order of the rows does not matter.
#[must_use]
pub fn facts_of(rows: &[String]) -> TableFacts {
    let sum: u128 = rows.iter().map(|row| u128::from(row_hash(row))).sum();
    TableFacts {
        rows: u64::try_from(rows.len()).unwrap_or(u64::MAX),
        digest: sum.to_string(),
    }
}

/// Facts over rows held in memory.
#[derive(Clone, Debug, Default)]
pub struct MemoryTables {
    tables: BTreeMap<String, Vec<String>>,
}

impl MemoryTables {
    /// No tables.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a table with its rows (each row as its canonical text).
    #[must_use]
    pub fn with_table(mut self, name: &str, rows: Vec<String>) -> Self {
        self.tables.insert(name.to_owned(), rows);
        self
    }

    fn facts(&self) -> BTreeMap<String, TableFacts> {
        self.tables
            .iter()
            .map(|(name, rows)| (name.clone(), facts_of(rows)))
            .collect()
    }
}

impl TableSource for MemoryTables {
    fn tables(&self) -> Fut<'_, BTreeMap<String, TableFacts>> {
        Box::pin(async move { Ok(self.facts()) })
    }
}

impl ExpectedTables for MemoryTables {
    fn replay(&self) -> Fut<'_, BTreeMap<String, TableFacts>> {
        Box::pin(async move { Ok(self.facts()) })
    }
}

/// Whether `name` is a plain lower-case table identifier (so it can be quoted into a query).
fn plain_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && name.len() <= 63
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// Facts read from a PostgreSQL database, for the tables the caller names (the entity tables of
/// the journal's catalogue; never the outbox, the checkpoint or any table that differs by design).
pub struct PgTables {
    backend: spot_postgres::PgBackend,
    tables: Vec<String>,
}

impl PgTables {
    /// A source over `backend` for `tables`.
    ///
    /// # Errors
    /// [`HostError::InvariantFailed`] for a name that is not a plain lower-case identifier.
    pub fn new(backend: spot_postgres::PgBackend, tables: Vec<String>) -> Result<Self, HostError> {
        if let Some(bad) = tables.iter().find(|name| !plain_identifier(name)) {
            // Not echoed: a refused name may be anything.
            let _ = bad;
            return Err(HostError::InvariantFailed("table_name_refused".to_owned()));
        }
        Ok(Self { backend, tables })
    }
}

impl TableSource for PgTables {
    fn tables(&self) -> Fut<'_, BTreeMap<String, TableFacts>> {
        Box::pin(async move {
            let client = self
                .backend
                .pool()
                .get()
                .await
                .map_err(|_| unreadable("database"))?;
            let mut out = BTreeMap::new();
            for table in &self.tables {
                // The name was validated as a plain identifier at construction.
                let query = format!(
                    "SELECT count(*)::bigint, coalesce(sum(hashtextextended(t::text, 0)::numeric), 0)::text \
                     FROM \"{table}\" t"
                );
                let row = client
                    .query_one(query.as_str(), &[])
                    .await
                    .map_err(|_| unreadable("database"))?;
                let rows: i64 = row.try_get(0).map_err(|_| unreadable("database"))?;
                let digest: String = row.try_get(1).map_err(|_| unreadable("database"))?;
                out.insert(
                    table.clone(),
                    TableFacts {
                        rows: u64::try_from(rows).map_err(|_| unreadable("database"))?,
                        digest,
                    },
                );
            }
            Ok(out)
        })
    }
}

/// Applied positions held in memory (a test, or a caller that collected them while rebuilding).
#[derive(Clone, Debug)]
pub struct MemoryPositions(pub AppliedPositions);

impl JournalPositions for MemoryPositions {
    fn applied(&self) -> Fut<'_, AppliedPositions> {
        Box::pin(async move { Ok(self.0.clone()) })
    }
}
