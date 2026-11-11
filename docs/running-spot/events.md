Part of [Running Spot](../running-spot.md).

# The event stream

`/api/v1/events/*` serves the **named consumers** of ADR-078 at delivery level **D1**: nothing is
lost, an event may arrive more than once, and the receiver deduplicates on `event_id`. There is no
exactly-once delivery anywhere in this service, and none is claimed.

**Opt-in.** The service is off by default (WP-INT-02.T6): a site with no `[services.events]`
table creates no consumer tables, mounts no route and does not appear in `Health`, exactly as if
the service did not exist (`/api/v1/events/*` answers a bare `404` with an empty body, the same as
every other path no route serves). A table that omits `enabled` is off too. Turn it on in `common.toml`:

```toml
[services.events]
enabled = true
```

A consumer is a durable position in the site's outbox: its topics, an optional filter, a partition
count, a payload mode and a checkpoint per partition. It is kept in the site's own database
(`spot_event_consumer*` tables, created at site start by the store's `prepare()` only when the
service is enabled, safe to repeat and safe from several nodes). Two sites never see each other's
consumers.

Operations: `event_consumer_list`, `_put`, `_get`, `_delete`, `_pull`, `_ack`, `_restart` and
`event_stream` (server-sent events). The contract is `contracts/native-api/openapi.yaml`.

## Who may use it (default deny)

Every operation, including a list, needs the `Tenant Administrator` role (unscoped) or a role named
in `services.events.consumer_roles`. A caller without it gets `403 permission.denied`, never an
empty success. A session that has not chosen its password yet may do nothing here.

The caller becomes the owner of the consumers it registers. Another user's consumer is `404
eventstream.consumer_not_found` on every operation, the answer a missing consumer gets; the
administrator sees all of them. The reserved platform topic prefixes `doc_`, `sys_` and `identity_`
need the administrator or a role in `services.events.stream_roles`, checked when the consumer is
registered and again on every pull and every few seconds of an open stream, for the owner.

A consumer sees only the events its **owner** may read: the owner's permission rules on the document
(with the document's committed values) and, for `payload_mode: fields`, the one field guard on the
export channel, so a field the owner may not read is removed or masked. An event that cannot be
decided is skipped, counted in `skipped` and never named. Rights are decided with the permission
snapshot served now, so a change reaches the next pull. An owner who is disabled or removed reads
nothing.

## Using it

```
PUT  /api/v1/events/consumers/billing   {"topics":["doc_posted"],"filter":"doc.subject.entity == 'sales_invoice'",
                                         "partitions":2,"payload_mode":"ids_only","start":{"kind":"tail"}}
POST /api/v1/events/consumers/billing/pull     {"partition":0,"limit":100,"wait_ms":10000}
POST /api/v1/events/consumers/billing/ack      {"partition":0,"position":"v1...."}   (X-Consumer-Lease)
POST /api/v1/events/consumers/billing/restart  {"partition":0,"from":{"kind":"from_beginning"}}
GET  /api/v1/events/stream?consumer=billing&partition=0        (Authorization header, Last-Event-ID)
```

- **Filter paths** are `doc.subject.entity`, `doc.subject.id`, `doc.meta.actor.kind` and the other
  fields of `meta` and `subject` (the engine's names; the contract's example omits the `doc.`).
- **A pull does not move the checkpoint; only an ack does.** A pull repeated before an ack returns
  the same events with the same `event_id`s. `next_position` is what to acknowledge once every
  returned event is durably applied. A stale, forged or foreign position is refused
  (`eventstream.ack_refused`, `eventstream.position_invalid`).
- **One connection per consumer.** The first pull or stream takes a lease from the site's
  coordinator and returns it in `X-Consumer-Lease`; send it back on every later pull, ack and
  restart. A second holder, or a pull while a stream is open, is `409 eventstream.consumer_busy`
  with `Retry-After`. A lease lapses after `lease_seconds` without use and is given back when a
  stream ends. A lease is a handle on **this node**: a pull and its ack must reach the same node.
- **The stream** sends one frame per event (`id:` the position to acknowledge, `event:` the topic,
  `data:` the envelope as one line) and `: keepalive` after `heartbeat_seconds` of silence. It ends
  after `max_idle_seconds` without an event, when the credential ends or loses the right, when the
  consumer is deleted and on server shutdown. A lost position produces one `resync_required` frame,
  and the stream closes; a pull answers `410 eventstream.resync_required` with `retained_from`. The
  browser `EventSource` cannot send the `Authorization` header and is not supported; no token is
  ever read from the URL. An event's `id` may be acknowledged on its own.
- **Restart** is the only way to move a checkpoint other than an ack: `tail`, `from_beginning` or a
  position previously returned for this consumer. After `resync_required`, rebuild from a snapshot
  and restart.
- **A consumer's definition cannot change.** Registering the same name with the same definition is
  `200`; a different one is `422 eventstream.spec_invalid` (a new partition count or topic list is a
  new consumer, so delete and register again).

## Configuration

`[services.events]` in `common.toml`; unknown keys are refused and a bad value names the key only.

| Key | Default | Meaning |
| --- | --- | --- |
| `enabled` | `false` | `false` serves none of the routes (opt-in; off creates no tables either) |
| `poll_ms` | `250` | how often an open stream or a waiting pull looks at the feed (20 to 5000) |
| `heartbeat_seconds` | `25` | keepalive interval of a stream (1 to 120, at least twice `poll_ms`) |
| `max_idle_seconds` | `900` | a stream with no event for this long ends |
| `lease_seconds` | `60` | the consumer lease's time to live (5 to 3600) |
| `revalidate_seconds` | `10` | how often a stream asks again whether its credential and rights hold |
| `max_streams` | `64` | open streams on the site |
| `max_streams_per_user` | `4` | open streams one caller holds |
| `window` | `1000` | delivered, unacknowledged events per partition before a stream waits for an ack |
| `consumer_roles` | `[]` | roles, besides the administrator, that may use the event stream |
| `stream_roles` | `[]` | roles, besides the administrator, that may stream the reserved topic classes |

**Connection budget.** Open streams are bounded by `max_streams` and `max_streams_per_user`
(`429 api.rate_limited` with `Retry-After`); the realtime hub keeps its own, separate limit, so a
burst of streams cannot starve sockets and the reverse.

## Level and failure

**Optional.** If the consumer store or the outbox cannot be read at start, the site starts degraded
(`events` in `/api/health/ready`) and the routes answer `503 server.unavailable`; the rest of the
site serves. A database that refuses the consumer tables' DDL stops the site start with a message
naming the service. In the cluster profile the leases live in the database coordinator; in the
single-node profile they live in the process.

## What is logged and audited

The audit target is `spot::audit::event_stream`: who registered, restarted, acknowledged or deleted
which consumer, and where (a position), and when a partition is parked. A restart the host makes
itself to repeat a batch is not audited. **No event content, position token, lease or credential is
logged, audited or put in an error.** Field access records go to `spot::audit::field_access`
without values.

## Limits and what to expect

- At-least-once: a crash between a pull and its ack redelivers exactly the unacknowledged events;
  after a server restart the consumer pulls again from its durable checkpoint.
- The in-flight window, leases and the positions a connection was delivered live in the process.
  After a restart an ack of a position delivered before it is refused; pull again and ack that.
- A partition that makes no progress for the park timeout stops consuming capacity (`parked` in
  the consumer's state) until a restart of the partition.
- Where a document has been deleted, the read filter decides without its committed values.
- Naming a consumer that belongs to someone else on `PUT` answers `422` (names are unique per
  site): a name is the only thing that can be learnt that way.
