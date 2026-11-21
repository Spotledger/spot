Part of [Running Spot](../running-spot.md).

# AI workload isolation

Twenty parallel agents must not make interactive users wait. With `[services.ai_isolation]` switched on,
the MCP endpoint (`docs/running-spot/mcp.md`) is admitted through `spot-ai-isolation`, reads on a
database pool and role of its own, and shrinks to nothing when people wait. It is **off by default**
because it needs a database role password only you can provide; MCP then runs as its guide describes
(audited, rate limited, strictly as the user) without the isolation.

Level: optional for the site, **required for MCP once it is on**. If the role, the pool or the password
cannot be set up, readiness says `degraded: ai_isolation (...)` and `/mcp` answers
`503 server.unavailable` until the site restarts healthy. An unisolated agent path is never served.

## Settings

`[services.ai_isolation]`:

| Key | Default | Meaning |
| --- | --- | --- |
| `enabled` | `false` | Turn the isolation on (and make it a requirement of MCP). |
| `password_file` | none, **required** | Absolute path of a file holding the `spot_ai` role's login password: 16 to 128 characters of `A-Z a-z 0-9 . _ ~ -`. Keep it readable by the server user only. |
| `pool_size` | `4` | Connections of the `ai` pool (1 to 64). |
| `statement_timeout_ms` | `30000` | The role's server-side statement timeout, which is also the call deadline. |
| `schema` | `public` | The schema the role is granted on. |
| `sample_ms` | `250` | How often interactive load is sampled. |
| `per_agent_concurrency` | `4` | Calls one agent may hold at once on this node. |
| `per_tenant_concurrency` | `16` | Calls one tenant may hold at once on this node. |

## What start does

The `spot_ai` role is created if absent (login disabled until the last step), with the crate's statements:
no DDL, no INSERT/UPDATE/DELETE/TRUNCATE grant, `default_transaction_read_only = on`,
`statement_timeout`, `idle_in_transaction_session_timeout`, `work_mem` and `max_parallel_workers_per_gather
= 0`, and only CONNECT, USAGE, SELECT on the schema's tables and EXECUTE on its functions. It is then
enabled for login with the password from the file and a connection limit of `8 * pool_size + 8` (a role is
shared by every site on the server). The database user that runs `spot serve` must be allowed to create
roles; a refusal is a start-up failure, never a fallback.

A second pool logs in as `spot_ai`. The MCP read tools (`query`, `search_*`) run on it with the same read
policy as every other path (the user's read rights compiled into the query, field masks applied). If the role
cannot read (for instance its grants were revoked), the call fails; it does not use the main pool.

## Humans first

Every `sample_ms` the service takes a connection from the **main** pool and times it, and reads how many
requests are queued. That wait, the queue depth and the p95 of the last 20 probes are the load signal. At or
above the high thresholds (queue depth 8, wait 50 ms, p95 250 ms) the AIMD limit on AI calls halves, down to
zero, and recovers one step per calm sample. A call that is not admitted is answered `429` with
`Retry-After` and counts as shed.

Other refusals the same way: per-agent or per-tenant concurrency or rate spent, an open circuit breaker
(repeated failures of an agent or a tenant; it closes again after recovery).

## Observing it

On `/metrics` (when metrics are on), gauges `spot_ai_isolation_aimd_limit`, `spot_ai_isolation_in_flight`
and `spot_ai_isolation_breakers_open`, and counters `spot_ai_isolation_shed_total` and
`spot_ai_isolation_refused_total`. A limit of 0 means AI
traffic is being shed because people are waiting. An open breaker names itself in the log; the typed report
is `Isolation::health()` (the `spot-ai-isolation` health report: breaker scopes, states and when each next
admits a probe).

## Limits of this version

- The pool serves the read tools. Document loads and writes by MCP go through the engine on the main
  pool (a second engine over the `ai` role is a later task).
- Readiness shows the start-up state only; the live AIMD and breaker state are metrics and the report above.
