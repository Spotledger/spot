Part of [Running Spot](../running-spot.md).

# sync_node: the edge's sync loop

Runs inside `spot serve` on a site with `role = "edge"` (see `node_role`). On any other role the
module is skipped. The edge always dials out; the hub never dials the edge.

## Configuration

Hub address, pinned fingerprint, node id and the enrolment code file are the
`[services.node_role]` keys `hub_url` (must be `https://`), `hub_fingerprint`, `enrol_code_file`
and `[partition] node`. The loop's bounds are `[services.sync_node]`:

| Key | Default | Meaning |
| --- | --- | --- |
| `backoff_min_secs` | 1 | shortest wait after a failed session |
| `backoff_max_secs` | 300 | longest wait (the schedule doubles up to it, jittered) |
| `min_reconnect_secs` | 5 | least time between two connection attempts |
| `batch_records` | 256 | records per pull and push batch |
| `hub_source` | `<site>#hub` | the hub's stream |
| `outbox_generation` | 1 | outbox generation the node's own stream reads |
| `identity_dir` | `<config root>/sync-node/<site>` | key, certificate and hub pin |

## First start

Create the code file with mode `0600`:

```
code=<one-time code>
hub_fingerprint=<sha-256 hex of the hub certificate>
```

The node redeems it once, stores the pin, and overwrites and deletes the file. It never needs the
code again. Without a code and a pin the loop does not run and the line reads `sync_node: not enrolled`.

## What you see

`sync_node: connected to hub-1 (lag 0.2s)` or `sync_node: offline 3h12m, 418 changes queued`.
Offline is normal and never makes the site not ready. The hub is authenticated by its pinned
certificate only; a different certificate (even a CA-signed one) is refused.

## Tables

`spot_sync_node_cursor` (this module) and the tables of `spot-sync-node-pg`. Run `spot migrate`; the
host never creates them.

## Edge intake: what is wired (WP-AI-07.T17)

On an edge the apply path (`sync_node_intake`) registers its intake factory only when it holds
five inputs from other modules. Otherwise the edge applies nothing and the start-up log names what
is missing (`sync_node_intake: the apply path is not wired`, with the list). State on dev, read
from the `ctx.provide` calls of the services:

| Input (name in the log) | Port | Provided by | Should be provided by |
| --- | --- | --- | --- |
| quarantine store | `dyn QuarantineStore` | the `quarantine` module (this task; unless `provide_store = false` or the site has no database) | done |
| partition registry | `dyn spot_partition::PartitionRegistry` | nobody as a port: `services/partition.rs` builds a `SnapshotRegistry` per site for the host, and the participants read it from the host | the `partition` module, providing its per-site `SnapshotRegistry` as the port (edge and hub roles, from `Host::for_site`) |
| replica audit | `dyn spot_replica_apply::ReplicaAudit` | nobody | a new `replica_audit` module (or `sync_node_intake`'s neighbour in `sync_node`), writing the apply path's audit markers to the platform audit chain, as `quarantine` does for its decisions |
| node catalog | `spot_sync_node_pg::PgNodeCatalog` | nobody | `sync_node`, which already holds the site's pool and the node's identity |
| node report | `dyn NodeReportSource` | nobody | `sync_node` (or `node_role`), which knows the software version, the metadata version, the migration epoch and the decodable majors |

So one of five is provided after this task; an edge still reports four missing parts. The
quarantine part is no longer the reason an edge applies nothing.
