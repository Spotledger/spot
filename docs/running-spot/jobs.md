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

## Requeueing a dead or retrying job

The job store can move a job that ran out of attempts (`dead`) or is waiting for its retry
(`retrying`) back to the queue. A requeued job is `pending` and due at once, its attempt counter
is back to 0 (it gets its full `max_attempts` again), its claim is cleared, and its last error
stays as history. The requeue is one atomic step in the store: only a `dead` or `retrying` job
without a claim is changed, so a worker that claims the job at the same moment, or a reaper that
releases it, is never overwritten, and of two requeues of the same job exactly one succeeds.

| Answer | When |
| --- | --- |
| the job, now `pending` | the job was `dead` or `retrying` |
| `job.requeue_refused` | the job is `pending`, `running` (a live claim), `succeeded` or `cancelled`; nothing changed |
| `job.requeue_key_held` | another live job holds the same idempotency key (site, name, key); nothing changed |
| `job.requeue_unsupported` | a custom store that does not implement the operation |
| the store's not-found error | no job has that id |

The PostgreSQL store makes the job due at the earlier of the caller's clock and the database
clock, so a node with a wrong clock cannot push a requeued job into the future. A requeued job
also leaves the retention window (its finish time is cleared), so the retention purge cannot
delete it. Each successful requeue is counted in `spot_job_requeued_total`, labelled with the
job's queue only (`default`, `short`, `long` or `other`); a refused requeue is not counted. Every
job store the host builds (PostgreSQL through `services::jobs::durable_store`, and the in-memory
profile's store) counts on the host's one Prometheus sink, so the series is on `/metrics` (the
metrics reference lists it); the label is never a tenant, site or job id. The
operator-facing remedy that uses this operation (`job.retry_failed`) is wired by the jobs part
of the world-access ports; see [world access](world-access.md).

## Jobs: the admin port and what a remedy can do to a job

The jobs module provides, besides the port that creates jobs, a **job admin port** built from the
site's own job store. It exists on both job profiles (PostgreSQL and `services.jobs.store =
"memory"`), carries the site id, and answers "the site is stopping" from the stop signal on. It
shows a job as a summary (id, name, queue, state, attempts, lease expiry), never its arguments,
context, idempotency key or error text, and a job of another site is invisible to it. It can list
a site's jobs by state, read one, requeue one, cancel one that is still waiting, and release lapsed
leases. It cannot create a job, change a job's arguments or read them.

The world access layer's jobs part (the remedies' `job.retry_failed` and `job.unstick_running`)
works through this port:

| Remedy operation | What it does |
| --- | --- |
| retry a failed job | requeues a **dead** job: pending, due now, attempts back to 0, last error kept. A job that is not dead, or whose idempotency key another live job holds, is refused and nothing changes |
| undo of a retry | cancels the requeued job **only while it is still pending and unclaimed** (the store decides atomically), so it ends in a terminal state without running again. If a worker already claimed it (or it finished), the undo is refused: "the job already ran again" |
| unstick a running job | releases a running job whose lease has expired (by the site clock) through the store's own reaper; the same jobs the reaper would release within two seconds |

The undo of a retry is **not exact**: a job can only reach `dead` by a worker running out of
attempts, so the store cannot put a requeued job back to `dead`. The undo leaves it `cancelled`
(shown as *done*, not *failed*) with its error history kept. A job that is already cancelled counts
as undone, and the undo of an unstick is not supported.

The cancel is one atomic store step, `JobStore::cancel_if_pending`: only a job that is `pending` or
`retrying` and holds no claim becomes `cancelled`, in one statement in PostgreSQL (`UPDATE ... WHERE
state IN ('pending', 'retrying') AND claim_token IS NULL`, no read first) and under the store's lock
in memory. A worker that claims the job at the same moment either wins (the job keeps running and
the undo is refused) or loses (the job is cancelled and its claim finds nothing): never both. The
store answers three things apart (`CancelOutcome`: cancelled, not cancellable because a worker holds
the job or it is already over, unknown id); none is an error, and the port turns the last two into
its fixed refusals. `JobStore::cancel` is unchanged (in PostgreSQL it also cancels a running job,
which is why the undo does not use it). Every store implements `cancel_if_pending`; there is no
default body, because a default that read the job and then called `cancel` would be the race again.

### The retrieval index generation

The remedies' `index.rebuild` enqueues the retrieval backfill and bumps a **generation** number by
exactly one; its undo restores the previous number only if nothing newer ran. The number is kept in
`sites/<site>/retrieval.generation` beside the retrieval cursor, as one decimal written to a
temporary file and renamed, so it survives a restart and the undo of a rebuild made before the
restart still compares against the real counter. Like the cursor, the file belongs to this node.
It is not the feed generation the cursor carries (a different number). A rebuild writes the new
number first and puts the old one back if the job queue refuses the backfill; a missing or
unreadable file reads as 0.
