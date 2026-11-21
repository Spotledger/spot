# spot-migration-audit

The migration audit record (blueprint WS12, WP-MIG-11.T1, ADR-22).

A migration runs **plan → approve → rehearse → apply → verify**. This crate is the record of
every run, kept in the tamper-evident audit chain ([`spot-audit-chain`]), plus the query API a
console lists and explains runs through, and the guard that a migration runs only through the
runtime with a role permission.

## What it holds

- `MigrationRun` — the plan (by reference and digest), its approvals, the rehearsal report, the
  apply journal, the outcome, the timings and the actor. Large artefacts are stored **by
  reference** (`ArtifactRef`) with their SHA-256; a plan secret or a connection string never
  enters the record.
- `RunRecorder` — the trait the apply and rehearsal engines call; `ChainRunRecorder` appends each
  lifecycle event (`kinds::RUN_*`) to the chain. `RunRecorder::begin` is the only source of a
  `RunHandle`.
- `RunStore` — `get` one run with its full evidence, `list` by outcome, actor or date.
  `ChainRunStore` folds the chain back into runs and caches the fold on the chain head.
- `explain` — a run rendered in plain language.
- `permit` — checks the runtime permission `migration.run` (`RUN_PERMISSION`) against the acting
  `spot-permissions` principal and requires a `RunHandle`, so an apply with no recorded run is
  refused. `Permit` and `RunHandle` have no public constructor.

## What it never does

Reads no clock, random source or environment (time, ids and keys are arguments, ADR-077); uses no
float arithmetic, no `unsafe` and no `unwrap`/`expect` outside tests; never stores or returns a
secret.

[`spot-audit-chain`]: ../spot-audit-chain
