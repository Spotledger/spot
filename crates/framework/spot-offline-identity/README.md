# spot-offline-identity

Offline identity for edge nodes (ADR-070 rule 12.13): a signed permission snapshot with a
lease per tier, fail-closed authorisation, node identity, and offline-write stamps.

## Where lease time comes from

The node never reads a wall clock. Hub time enters the store **only inside a message the hub
signed** and the node verified against the pinned root and the delegated timestamp key:

- the `issued_at` of a signed snapshot,
- the `signed_at` of a signed timestamp,
- a signed `HubTime { tenant, node, epoch, hub_ms, nonce }`, which carries time and nothing else.

A heartbeat, a transport header or any other unsigned input may prove the hub is reachable, but
it cannot move the clock: `Received` holds the node's own clocks only. Someone on the path who
could set the "hub time" a node believes could stretch every lease, so unsigned time
is refused by construction and there is no constructor that takes a bare hub time.

The hub clock (`SnapshotStore::hub_time_ms`) only moves forward. A signed time that is not
later than the newest one accepted (a replay, a reorder, a rollback) is refused with
`stale_timestamp` and changes neither the clock nor the lease. A snapshot issued before the
clock is accepted on its epoch, but its lease counts from the clock, never from the older
issue time.

## What a restart costs

The lease is saved at every accepted signed message and at every `checkpoint`. Time spent
running is counted on the monotonic clock. When the node starts:

- **same boot** (the process restarted, the machine did not): the elapsed monotonic time is
  measured and subtracted; nothing else is charged;
- **another boot**: the time since the last checkpoint cannot be measured, so
  `MAX_CHECKPOINT_GAP` (30 seconds) is charged against every tier. The remaining lease is
  `saved - MAX_CHECKPOINT_GAP` until a signed hub time arrives. Call `checkpoint` at
  least that often (every heartbeat).

A signed hub time, timestamp or snapshot then replaces the charge. A `HubTime` restores each tier
to `held deadline - hub_ms`, capped at what was saved, so it can undo the charge but never extend
a lease that was not charged. A node that cannot reach the hub after a restart therefore only
degrades faster, never slower. A restart twice in a row without a signed time charges twice.

Not covered: time spent powered off beyond the charge cannot be measured without a trusted
counter. Only the first signed hub time after the restart corrects it.

## A replayed disk image

Someone who can write the node's disk (a cold image replay is enough; the sealing key is not
needed) can put back an older image of **everything** the node remembers while it is off: the
state, the epoch fence and the lease. The node cannot see that, and this crate does not claim
it can. What it does:

- **The node tells the hub what it holds.** A request for signed hub time or a snapshot is a
  `NodeRequest` that carries a `NodeReport` (highest epoch, snapshot hash, newest hub time,
  newest hub nonce), signed by the node key. `verify_node_request` refuses a request without
  the report or with a report that was changed. A node that ran on a replayed image reports the
  replayed values, so the hub sees the rollback at the next contact.
- **Hub replies carry a fresh nonce.** Every `Timestamp` and `HubTime` has a nonce strictly
  higher than the last one the hub signed for the node; the node refuses one that does not
  advance and remembers the newest in its sealed lease record. A snapshot carries none: the hub
  pairs it with a signed hub time.
- **A stale lease image restores nothing.** On another boot, a lease image whose nonce is older
  than the newest nonce the node's backend has ever seen (`Persistence::nonce_floor`; the
  sealed backend keeps it in the fence slot) has no remaining lease. The node must reach the
  hub, with a signed timestamp or snapshot, before it continues. A signed hub time alone does
  not bring it back.

What stays open: if the attacker replays the state **and** the fence slot, the node's own
memory is consistent with the old image and it heals only at the next hub contact. Until then
its authority (a revoked user stays active) is bounded by the lease and nothing else. Closing
that needs a counter outside the disk (a TPM, say), which is not part of this crate.
