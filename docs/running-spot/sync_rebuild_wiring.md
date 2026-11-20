Part of [Running Spot](../running-spot.md).

# Sync and rebuild wiring

Process distribution (`sync_process`) and the offline rebuild (`spot-journal-rebuild-host`) were
written against seams. This module, `[services.sync_rebuild_wiring]`
(`crates/framework/spot-cli/src/services/sync_rebuild_wiring.rs`, start order 1235, before
`sync_process` at 1240), is where their production providers live.

```toml
[services.sync_rebuild_wiring]
enabled = true            # false: no pin source and no hub stream (nothing is retired, hub side off)
pin_refresh_seconds = 5   # how often the pinned Cases are counted
pin_max_age_seconds = 30  # a count older than this is not believed
queue_capacity = 1000     # hub process records that may wait for the hub sync service
```

## On a running site

- **Pin source.** The number of running Cases pinned to each `(process, version)` is counted from
  the BPM case table every few seconds and answered from memory. **A count that was never read, or
  is older than `pin_max_age_seconds`, says one Case is pinned**, so a process version is never
  retired on a guess (`sync_rebuild_wiring.pins_unreadable` is logged once while the table cannot
  be read). Edge definitions are read through `ProcessSync` from `spot_sync_process_definition`:
  the BPM store assigns versions itself, so hub versions are not installed there.
- **Hub stream.** Publishing a process version queues the hub-authored record with its scope keys;
  a flusher hands it to the hub sync service, oldest first, retrying with backoff
  (`sync_rebuild_wiring.hub_sink_refused`). A full queue refuses the publish
  (`sync_rebuild_wiring.hub_queue_full`) rather than dropping a record. It is provided **only on a
  hub or standalone role and only when the hub sync service provides its master record sink**;
  without it the hub side of process distribution stays off (`sync_rebuild_wiring.hub_sink_absent`
  at debug level) and an edge is unaffected.

## Offline rebuild

The CLI (WP-RTB-04) builds the rebuild host's dependencies from these:

| Piece | What it does |
| --- | --- |
| `LeaseGuard` | Refuses (`site_running`) while the shipper's writer lease `journal-ship:<site>` is live, or when the coordinator cannot answer; then takes the maintenance lease `journal-rebuild:<site>` for the run (renewable, released at the end). |
| `StoreJournal` | The catalogue from the **verified** manifests and snapshot indexes of one tenant (another key or tenant is refused), and the fork recorded through the relay. `before <timestamp>` is not resolvable (the journal has no time index): name a position. |
| `StoreRebuilder` | The executor over the read-only journal store; with a `Reveal` it turns protected fields back into their values (an erased subject reads as the documented placeholder). No upcaster is registered yet. |
| `InvariantSuite` | Runs the registered `InvariantCheck`s in order. **With no check registered it fails** (`sync_rebuild_wiring.no_invariant_registered`): a rebuild nobody checked is not reported as sound. |

## The invariants a rebuild must pass

The rebuild host runs a default suite, `ProductionInvariants`, after the projectors. Three checks,
each of which fails the rebuild:

| Check | What must hold |
| --- | --- |
| `row_counts` | Per table, the rebuilt database holds as many rows as the journal replayed into a **scratch store**. |
| `content_digest` | Per table, an order-independent digest of the rows equals the digest of the same replay. A row that changed, appeared or vanished changes it. |
| `journal_sequence` | The positions applied between the snapshot and the head have no gap and no duplicate; the last applied position is the head. |

A failing check names the table or the range and nothing else: `row_counts:invoice`,
`content_digest:invoice`, `journal_sequence:g1 gap 43..44`, `journal_sequence:g1 duplicate 42`,
`journal_sequence:g1 short of head 44..45`. Never a row, a value or a digest. A source that cannot be
read (the database, the scratch replay or the journal positions) fails the rebuild with
`..._unreadable` naming only the source.

The digest is the sum of a per-row hash (PostgreSQL `hashtextextended` of the row's text), so memory
is constant and the order of rows does not matter. The rebuilt database and the scratch replay must
be on the **same server version**. The tables compared are the ones the caller names (the entity
tables of the journal catalogue); never the outbox, the checkpoint or any table that differs by
design.

The scratch replay is the same executor and apply path as the rebuild, into a scratch database on
the same server (the journal audit's scratch run does the same); it is dropped after the digests are
read and never touches the rebuilt database.
