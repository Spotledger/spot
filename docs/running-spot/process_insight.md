Part of [Running Spot](../running-spot.md).

# Process insight (feed, checkpoint, access)

Process insight turns what the process engine did into numbers an operator reads: how long each
step takes, how much of that is waiting, which step is the bottleneck and which Runs are stuck.
This module (`services/process_insight.rs`, `ORDER` 1190) is the **feed**. The reports and the
schedule that refresh them are a separate module (WP-BPM-11.T4) that reads this one through the
`ProcessInsightPort`.

## Where the numbers come from

The engine's store appends one row to the **node-run transition log** (`spot_bpm_node_run_event`)
in the same transaction that changes a node, so the log says *when* each attempt started, waited,
finished or failed. Every `fold_interval_secs` (default 10) the module reads the next batch after
its checkpoint, maps each record to an insight event, folds it, and saves the folded state and the
log position together in one compare-and-set checkpoint (consumer `process_insight`). It reads no
payload, no Case variable and no failure text: a record holds codes, keys, ids and instants only.

A cancelled attempt closes as a failure with the code `bpm_insight.cancelled` (insight has no
cancelled state), so a cancelled Run never lingers as stuck. A record whose process version is not
loaded is skipped and counted by process name.

## One node folds, every node reads

The module takes the lease `process_insight` (the coordinator the job worker uses). The holder
folds; any other node adopts the checkpoint the holder saved, so a snapshot anywhere is at most one
interval old. A lease lost between reading a batch and saving it drops the batch and makes
readiness say `process_insight.lease_lost` until the next good fold. Nothing is lost: the next
fold starts again from the saved checkpoint.

## A restart and a rebuild

A restart restores the accumulator from the checkpoint and carries on; its numbers equal a fresh
fold of the whole log. To rebuild from zero, delete the `process_insight` row of
`spot_bpm_node_run_consumer` (or fold under another consumer name): the next fold reads the log
from the start and gives the same numbers. A repeated batch (a crash between the fold and the
save) is harmless: the fold ignores an attempt it already finished.

## Memory and retention

The accumulator is O(steps x histogram buckets) plus O(open attempts): one fixed-bucket
cycle-time histogram per step, never a list of samples, and the checkpoint state is the same
size. Percentiles are the nearest-rank bucket value: whole milliseconds, exact up to 31 ms and
less than 3.2% above beyond. The log is **not** unbounded by itself: how long it is kept is yours
(`prune_before`, never scheduled by the engine). A rebuild after pruning covers only the retained
window, so keep the log at least as long as the period you want insight over, or do not delete the
checkpoint after pruning.

## Who sees what

* **Aggregates.** A viewer sees a process only if they may read it (`process_supervise` or
  `process_read_versions` today; one permission covers every process). A process they may not read
  is **absent**, never a row of zeros. If the permission source cannot answer, the answer is no.
* **Stuck Runs.** A Run is listed only to its initiator, to the assignee of the stuck node (asked
  of the task inbox when one is hosted; no inbox, or an inbox that errors, means no assignee), or
  to a holder of `process_supervise`. An entry shows ids, the node, a reason code
  (`waiting_for_task`, `timer`, `integration_retry`, `running`) and the owner, nothing else.

## Settings

`[services.process_insight]`:

| Key | Default | Range |
| --- | --- | --- |
| `enabled` | `true` | A disabled module registers nothing |
| `batch_size` | `500` | `1..=1000` records per batch |
| `fold_interval_secs` | `10` | `1..=300` |
| `stuck_after_secs` | `3600` | `>= 1`: how long an attempt may stay in one state |

Unknown keys are refused. The module is inert when the process module is absent or disabled, and
it is optional: a broken insight degrades readiness, it never stops documents.

## Metrics

`spot_bpm_insight_folded_total`, `spot_bpm_insight_skipped_total{process}`,
`spot_bpm_insight_open_attempts{process}` and `spot_bpm_insight_stuck_runs{process}`. The only
label is the process name, and at most 50 distinct names are ever used (every other name is
`_other`). No Run id, user id, node id or error text is a label; ids appear in logs only.

# Process insight reports and the scheduled fold

`services/process_insight_reports.rs` (`ORDER` 1195, after the feed) registers the insight reports
with the report engine and schedules the fold. It needs the process-insight port (module
`process_insight`), the report-source port (module `report`) and the internal-schedule port (module
`jobs`); a module whose port is absent is skipped quietly, so a site that turns the report service
off simply has no insight reports.

## The reports

The report engine serves them like any report: `report_list`, `report_describe`, `report_run`
(there is no operation of their own).

| Report | What it shows | Where the numbers come from |
| --- | --- | --- |
| `bpm_cycle_time_by_step` | per process version and node: completed attempts, average cycle (ms), longest wait (ms), total work (ms), longest bottleneck first | the insight snapshot, one row per step |
| `bpm_stuck_queue` | the Runs not progressing: Run id, process, node, owner, reason code (in `state`) | `ProcessInsightPort::stuck`, per caller |
| `bpm_throughput_by_week` | Runs completed per process version and ISO week | a `ThroughputFeed`; **not registered while no module provides one** |

Titles and the Run column heading come from the run label pair (ADR-080): `Stuck Requests` and
`Request` by default; set `run_label_singular` and `run_label_plural` to change them. No title or
heading says "Case".

Limits of the snapshot, so nobody reads a column for more than it is: insight keeps one aggregate
per step, not one row per attempt. The reports' counts, sums and averages are exact integers (whole
milliseconds; the average is the report engine's truncated division). *Longest wait* is empty
because no longest wait is kept per step. The stuck queue shows ids, the node, a reason code and
the owner only: `node_kind` and `started_at` are empty there.

## Who sees what

The report's `roles` decide only who sees that it exists (`roles` below). The data is then decided
by the process-insight port for that caller:

* a process the caller may not read is **absent** from the cycle-time and throughput reports, never
  a row of zeros; a caller who may read none sees an empty report, the same answer as a site
  without those processes;
* the stuck queue lists a Run only to its initiator, the assignee of the stuck node or a holder of
  `process_supervise`;
* a port that errors denies: the caller is answered `404 report.not_found` as for a report that
  does not exist, and the refusal is audited (`spot::audit::report`, `report source refused`, with
  the site, user, report and reason code).

## The scheduled fold

One internal schedule, `process_insight:fold`, calls `fold_once` every `fold_interval_secs` of
`[services.process_insight]` (10 s by default; intervals over a minute round down to whole
minutes). Missed runs coalesce into one: the fold reads from its checkpoint, so one run catches up
everything. It runs under the scheduler's leader lease, once per tick across nodes, as the service
`process_insight` (named in the audit line `spot::audit::internal_schedule`, never as a caller); a
restart resumes from the durable checkpoint. A lost insight lease is not a failure (another node
folds); any other error retries by its code.

## Settings

`[services.process_insight_reports]`:

| Key | Default | Meaning |
| --- | --- | --- |
| `enabled` | `true` | Off registers neither the reports nor the schedule |
| `roles` | `Tenant Administrator`, `Process Supervisor`, `Process Reader` | The roles that see the reports exist |
| `run_label_singular` | `Request` | The singular label of a Run |
| `run_label_plural` | `Requests` | The plural label of a Run |

Unknown keys are refused.
