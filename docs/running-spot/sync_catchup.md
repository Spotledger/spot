Part of [Running Spot](../running-spot.md).

# Catch-up and bootstrap (`[services.sync_catchup]`, hub only)

A node offline for two days catches up in dependency order with the mandatory lane first; a new node bootstraps from a snapshot; a node one release behind keeps syncing and an undecodable record is held alone and released after the upgrade (ADR-066). This is the hub-side routing; the planner, the bootstrap and the skew rules are the libraries `spot-sync-catchup`, `spot-sync-bootstrap` and `spot-sync-skew`.

```toml
[services.sync_catchup]
catchup_threshold_events = 5000   # the largest gap still replayed in position order (1 to 10,000,000)
snapshot_share_percent = 40       # a coalesced plan dearer than this share of a snapshot becomes a bootstrap (1 to 100)
bundle_records = 500              # records per chunk (1 to 10,000)
max_parallel_bootstraps = 4       # per tenant (1 to 64): 100 returning nodes do not read 100 snapshots at once
retry_base_ms = 2000              # the least a refused node waits
retry_jitter_ms = 3000            # the spread the node id picks inside
chunk_interval_ms = 100           # one chunk token per node per interval
chunk_burst = 4
allowed_release_skew = 1          # how many releases a node may be behind the hub
```

Unknown keys are refused. Run `spot migrate` first: the progress table `spot_sync_catchup_progress` is created by it.

## The routes

| The node is | The hub does |
| --- | --- |
| close behind, prefix still in the hot queue | live relay |
| far behind | a coalesced plan: the latest image per record, links first, the mandatory lane first, bounded chunks; the queue shows `catching_up 40% (chunk 4 of 10)` |
| new, resynced, or the coalesced plan costs more than the snapshot share | a snapshot bootstrap from the latest **verified** journal snapshot, in chunks, then the stream from P+1; `bootstrapping 60% (chunk 6 of 10)` |
| one of too many bootstrapping | `Wait` with a retry-after (a base plus a spread that depends on the node id) |

Progress is stored on every change: a restart resumes a plan at the next chunk and never from zero; an out-of-order chunk is refused.

## Version skew

At `hello` the node's decode capability is recorded. A record outside it is held on the hub together with what depends on it ("dependency held"); nothing is dropped and nothing is delivered half. A node that reconnects with a higher capability has its held records queued again in position order. A node more releases behind than allowed is refused with a `sync_skew.*` code and the upgrade channel only.

## Security

A bootstrap serves only the records in the requesting node's scope, filtered by the hub's destination scope over the node's *registered* subscription (never a scope the node sends) and redacted by `excluded_fields.down` before it leaves the hub; a chunk key outside the snapshot index is refused and chunk hashes are verified before apply. Personal fields stay sealed: an edge receives the sealed form or the erased placeholder, never the subject key.

## Wiring state

The director and the skew gate are built and routed here; the hub host (WP-RTB-02.T4, T13) has to call them from its hello and pull paths and read the snapshot chunks through `spot-sync-bootstrap-reader`, and the node host (T5) has to apply them: those files are not part of this task, so until they call `Director::route` a returning node is relayed as before.
