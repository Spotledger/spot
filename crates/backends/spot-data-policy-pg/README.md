# spot-data-policy-pg

The PostgreSQL **ledger** half of `spot-data-policy`'s `RetentionStore` (WP-SEC-05.T2, ADR-071,
ADR-082 class C): per-item outcomes, run checkpoints and append-only deletion proofs, in the
tenant's own schema, over the shared blocking bridge `spot-pg-bridge`. The live records come from
a `RecordSource` the host implements over the document lifecycle; `PgRetentionStore<S>` composes
the two.

## Where it may be used (ADR-082 section 2)

- From a control-plane process, a CLI command or a job worker. **Never from a request handler**,
  a hook, a lifecycle step or any task on an async runtime: the bridge refuses the call with
  `pg_bridge.wrong_thread`, and building the store there is refused with
  `data_policy_pg.unsupported_context`.
- Never inside a transaction an async store holds. The ledger takes its own pooled connection.
- Every call has a deadline constant in `ledger.rs` (point 5 s, scan 30 s, apply 60 s, prepare
  30 s). A timed-out write may or may not have been applied; items are idempotent, so retry.

## Sizing

One retention run applies items one at a time: one connection per active run. Set the bridge's
`max_in_flight` to the number of runs a process may have at once (1 for a CLI or one worker) and
the pool to at least `max_in_flight + 2`. `PgLedger::new` takes the pool size and refuses a bridge
whose `max_in_flight` exceeds `BridgeConfig::largest_in_flight(pool_size)`.

## Tables (all `spot_`-prefixed, no extension needed)

`spot_retention_outcome` (primary key run id, record id), `spot_retention_checkpoint` (run id),
`spot_deletion_proof` (record id; never updated or deleted by this crate, independent of the
record's lifetime). Created by `prepare()` in one transaction under an advisory lock; the tenant
schema must already exist.

## `apply_item`

One transaction: insert the outcome (`ON CONFLICT DO NOTHING`), run the effect through the
`RecordSource` on a blocking thread, insert the proof for a deletion, commit. The same item twice
(sequentially or concurrently) applies the effect once; a failing effect rolls everything back.
A crash after the effect and before the commit is the one window the database cannot close, so a
`RecordSource` must treat a repeated effect as success.

## Reads that can fail

Every read of the trait returns a `Result`: a database failure is `data_policy.store_failed`
naming the operation (`outcome`, `checkpoint`, `run_outcomes`, `proof`, `proofs`), never "no row".
The engine stops at the first failed read, applies nothing further and leaves its checkpoint as it
was; a later run with a healthy store resumes from the outcome rows with no duplicate effect.
`RecordSource::records` and `record` are fallible the same way.

## Tests

`SPOT_TEST_PG_URL=postgres://... cargo test -p spot-data-policy-pg` (skipped with a message when
unset; `SPOT_REQUIRE_PG=1` makes that a failure). `tests/parity.rs` runs one table of cases
against `MemoryStore` and the PostgreSQL store.
