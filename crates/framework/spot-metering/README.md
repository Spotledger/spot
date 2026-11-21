# spot-metering (WP-CTL-05.T1)

Pure library for tenant usage accounting and quota decisions. It reads neither a
clock nor the environment, opens no network connection, and owns no database engine.
Every instant is an explicit UTC-second argument. Zone rules come from jiff's bundled
tzdb, not host tzdata.

## Accounting contract

| Meter | Combination | Charged observation |
| --- | --- | --- |
| ActiveUsers | DistinctSet | Producer-supplied tenant-scoped member digest |
| Transactions | Counter | Committed posting of a caller-allowlisted DocType |
| StorageBytes | Gauge | Maximum observed total bytes, not deltas or sums |
| AiTokens | Counter | Input, output, cache-read and cache-write tokens |
| Custom(name) | Counter | Caller-declared integer amount |

`MeterEvent.id` identifies a fact; `(tenant, id)` is the deduplication key.
First write wins, including after compaction. Producers must use disjoint event-id
namespaces across facts/meters within a tenant. AI events preserve the router's
request id exactly. Member digests are supplied by the producer: choose a stable,
tenant-scoped privacy-preserving digest, never pass usernames or other personal data.
Document contents, prompts, providers and model names are not retained by the store.

The in-memory store validates/resolves batches and preflights counter overflow before
writing. It keys totals by the full PeriodKey and Meter enum, not just a display label
or meter name. Compaction removes raw observation metadata but retains totals, member
sets and idempotency tombstones; the reference store is consequently not a bounded
production database. `contract::run_store_contract` is for an empty backend instance.
It tests deduplication, aggregation, tenant/period isolation, the retention horizon,
tenant binding, the batch bound and post-compaction replay.

## Tenant binding and the batch bound

A store is constructed for **one** tenant (`MemoryMeterStore::new(tenant)`,
`PgMeterStore::new(pool, tenant)`). `MeterStore::tenant` names it. `add_events` refuses
a batch containing an event of any other tenant with `TenantMismatch` before it resolves
or writes anything, and `period_totals`/`distinct_members` refuse to answer for another
tenant the same way: the tenant is a property of the store a caller was handed, never of
the payload. The same event id under two tenants is therefore two facts in two stores,
which is what a multi-tenant deployment needs and what a producer naming a victim tenant
can no longer abuse.

One `add_events` call accepts at most `MAX_BATCH_EVENTS` (10 000) events; a bigger batch
is refused whole with `BatchTooLarge` before any write, so no call holds one transaction
for an unbounded time. A caller with a bigger backlog sends several calls — recording is
idempotent by id, so the totals are identical.

## Retention, tombstones and replay

Idempotency needs a tombstone — the remembered id of every event ever counted — and
keeping them forever makes storage grow with the platform's whole history.
`RetentionPolicy { tombstone_horizon_periods }` is an argument to `compact(before,
policy, resolver)`, never a clock read. Compaction removes the tombstones of periods
that are **both** final (the caller's `before` cut: those periods are closed and
reconciled) **and** at least `horizon` periods older than that cut.

The same cut becomes the store's late-arrival floor, which is what makes removal safe:

- a replay **inside** the horizon finds its tombstone and is a no-op;
- anything **beyond** the horizon — a replay whose tombstone was purged, or a first-time
  event that late — is refused as `late_beyond_horizon` and never counted.

The purged set and the refused set are the same set by construction, so a purged id
cannot be counted twice. The floor only ever moves forward; a later compaction with an
earlier cut does not re-open a period.

**Replay risk beyond the horizon is real and deliberate**: a producer that replays a
fact after the horizon loses that fact. Nothing else detects it, and raising the horizon
later does not recover it, because the floor never recedes. Size the horizon as the
longest producer retry window plus the lag between a period closing and its last
reconciliation, plus one period of slack:

```text
horizon  >=  longest producer retry window
          +  reconciliation lag after a period closes
          +  1 period of slack for a month-boundary race
```

