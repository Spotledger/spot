Part of [Running Spot](../running-spot.md).

# Realtime

`GET /api/v1/realtime` upgrades to a WebSocket speaking `spot.v1`. The first message is
`auth` with a session token or an API key, checked exactly like an HTTP bearer; a user who has
not yet chosen a password is refused. Rooms are decided by the site's permission snapshot:
`doctype:<name>` needs list read on the DocType, `doc:<name>:<id>` needs read on that document
(including its owner and state rules), `user:<id>` only that user. A notification carries
identifiers, never values. A permission change never closes a
connection: when a new permission snapshot (or a removed role assignment, noticed within a few
seconds) no longer lets a principal read a room it joined, the server takes the principal out of
that room. Its connections stay open and in their other rooms (its own `user:` room included),
no further event of the room reaches them (events already queued for them included), and the
client may ask to join again and is decided afresh. A connection is closed with code 4009 only
when its **session** ends: the token expired, was revoked (sign-out, a password change) or the
user was disabled; the server asks again every few seconds.

A hook's `ctx.notify_user(user, payload)` reaches an open connection of that user, in its
`user:<id>` room, as a `notification` event (`event = "user_notice"`, the payload under `data`)
once the transaction commits; a rolled-back transaction delivers nothing and no other user hears
it. A notice carries ids and short scalars only, never a document's content. A change feed that
cannot be read degrades the site (`realtime`) instead of stopping it.

## Limits and metrics

### The connection cap

`[services.realtime] max_connections` (default 5000, from 1 to 100000; `0` and larger values stop
the site at start-up naming the key) caps the WebSockets one site holds open. The cap applies
**before** the upgrade, so a socket that never sends `auth` counts until the 10 second auth
deadline closes it. Past the cap `GET /api/v1/realtime` answers `429` with the native error
envelope `realtime.too_many_connections` (`retryable: true`) and `Retry-After: 5`. A client
reconnects with backoff from 1 s to 30 s, as the contract says for any refused or dropped socket.
The cap is a resource bound, not an authentication step: one abusive client can still fill it.
Per-address and per-principal limits are left to the server's connection lease and the edge proxy.

### Metrics

On `/metrics` (read every 5 seconds; no label carries a user, room, token or site):

| Metric | Meaning | Alert suggestion |
| --- | --- | --- |
| `spot_realtime_connections` (gauge) | sockets open, authenticated or not | above 80 percent of the cap |
| `spot_realtime_subscriptions`, `spot_realtime_rooms` (gauges) | bus state | sudden growth |
| `spot_realtime_events_published_total` | room events the pump published | flat while writes happen |
| `spot_realtime_joins_total{result}` | `ok`, `denied`, `resync` | many `denied` |
| `spot_realtime_closes_total{code}` | contract close codes 1000, 1001, 1003, 1008, 1009, 4001, 4008, 4009 | 4008 (slow consumers) per minute |
| `spot_realtime_upgrades_rejected_total{reason}` | `cap`, `bad_request` | any `cap` |
| `spot_realtime_resync_required_total` | `resync_required` answers | bursts after a restart |
| `spot_realtime_pump_lag_seconds` | seconds the pump has been continuously behind the change feed (0 when caught up) | above 30 |

The WebSocket counters are process totals. Readiness shows `connections n/max, rooms r, pump lag s`
numbers through `Realtime::readiness_detail`; the hub is degraded (the site still serves) when the
pump lag exceeds 30 s or connections pass 90 percent of the cap, and the log says when it crosses.

### Troubleshooting

| Symptom | Cause | What to do |
| --- | --- | --- |
| Clients get 429 | The site is at `max_connections` | Raise it, or find the client that leaks sockets (`closes_total` and `connections`) |
| Connections climb and never fall | A proxy keeps dead sockets, or clients never close | Check the heartbeat (`heartbeat_seconds`); idle sockets close at twice it |
| Many 4008 closes | Clients do not read fast enough | Fix the client; the server never waits for a slow reader |
| Pump lag growing | The change feed is read slower than it is written | Lower `poll_ms`; check the database |
| Sockets drop after about a minute | A proxy drops idle connections | Set the proxy idle timeout above twice `heartbeat_seconds` |
