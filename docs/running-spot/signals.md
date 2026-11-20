Part of [Running Spot](../running-spot.md).

# Operational signals: the series of the journal, sync, quarantine, conditions and migration

`/metrics` serves HTTP, database and lease series (`spot-telemetry`) and, with the signals
service of `spot serve` (WP-OPS-01.T3), the series below. The names, units and labels live in
`crates/framework/spot-ops-signals/src/catalogue.rs`; this table is checked against it by a test,
so it cannot drift. Every series is `spot_`-prefixed, counters end `_total`, durations are
seconds and counts have no unit suffix.

A series that a site does not host is **absent**, not zero: a zero reads as "healthy".

| Series | Kind | Unit | Labels | Meaning | Suggested alert |
| --- | --- | --- | --- | --- | --- |
| `spot_journal_ship_lag_seconds` | gauge | seconds | - | Age of the oldest outbox entry not yet durable in the journal; 0 when none | `> 300 for 10m` |
| `spot_journal_unshipped_entries` | gauge | count | - | Outbox entries not yet durable in the journal | growing for 30m |
| `spot_journal_ship_batches_total` | counter | count | `outcome` | Ship batches: `ok`, `retry`, `failed` | `rate(outcome="failed") > 0` |
| `spot_journal_durability_wait_seconds` | histogram | seconds | - | How long a commit waited for journal durability | p99 `> 2s for 10m` |
| `spot_journal_durability_decisions_total` | counter | count | `decision` | `acked`, `degraded`, `halted`, `unknown_outcome` | any `halted` or `unknown_outcome` |
| `spot_journal_snapshot_age_seconds` | gauge | seconds | - | Age of the newest journal snapshot | `> 2 days` |
| `spot_journal_audit_age_seconds` | gauge | seconds | - | Time since the last clean journal audit | `> 8 days` |
| `spot_journal_audit_runs_total` | counter | count | `result` | Audit runs: `clean`, `mismatch`, `failed`, `unknown_topics` | any `mismatch` |
| `spot_sync_apply_total` | counter | count | `outcome` | Records by apply outcome: `applied`, `duplicate`, `parked`, `quarantined`, `refused` | `rate(outcome="quarantined") > 0` |
| `spot_sync_send_queue_depth` | gauge | count | - | Records queued to send across all nodes (hub) | growing for 1h |
| `spot_sync_node_lag_seconds` | gauge | seconds | `stat` | Lag over nodes: `p50`, `p95`, `max` | `stat="p95" > 3600 for 1h` |
| `spot_sync_nodes` | gauge | count | `state` | Nodes by state: `healthy`, `lagging`, `silent`, `suspended`, `too_old` | `state="silent" > 0 for 1h` |
| `spot_sync_obligations` | gauge | count | `level` | Sync obligations: `due`, `overdue`, `red` | `level="red" > 0` |
| `spot_sync_holds` | gauge | count | - | Records held for version skew | `> 0 for 1d` |
| `spot_quarantine_open` | gauge | count | - | Open quarantine items | `> 0 for 1d` |
| `spot_quarantine_oldest_age_seconds` | gauge | seconds | - | Age of the oldest open quarantine item | `> 7 days` |
| `spot_conditions_open` | gauge | count | `severity` | Open conditions: `info`, `amber`, `red`; all three are set every time | `severity="red" > 0` |
| `spot_migration_gate_closed` | gauge | count | - | 1 while the maintenance gate is closed | `== 1 for 1h` |
| `spot_migration_verbs_total` | counter | count | `verb`, `outcome` | Migration verbs (`plan`, `approve`, `rehearse`, `apply`, `hold`, `release`, `rescue`) by `ok`, `failed`, `refused` | any `outcome="failed"` |

The histogram `spot_journal_durability_wait_seconds` renders with the exporter's default shape
until `spot-telemetry` configures buckets (5 ms to 10 s) for it; see the task report.

## The cardinality rule

No label names a site, tenant, node, user, document, topic or condition. A hundred branch nodes
would otherwise be a hundred series for each metric, and a topic or document name is data. Every
label value is a closed enum in `spot-ops-signals`; a helper takes the enum, never a string, so a
caller cannot pass an unbounded value. Per-node and per-condition detail is the admin screens' and
the audit's job. The test `catalogue.rs` refuses a forbidden label name, and `recorder.rs` records
every enum value and asserts the number of series equals the bound the catalogue declares.

## Counts

A count is a `u64` converted to `f64` at the facade. Above 2^53 it would lose precision, so the
helpers clamp it to 2^53 (9 007 199 254 740 992). No real count reaches it.

## How to add a series

1. Add the name constant and the typed helper (with its label enum) in the module of the subsystem.
2. Add a `spec(...)` line to `SERIES` in `catalogue.rs`.
3. Add a row to the table above, with the label names in backticks (or `-`).
4. Run `cargo test -p spot-ops-signals`: the naming, cardinality and doc tests fail until all three
   agree.
