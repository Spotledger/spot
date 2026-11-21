Part of [Running Spot](../running-spot.md).

# World access: what a remedy runs on

A remedy never touches the site directly. It changes the site through seven typed ports (jobs, the
cache, the retrieval index, edge sync, quarantine, BPM cases, documents), and something has to
hand the remedy host those ports. That is the **world-access composer**. Without it the
[remedy host](remedies.md) can list remedies and prove their self-tests, but a dry-run, apply or
undo answers `remedies_host.no_world`, and the [fix pipeline](fix-pipeline.md) stays degraded and
does not install.

Code: `crates/framework/spot-cli/src/services/world_access.rs` and `services/world_access/`.

## What a part is

The composer is assembled **part by part**, one part per port:

| Part | Over | File |
|---|---|---|
| `jobs` | the background job store | `world_access/jobs.rs` |
| `cache` | the cache service | `world_access/cache.rs` |
| `index` | the retrieval index | `world_access/index.rs` |
| `edge` | edge sync | `world_access/edge.rs` |
| `quarantine` | the quarantine store | `world_access/quarantine.rs` |
| `cases` | BPM cases | `world_access/cases.rs` |
| `documents` | the document lifecycle | `world_access/documents.rs` |

Each part has two faces:

- **live**: the real subsystem. A change made through it is a change to the site.
- **rehearsal**: a **snapshot**. The part reads the live state into an in-memory copy, so a
  rehearsal can be changed and thrown away and never writes to the site. The fix pipeline rehearses
  every fix on one before it touches the live site, and a remedy's dry-run reads the live world
  only. Two rehearsals are independent of each other, and the seven snapshots of one rehearsal are
  taken together under one lock, so a rehearsal never sees half of a change.

A part is present when its subsystem runs on the site (a site without the retrieval service has no
`index` part). The composer asks each part once, after every module has registered.

## What degraded means

A part the site does not have is **degraded, not missing the world**. The composer stands in for it
with an *unavailable* part: reads answer empty and every change answers "not available on this
site". So:

- remedies over the other subsystems still run;
- a remedy whose precondition reads the missing part is refused cleanly by the framework (the job,
  namespace, index or item it asks for does not exist), never with `remedies_host.no_world`;
- readiness names the missing parts: `degraded: world_access (no jobs, cache)`.

The site is never failed by a missing part: the service is optional.

## How to see which parts are live

`/api/health/ready` shows the line `degraded: world_access (no ...)` for a site
with missing parts, and nothing for a site with all seven. The start-up log carries the same list
(`world_access: some parts are not available on this site`). The jobs, cache and index parts are
written (below). The edge, quarantine, cases and documents parts are written too, but each is
present only when a module provides the port it needs, and **none does yet** on a standard site, so
they show as missing (`no edge, quarantine, cases, documents`) until that wiring lands.

## The jobs, cache and index parts

**Jobs.** Present on a site whose job store is the durable (PostgreSQL) one. The part opens the
same store the jobs module uses, through the store's own interface, never a table. On the
in-memory job profile the store is private to the jobs module and the part is missing.

- *failed* are the site's dead jobs (no attempts left); *running* are the claimed ones; a job
  waiting for its next attempt is queued.
- *unstick* releases a running job whose lease has expired (by the site clock) through the job
  store's own reaper call. That call releases every lapsed lease, which is what the jobs module's
  reaper does every two seconds anyway.
- *retry* and *undo* of a retry or an unstick are **not supported** yet: the job store has no
  operation that moves a dead job back to the queue, so they answer "not supported by the jobs
  port" and change nothing. A rehearsal supports them on its copy.

**Cache.** Present when a cache store is provided (always, on a standard site). The store keeps
flat keys and cannot list them, so the one namespace is **step** (the `step:` prefix the steps'
cache uses). *Flush* removes that prefix only, never the whole site cache; the entry count shown
is the site's live entry count. *Undo* of a flush records the reversal and restores nothing: a
cache is derived data and refills.

**Index.** Present when the retrieval service is on for the site and the job-submit port exists.
The one index is **retrieval**. It is *stale* when this node's indexer cursor is behind the change
feed. *Rebuild* queues the platform's backfill job (the job resets the cursor and replays the
feed on the node that holds the indexer lease) and bumps the index generation by one. One backfill
is in flight at a time: a second rebuild while one is queued is refused. *Undo* puts the
generation back only if no newer rebuild ran; the queued job itself cannot be withdrawn and is
harmless. A rehearsal never queues anything.

## The edge, quarantine, cases and documents parts

All four act **as the remedy's own identity** (`agent:<id>`): the subsystem is told who acts and
applies that identity's permission; a refusal is a refusal. A part opened with no identity refuses
every change, so a world is never run as a system principal. Each answers "not supported by the
<subsystem> port" for the undo of a change the subsystem cannot take back, and does nothing for a
state the item already has.

- **Quarantine** is the one with a port today: the quarantine module's `QuarantineBackend`. A retry
  moves a held or parked item to retry and records the decision `quarantine_retried` with the
  remedy's actor. It sees ids, states and reason codes, never an item's image. The part is present
  once a module provides the backend (the quarantine module itself does not yet).
- **Edge** is written over a small desk seam (list edges with expected and actual position,
  resynchronise one). The sync services expose no such port yet, so the part is missing.
- **Cases** is written over a desk seam (stuck cases, a case's state, unstick as an actor). The BPM
  service exposes no stuck-case listing or unstick through a port yet, so the part is missing.
- **Documents** is the most conservative. A change goes through the document lifecycle as the
  remedy's actor and never as a row write; a submitted financial record changes only through
  cancel, amend or a journal entry (the framework's hard floor, enforced again here); the change
  record carries the operation's name and the lifecycle states only, never a field name, a reason
  text or a value. A rehearsal reads the live document read-only and keeps its own changes in
  memory. The lifecycle is reached through a seam no module provides yet, so the part is missing.

## Settings

```toml
[services.world_access]
enabled = true     # default true; false: nothing is provided and the remedy host answers no_world
rehearsal = true   # default true; false: a rehearsal is refused, so the fix pipeline cannot rehearse
```

Unknown keys stop the site from starting (`world_access.settings_invalid`). With `rehearsal =
false` only reversible T1 settings can be applied (they run directly on the live world); any fix
that needs the pipeline is refused until rehearsal is back on.

## Start order and threads

The composer starts just before the remedy host (order 1265, the host is 1270) so the host finds
it. Because some subsystems start after the host, the composer resolves its parts at the end of
site assembly. The host calls the world on a blocking thread; a part over asynchronous storage
blocks on the runtime handle it captured when the site was built.

## Audit

The composer writes no audit record of its own. The remedy host writes the intent record before a
world is touched and the events after, exactly as described in [remedies.md](remedies.md). A world
never carries a parameter or a business value into an error or a log line.

## Codes

`world_access.settings_invalid`, `world_access.rehearsal_off`, `world_access.not_assembled`,
`world_access.rehearsal_unavailable`.
