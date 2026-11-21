Part of [Running Spot](../running-spot.md).

# Reports

`/api/v1/report*` serves the reports the installed apps ship: a client lists the reports it may
see (`report_list`), asks one for its parameters and columns (`report_describe`) and runs it with
filters (`report_run`). The contract is `contracts/native-api/openapi.yaml`. The rules live in
`spot-reports` (the definition, the compiler, the runner); this page is what the host adds and
what an operator can turn.

A report is a named, declared query over one DocType and its links. It runs as **one storage
query** with the caller's read filter ANDed on. There is no free SQL anywhere.

## Where a report comes from

An app ships a report as one TOML file, `<app>/reports/<name>.toml`, next to its `doctypes` and
`permissions` directories. An app without a `reports` directory ships none; files that do not end
in `.toml` are ignored.

```toml
name = "hours_by_task"          # lowercase letters, digits and underscores; unique on the site
label = "Hours by task"
base = "time_entry"             # the base DocType
roles = ["Project Manager", "Team Member"]   # who may see that the report exists
group_by = ["task"]
limit = 100                     # the default row limit (1,000 when absent)
max_rows = 1000                 # the largest limit a run honours (10,000 when absent, 100,000 at most)

[[parameters]]
name = "from_date"
label = "From date"
type = "date"                   # text, int, decimal, date, link or select
required = true                 # a required parameter has no default

[filters]
field = "work_date"
op = "ge"
param = "from_date"

[[columns]]
field = "task"

[[columns]]
field = "hours"
label = "Hours"
aggregate = "sum"               # sum, count, min, max or avg

[[order_by]]
field = "task"
```

`spot-reports` documents the whole format: joins (`alias`, `doctype`, `on`), computed columns,
`all`, `any` and `not` filter nodes, and `link` and `select` parameters with their options.

## When the definitions are read

- **Site start.** Every installed app's `reports/*.toml` is read, and then compiled against the
  metadata the site serves. A file that does not parse, a base DocType the site does not have, a
  field a DocType does not declare, a filter bound to a parameter that is not declared, a limit
  above the maximum and the same report name in two apps all **stop the site's start**, and the
  message names every problem (the report, its app and the key). Fix them in one pass.
- **A metadata swap.** A customisation that adds or drops a field swaps the served metadata. The
  compiled reports belong to the snapshot they were compiled for: the swap drops them and the
  next request compiles the definitions against the new snapshot. A report that no longer
  compiles there is **left out** of that snapshot (it is listed nowhere and answers
  `404 report.not_found`) and the log names it and the problem. Nothing is run against metadata it
  was not written for. A report is never cached across a swap, so there is no stale compiled
  report to invalidate.

## What a caller sees

- **Which reports.** A report whose `roles` the caller holds none of is not listed, and
  `describe` and `run` answer `404 report.not_found`: the same answer as a report that does not
  exist, so the answer does not tell a hidden report from a missing one. The platform's own
  principals see every report.
- **Which rows.** The caller's read filter (roles, user permissions, `if_owner`) is part of the
  query. A caller with no read right on the base DocType gets a result of the same shape with no
  rows, not an error that reveals the DocType.
- **Which columns.** A column the caller may not report on (the Report channel of the field
  guard) is removed from the result, and the result's `messages` says how many were removed
  (`report.columns_removed`) without naming them. A column the caller may see only masked is
  masked.
- **No inference.** A filter, a sort, a group or an aggregate on a field the caller may not report
  on is `403 permission.field_denied` naming the field. It is refused, never silently dropped, so
  a result cannot leak a value through what it was filtered or sorted by.
- **Describe.** `report_describe` states the parameters and the columns the caller may see, in
  result order. The columns come from the runner itself, run over no rows with the report's
  required parameters bound, so a description and a run cannot disagree. A column removed for the
  caller is not listed and not counted.
- **Arguments.** An unknown parameter, a missing required one, or a value that does not fit its
  parameter's type is `422 report.bad_argument`, naming the parameter and never its value. A
  request's `limit` never exceeds the report's `max_rows`; a result cut by the limit says
  `truncated: true`.

## Limits and failure

- **Level: required.** The report service is required when the site's apps ship reports: a
  definition that does not compile fails the start-up check and stops the site. With no report
  shipped it has nothing to fail. Readiness (`/api/health/ready`) lists it as `report`.
- **Limits.** Rows are bounded by `max_rows` (at most 100,000) and the query by the storage IR's
  own limits (depth, nodes, joins), which the compiler checks when the report is compiled, not
  when it runs.
- **A session that has not chosen its password yet** may do nothing here.
- **Not yet.** Prepared (stored) reports are a later task; every run is live. The report builder
  is a UI-stage task.

## Settings

`[services.report]` in `common.toml`:

| Key | Default | Meaning |
| --- | --- | --- |
| `enabled` | `true` | `false` serves no report: `report_run` answers `404 report.not_found` and `report_list` and `report_describe` are not routed. |
| `max_concurrent` | `4` | The most reports that run at once on the site. At least 1; `0` stops the start naming the key. |
| `max_concurrent_per_user` | `2` | The most reports one signed-in user runs at once. At least 1; `0` stops the start naming the key. |

An unknown key is refused when the configuration is read.

## Audit

