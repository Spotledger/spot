Part of [Running Spot](../running-spot.md).

# Durable jobs and coordination

Jobs live in the site's database (`spot_job*` tables, created at start). A job enqueued by a
document hook is an outbox row of the document's own transaction, so it exists only if the
transaction committed; the intake takes committed events in with the event id as the
deduplication key and stores its position, so a restart neither loses nor repeats one.

The queue consumer (the worker and the intake) and the scheduler each run under a coordinator
lease, `serve:<site>:jobs` and `scheduler:<site>`. The holder renews at a third of the lease's
time to live and, before every intake batch, asks the coordinator whether its fencing token is
still the current one; a node that cannot renew or whose token is stale stops consuming and
goes back to standby, and a stopping node releases its lease at once. A node killed with
`kill -9` loses its lease when the time to live runs out, and the claims it held lapse on the
store's own clock: a standby (or the same node after a restart) takes the queue over and runs
the interrupted job again, with a higher attempt number.

The single-node profile keeps the in-process coordinator, which refuses a second node in the
same process. It cannot see another *process*: never run two `spot serve`
processes on one site's database in the single-node profile; use `profile = "cluster"` for that. The cluster profile uses the database coordinator. `services.jobs.store =
"memory"` keeps jobs in the process only; the cluster profile refuses it.
