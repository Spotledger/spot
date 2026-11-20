Part of [Running Spot](../running-spot.md).

# edge_offline

Offline operation of an edge: leased naming ranges, the identity lease and the balance policy.
Off unless `[services.edge_offline]` is configured.

| Key | Default | Meaning |
| --- | --- | --- |
| `identity_lease_hours` | 24 | identity lease; capped at 168 |
| `series` | none | `{ name, block = 1000 }` entries |
| `balances_default` | `online-only` | `online-only`, `allocated-quota`, `accept-and-reconcile`; anything else is online-only |

Readiness: `edge_offline: lease 17h left, series INV 640/1000, identity ok`. After the lease
lapses the node refuses new sign-ins and privileged actions. A clock set back never extends the
lease. When a series runs out offline, creation fails with `edge_offline.series_exhausted`
(retryable); numbers are never reused.

## What runs (WP-RTB-03.T7)

| Part | Needs | Without it |
| --- | --- | --- |
| Durable leased ranges (`spot_leased_range`) | `[partition] node`, one or more `series`, the table from `spot migrate` | the module refuses to start and says to run `spot migrate`; with series but no partition node it says so |
| Carrier loop (lease and snapshot requests) | a carrier link from the sync node service | ranges already held keep working; readiness says `carrier not wired` |
| Offline identity keeper | the trust anchor and the sealed persistence from the module that holds the sealing key | readiness says `no snapshot`; every offline sign-in and privileged action is refused |
| Shared-balance gate | nothing | the default is online only |

Readiness: `edge_offline: lease 17h left, series INV 640/1000, snapshot 7h old, identity ok`.

### Numbers

A configured series is numbered from its leased range, online and offline alike, so two branches
never collide after the first sync. The stored high-water mark is a reservation of up to 100
numbers ahead: after a crash the series resumes above everything that was handed out and the few
numbers in between are skipped (a gap), never reissued. A request for a range carries the id
`lease:<series>@<last number held>`: asked again after a lost answer or a restart, the hub answers
with the same range and it is installed once.

### Backoff

A refusal or a missing answer doubles the wait for that request kind, from 5 seconds to 15
minutes; a success resets it. Offline nothing is sent and the loop wakes every 30 seconds.

### Operator notes

- The identity state is sealed before it reaches `spot_offline_identity_state`; the table holds
  no readable snapshot or key.
- A clock that moves back refuses new messages (`edge_offline.clock_moved_back`) and every
  offline authorisation (`clock_fault`); it never extends a lease.
- The node certificate is renewed through the hub in a separate exchange; until it is renewed
  offline sign-in is refused.
