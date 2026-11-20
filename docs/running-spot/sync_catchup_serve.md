Part of [Running Spot](../running-spot.md).

# Catch-up served and applied

`sync_catchup` routes a returning node (live relay, a coalesced catch-up plan or a snapshot
bootstrap). `sync_catchup_serve` (`[services.sync_catchup_serve]`, start order 1155) moves the data.

```toml
[services.sync_catchup_serve]
enabled = true
chunk_max_records = 500     # most records in one bundle
chunk_max_bytes = 4194304   # most image bytes in one bundle
hard_limit_records = 5000   # a dependency group larger than this refuses the bootstrap
```

## On a hub

The chunk server cuts **bootstrap chunks** from the latest **verified** snapshot of the journal
store (read only), for the node's *registered* scope (the node cannot name its own), whole
dependency groups at a time, redacted by `excluded_fields.down`. While no snapshot is verified a
returning node waits (`sync_catchup_serve.no_verified_snapshot`) instead of being served an
unverified base. **Catch-up chunks** come from the planner's plan: the mandatory lane event by
event and in order, then the coalesced bundles, then the audit events addressed to the node. Every
chunk carries the hash of its body. A chunk is served idempotently (asking again plans again and
gives the same chunk); the hub's bookkeeping moves only when the node acknowledges the chunk, and
the last acknowledgement ends the bootstrap and frees its slot.

## On an edge

The chunk applier verifies the hash (a mismatch is refused whole, nothing applied), refuses an index
that is not the next one, applies the chunk through the node's apply path (one transaction per
bundle, idempotent by record name and version) and **only then** moves its checkpoint, kept in the
same progress table the hub uses (`spot_sync_catchup_progress`, keyed by the hub). After a cut the
node asks for the chunk its checkpoint names: at most the chunk in flight is repeated, and a repeat
changes nothing. The last chunk of a bootstrap gives the position to continue from on the live
stream (`P + 1`).

## Not ready

Nothing is served or applied on a guess. On a hub the module needs the director (`sync_catchup`),
the read-only journal store and manifest keys (`journal_wiring`) and the hub's node scope provider;
on an edge it needs the node's apply path. A standalone site has no catch-up to serve. Anything
missing is named in readiness: `sync_catchup_serve.not_ready: <what> not provided`.
