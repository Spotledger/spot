Part of [Running Spot](../running-spot.md).

# Sync catch-up wired to the hub and the edge

`sync_catchup` (the director) and `sync_catchup_serve` (the chunk server and applier) are
libraries until something calls them. `sync_catchup_wiring` is the call site. It starts after both
(order 1160) and before the sync hub (1300).

## Hub

A returning node asks three questions over the sync listener, under `/sync/v1/catchup/`. The node
is the one the listener authenticated (its certificate), never a name in the request.

| Call | Answer |
| --- | --- |
| `POST route` | `live`, `catch_up`, `bootstrap` (each with the chunk to start from) or `wait` (no bootstrap slot; try again after `retry_after_ms`). The director decides from the node's gap. A stored plan is resumed, never restarted. |
| `GET chunk?index=n` | chunk `n`, sealed with its hash. Served again as often as asked; never past the next unacknowledged chunk (`chunk_out_of_order`). A node that is live gets no chunk (`out_of_range`). |
| `POST ack?index=n` | `next`, `wait` (the node is out of chunk tokens) or `complete`. The director moves only here. |

A hub whose prerequisites are missing still answers these paths, every call with
`sync_catchup_serve.not_ready` (HTTP 503), and says what is missing in readiness. It never answers
404, which would look like "no such hub".

The module needs, on a hub: the director (`sync_catchup`), the chunk server
(`sync_catchup_serve`) and a `CatchupFeed` port from the hub host, which supplies the node's gap
(applied prefix, stream head, scope) and the coalesced plan from the stream, the outbox and the
journal segments.

**One hook the sync listener must call** (not part of this module): merge the routes behind the
guard, `router.merge(routes.router())` where `routes` is the `CatchupRoutes` port. Until it does,
nothing on a real listener answers these paths.

## Edge

`CatchupNode` runs the catch-up: `begin()` asks how to come back; `step()` fetches the node's next
chunk, applies it through the node's apply path and only then acknowledges it. Each chunk is
verified against its hash first. A failed apply acknowledges nothing, the checkpoint does not move,
and the hub's director stays on the same chunk, so a restarted node resumes at the chunk that
failed and no half-applied chunk is ever reported as done. When the last chunk is applied
`step()` returns `Done`; after a bootstrap the node continues on the live stream from
`resume_from` (the snapshot position plus one).

The module needs, on an edge: the chunk applier (`sync_catchup_serve`) and a `ChunkSource` port
from the node host (its authenticated connection to the hub).

## Configuration

```toml
[services.sync_catchup_wiring]
enabled = true
```

A standalone site wires nothing.
