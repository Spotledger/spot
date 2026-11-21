Part of [Running Spot](../running-spot.md).

# sync_hub_grants

The hub half of the offline edge: it answers an edge's request for a range of document numbers
and for a signed permission snapshot. Off unless `[services.sync_hub_grants]` is configured; runs
on the hub next to `sync_hub`.

| Key | Default | Meaning |
| --- | --- | --- |
| `default_max` | library default | the largest range of a series without its own maximum |
| `min_interval_secs` | library default | the least time between two grants of one series to one node |
| `max_series` | library default | how many distinct series the hub tracks |
| `series_max` | none | per-series maximum range, e.g. `{ INV = 5000 }` |
| `entitle` | none | `[{ node = "edge-khi#branch_07", series = "INV" }]`: a node may lease only what it is entitled to |

## What it needs

The node registry (standing), the audit chain, the durable grant log (`spot_lease_grant_log`,
created by `spot migrate`) and, when the hub serves snapshots, the snapshot signing, come from
the modules that own them as one port. Without it the module says so in its check and grants
nothing.

## What it guarantees

- The node is the one the connection authenticated; nothing in a request names it. A request
  that does not parse gets no answer.
- A number is never granted twice: the grant is logged before the answer, and after a restart the
  service rebuilds from the log. A repeated request id gets the original range.
- Every grant, replay and refusal is audited with ids and numbers only.
- A suspended, decommissioned or unknown node gets nothing; a node that reports an older state
  than the hub gave it is flagged for re-enrolment and refused.
- A snapshot is the node's own tenant's, signed and paired with signed hub time; a node that
  already holds the current one gets a timestamp that renews its lease.

## Retiring a node

`retire_node` reports and audits the unused remainder of the node's ranges above the highest
number the hub has seen used. The numbers are **not** handed out again: the allocator only moves
up, which is what makes a double grant impossible. Re-granting them needs a release operation on
the allocator.
