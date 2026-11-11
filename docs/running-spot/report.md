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

An unknown key is refused when the configuration is read.

## Audit

Target `spot::audit::report`: a line for every run (who, report, rows returned, whether the
result was cut) and for every refusal (who, report, and the reason code). **Never** an argument
value, a filter value or a row. The field guard writes its own `spot::audit::field_access` lines
for the columns it decides.

## Trying it

```
GET  /api/v1/report
GET  /api/v1/report/hours_by_task
POST /api/v1/report/hours_by_task/run   {"filters":{"from_date":"2026-10-01"},"limit":100}
```
