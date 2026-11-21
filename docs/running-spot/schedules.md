Part of [Running Spot](../running-spot.md).

# Schedules: what fires, and what a failover does

A `[[schedules]]` entry of an installed app, of the embedded platform app, and the internal
schedules services register (WP-BPM-11.T3) all go through one planner in the `scheduler` service
of `spot serve`. This page says where they come from, what a firing becomes, and what happens when
the node that plans them stops.

## Where schedules come from

- **Apps.** Every `[[schedules]]` entry in the `app.toml` of an app listed in the site's `apps`
  (and found in the `common.toml` catalogue).
- **The platform app.** The manifest embedded in the binary (`apps/platform/app.toml`). A site that
  installs `platform` gets its schedules even when `common.toml` lists no `apps/platform`
  directory. A site that lists the directory gets each schedule once, not twice.
- **Services.** A service registers an internal schedule with the jobs module; its app part is the
  service's name.
- **Dynamic sources.** A service whose schedules change while the site runs (script schedules)
  registers a `ScheduleSource`; its schedules are re-read at every tick. A changed schedule starts a
  fresh in-memory mark; the mark of a dynamic schedule is not kept in the coordinator.

A manifest schedule has no zone and no missed-run policy field: it is UTC and `skip`.

## The job and its key

A firing becomes one job named `<app>:<schedule>` (for example `platform:function_evict_idle`) on
the schedule's queue, with the manifest `step` and `budget_ms` in its arguments. The
deduplication key is `<app>:<schedule>:<instant>` (the scheduled instant in UTC, second precision),
so a second enqueue of the same firing is dropped and two apps that both declare a schedule called
`nightly` make two jobs, never one. A handler registered under `<app>:<schedule>` receives it.

## What `skip` means

Each tick plans the firings in `(mark, now]`. A firing is **on time** when the tick reaches it
within the grace period, and then it runs under every policy. The grace is
`max(3 x tick interval, 60 s)` (`services.scheduler.tick_ms`, default 1 s, so 60 s). Only a firing
older than the grace when a tick first reaches it was *missed* (no node was ticking); `skip` drops
those. A tick that runs a little late therefore never drops a firing, and a node that was paused
for an hour does not replay the hour.

## The mark, and where it lives

The planner keeps a high-water mark per schedule in memory. The same mark is kept in the
coordinator as the counter `schedule_mark:<site>:<app>:<schedule>` (whole seconds since the epoch,
never lowered). It is the last firing whose **job exists**: it moves only after the job was
inserted (or was found already there). A node that becomes leader loads these marks before its
first tick. If the coordinator cannot answer, the node does not lead this round (it logs a
warning, gives the lease up and retries at the next standby tick); it never starts at `now`.
A schedule with no stored mark starts at `now`.

If inserting a job fails (the store is down), the planner's in-memory mark is put back to where it
was before that tick and the failure is logged at `warn` with the schedule name and the error
code; the next tick plans the same firing again under the same key. Under `skip` the retry has the
grace period to succeed. A slot the job engine can never accept (an unknown queue) is logged and
counted as handled, and does not hold up the other schedules.

## What a failover does

Node A leads and stops; node B takes the leader lease when A's lease lapses (or at once, when A
released it). B reads the stored marks and plans from there:

| Policy | Firings missed during the gap |
| --- | --- |
| `skip` (every manifest schedule) | dropped; the next firing runs |
| `run_once` (internal schedules may choose it) | one job, for the last missed firing |
| `catch_up(n)` (internal) | up to n jobs, the most recent missed firings |

Firings A already enqueued are behind the mark and are not planned again; if A died between the
insert and the mark write, B plans them again and the deduplication key drops the second insert.

## A restart of a single node

The single-node profile's coordinator is in process memory, so a restart forgets the marks and the
node starts at `now`: runs between the stop and the start are lost under every policy. The cluster
profile keeps the marks in PostgreSQL and resumes. (Making the single-node mark durable needs a
table or the node-local state file; not done here.)

## How to see that a schedule fired

- `/metrics`: `spot_schedule_fired_total{app,schedule}` counts jobs enqueued;
  `spot_schedule_last_fire_seconds{app,schedule}` is the scheduled instant (seconds since the
  epoch) of the latest one. Alert on a schedule whose last fire is older than twice its period.
- The job list: jobs named `<app>:<schedule>` with the key above.
- The log: `schedule fired` at `info`, `scheduled job was not enqueued` and
  `schedule marks unavailable; not leading` at `warn`.

## Troubleshooting

| Symptom | Cause | What to do |
| --- | --- | --- |
| Jobs named `:<schedule>` with a leading colon | A build from before WP-PLT-07.T3: the app name was empty | Upgrade; the handlers are registered as `<app>:<schedule>` |
| No job at all | The app is not in the site's `apps`, the schedule is not in its `app.toml`, or no node leads | Check `apps` in the site file; look for `schedule marks unavailable` in the log; check the `serve:<site>` leases |
| A handler is never found | The handler is registered under another name than `<app>:<schedule>` | Register it under the job name shown in the job list |
| A schedule fires twice after a failover | It should not: the key makes the second insert a duplicate | Compare the two jobs' idempotency keys; different keys mean different instants |
| A run was skipped after a pause | The pause was longer than the grace and the policy is `skip` | Expected; lower `tick_ms` does not help, an internal schedule can use `run_once` |
| A run was lost after a restart of a single node | The in-process coordinator forgot the marks | Expected on the single-node profile; use the cluster profile for resume |
| The site refuses to start naming a schedule | A manifest schedule the planner rejects, or an internal schedule whose job name clashes with a manifest schedule or step | Fix the name or the cron in `app.toml` |
| `scheduled slot was refused` in the log | The schedule names a queue the job engine does not know | Use `default`, `short` or `long` |

## Durable marks (single node)

The last enqueued firing of each schedule is its high-water mark. In the cluster profile the coordinator keeps it. The single-node coordinator is in memory, so a restart used to forget it. A site can now also keep it in a table, `spot_schedule_mark` (`site`, `app`, `schedule`, `mark` in whole seconds), created if missing:

- The mark is written to the table first and to the coordinator second, and read as the later of the two. It never goes down.
- The job is inserted before the mark is raised, and the insert is idempotent by the firing's key. A crash between the two leaves the mark behind the job; the next start plans that firing again, the store drops it as a duplicate, and the mark then moves. A firing is never doubled and never skipped.
- On start the leader reads the marks and applies each schedule's missed-run policy once from them: `skip` fires nothing for the gap, `run_once` fires one catch-up. Two nodes share the table and the leader lease, so still one firing.
- The table is installed per site by the host with `schedule_marks::install_table(site, PgMarkTable)` (see the report: the call belongs in the jobs service assembly).
