Part of [Running Spot](../running-spot.md).

# Sync hub (`services.sync_hub`)

The hub half of sync inside `spot serve`: edges dial in over mutual TLS; the hub never dials out. The service is **off unless configured** and runs only on a site whose node role is `hub`. A hub site without `listen` refuses to start.

```toml
[services.sync_hub]
listen = "0.0.0.0:7443"
[services.sync_hub.tls]
cert = "sync/hub.pem"        # the hub's certificate chain
key = "sync/hub.key"         # its private key, mode 0600
client_ca = "sync/edge-ca.pem"   # client certificates are verified against this only
```

| Key | Default | Bounds |
|---|---|---|
| `per_node_connections` | 2 | 1 to 16 |
| `global_connections` | 2000 | 1 to 100000 |
| `push_per_second` | 20 | 1 to 1000 |
| `max_body_mib` | 64 | 1 to 256 |
| `max_batch_records` | 4096 | 1 to 65536 |
| `long_poll_secs` | 25 | 1 to 60 |
| `heartbeat_secs` | 5 | 1 to 30 |
| `audit_interval_secs` | 60 | 10 to 3600 |
| `window` | 4096 | 1 to 65536 |
| `drive_interval_ms` | 200 | 20 to 5000 |

Readiness: the service is **required**. The site is not ready if the port is taken, the certificate files cannot be read, or the relay is missing. Run `spot migrate` first: the hub's tables are created there, never by the host.

Behaviour operators see: a node revoked in the directory is refused at its next request (a waiting pull ends within the long-poll cap); a node naming another node's queue, or another site in its hello, gets `sync_http.partition_violation`; a node over its rate gets 503 and retries; refusals are counted per reason and audited at most once per node per `audit_interval_secs`.

## The hub's parts (`sync_hub_parts`)

`services/sync_hub_parts.rs` (hub only, start order 1250) builds what the listener is handed, from
the site database, before anything is served:

| Part | What it is | If it cannot be built |
| --- | --- | --- |
| node directory | the enrolled nodes, loaded once, then refreshed every `directory_refresh_secs` (default 2) | the site does not start (`sync_hub_parts.part_unavailable`); after start, 30 s without a successful refresh and the directory refuses every node |
| enrolment endpoint | `POST /sync/v1/enrol` | the site does not start |
| `last_seen` | the node registry, written from a queue on its own thread | the site does not start |
| relay core, tenant sinks, metadata epoch | the relay assembly module (`RelayAssembly` port) | `sync_hub_parts.part_missing`, naming the part |

Nothing is defaulted: a hub with no directory would admit nobody and a hub with an invented one
would admit anybody, so a missing part stops the site with a message that names it. A missing hub
table says to run `spot migrate`.

### Enrolling a node

An administrator creates an enrolment (the enrolment service prints the one-time code once; it is
never stored). The operator puts the code in the node's enrolment file (mode 0600). On first start
the node generates its key and a self-signed certificate and posts
`{ "node", "code", "certificate" }` (certificate as hex DER) to `POST /sync/v1/enrol` over TLS,
pinning the hub's certificate by its fingerprint.

* The endpoint is the one **open** path of the listener: it needs no client certificate (the node
  has no identity yet) and is authenticated by the code alone.
* An unknown, expired, used, malformed or wrong-node code is **one** answer (403,
  `sync_enrol.code_invalid`, one text). Repeated failures from one source are locked out (429,
  `sync_enrol.locked_out`).
* The endpoint is throttled as a whole (`enrol_per_second`) and bounds the redemptions waiting for
  the database (`enrol_queued`); the body is capped at 16 KiB.
* The code and the certificate are never in a log line, an error or an audit record.

### Revocation

Revoking or suspending a node takes effect at once in the directory (the enrolment service marks
the local snapshot and the next refresh carries it to the others). A pull that is already open
re-checks the certificate at every heartbeat (`heartbeat_secs`, default 5): a revoked node's open
pull ends within one heartbeat, not at the long-poll cap.

### Wiring the listener

`sync_hub` merges the `EnrolRoute` port's router into the listener's router **outside** the guard
and builds its TLS configuration with `spot_sync_http::guard::pinning_server_config`: the hub's own
chain and key, and a client-certificate verifier that requests a certificate, keeps the handshake's
proof of possession and does not chain it to a CA. Enrolled nodes use self-signed certificates that
the directory trusts by pinned fingerprint, so a verifier that chained to a CA would refuse every
node at the handshake (the `client_ca` setting becomes unnecessary). Authorisation is the
directory's: the guard resolves the certificate on every other path, and a connection without one
gets `sync_http.identity_required`.

## The edge desk (WP-AI-07.T9)

On a hub site the service provides one more port, `dyn EdgeDesk`: the seam the remedy host's edge
part (`edge.repair_drift`) reads. Before this the port was provided by nobody, so the edge part
answered "no edge". A non-hub role provides nothing (the module runs in the hub role only).

**What it lists.** Each enrolled, non-revoked edge with two counts:

- `actual`: the position the edge has acknowledged (the serve sequence just below the oldest item
  still queued for it);
- `expected`: the relay's head for that edge: the newest serve sequence the relay assigned for it,
  which the driver publishes with each round when its relay core reports one, so it is exact however
  deep the queue is. When the core reports none the desk falls back to `actual` plus the items in
  the published serve window (at most `window`), which understates a deeper queue.

An edge with nothing waiting is in sync. Both are read from the serve cache the driver publishes
to; no table is read and no acknowledgement is sent. A hub that is not running, or an edge the hub
cannot read, answers the fixed refusal `the hub cannot read its edges` (the edge part reads that as
"no edge").

**Who is "enrolled".** The desk takes the roster from a roster port when a module provides one;
else from the enrolment directory (`sync_hub_parts` provides it as a port), which lists every
enrolled edge with its status; else, on a hub with no directory port, from the relay's own
destinations, which cannot tell a revoked edge from an enrolled one. A revoked edge is neither
listed nor resynced. A revocation made through this process shows at once; one made elsewhere shows
after the directory's next refresh (`directory_refresh_secs`, 1 to 5 seconds, default 2). A
directory that could not refresh for 30 seconds is unreadable: the desk answers `the hub cannot
read its edges` rather than list a stale roster. See `sync_hub_edge_desk.md`.

**Resync.** `resync(edge)` asks the catch-up path how that edge comes back (the same call the
edge's own catch-up request makes, decided from the edge's own acknowledged position), then answers
its state. It never deletes or rewrites relay data and never skips a queued item; asking twice is
asking once. An unknown edge answers `no such edge`, a revoked one `the edge is revoked`, and a hub
with no catch-up path wired answers `not supported` (the catch-up wiring needs a catch-up feed,
which the `sync_catchup_feed` module now provides on a hub; see `sync_catchup_wiring.md`). Positions move only when the edge pulls: after a resync the edge's
next pull acknowledges what it applied, the driver trims the queue and `expected` equals `actual`.

Logs and metrics carry edge ids and counts only.
