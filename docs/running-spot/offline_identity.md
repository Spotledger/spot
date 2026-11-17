Part of [Running Spot](../running-spot.md).

# Offline identity on the hub (`[services.offline_identity]`)

A branch or offline node may keep working without the hub. What it may believe in that time is a signed
permission snapshot that expires (`spot-offline-identity`). `crates/framework/spot-cli/src/services/offline_identity.rs`
decides what the snapshot says and what the hub believes about what a node did offline. Level: **optional**.

```toml
[services.offline_identity]
enabled = true                       # default
identity_lease_hours = 24            # 1 to 168: how long a snapshot lets a node confirm a user
tier_bounds_minutes = [60, 720, 1440]  # age bounds of tier 1, 2 and 3 offline writes (1 minute to 7 days)
```

Unknown keys are refused.

## What a snapshot says

For one node: the assignments **in force now** of each user the node serves, read from the hub's published
role snapshot (never from what a node claims), each with its flattened permissions (`doctype.right`,
`:owner`, app permission names) and its own scope (`scope:<doctype>:<field>=<allowed|...>`); the directory
(enabled, allowed offline, highest tier); the tier expiry (the shorter of the lease and the tier bound); the
protected key epoch; the sync classes; the revocations. A disabled user, or one not allowed offline, is in
the directory with no grants. Without a role snapshot, a directory or an active node **nothing is issued**.

## Epochs and revocations

- The same content keeps its epoch while more than half of its lease is left. Any change (a grant removed,
  a permission reduced, a user disabled, a new user) raises the epoch; so does an old snapshot.
- A removed assignment, and a user who is disabled or no longer served, become **revocations** carried in
  every later snapshot until the user is reinstated.
- The hub records the epoch each node reports. A node below the latest epoch is listed as stale: readiness
  says `offline_identity_host.stale_nodes` (the site is degraded, not stopped) and the "needs you" list has
  a line for it.

## What the hub does with offline writes

Each offline write arrives stamped (user, assignment, snapshot epoch, age, device position). The hub sorts it
against its **current** state: accepted; accepted and flagged (an older epoch, a gap); or **quarantined** (a
revoked author, an assignment the hub never issued, an epoch it never issued, a bound exceeded, another
tenant or node). A quarantined write is held for a person, never dropped, and a resend of the same journal
position stores nothing new.

## Every way in is decided (the coverage table)

`ENTRY_POINTS` lists API, realtime, jobs, scripts, controllers and management, the file that must call the
one decision (`decide_at`) and whether it does yet. **In this task no file calls it yet** (they belong to
other tasks): every row says `wired: false`, and the follow-up that wires a file flips its row. A test refuses
a row without a file or a duplicate, and a grep test fails when a new module that adds routes is in neither
the table nor the reviewed list.

## Known limits

- The hub's ledger (epochs per node, revocations, known assignments, journal position) is in memory: after a
  restart the hub re-issues from the live roles with higher epochs only for nodes it knows again, and a
  journal position restarts at the beginning (idempotent quarantine makes a resend safe). A durable store
  needs a table and a task.
- Field-level grants are not carried; a node enforces field rules from its own model.
- The directory, the node registry and the key epoch come from the `HubFacts` port, which no module provides
  yet; until one does the module reports `offline_identity_host.no_directory`.
