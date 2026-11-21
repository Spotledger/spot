# spot-l7-events

Emitted-event conformance for the L7 engine workload (WP-TST-07.T4). Six invariants read **every**
outbox event of a run and judge it against `contracts/events`:

`events.registered`, `events.envelope`, `events.payload_schema`, `events.class`,
`events.no_secret_values`, `events.coverage` (see `src/lib.rs` for what each proves).

**No emitter without a schema.** A topic the registry says `spot-document`, `spot-jobs` or
`spot-doc-audit` emits must validate against a schema in `lifecycle`, `state`, `system`,
`system_platform` or `journal`. A new topic: add its schema in `contracts/events` first, then the
emitter, then (if a standard run must produce it) a line in `src/coverage.rs` with the reason.

## Running

```text
cargo test -p spot-l7-events                      # fake: always on; PostgreSQL when configured
SPOT_TEST_PG_URL=postgres://... SPOT_REQUIRE_PG=1 cargo test -p spot-l7-events --test postgres
```

The per-topic table (events seen, family, ok, unschematised) is `table::TopicTable`, printed by the
fake run and available as JSON.

## What is not covered

The L7 job workload uses the in-memory job store, so it writes no outbox events; the audit step is
not installed in the L7 world. Both are listed in `coverage::NOT_IN_THIS_WORLD`. They join the
expected table when a world installs them.

## Seeded failures

`tests/seeded.rs`: an unknown platform topic, a wrong-typed field, an extra field, a secret value in
a payload, a missing expected topic: each is caught by exactly one invariant; the clean outbox by
none.
