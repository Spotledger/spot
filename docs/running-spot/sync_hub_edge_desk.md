Part of [Running Spot](../running-spot.md).

# The hub's edge desk (WP-AI-07.T9, WP-AI-07.T16)

The edge desk is what the remedy host's edge part (`edge.repair_drift`) reads on a hub: every
enrolled edge with where it is and where it should be, and a way to bring one back. It is provided
by the `sync_hub` module as `dyn EdgeDesk` on the hub role only; any other role provides none.

## What it lists

Each enrolled, non-revoked edge with two numbers:

- `actual`: the position the edge has acknowledged (the serve sequence just below the oldest item
  still queued for it);
- `expected`: the newest serve sequence the relay assigned for the edge.

An edge with both equal is in sync. The numbers come from the serve cache the relay's driver
publishes to each round: `expected` from the queue head the driver publishes for each destination
(`ServeCache::head`, exact however deep the queue is), `actual` from the front of the published
window. No table is read and no acknowledgement is sent. When the relay core reports no head the
desk reads `actual` plus the items in the window instead, which understates a queue deeper than the
`window` setting of `[services.sync_hub]` (default 4096). A hub that is not running answers the fixed
refusal `the hub cannot read its edges`.

## The roster, and a revocation

The roster says which edges are enrolled and which are revoked. The desk chooses it when the
service registers:

1. a roster port some module provides (`dyn EdgeRoster`);
2. else **the enrolment directory** (`DirectoryRoster`): `sync_hub_parts` provides its
   `DurableDirectory` as a port, and the roster reads that directory's in-memory snapshot, never the
   database. An enrolled edge is listed under the partition of its node id (`<site>#<partition>`);
   a pending enrolment (a code created but not yet redeemed) is not listed; a **revoked** enrolment
   is listed as revoked and is then neither shown by the desk nor resynced; a suspended one is
   listed as live;
3. else, on a hub with no directory port, the relay's own destinations (`RelayRoster`), which cannot
   see a revocation.

A revocation made through this process (the enrolment admin) shows at once. One made by another
process shows after the directory's next refresh, which `sync_hub_parts` runs every
`directory_refresh_secs` (1 to 5 seconds, default 2). The directory refuses everyone after 30 seconds
without a successful refresh; the roster is then unreadable too, and the desk answers the fixed
refusal `the hub cannot read its edges` rather than a stale list.

## Resync

`resync(edge)` asks the catch-up path how that edge comes back, then answers its state. It is the
**same call an edge's own catch-up request makes**: the catch-up hub routes the edge from its gap as
the hub recorded it (the director decides, never the caller), under the edge's node id (the roster
maps the edge's name back to it). A stored plan resumes; nothing is deleted, rewritten or skipped,
and asking twice is the same as asking once.

| Answer | When |
| --- | --- |
| the edge's state | the catch-up path accepted the edge |
| `no such edge` | the roster does not list the name |
| `the edge is revoked` | the roster lists it as revoked (the catch-up path is not asked) |
| `not supported` | no catch-up path is wired on this hub |
| `the catch-up path refused the edge` | the catch-up hub refused (for example the hub's stores cannot answer, or the director has no scope for the node) |
| `the hub cannot read its edges` | the hub is not running, or its directory is stale |

A hub wires the catch-up path when `sync_catchup_wiring` holds a director, a chunk server and a
feed; the feed is provided by `sync_catchup_feed` (see `sync_catchup_wiring.md`). Positions move
only when the edge pulls: after a resync the edge's next pull acknowledges what it applied, the
driver trims the queue and `expected` equals `actual`.

Logs and metrics carry edge ids and counts only; every refusal is a fixed sentence.
