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

## The hub's catch-up feed (`sync_catchup_feed`, WP-AI-07.T16)

The hub half above needs a `CatchupFeed`: the hub host's view of a node's gap and the plan for it.
The `sync_catchup_feed` module provides one on a **hub** role (an edge, a standalone site and a
disabled module provide none, and the wiring then names the missing piece as before). Start order
1158, between the chunk server (1155) and this module (1160); the feed's own inputs (the hub's
registry and outbox stores, built by `sync_hub_parts` at 1250, and the sync policy) arrive later, so
the module provides a late-bound feed and a finaliser binds the real one once every module has
registered. Until it is bound, and when an input is missing (the readiness line names it:
`sync_catchup_serve.not_ready: the hub's stores, the sync policy not provided to the catch-up
feed`), every question is answered "no" and nothing is planned on a guess.

**What it answers.** `view(node)`: the node's identity, tenant and generation, the applied prefix
the hub recorded from the node's acknowledgements (never from a request body), the stream head and
the gap: the in-scope events after the applied prefix up to the head, counted by class (every
forwarded event is a `doc_state` state event), the distinct records and bytes a coalesced plan would
carry, and the oldest position the hot outbox still holds. `plan(node)`: the coalesced plan over
those events, in dependency order, scoped by the node's subscription and the policy's scope keys and
redacted by its `excluded_fields.down`. A node that is unknown, of another tenant, suspended,
revoked or without a readable subscription is answered `None`.

**A gap the hub cannot plan is a bootstrap.** A node that never acknowledged anything, a prefix
above the head, a prefix below the oldest position the outbox holds, a gap with more rows than
`max_events`, an event that does not decode, and a gap the planner refuses (a link the metadata
cannot resolve) are reported as a resync, which the director turns into a snapshot bootstrap, and
`plan` answers `None`. The plan is never built from a partial read.

```toml
[services.sync_catchup_feed]
enabled = true         # false provides no feed
max_events = 20000     # 100 to 50000: the most outbox rows one computation reads
cache_secs = 120       # 5 to 3600: an unused computation is dropped after this
max_nodes = 32         # 1 to 1024: nodes kept
snapshot_bytes = 0     # optional estimate of a full scoped snapshot in bytes; 0 = unknown
```

The planner's thresholds are the director's (`[services.sync_catchup]`: `catchup_threshold_events`,
`snapshot_share_percent`, `bundle_records`), so the route and the plan agree. With `snapshot_bytes`
unknown the cost rule never prefers a snapshot to a coalesced plan.

**Bounds.** One computation reads at most `max_events` outbox rows (plus the rest of one source
transaction), the node's registry row, the stream head and the oldest position; no lock is held
across a database read. The result is cached per node while its applied prefix and the stream
generation are unchanged and it was used within `cache_secs` (a catch-up asks for the same plan
chunk after chunk, and the same plan must give the same chunks), at most `max_nodes` nodes. A node
silent for longer than `cache_secs` in the middle of a plan is planned again from the head of that
moment. No event payload is put in a log line, an error or a metric label; the log codes are
`sync_catchup_feed.store_unavailable` and `sync_catchup_feed.no_scope`.

**What else a hub needs for catch-up to serve.** The chunk server (`sync_catchup_serve`) also needs
the journal store and keys and a `dyn SeamsProvider` (each node's scope, references and redactor for
a bootstrap). Until a module provides the seams provider the chunk server is not built, the wiring
stays not ready, and the edge desk's `resync` answers `not supported`: the feed alone is not enough.

