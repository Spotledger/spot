Part of [Running Spot](../running-spot.md).

# Workload routing

`services.workload_routing` hosts `spot-workload-routing` (WP-PLT-20.T1), the pure policy
engine that decides where a request runs and where its reads go. This page is about the
hosting: the settings, the HTTP behaviour and what is still advisory.

## Queue affinity

Every node declared in `nodes` gets the queues the standard role → queue mapping assigns its
roles (`interactive`/`gateway` → `default`, `interactive`; `report` → `reports`, `exports`;
`ai` → `ai`; `bulk` → `exports`). `QueueAffinityPort` (`spot-http`) exposes the local node's
queues to any later module.

**Advisory today.** The job worker (`services.jobs`) still claims exactly the queues
`services.jobs.queues` names; it does not read this port. Wiring it so the worker refuses a
queue the policy does not assign this node is `jobs.rs`'s own change, owned by another task
(see WP-PLT-20.T2's Open questions) — until then, a misconfigured `services.jobs.queues` is not
caught by this module.

## Report routing

A request under `/api/v1/report/` is routed by the same standard path rule: served locally on a
node that carries the `report` role, and on any other node answered with an HTTP
**`307 Temporary Redirect`** to the owning node's configured `host`. This is the chosen library
convention (the alternative is an internal mTLS proxy hop); a redirect was chosen because it
needs no new transport dependency and keeps every security decision exactly where it already
lives: the caller's own client makes the next request directly against the target node, which
authenticates and re-authorises it itself, under its own session or API key, precisely as it
would any other request. This module never reads, holds or forwards a credential.

A node with no configured `host` is **unavailable** as a redirect target (there is nowhere to
send the client). What happens then is `fallback_to_local`:

- `true` (the default): the request runs on this node instead.
- `false`: the request is refused with `503` and `workload_routing.no_target`.

A path rule that matches no node *at all* (an empty pool, not merely a node with no `host`)
always falls back to local — that is the policy engine's own rule, from WP-PLT-20.T1, and this
setting does not change it.

## Replica reads

Only the reports named in `replica_reports` may read a replica (`replica_ok`), each within its
own `max_lag_ms`; a report not named there always reads the primary, however fresh a replica is.
`replicas` is this node's live view of the replica pool (id, lag, applied token) — there is no
live replication-lag monitor wired yet, so an operator or a side process updates it; see Open
questions. A replica whose lag exceeds the report's bound, or an empty `replicas` list, falls
back to the primary. The chosen [`ReadPreference`] is attached to the request's extensions for a
storage adapter to honour; `spot-postgres` does not read it yet (WP-PLT-20.T1's own open
question, unchanged here).

## `common.toml`

```toml
[services.workload_routing]
enabled = true
local_node = "report-1"
fallback_to_local = true
max_hops = 4

[[services.workload_routing.nodes]]
id = "interactive-1"
roles = ["interactive"]
workers = 8
max_queue_depth = 200

[[services.workload_routing.nodes]]
id = "report-1"
roles = ["report"]
host = "https://report-1.internal:8443"
workers = 4

[[services.workload_routing.replicas]]
id = "replica-1"
lag_ms = 120
applied_token = 48213

[[services.workload_routing.replica_reports]]
report = "sales_summary"
max_lag_ms = 5000
```

A site with no `[services.workload_routing]` table runs as a single node (`local`, role
`interactive`) with no replica and no allow-listed report: every request is served locally and
every read goes to the primary, exactly as before this module existed.

## Security

The redirect carries no credential and this module reads none: the client's own re-request to
the target node goes through that node's normal authentication and authorisation, so a request
can never reach report data it was not already allowed to see. Nothing here bypasses permissions
or tenancy; nothing here is a new trust boundary.

## Troubleshooting

| Symptom | Look at |
| --- | --- |
| A report request loops between two nodes | `nodes[].host` on both ends: a redirect target whose own policy would redirect it right back is a configuration error, not something this module detects. |
| A report node answers `503` instead of running the report | `fallback_to_local = false` and the target node has no `host` configured, or no node carries the `report` role at all with a `host`; set one or allow the fallback. |
| A replica is never used | The report's name is not in `replica_reports`, `replicas` is empty, or every replica's `lag_ms` exceeds the report's `max_lag_ms`. |
| `services.workload_routing.local_node` fails the site's start | It must name one entry of `services.workload_routing.nodes`. |

## Open questions

- Queue-affinity enforcement in the job worker (`services.jobs`/`jobs.rs`) is not wired: see
  WP-PLT-20.T2's report.
- Live replica lag monitoring is not wired: `replicas` is a settings-declared, operator- or
  side-process-maintained view, not a telemetry feed from the database.
- `spot-postgres` does not yet read the `ReadPreference` this module attaches to a request's
  extensions (carried over from WP-PLT-20.T1).