Target `spot::audit::report`: a line for every run (who, report, rows returned, whether the
result was cut) and for every refusal (who, report, and the reason code). **Never** an argument
value, a filter value or a row. Every line carries the request id and the result class; a run
also carries its duration in milliseconds. The field guard writes its own `spot::audit::field_access` lines
for the columns it decides.

## Limits, timeouts and what the client sees

**Two caps.** A run takes a place before anything is compiled or read: at most `max_concurrent`
runs at once on the site and `max_concurrent_per_user` for one signed-in user (the user id the
credential resolves to, never a header). A run that finds either cap full is refused **at once**
with `429 api.rate_limited` and `Retry-After: 2`. Nothing queues, so memory stays bounded and a
busy site stays answerable. The place is freed on every exit, including a client that goes away
mid-run. `report_list` and `report_describe` take no place. A report a service serves from its
own data (a report source) runs through the same door.

**The statement timeout.** A run is one read-only storage query. The PostgreSQL read path sets
`statement_timeout` from the storage contract's transaction defaults (`TxOptions`, 30 seconds
today) with `set_config` inside the read transaction, and `lock_timeout` (5 seconds) beside it.
There is no per-site setting for it in this release; the report service does not run a second
timer. Whatever the database answers is classified below.

**What a client sees.** The text of a storage failure never reaches the client: SQL, table and
column names can ride in it. Each class has one fixed message.

| The run ends with | The client sees | What a client should do |
| --- | --- | --- |
| The statement timeout | `report.timeout` (rendered by the native API as an app-coded error) | Narrow the filters or the date range. Do not retry the same request unchanged. |
| The database is unreachable, a lock timeout, a serialisation conflict or a deadlock | `503 server.unavailable` with `Retry-After` (5 s) | Wait for `Retry-After` and run it again. |
| The site or the user is at its cap | `429 api.rate_limited` with `Retry-After` (2 s) | Wait for `Retry-After`; a dashboard should open its reports a few at a time. |
| Anything else from storage | `report.failed`, fixed text, no detail (rendered as an internal error) | Quote the request id to an administrator. |
| The storage layer refuses a compiled report's query | `report.invalid_definition`, fixed text plus the stable storage code (for example `storage.invalid_query`) | Tell the report's author; the definition or the metadata is wrong. |

The operator's view is the `spot::audit::report` line written at `warn` for a failure behind
those answers: the report name, the **request id** and the stable storage code
(`storage.statement_timeout`, `storage.unavailable`, ...). Use the request id from the client's
error to find it.

**Metrics** (`GET /metrics`, labels are bounded: a result class, never a report name or a user):

| Metric | Meaning | Alert suggestion |
| --- | --- | --- |
| `spot_report_runs_total{result}` | Runs by result: `ok`, `timeout`, `unavailable`, `refused_busy`, `denied`, `failed` | `timeout` rising for 10 minutes means a report or the data changed. `refused_busy` above a few per minute means the caps are too low or a client is flooding. |
| `spot_report_run_seconds` | Histogram of the run's duration | p95 near the statement timeout means reports are about to start timing out. |
| `spot_report_rows_returned_total` | Rows returned by successful runs | A sudden rise with no new users points at one report whose limit was raised. |
| `spot_report_in_flight` | Runs in flight now | A value pinned at `max_concurrent` for minutes means the site is saturated with reports. |

**Troubleshooting.**

| Symptom | Likely cause | What to do |
| --- | --- | --- |
| A report that worked now answers `report.timeout` | The table grew, an index is missing, or a wider date range is used | Narrow the range; look at `spot_report_run_seconds`; ask for an index on the filtered field. |
| `429` on a dashboard that opens many reports | More than `max_concurrent_per_user` at once | Open them a few at a time, honouring `Retry-After`, or raise `max_concurrent_per_user` and watch `spot_report_in_flight`. |
| `503 server.unavailable` during a database failover | The primary is unreachable (storage `Unavailable`) | Wait for `Retry-After`; once the new primary is up runs succeed on their own. |
| `report.failed` with a request id | A storage failure the client is not told about | Find the request id in the `spot::audit::report` line; the `storage_code` field names the cause. |
| A heavy report starves interactive pages | Reports share the site's connection pool | Lower `max_concurrent`, and route reports to a report node (WP-PLT-20), which moves the load off the interactive nodes without changing the caps. |
| `report.invalid_definition` at run time with a `storage.` code | Metadata changed under a compiled report | The report is recompiled on the next metadata swap; fix the definition if it persists. |

## Trying it

```
GET  /api/v1/report
GET  /api/v1/report/hours_by_task
POST /api/v1/report/hours_by_task/run   {"filters":{"from_date":"2026-10-01"},"limit":100}
```

## Readiness line

The site's readiness report carries a `reports` check that is always passing and whose detail is `in flight n/max`: the runs in flight now against `max_concurrent`. `n` never exceeds `max` (a run at the cap is refused with 429 before it counts), a refused run does not change `n`, and the text is two numbers, never a report name, a user or a site. The module provides the check as the port `ReportReadiness`; the host adds it to the readiness list beside the service health.

`report.timeout` and `report.failed` still render as HTTP 500 until the contract's error registry carries them (504 for the timeout); that mapping is not made here.
