Part of [Running Spot](../running-spot.md).

# Graph projection (`[services.graph_projection]`)

Optional, **off by default**: nothing is created unless `enabled = true`. Mirrors edge keys and edge
types (never document field values) one way to an external graph engine, from the `sys_graph_edge`
outbox topic, through the egress guard only.

| Setting | Meaning |
| --- | --- |
| `enabled` | default false |
| `endpoint` | https URL; its host must be in `allowed_hosts` |
| `allowed_hosts` | this service's egress allow-list |
| `secret` + `[services.graph_projection.sealed]` (`sealed`, `aad`) | the credential's name and its sealed text; opened with the `[services.secrets]` key at start; never a value in the config |
| `database` | the external graph name (one per site) |
| `batch`, `interval_ms`, `max_backoff_secs`, `timeout_secs` | tuning (100, 1000, 300, 10) |

A bad endpoint or secret is a start-up finding in the log (`spot::graph_projection`); the service stays
off and the site serves. The cursor is `sites/<site>/graph_projection.cursor`; a failed push retries
with backoff and loses nothing; pushes are idempotent upserts. A down engine never slows writes.
