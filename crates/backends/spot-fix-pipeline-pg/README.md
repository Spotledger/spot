# spot-fix-pipeline-pg

The PostgreSQL `FixJournal` of `spot-fix-pipeline` (WP-AI-08.T2, ADR-082 class C): the
write-ahead journal of agent actions, durable, so `recover` can finish or roll back a fix after a
crash. Two tables in `spot_control`: `spot_fix_journal` (`seq` primary key, `fix`, `at`, `record
jsonb`) and `spot_fix_journal_head` (one row, the last number handed out). No extension is needed.

## Where it may be used

The trait is synchronous, so the journal blocks on the shared `spot-pg-bridge`. That is
legitimate in **a control-plane process, a CLI command or a job worker**. It must **never** be
called from a request handler (the bridge refuses with `pg_bridge.wrong_thread` on any tokio
runtime thread, and the journal returns `fix_pipeline.journal_failed` naming it) and never inside
a transaction an async store holds (it takes its own pooled connection; holding one while waiting
for another can exhaust the pool).

## Use

```rust,ignore
let journal = PgFixJournal::new(bridge.clone(), &backend.pool())?; // plain thread
journal.ensure_schema()?; // once at start-up; idempotent, under an advisory lock
let seq = journal.append(&fix, now, record)?;
```

`new` refuses a bridge configuration that does not fit the pool and a current-thread runtime
(`fix_pipeline_pg.setup_failed`).

## Semantics

* `append`: one transaction updates the head row (taking its lock) and inserts the entry with the
  new number. Numbers start at 1, are gapless, never repeat, and a rolled-back append consumes
  none. `at` is the caller's argument; the crate never reads a clock. An append whose record would
  not read back equal is refused before anything is written.
* `entries(fix)` and `all`: records in sequence order, read in pages of `PAGE` = 500 inside one
  repeatable-read snapshot. Ceilings: `ENTRIES_CEILING` = 10,000 per fix, `ALL_CEILING` =
  200,000 overall; above them the call fails with `fix_pipeline.journal_failed` instead of
  truncating. `page_after(seq, limit)` is the streaming alternative for a longer journal.
* A row that no longer deserialises is `fix_pipeline.journal_failed`, never skipped.
* Deadlines: 10 s append, 5 s page, 30 s full reads. A timed-out append may or may not have been
  applied; read the fix's entries again before retrying (`recover` does).
* Errors carry only a bridge code or SQLSTATE: no SQL, record, fix id or connection string.

## Append-only

Three layers: the application role `spot_fix_journal_app` has `SELECT, INSERT` on the journal
(and `SELECT, UPDATE` on the head only); a trigger raises on `UPDATE`, `DELETE` and `TRUNCATE`
for every role including the owner, with no maintenance bypass; and the only statement the crate
issues against existing rows is the head bump. A host connects as the application role.

## Sizing

Appends serialise on the single head row, so more concurrency buys nothing. Use
`max_in_flight = 2`, `queue_capacity = 8` and a pool of 4 (the bridge needs
`max_in_flight <= pool - 2`); the pool is shared with the other class C stores of the process,
so add their `max_in_flight` to the pool size rather than raising this one.

Fingerprint measures are stored as decimal strings, so a record that carries one reads back from the
journal (the append read-back check stays as the safety net for any future type).

## Tests

Need `SPOT_TEST_PG_URL` (admin URL); skipped with a message when unset.
