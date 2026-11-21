Part of [Running Spot](../running-spot.md).

# Process distribution on the hub and the edge

A published process version is a **master record**: it flows from the hub to every edge in scope and
never back (`spot-sync-process`, WP-SYN-12.T1). This service hosts it.

## Hub

`ProcessSync::publish` emits a published version as a hub-authored record `process:<process>@<version>`
with its scope keys, through the hub stream of the sync hosting; a node whose subscription does not
cover the keys never receives it. Publishing the same version with the same definition again emits
nothing; a changed definition under one version is refused (a defect). A `process:*` record a node
pushes is refused (`sync_process.edge_authored`) and recorded as a refusal. A hub without a hub stream
has this side off.

## Edge

`ProcessSync::receive` verifies the tenant and the definition hash (a mismatch is **quarantined and
counted**, never installed), then:

- **installs beside** the versions the node already has: a running Case is pinned to its version and
  keeps running on it; new Cases start on the newest runnable version (`start_version`, what the BPM
  runtime asks);
- **holds** a record whose `min_engine_version` is above the node's engine version
  (`services.sync_process.engine_version`): it is not installed and not dropped, the amber `SCHEMA_HOLD`
  condition is raised for it and shows in the sync health, new Cases keep starting on the version the
  node can run, and an upgrade (`on_upgrade`) installs it and clears the condition;
- **retires** a version only when no Case is pinned to it (the BPM runtime provides the pin counts;
  without them nothing is ever retired).

A version is answered `installed` only after its row is durable (`spot_sync_process_definition`, created
by `spot migrate`; the start-up check degrades the service when the table is missing). Level optional: a
node whose distribution cannot run keeps trading on the versions it has.

## Configuration

```toml
[services.sync_process]
enabled = true
engine_version = 2        # this node's engine version
accepts_breaking = false  # whether new Cases may start on a breaking version
```

## Security and tenancy

Scope is by subscription keys, never by anything a node says. A second tenant on the same hub never
sees or installs this tenant's process (the record names its tenant and the intake refuses another's).
A revoked node receives nothing further: the connection is refused before the stream (the sync hosting's
rule).
