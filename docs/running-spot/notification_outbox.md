# The task-inbox notification outbox

This page is for the operator and for the host developer. It describes the durable notification
path of the task inbox (WP-BPM-05.T14, crate `spot-bpm-tasks`, module `outbox`).

## What it guarantees

- **Written with the state.** A notification caused by a task change (created, delegated,
  reassigned, completed, an SLA reminder or escalation) is a row written in the same storage
  transaction as that change. If the change commits, the row exists; if it rolls back, it does
  not. A crash can neither lose a notification of a committed change nor send one for a change
  that never happened.
- **At least once.** A worker sends due rows through a delivery transport and marks them `sent`.
  A row can be sent twice (the worker died after the channel accepted it and before the result
  was recorded); it is never silently dropped.
- **Duplicates are harmless.** Each row has an idempotency key
  `{task id}:{task version}:{kind}:{recipient}`, passed to the transport on every attempt. A
  channel that can suppress a repeated key delivers one message.
- **Ids and titles only.** A row carries the same `Notification` as the best-effort path: the
  task id, the Case and node, the kind, the run label and the title. Never a form value or a
  comment. A failure detail is cut to 256 characters; a transport must not put content in it.

## A row

One row per recipient. Fields: key (also the row id), task id, kind, recipient, the notification,
`created_at`, `attempts`, `next_attempt_at`, `status` (`pending`, `sent`, `dead`), the lease
(worker and end) while an attempt is in flight, `last_error`, `sent_at`.

## The worker

`OutboxWorker::run_once(now)` is one pass; the host runs it from a scheduler job. Any number of
workers may run at once: claiming is atomic, so two workers never take the same row.

1. **Claim** rows that are `pending`, due (`next_attempt_at <= now`) and not under a live lease,
   oldest first, at most `batch`. Each claim sets a lease of `lease_secs` and adds one to
   `attempts`. A claim counts as an attempt, so a worker that dies each time still runs the row
   towards `dead`.
2. **Send** through the transport, one row at a time.
3. **Record.** Success: `sent`. Failure: back off, or `dead` when `attempts >= max_attempts`.
   A result from a worker whose lease was taken over is refused; the new holder's result wins.

A worker that crashes, or whose future is dropped, simply stops. Its lease ends after
`lease_secs` and another worker (or the same one after a restart) claims the row again. A row
whose every attempt ended without a result becomes `dead` without another send.

### Backoff

The delay after the n-th failed attempt is `base_secs * 2^(n-1)`, capped at `cap_secs`.
Defaults (`BackoffPolicy::default()`): base 30 s, cap 3,600 s, 8 attempts, lease 300 s, batch 50.
The lease must be longer than a batch takes to send, or a slow batch is taken over while still
running (the result is then a harmless duplicate, and `RunReport::lost_lease` counts it).

## Dead rows

A `dead` row is the operator's: `OutboxStore::list(Some(OutboxStatus::Dead))` shows it with its
last error. The reference store has no re-queue action; the host's adapter may add one that sets
the row back to `pending` with `attempts = 0`.

## The SLA reminder

The order is: the `Reminded` event is stored first and the outbox row is written in the same
transaction (`TransactionalTaskStore::commit_with_outbox`). A crash therefore loses no reminder
and repeats none: the row either exists with the event, or neither does.

## What the host provides

- A `TransactionalTaskStore` (the PostgreSQL adapter), writing the rows in the task transaction.
  The reference `InMemoryOutboxTaskStore` defines the contract.
- An `OutboxStore` over a table. The table needs a migration that the storage contract allows;
  the contract has no operation for it yet (open question on the task).
- A `DeliveryTransport` over the real channel; `NotifierTransport` adapts a `Notifier` that has
  no failure to report.
- A scheduler job calling `run_once(now)`.

## Not yet wired

`TaskService` still calls `TaskStore::commit` and queues notifications in memory (WP-BPM-05.T12
and T13). Switching it to `commit_with_outbox`, and retiring the in-memory queue, is a change to
`service.rs` outside this task's owned paths.
