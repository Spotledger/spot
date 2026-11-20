Part of [Running Spot](../running-spot.md).

# Process throughput (completed Runs per week)

The `bpm_throughput_by_week` report shows how many Runs of each process version finished in each
week. Its numbers come from this module (`services/process_throughput.rs`, `ORDER` 1192), which
sits after process insight (1190) and before the insight reports (1195) that look its feed up.
Without it the throughput report is not registered at all; the other two insight reports do not
depend on it.

## What it counts

Completed Runs only (a Run that failed, was cancelled or is still open is not counted), grouped by
process name, process version and the **UTC ISO week of the Run's end** (Monday 00:00 to Sunday
23:59:59, labelled `YYYY-Www`; the last days of December and the first of January can belong to
the other year's week 52 or 53 or to week 1). The window is the current ISO week and the `weeks`
weeks before it, taken from the site clock: a Run that ended before the window starts is not in the
report. Counts are whole numbers.

## How it stays fast

The report executor asks a synchronous question, so the module never queries the database from it.
A small service keeps the latest counts in memory and refreshes them once at start-up and then
every `refresh_secs`. The numbers are therefore **up to `refresh_secs` old** (five minutes by
default). Every node of a site keeps its own copy; they read the same table and agree to within
one refresh.

If a refresh fails, the previous counts stay in use and one warning is logged for the whole run of
failures (not one per attempt); the next good refresh replaces them and ends the streak. The log
carries no process name and no count.

## Before the first refresh

Until one refresh has succeeded there are no counts. The feed then answers "cannot answer", and
the report answers an error (the same unavailable answer the other insight reports give when their
source fails), **never an empty table**: an empty table would read as "nothing finished this
week". Readiness reports the service as not ready only until the first refresh has been tried, and
never for a later failure (the service is optional: losing it removes one report, it never stops
documents).

## The one index

At start-up the module creates, if it does not exist, one partial index:

```sql
CREATE INDEX IF NOT EXISTS spot_bpm_case_ended_idx
    ON spot_bpm_case (tenant, ended_at) WHERE ended_at IS NOT NULL
```

It lets the weekly query read only the window. Creation is serialised with an advisory lock, so
several nodes starting together are safe. If the index cannot be created the site does not start,
and the error names the site. The index is small (one entry per finished Run) and is dropped
with the table.

## Settings

`[services.process_throughput]`:

| Key | Default | Range |
| --- | --- | --- |
| `enabled` | `true` | A disabled module registers nothing, so the report is not registered |
| `weeks` | `26` | `1..=104`: how many ISO weeks back from the current one |
| `refresh_secs` | `300` | `30..=3600` |

Unknown keys are refused, and a value outside its range stops start-up with an error naming the
key (`services.process_throughput.weeks: out of range`). The module is inert when the process
engine is not hosted.

A very busy site with many processes and versions can have more week/process/version groups in the
window than one refresh reads (5,000, read in week order, so the newest weeks would be the ones
cut); a smaller `weeks` keeps the window inside that bound.
