# Journal snapshots (`[services.journal_snapshots]`)

A snapshot is a base image of every captured record as of one journal position `P`, written beside the journal from a consistent database read. A rebuild starts from the newest trusted snapshot instead of replaying the whole journal, which is what keeps the recovery time objective (4 hours standard, 1 hour premium) (ADR-061 section 6).

It runs whenever `[services.journal]` is on. Nothing to configure unless you want to change the defaults:

```toml
[services.journal_snapshots]
enabled = true              # false: no snapshots while the journal stays on
at = "02:30"                # the daily time, in the site's offset
utc_offset_minutes = 0      # the site's offset from UTC (no zone database is read)
keep_daily = 7              # retention: the newest of each of the last 7 days
keep_weekly = 4             # and of each of the last 4 weeks
page_size = 500             # records per page: memory is bounded by this, not by the tables
poll_seconds = 30
reach_timeout_seconds = 300 # how long a snapshot waits for the journal to reach its position
statement_timeout_seconds = 600
```

## When one is taken

Daily at `at`, and right after a migration (the site's metadata swapped). One at a time per site: a coordinator lease (`journal-snapshot:<site>`). Nothing is taken while the journal is empty.

## How it stays honest

- **Consistent.** One repeatable-read, read-only transaction; the outbox head is read inside it. A write that commits during the snapshot is not in it. No table lock is taken (a snapshot never blocks writers); there is a statement timeout and a lock timeout.
- **Sealed.** Personal fields are sealed (the same protect layer as the journal, WP-RTB-01.T8) and parts are hash-verified by the reader. A snapshot of one site is written only under that site's journal prefix.
- **Backed by the journal.** The signed index is written **last**, and only after the journal's shipped watermark has reached `P`. If the journal is behind, nothing is listed and `journal_snapshot_pg.journal_behind` is recorded; a snapshot with no index never counts.
- **Unverified until proved.** A snapshot is listed unverified. The audit run calls `SnapshotHandle::mark_verified` when a scratch rebuild from it is clean. A rebuild uses only verified snapshots unless an operator overrides (the planner decides; this service does not weaken it).

## What you see

- Readiness text: `snapshots: last g1:s20931 4h ago (verified), next 02:30` (`unverified` until proved, `, failing x2` after failures).
- `sys_journal_admin` records: `snapshot_taken` (target `g<generation>:s<position>`), `snapshot_failed` (with the error code).
- Two failures in a row raise the amber condition `SNAPSHOT_FAILING` (parameter `reason`), cleared by the next success. `JOURNAL_GAP` is reserved for chain damage.
- Retention is **planned and reported**, not applied: the journal store refuses to delete anything under `snapshots/`, so old snapshots stay until a provider-level prune exists.
