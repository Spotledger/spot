//! [`PgFixJournal`]: the `FixJournal` over the tables of [`crate::schema`].

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use deadpool_postgres::Pool;
use spot_fix_pipeline::{FixId, FixJournal, JournalEntry, JournalRecord, Result, Seq};
use spot_pg_bridge::Bridge;
use tokio_postgres::IsolationLevel;

use crate::error::{SetupError, journal_failed, journal_message};
use crate::schema;

/// The deadline of `append`: one lock-and-bump statement and one insert in a transaction.
pub const APPEND_DEADLINE: Duration = Duration::from_secs(10);
/// The deadline of `page_after`: one indexed range read.
pub const PAGE_DEADLINE: Duration = Duration::from_secs(5);
/// The deadline of `entries` and `all`, which read up to their ceiling in pages.
pub const READ_DEADLINE: Duration = Duration::from_secs(30);
/// The deadline of the schema preparation.
const SCHEMA_DEADLINE: Duration = Duration::from_secs(30);

/// Rows read per statement.
pub const PAGE: usize = 500;
/// The most records `all` returns. A longer journal fails with `fix_pipeline.journal_failed`
/// rather than returning a truncated answer (a truncated journal would make `recover` fold a
/// wrong state); read it with [`PgFixJournal::page_after`] instead.
pub const ALL_CEILING: usize = 200_000;
/// The most records `entries` returns for one fix. A fix writes tens of records; this is a
/// guard, not a working limit.
pub const ENTRIES_CEILING: usize = 10_000;

/// A stored row as read: sequence number, fix, instant and the document as JSON text.
type Row = (i64, String, i64, String);

const APPEND_HEAD: &str = "UPDATE spot_control.spot_fix_journal_head \
     SET last_seq = last_seq + 1 WHERE id = 1 RETURNING last_seq";
const APPEND_ROW: &str = "INSERT INTO spot_control.spot_fix_journal (seq, fix, at, record) \
     VALUES ($1, $2, $3, $4::text::jsonb)";
const PAGE_ALL: &str = "SELECT seq, fix, at, record::text FROM spot_control.spot_fix_journal \
     WHERE seq > $1 ORDER BY seq LIMIT $2";
const PAGE_FIX: &str = "SELECT seq, fix, at, record::text FROM spot_control.spot_fix_journal \
     WHERE fix = $3 AND seq > $1 ORDER BY seq LIMIT $2";

/// The fix journal over PostgreSQL. Use it from a control-plane process, a CLI command or a job
/// worker; never from a request handler and never inside a transaction an async store holds
/// (see the crate docs).
#[derive(Clone)]
pub struct PgFixJournal {
    bridge: Arc<Bridge>,
}

impl PgFixJournal {
    /// A journal that reaches the database through `bridge`, the process's shared bridge, built
    /// on `pool`.
    ///
    /// # Errors
    /// `fix_pipeline_pg.setup_failed` when the bridge configuration does not hold against the
    /// pool's size, or when called on a current-thread tokio runtime (the bridge's blocking
    /// calls could never run there).
    pub fn new(bridge: Arc<Bridge>, pool: &Pool) -> std::result::Result<Self, SetupError> {
        bridge
            .config()
            .validate(pool.status().max_size)
            .map_err(|_| {
                SetupError::new(
                    "the bridge configuration does not fit the pool size; see the README",
                )
            })?;
        if let Ok(handle) = tokio::runtime::Handle::try_current()
            && handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::CurrentThread
        {
            return Err(SetupError::new(
                "the journal cannot be built on a current-thread runtime; build it on a plain thread",
            ));
        }
        Ok(Self { bridge })
    }

    /// Creates the schema, tables, role and triggers if they are missing (idempotent, safe from
    /// several processes at once). A host calls it once at start-up.
    ///
    /// # Errors
    /// `fix_pipeline_pg.setup_failed`.
    pub fn ensure_schema(&self) -> std::result::Result<(), SetupError> {
        schema::ensure(&self.bridge, SCHEMA_DEADLINE)
    }

    /// Up to `limit` records with a sequence number above `after`, in sequence order: the
    /// streaming alternative to [`FixJournal::all`] for a journal longer than [`ALL_CEILING`].
    /// Start with `after = 0` and pass the last returned `seq` each time. `limit` is clamped to
    /// `1..=`[`PAGE`].
    ///
    /// # Errors
    /// `fix_pipeline.journal_failed`.
    pub fn page_after(&self, after: Seq, limit: usize) -> Result<Vec<JournalEntry>> {
        let after = to_i64(after)?;
        let limit = limit_i64(limit.clamp(1, PAGE));
        let rows = self
            .bridge
            .run(PAGE_DEADLINE, move |conn| {
                Box::pin(async move {
                    let rows = conn.query(PAGE_ALL, &[&after, &limit]).await?;
                    rows.iter()
                        .map(read_row)
                        .collect::<std::result::Result<Vec<Row>, _>>()
                })
            })
            .map_err(|e| journal_failed("the fix journal could not be read", &e))?;
        rows.into_iter().map(decode).collect()
    }