`RetentionPolicy::DEFAULT_HORIZON_PERIODS` is 13 (a year plus the reconciliation month), and
compaction refuses anything below `RetentionPolicy::MIN_HORIZON_PERIODS` (2) with
`retention_horizon_too_short`: a horizon inside a producer's retry window loses revenue, so it
is a configuration error, not a storage trade-off. Before a retention policy is in force
nothing bounds the late-arrival window at all; the horizon is what creates it, so an operator
who shortens it shortens the promise. See `src/retention.rs` for the invariant and the exact
walk-back rule.


`BoundedSink` preallocates its configured slots. Producers use `try_lock`: contention
or overflow returns a dropped receipt immediately, incrementing an atomic cumulative
loss counter. The flusher may take a queue lock. Draining transfers ownership to the
host; failed store writes must be retried by the host, and queue drops must be replayed
from the producer's durable source using the same id. This buffer is not an outbox.

`MeterTelemetry` is an injected counts-only bridge for the `metering.dropped` counter.
Its host implementation must be non-blocking and add no tenant, member or event-id
labels. `spot-telemetry` currently exposes metric functions, not the `Telemetry`
handle named by the brief; this crate adds no exporter dependency or process-global
recorder. Always pass `sink.dropped()` into quota refresh/reconciliation, even when no
telemetry bridge is installed. Loss is cumulative; successful retries repair totals,
not the history of drops. A report may have agreeing rows but a nonzero drop count.

## Periods and quotas

Calendar months use tenant-local business dates; anniversary months clamp the chosen
anchor day in short months. Windows are half-open UTC ranges. Keep a tenant's period
spec stable for historical facts; changing zone/basis is a host migration decision.

Use `QuotaPolicy::from_entitlements_with_policy` to combine licence bounds and tenant
modes/fractions without allowing the tenant to enlarge a licensed limit. Extra limit
aliases use the stricter bound; full u64 extra limits remain bounded. Unknown extras
are returned as warnings for the host to surface. Defaults are warn at 8000 basis
points, 1000 basis points tolerance, WarnOnly, and 24 hours soft grace.

Active-user quotas gate new users. Transactions, storage, AI and custom quotas gate
new metered documents. Quotas never gate reads, own-data exports, existing-user
sign-ins, reversals or cancellations. Licence-state decisions are composed separately:
this library does not override restrictions imposed by `spot-license` itself.

A `QuotaGuard` is bound to one tenant and answers `allow(action, tenant, now)` from a
single ArcSwap snapshot, without allocating, locking, or querying a store. Refresh
with a `RefreshContext` after flushing; initialise from an evaluated policy before
using it for hard quotas (an explicitly empty snapshot has no configured rules).
Stale snapshots fail open except for bounded HardBlock meters. Unlimited meters never
fail closed. `staleness(now)` exposes the age; backwards time and period rollover also
make the hot-path reading stale. Soft grace deadlines survive repeated refreshes in a
period; older refresh results cannot replace newer snapshots. This is a cached guard,
not an atomic reservation mechanism for concurrent document creation.

## AI source limitation

The existing `UsageLedger` can sum only UTC day keys and cannot enumerate records or
filter arbitrary instants. `AiTokensFromLedger::total_for_period` therefore returns a
typed error for partial UTC-day periods instead of presenting an approximation as
truth. UTC-day-aligned periods honour all SumQuery filters. For tenant-zone boundaries,
`total_for_records` computes an exact, request-id-deduplicated range total from records
enumerated by the caller. `events_for_records` supplies the corresponding event stream.
Upstream enumeration/range-query support requires a separate router-owned change.
Combined token amounts and store counter overflow return errors rather than saturate.

## Verification

- `cargo fmt --all -- --check`
- `cargo clippy -p spot-metering --all-targets -- -D warnings`
- `cargo test -p spot-metering`
- `cargo test -p spot-license -p spot-llm-router -p spot-errors`
- `python3 tools/lints/error-codes/error_codes_lint.py`

Acceptance includes 10,000 seeded random facts with reorderings, duplicates and real
bounded-queue drop/retry; independent truth reconciliation; proptest permutations and
duplications; DST gaps/folds, leap day, half-hour offsets, month ends and anniversary
clamping; quota action exemptions, stale-mode behaviour, grace expiry, exact AI ranges,
large licence bounds, tenant isolation, overflow atomicity and compaction replay.
