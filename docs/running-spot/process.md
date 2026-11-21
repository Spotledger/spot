Part of [Running Spot](../running-spot.md).

# Process engine

The process engine (`spot-bpm-engine`) runs durable Business Process Cases over PostgreSQL:
a Case survives a restart exactly where it left off, a timer fires once, and a node left
`started` by a killed process replays with `is_retry == true` rather than being lost.

## Where state lives

Two tables, created at start by the module's own `prepare()` (idempotent, so a reload or a
second site on the same database is harmless): the Case store (`spot-bpm-pg`'s
`PgCaseStore`) and the definition store (`spot-bpm-defs-pg`'s `PgDefinitionStore`). Both are
scoped to the site's tenant, so two sites sharing one database never see each other's Cases
or published processes.

Every published process version is loaded into the engine's registry at start. A version
that no longer compiles is a logged, named finding — never a reason to stop loading the
rest, and the log never carries the process's text. [`ProcessHost::reload_definitions`]
rebuilds the registry after a publish without a restart (the publish path itself is a later
task; this module only exposes the hook).

## Node handlers

This module ships handlers for the kinds `spot-bpm-engine` defines itself (Trigger, Wait,
the Logic kinds). Other node kinds — Decision, Document, Integration, forms, messages — are
hosted by their own modules, each with a smaller `ORDER` than this one (1110 to 1230) so they
run first and register their handler with this module's `HandlerBuilder` port before the
process module finalises the engine.

A process that uses a kind nothing has registered a handler for is **refused when a Case
would start on it**, not failed partway through a run: `start_run` checks every node kind
against the registry and reports which kind is missing before creating anything. The
readiness check repeats the same scan over every loaded process and names the first problem
it finds, so an operator sees it before anyone tries to start a Case.

## Case-step jobs and durable resume

A Case advances through `default`-queue jobs (`services.jobs.queues`), one per `step`; the
idempotency key is the engine's own, so a double enqueue is dropped by the job store, never
run twice. A step that made progress but did not finish the Case re-enqueues itself once —
the engine only advances the nodes that were ready *at the start* of the step it was given.

At start, and then every `resume_interval_secs` under a held outbox lease
(`outbox:<site>:process`), the module lists every Case the store reports runnable — a ready
node, a due timer, or a child that can now complete its parent — and enqueues a case-step job
for each. This is what finds a Case a crashed process left mid-step: the next resume scan
sees it runnable and the engine's own contract reruns the interrupted node as a retry.

## Timers

Timers are routed through a `FencedTimerPort`: an in-process index guarded by a lease, so
only the instance holding the fence can fire one. The lease is scoped to this process only —
true cross-process fencing of timers across a multi-node deployment needs a durable
`LeaseStore` this workspace does not provide yet (tracked as a follow-up, **WP-PLT-18**:
a PostgreSQL-backed `LeaseStore`); until then, run one instance of the process module's
timer port per site. This is a known, accepted gap, not a correctness bug: a wait firing
more than once is actually prevented by the *durable* Case record, not by this lease —
`Engine::step` fires a timer only when it is still present and due in the Case's own
`timers` map (read from the store), and `ProcessHost::resume_once` lists runnable Cases
through the store-backed `CaseStore::list_runnable`. Both are tenant-scoped and durable; the
in-memory `FencedTimerPort`/`LeaseStore` only decides who is allowed to *register* a new
wait, which is harmless to do twice.

## Configuration

`[services.process]`:

| Key                     | Default | Notes                                           |
|--------------------------|---------|--------------------------------------------------|
| `enabled`                | `true`  | `false` makes the module inert: no table, no port, no job handler, no resume loop. |
| `resume_interval_secs`   | `15`    | Clamped `5..=300`.                                |

Unknown keys are refused.

## Health

The readiness check loads every published process exactly once (the first poll after the
engine finishes building), caching which `(name, version)` pairs loaded. Every later poll
only re-checks handler coverage against that already-loaded registry — a read, never a
reload — so readiness polling never re-mutates the live `ProcessRegistry` on a schedule.
[`ProcessHost::reload_definitions`] is the only other reload path, run after a publish
(WP-BPM-09.T4). Failures name the process and version (and, for a missing handler, the node
kind); they never include the process text or any case's data.

**A Case pinned to a retired version.** The readiness load and `reload_definitions` load every
published version, plus each retired version the definition store's `running_pins` still lists,
so a Case keeps running on its pinned version (`Run::dag_version`) across a restart.
`ProcessRegistry::latest` considers published versions only, so a retired version is never
chosen for a new start (WP-BPM-09.T5). A failure to read the pin list aborts the load.

## Metrics

Counters only, and none of them ever carries a label — a run id, a case id, a process name
or an error's text can never reach a Prometheus label, by construction:

- `spot_bpm_runs_started_total`
- `spot_bpm_runs_completed_total`
- `spot_bpm_runs_failed_total`
- `spot_bpm_steps_run_total`
- `spot_bpm_resume_backlog` (a gauge: how many Cases the last resume scan found runnable)

Anything that needs a run id or a case id belongs in the structured logs, not in a metric.

## The case desk (stuck cases and unstick)

The module provides `dyn CaseDesk` (`services/process/case_desk.rs`), the port the remedy
world's cases part sits on, so the `case.unstick` remedy runs on a live site. It is provided
from the module's finaliser, after every module registered, and only when the process insight
module (`process_insight`, `ORDER` 1190) is running: the definition of "stuck" comes from it.
Without insight there is no desk and readiness for `world_access` says `no cases`, as before.

**Stuck, in one rule** (`case_desk::standing`, pinned by unit tests). Process insight reports an
open attempt that stayed in its state longer than its node expects, with a reason. The desk
narrows that to what the platform can clear:

| Insight reason      | Desk says | Meaning                                                        |
|---------------------|-----------|----------------------------------------------------------------|
| `running`           | stuck     | runnable, or running past its lease, and not progressing       |
| `integration_retry` | stuck     | the retry is exhausted or overdue                              |
| `waiting_for_task`  | waiting   | a person must act; never pushed by the platform                |
| `timer`             | waiting   | a date or an event is awaited                                  |

Any unstickable reason makes a case stuck even if another branch waits. A case with only waiting
reasons is not listed by `stuck()`, `life()` answers `running` for it (the remedy world has no
waiting state), and `unstick` refuses it. A finished case is `done` and is refused. The reasons
come from the insight's last fold and the site's `[services.process_insight].stuck_after_secs`.

**Unstick.** `unstick(actor, case, now)` requires that the actor holds the `process_supervise`
permission on its **own** principal (the actor is a user record id, with or without an `agent:`
or `user:` prefix; an agent's role set is its user record's assignments). An actor that cannot be
resolved, or lacks the permission, is refused, and the platform's system principal never counts.
The case must exist on this site (the store is the site's own, so another site's case is "no
such case") and be stuck. Then one case step is queued through `ProcessHost::unstick_step`, the
host's idempotent enqueue (the engine's own key, so a second unstick while the first job is
queued is dropped by the job store). The answer is where the case stands afterwards: `running`
(a step is queued) or `done`.

The refusals are fixed texts that name no case data: `the actor may not supervise processes`,
`no such case`, `the case is waiting, not stuck`, `the case is not stuck`, `the case has
finished`, `the process engine is not available`, `the case step could not be queued`. The call
is logged with the actor id and the case id only; the remedy host records the audit events of
the remedy itself.
