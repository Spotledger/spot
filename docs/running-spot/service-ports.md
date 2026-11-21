Part of [Running Spot](../running-spot.md).

# Service ports of the jobs module

The `jobs` module (order 600) provides three narrow ports to modules that register after it.
A module takes one with `ctx.require_port::<dyn JobSubmit>()` (or `ctx.port()` when it can do
without). A module that registers before `jobs` gets the registry's error, which names the
module and the port. The code is `crates/framework/spot-cli/src/services/ports.rs`.

Every port belongs to one site. It is built from that site's own job store, coordinator,
change feed and engine, and no method takes a site id. A port of site A never reads or writes
site B.

## JobSubmit

Can: create one job with a name, a queue, arguments, an idempotency key and an optional
not-before time. It answers `true` for a new job and `false` when a job with the same name and
key already exists.

Cannot: list, read, cancel or retry any job. It refuses a malformed name, a queue the site's
workers do not claim, an empty or over-long key (200 bytes) and arguments over 64 KiB.

## OutboxLease

Can: take, renew, check and release a lease named `outbox:<site>:<consumer>` (a consumer name is
lower-case letters, digits, `_` and `-`, up to 64 bytes), and read published outbox events after
a position, at most 500 at a time. A holder that finds `still_held` false must stop acting.

Cannot: touch the jobs intake lease (`serve:<site>:jobs`) or the scheduler lease, accept a lease
made for another site, or write to the outbox. The port keeps no cursor: the consumer stores
its own `FeedPosition` and passes it to `read`.

## SystemReader

Can: load one document by DocType and record id, as the platform itself (a system actor named
`service_ports`), for the current site. It answers `None` when the site has no such document.

The system principal is not filtered by permissions or protection groups, so a document comes
back whole except for `secret` fields: the engine keeps those out of a `Document` altogether
(their sealed bytes live in engine-managed columns), so `webhook.secret` and
`user.password_hash` never reach a module. A JSON field that holds a sealed value (webhook
`headers`) returns it sealed, as stored. A module that takes this port must not copy a
document's fields into a log, a job argument or a delivery record.

Cannot: write, list, query, or read another site. It is not reachable from a request path or a
DocType script; only a module that takes the port from the registry holds it.

## After the stop signal

When the site's stop signal arrives every method answers `PortError::Stopped` at once (it does
not wait). The one exception is `OutboxLease::release`, so a holder can still hand its lease
back. Errors never quote a payload, key, document name or secret.

## Job handlers

A handler is registered with `ctx.job_handlers()`, as mail does; the jobs module takes the
registry when every module has registered. It is not a port.

## Reloads and the memory store

Ports are provided on the assembling build only. A reload keeps the running services and has no
store to build from, so a module that needs a port builds its service only when
`ctx.assembling()` is true. With `services.jobs.store = "memory"` the ports exist as well (tests
use them) and the memory-store notice is unchanged.

## Registration ports: report sources and internal schedules

Two modules offer a port a later module uses to register something of its own. Both follow the
rule above: the provider has the smaller `ORDER`, the port exists on the assembling build only
(`ctx.port::<..>()` answers `None` on a reload, and a module skips quietly), and what is
registered is per site: nothing is shared between sites.

### `ReportSources` (module `report`, `ORDER` 1020)

`services/report.rs` serves the reports apps ship as `<app>/reports/*.toml`. A service whose data
is no DocType (the process engine's outcome tables) registers a report of its own from its
`register`, with `ctx.port::<dyn ReportSources>()` and `register(ReportSource { .. })`:

- the `ReportDef` (already validated by the caller);
- a minimal in-memory **source schema** for the definition's base name: DocType-shaped
  `spot_meta::Definition`s, enough for the report compiler. It is never persisted, never added to
  the metadata the site serves and never visible to a `meta` route;
- a `SourceExecutor` that answers the report engine's one storage query over the source's own data
  and applies the caller's `Admission`;
- a `SourceVisibility` hook, `admit(&AdmitRequest) -> Result<Admission, Denied>`, which returns
  `Admission::All`, `Admission::Rows(keys)` (a row filter the executor applies; the engine never
  interprets a key) or a refusal with a reason code;
- `required`: whether a source that does not compile stops the site. Otherwise it is named in the
  log and left out.

The report engine stays the one runner: `report_run`, `report_list` and `report_describe` serve a
source report exactly like a shipped one (the report's `roles`, the row cap, `truncated`, the
Report-channel field guard on columns, the audit lines). The source's schema gets a permission
engine of its own that lets the report's roles read it, since the site's engine knows nothing of
it; which rows a caller sees is the hook's decision.

**Fail closed.** A hook that returns an error, panics or takes longer than five seconds denies the
run with the answer of a report that does not exist (`404 report.not_found`) and an audit line
(`spot::audit::report`, `report source refused`) with the site, user, report and reason code. A
denied source is absent from the list, never an empty result. A request can never name a source or
an executor: only a registered report name resolves.

**Registration rules.** A report name registered twice names both services. A source report named
like a shipped one stops the start-up check naming both. The registry closes at the start-up
check; a later registration is refused.

### `InternalSchedules` (module `jobs`, `ORDER` 600)

`services/jobs.rs` plans the schedules of app manifests. A service registers a schedule of its own
with `ctx.port::<dyn InternalSchedules>()` and `register(InternalSchedule { .. })`: the service
name, the schedule name, a six-field cron expression (seconds first, UTC), the missed-run policy,
the queue (`default`, `short`, `long`), the budget and the handler.

The job is called `<service>:<name>`. The scheduler plans it like a manifest schedule (the leader
lease, a high-water mark per schedule, `enqueue_scheduled`), and the worker runs the handler from
the same registry as manifest handlers, so the claim gate, the side-effect hold and the metrics
apply unchanged. Every run writes an audit line (`spot::audit::internal_schedule`) naming the
service, never a caller. A job name that a manifest schedule (its planned name or its step) or an
earlier handler already uses stops the start-up check. The scheduler's health text shows internal
schedules by name only.

The missed-run policy is the scheduler's: `run_once` collapses missed runs into one, `catch_up(n)`
runs up to `n`, and `skip` plans nothing for a firing that is due (see the open question in the
task report: the manifest schedules use `skip`).
