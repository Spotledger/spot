# spot-audit-store

The PostgreSQL home of the tamper-evident audit chain (WP-AI-11.T2). It implements
`spot-audit-chain`'s `AuditStore` over a caller-supplied `deadpool_postgres::Pool`, bridges the
agent authoriser's decisions into the chain, runs a scheduled verification job, and builds a
human review digest.

## The database cannot rewrite history

`PgAuditStore::prepare` creates two tables and one role:

- `spot_audit_chain(site, seq, prev_hash, hash, record, at)`, primary key `(site, seq)`. An append is
  one statement guarded by the head, so two writers can never fork the chain.
- `spot_audit_checkpoint(site, seq, head_hash, at, key_id, signature)`, primary key `site`: the newest
  signed checkpoint per site.
- the role **`spot_audit_app`** (NOLOGIN). It is **never** the pool's login role; production
  connects as it, so the grants below are the application's day-to-day permissions. It gets
  `SELECT, INSERT` on `spot_audit_chain` and `SELECT, INSERT, UPDATE` on `spot_audit_checkpoint`. It is
  explicitly **denied** `UPDATE, DELETE, TRUNCATE` on `spot_audit_chain`.

Databases created before the `spot_` prefix hold `audit_chain` and `audit_checkpoint`. `prepare`
renames them, with their constraint names and the guard trigger and function, in the one
advisory-locked transaction that also creates anything missing: every row stays in place (the rename
rewrites nothing, and the hash chain covers record bytes, not table names), the second call changes
nothing, and a database holding both the old and the new name of a pair, or an old-named table with
other columns, is refused with an `auditstore.unavailable` error that names the tables and drops
nothing.

Two guard triggers, `spot_audit_chain_no_rewrite` and `spot_audit_chain_no_truncate`, additionally refuse `UPDATE`, `DELETE` and `TRUNCATE` on
`spot_audit_chain` **even for the table's owner**, unless the maintenance setting
**`spot.audit_maintenance`** is `on`. That setting is the documented restore path: a restore turns
it on for the connection doing the restore, and nothing else ever sets it. Any edit that still
reaches the table is made detectable by the chain's hashes; the verification job (below) reports
the exact `seq`.

## Through the migrator (WP-AI-11.T4)

`TABLE_DDL` holds the two `CREATE TABLE IF NOT EXISTS` statements as `(marker table, statement)`
pairs, byte-identical to what `prepare` runs, so a host hands the migrator one
`ExtraTable::new(table, vec![statement])` per pair (`PlanOptions::extra_tables`). The role, the
grants and the triggers cannot be an `ExtraTable` (the migrator only creates objects), so:

- `install_guard(pool)` installs them, idempotently and under `prepare`'s advisory lock; it
  creates no table and fails with `auditstore.tables_missing` if either is absent. `spot migrate`
  calls it **after** the plan.
- `verify(pool) -> Protection` is read-only and reports, by name, whether each table exists, both
  triggers are present and enabled, the role exists and holds no `UPDATE`/`DELETE`/`TRUNCATE` on
  the chain. Serve start-up calls `Protection::require_complete()` (`auditstore.protection_incomplete`)
  and refuses to serve otherwise: between the plan and `install_guard` the chain is unguarded.

## Verification

`VerifyJob` is the body of a scheduled job (a WP-PLT-06 handler): it verifies the chain
incrementally from the last good checkpoint with the keys it trusts, signs a new checkpoint when
the head has advanced (idempotent for the same range), and — on a broken chain — returns a
`ChainBroken { kind, seq }` and `HandlerResult::Fail`. A wiring task publishes the signal; this
crate does not own an `OutboxEvent`.

## The decision sink

`AgentDecisionAudit` is a `spot-agent-tiers` `DecisionSink`: `record` never blocks, pushing to a
bounded channel a `DecisionReceiver` drains (`drain`) into the chain. A dropped record (a full
channel) raises an `audit.dropped` counter through the injected `Telemetry`; `Telemetry` is
defined here, with a `NoTelemetry` default, until a `spot-telemetry` bridge lands.
`AuditedAuthoriser` is the fail-closed path: it reserves a channel permit before deciding and,
with none, refuses the action without running it (or, under `OverflowPolicy::FailOpen`, runs it
and counts the drop). A decision record carries the action's **name and decision code only**,
never its arguments, and is marked masked.

## The digest

`digest(store, verifier, site, since, until)` counts a range by kind and actor class, lists every
refusal and every unmasked entry, and carries the range's verification status; it renders as text
(`Digest::to_text`) and JSON (`Digest::to_json`) for the support console.
