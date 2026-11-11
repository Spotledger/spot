# spot-journal-audit-pg

The PostgreSQL sides of the journal audit (WP-JRN-07.T6, ADR-061 s8 and s9). The scheduled job
plugs them into `spot-journal-audit-run`'s `LiveSide` and `ScratchSide`.

| side | what it is |
| --- | --- |
| `PgLiveSide` | the tenant's real database, read-only, as of one recorded position |
| `PgScratchSide` | a throwaway schema the rebuild applies into, dropped whatever happens |

## The live side: one snapshot, one position

`PgLiveSide::open(bridge, tenant, catalog)` begins **one** transaction and keeps it for the life of
the side:

```sql
BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY;
SET TRANSACTION READ ONLY;
```

The head is the change feed's last published position, read inside that transaction
(`spot_meta.feed_generation` and `spot_feed_state.last_seq` of the `changes` log). Every later
answer — `summaries(as_of)`, `fingerprints(as_of)` — comes from the same snapshot, so a record that
commits while the audit runs does not count, and a transaction that publishes after the snapshot
does not move the head.

`as_of` may only be the recorded head: the live database cannot be read in the past. Any other
position is refused with `journal_audit_pg.as_of_not_head` (`summaries_for` / `fingerprints_for`
report it; the traits' infallible answers record it on the side and yield nothing, which
`PgLiveSide::failure()` reports).

Read-only is the transaction's mode, not a convention: a statement that writes fails with SQLSTATE
`25006` (`journal_audit_pg.read_only`), and the crate sends no writing statement for the live side
(a unit test pins that every reader statement is a `SELECT`).

## What both sides answer

A **record summary** is `(entity, id, version, content_digest)`, sorted by entity then id, streamed
in bounded keyset pages (`WHERE id > $1 ORDER BY id LIMIT $2`, one page in memory at a time). The
digest is `spot_journal_audit::content_digest` of the canonical JSON of the record's image body —
the ADR-055 encoding, applied to the stored row exactly as the in-memory sides digest the image
they are handed (`spot_journal_audit_run::memory::body_digest`). `src/encode.rs` states every rule
and `tests/pg.rs` proves the equality for a record with every storage type. A child table is part
of its parent's body (an array of row objects carrying `id`, `idx` and the row's fields), never a
record of its own: a child row has no version.

Entities are the catalogue's document entities, minus the ones flagged derived (ADR-061 s3):
`with_derived` sets that set, and `spot_journal_audit::compare` skips them again, so a derived
balance table never counts as a difference.

**Financial fingerprints** are the named numeric sets of WP-MIG-06 (Trial Balance, Stock Balance,
AR and AP ageing) as *declarations* (`FingerprintQuery`: the entity, its key columns, the summed
column, and whether the sum is integer minor units or an exact decimal — never a float, ADR-12).
`with_fingerprints` supplies them. A declaration whose entity the catalogue does not hold is
skipped, so a stage without the ledger answers **no** fingerprints and the compare covers records
only; Foundation and the ERP register the real declarations later.

## The scratch side: created, applied, always dropped

`PgScratchSide::create(bridge, tenant, catalog, run, created_millis)` creates one schema in the
tenant's own database, named `spot_scratch_<tenant>_<run>`, where both parts are validated storage
names (lowercase letters, digits, `_`) and the whole name is bounded to PostgreSQL's 63 bytes: no
user text ever reaches an identifier. The schema holds the catalogue's tables, created the way
`prepare` creates them (`src/ddl.rs`), in one transaction — so a failure leaves no half-created
schema.

The rebuild applies into it through `ApplySink`: `apply_image` upserts one record by id and rewrites
its child rows, `apply_delete` removes a record and its child rows, each in its own short
transaction. The scratch carries the primary key of every table but no foreign keys, indexes,
exclusions or search projections: it is a compare target written in one pass from a journal whose
records are already the final state (ADR-061 s6), never a serving database, and a foreign key it
does not need would only make the order of images matter. Audit-class entries are noted and
dropped — the record compare does not read them.

It is dropped by:

1. `ScratchSide::discard()` — the runner's call on every path;
2. `Drop` — the guard, which runs on a refusal, an error and an unwind alike, so a panicking rebuild
   still leaves no schema;
3. `PgScratchSide::sweep(bridge, tenant, older_than_millis)` — for the process that died before
   `Drop` could run. The schema's comment records the tenant, the run and the creation instant
   (`created_millis` is an argument: the library reads no clock), so a sweep with an instant past a
   run's start drops exactly the leftovers and never a run still in flight.

## The held session, and why it exists

`spot-pg-bridge` hands a connection to a call and takes it back when the call returns, and a call's
deadline is clamped to the bridge's `max_deadline`. A transaction therefore cannot be held *across*
calls, so a side keeps **one call open for its whole life**: a thread it owns runs the call, the
transaction (live) or the schema (scratch) is begun inside it, and the side's methods reach it over
a channel. Two consequences for the host:

- **`max_deadline` must outlast the longest audit.** A live side's call lasts as long as the run —
  the rebuild happens while the snapshot is open — so size the bridge's ceiling above it, not at the
  default 30 seconds. `PgLiveSide::open` asks for an hour and the bridge clamps it.
- **No statement runs on the caller's thread.** The audit runner is driven inside a tokio runtime
  context (`Handle::block_on`), where the bridge refuses a call (`pg_bridge.wrong_thread`); every
  statement of these sides runs on a plain thread the side owns, so the sides work there.

## Tests

```bash
export SPOT_TEST_PG_URL=postgres://spot:spot@127.0.0.1:5432/postgres   # an admin connection
export SPOT_REQUIRE_PG=1                                               # make a missing URL fail
cargo test -p spot-journal-audit-pg
```

Without `SPOT_TEST_PG_URL` the real-database tests print one `SKIPPED:` line each and pass, exactly
as the sibling backend crates do. They prove: the live digest equals the in-memory digest for the
same image; a write after `open` does not count; a populated scratch and the live database compare
equal through `spot_journal_audit::compare`, fingerprints included; a write through the live
connection is refused with `25006`; a scratch schema is gone after `discard`, after `Drop` and after
a panic; the sweep removes a crashed run's leftover and leaves a running one; pages are bounded and
the stream is sorted; a wrong tenant and a position other than the head are refused.
