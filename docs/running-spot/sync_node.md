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
