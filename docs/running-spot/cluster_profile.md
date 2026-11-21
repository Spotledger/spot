Part of [Running Spot](../running-spot.md).

# The cluster profile: what is guaranteed, who leads, what a failover looks like

`services.profile = "cluster"` coordinates through the site's PostgreSQL database, with no Redis.
It needs `services.jobs.store = "postgres"` and `services.metadata.bus = "polling"`; `spot serve`
refuses to start otherwise. The tables are `spot_coord_lease`, `spot_coord_token`,
`spot_coord_counter`, `spot_coord_rate` and `spot_coord_node`.

## What the profile guarantees

- **One scheduler leader per site.** The lease `scheduler:<site>` has one live holder. Only the
  holder plans schedule firings and inserts their jobs. Every firing has one job: the
  idempotency key is `<app>:<schedule>:<instant>`.
- **No job claimed twice.** The job store (`spot_job`) fences every claim with a token; two
  workers never run the same claim. The hosted worker also takes the lease `serve:<site>:jobs`,
  so on one site one node consumes the queue at a time and the others stand by. A test that
  wants several nodes sharing the work runs workers on the store directly.
- **A lost leader loses no firing and repeats none.** The high-water mark of each schedule is
  the counter `schedule_mark:<site>:<app>:<schedule>`. A new leader reads it before its first
  tick, and the mark moves only after the job exists.
- **Rate limits are the database's.** `Coordinator::take` on any node draws on one bucket per
  key, so the admitted calls never exceed the limit however many nodes ask.
- **A stale leader cannot write.** Each acquisition gets a larger fencing token; `check_fence`
  refuses every older one.

## What runs on one node only

The scheduler loop and the queue consumer. Everything else (HTTP, realtime, reports) runs on
every node.

## Which node leads

```sql
SELECT name, holder, token, expires_at FROM spot_coord_lease WHERE name = 'scheduler:<site>';
```

The holder is `node-<site>-<random part>`, chosen at each start and written in the node's log
lines. `select value from spot_coord_counter where name = 'schedule_mark:<site>:<app>:<schedule>'`
is the last firing whose job exists.

## What a failover looks like

- A node that stops cleanly releases both leases; another takes the scheduler lease at its next
  standby tick (a third of the lease time to live, at least 100 ms). The token grows by at least
  one.
- A node that dies keeps its lease until it lapses (`services.scheduler.lease_ttl_ms`, default
  30 s, and `services.jobs.lease_ttl_ms`, default 15 s). Another node then takes it within a
  standby tick of the lapse. Firings that came due meanwhile are still on time under `skip` as
  long as they are inside the grace period (`max(3 x tick, 60 s)`).
- In the metrics, `spot_lease_events_total{lease="scheduler",event="acquired"}` rises on the new
  node, `lost` or `released` on the old one, and `spot_lease_held{lease="scheduler"}` is 1 on the
  holder and 0 elsewhere. Metrics are per process: sum them across nodes and expect 1.

## The single-node refusal

The single-node profile uses the in-process coordinator, which serves one node. Started over a
node presence that shows another node (`NodePresence`, database-backed in `spot-coordinator-pg`),
it fails with the code `coordinator.second_node` and names the other node. Read the code, not the
sentence: the sentence changes. The fix is to run the cluster profile, or to stop the other node.
Note: `spot serve` builds the single-node coordinator over an in-memory presence today, so two
hosts of the single-node profile are not yet refused at start-up (WP-PLT-18.T3 open question).

## The test

`crates/framework/spot-cli/tests/cluster_three_hosts.rs` starts three hosts over one database
(`SPOT_TEST_PG_URL`) with leases of 1.5 s and a schedule that fires every second.