    /// Reads every record (of `fix`, when given) in pages inside one repeatable-read snapshot, so
    /// a concurrent append can neither tear nor duplicate the answer.
    fn read(&self, fix: Option<&FixId>, ceiling: usize) -> Result<Vec<JournalEntry>> {
        let key = fix.map(|f| f.as_str().to_owned());
        let page = limit_i64(PAGE);
        let rows = self
            .bridge
            .run(READ_DEADLINE, move |mut conn| {
                Box::pin(async move {
                    let tx = conn
                        .build_transaction()
                        .isolation_level(IsolationLevel::RepeatableRead)
                        .read_only(true)
                        .start()
                        .await?;
                    let mut out: Vec<Row> = Vec::new();
                    let mut after = 0_i64;
                    loop {
                        let rows = match &key {
                            Some(k) => tx.query(PAGE_FIX, &[&after, &page, k]).await?,
                            None => tx.query(PAGE_ALL, &[&after, &page]).await?,
                        };
                        let got = rows.len();
                        for row in &rows {
                            out.push(read_row(row)?);
                        }
                        if out.len() > ceiling {
                            return Ok(None);
                        }
                        match out.last() {
                            Some(last) if got == PAGE => after = last.0,
                            _ => break,
                        }
                    }
                    tx.commit().await?;
                    Ok(Some(out))
                })
            })
            .map_err(|e| journal_failed("the fix journal could not be read", &e))?;
        let Some(rows) = rows else {
            return Err(journal_message(
                "the fix journal is longer than the read ceiling; read it with page_after",
            ));
        };
        rows.into_iter().map(decode).collect()
    }
}

impl fmt::Debug for PgFixJournal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PgFixJournal").finish_non_exhaustive()
    }
}

fn read_row(row: &tokio_postgres::Row) -> std::result::Result<Row, tokio_postgres::Error> {
    Ok((
        row.try_get(0)?,
        row.try_get(1)?,
        row.try_get(2)?,
        row.try_get(3)?,
    ))
}

fn to_i64(seq: Seq) -> Result<i64> {
    i64::try_from(seq).map_err(|_| journal_message("the sequence number is out of range"))
}

fn limit_i64(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// The record as JSON text for the `jsonb` column, or an error when what would be stored does not
/// read back as the same record.
///
/// Text, not `serde_json::Value`: the text cast keeps every digit (jsonb numbers are numeric).
/// The read-back check fails the append, before anything is written, rather than poison the
/// journal with a row that `recover` could never read: `JournalRecord` is an internally tagged
/// enum, serde buffers its payload, and the buffer cannot hold an `i128`, so a record that
/// carries a fingerprint measure does not round-trip until the trait crates change their form
/// (see the README, "Known limit").
pub(crate) fn encode(record: &JournalRecord) -> Result<String> {
    let text = serde_json::to_string(record)
        .map_err(|_| journal_message("the journal record could not be encoded"))?;
    match serde_json::from_str::<JournalRecord>(&text) {
        Ok(back) if &back == record => Ok(text),
        _ => Err(journal_message(
            "the journal record does not read back as written, so it was not stored",
        )),
    }
}

/// A stored row back into an entry. A row that no longer deserialises is an error, never skipped.
fn decode(row: Row) -> Result<JournalEntry> {
    let (seq, fix, at, doc) = row;
    let invalid = || journal_message(format!("stored journal row {seq} is not a valid record"));
    let seq = Seq::try_from(seq).map_err(|_| invalid())?;
    let fix = FixId::new(&fix).map_err(|_| invalid())?;
    let record: JournalRecord = serde_json::from_str(&doc).map_err(|_| invalid())?;
    Ok(JournalEntry {
        seq,
        fix,
        at,
        record,
    })
}

impl FixJournal for PgFixJournal {
    fn append(&self, fix: &FixId, at: i64, record: JournalRecord) -> Result<Seq> {
        let doc = encode(&record)?;
        let key = fix.as_str().to_owned();
        let seq = self
            .bridge
            .run(APPEND_DEADLINE, move |mut conn| {
                Box::pin(async move {
                    let tx = conn.transaction().await?;
                    // The update takes the head row's lock, so appends queue here and each sees
                    // the number the previous one committed. A rollback undoes the bump too.
                    let head = tx.query_one(APPEND_HEAD, &[]).await?;
                    let seq: i64 = head.try_get(0)?;
                    tx.execute(APPEND_ROW, &[&seq, &key, &at, &doc]).await?;
                    tx.commit().await?;
                    Ok(seq)
                })
            })
            .map_err(|e| journal_failed("the fix journal could not be written", &e))?;
        Seq::try_from(seq).map_err(|_| journal_message("the journal head is out of range"))
    }

    fn entries(&self, fix: &FixId) -> Result<Vec<JournalEntry>> {
        self.read(Some(fix), ENTRIES_CEILING)
    }

    fn all(&self) -> Result<Vec<JournalEntry>> {
        self.read(None, ALL_CEILING)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use spot_fix_pipeline::JournalRecord;

    #[test]
    fn encode_stores_only_what_reads_back_equal() {
        let plain = JournalRecord::ApprovalRequested {
            reason: "needs a human".to_owned(),
        };
        let text = encode(&plain).unwrap();
        assert_eq!(serde_json::from_str::<JournalRecord>(&text).unwrap(), plain);
        assert_eq!(
            serde_json::from_str::<JournalRecord>(&encode(&JournalRecord::VerifyStarted).unwrap())
                .unwrap(),
            JournalRecord::VerifyStarted
        );
    }

    #[test]
    fn a_record_that_cannot_round_trip_is_refused_not_stored() {
        use spot_fix_pipeline::testing::FakeWorld;
        let world = FakeWorld::with(&[("job:1", 10)]);
        let fingerprint = world.fingerprint().unwrap();
        let record = JournalRecord::PreFingerprintCaptured {
            fingerprint: Box::new(fingerprint),
        };
        // Whatever the trait crates' serde form does today, `encode` either returns text that
        // reads back equal or an error; it never returns text that reads back wrong or not at all.
        match encode(&record) {
            Ok(text) => assert_eq!(
                serde_json::from_str::<JournalRecord>(&text).unwrap(),
                record
            ),
            Err(error) => assert_eq!(error.code(), spot_fix_pipeline::codes::JOURNAL_FAILED),
        }
    }
}
