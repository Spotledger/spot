Part of [Running Spot](../running-spot.md).

# The task inbox

A Decision node of a Process (`crates/framework/spot-bpm-tasks`) opens a durable task and parks
the Run on it. This module (`crates/framework/spot-cli/src/services/process_tasks.rs`, start
order `1120`, after the process engine and the Document node module) hosts that inbox for each
site: the task store in PostgreSQL, the permission checks that decide who may act, SLA timers,
notices, and the seven inbox operations of native API 0.11.0 (`task_list`, `task_get`,
`task_claim`, `task_release`, `task_complete`, `task_delegate`, `task_reassign`).

## Who may act

- A **role's members** are the active role assignments of that role held by an **active user**.
  A user's **manager** is the `manager` link of their user record. An inactive user is never a
  role member, a manager, an approver, or the target of a delegation or a reassignment.
- A task grants **no access to the document**. The approver reads the document through the
  ordinary document operations under the ordinary permission rules (or the role grant the
  process declares). The task names the document's DocType and id only. An approver without read
  on the document still gets the task, and a `404` on the document.
- A user who is neither an assignee, nor the assignee's manager, nor allowed to read the
  guarded document gets `404` for `task_get`, `task_claim` and `task_complete`: the same answer
  as an unknown task, so the inbox is never an existence oracle.
- If the permission engine is unavailable, or the document's facts cannot be read, the answer is
  "no": the request is denied, never allowed.
- **Segregation of duties.** A node that declares `distinct_from_creator` refuses the creator of
  the guarded document (`task.segregation`, task unchanged); a node with a `duty` or
  `distinct_from_creator` also refuses a user who already decided another task of the same Run.

## SLA

Reminders, escalations and the node's final action fire from timers kept by the lease holder of
the site. A timer is only an index. The open task and its event trail are the truth: after a
restart or a change of leader the holder re-arms every open task from the store, and does so
again every few minutes.

**Auto-approve** exists only when the node declares `sla_auto_approve = true` and has no
`distinct_from_creator` and no `duty`. The actor is `system:sla`, the event is flagged `auto`,
it is never counted as a human approver (`human_approvers`), and each one adds to the
`spot_bpm_task_auto_approved_total` counter and sends the supervisor a notice. A node without
the flag cannot even open a task with an auto-approve final action.

Without a calendar, SLA offsets run on **wall time** and readiness reports
`process_tasks_calendar` as degraded to say so.

## Notices

Assignment, reminder, escalation, delegation, completion and auto-approval notices go to the
notice feed or the mail service when a module provides one (`TaskNoticeSink`). Delivery is best
effort: a notice that fails (or panics, or has no sink) is counted
(`spot_bpm_task_notices_failed_total`, `spot_bpm_task_notices_dropped_total`) and never fails the
action. A notice carries ids and the task's own title, never a document value or a comment.

## `complete` is idempotent

Send an `Idempotency-Key`. A repeat of the same request returns the first outcome with
`replayed: true` and does not resume the Run again. The same key with a different request is
`api.idempotency_conflict`; a different key on a task that is already completed is
`task.not_active`. A stale `If-Match` is `document.stale_version` with the task's current
version.

## Configuration

```toml
[services.process_tasks]
enabled = true        # default; false makes the module inert (no table, no port, no route)
tick_secs = 15        # how often SLA timers are checked; 5 to 300

[services.process_tasks.calendar]   # optional; without it SLAs count wall time
days = ["mon", "tue", "wed", "thu", "fri"]
start = "09:00"
end = "17:00"
utc_offset_minutes = 300
```

An unknown key, a `tick_secs` out of range or a calendar that does not parse stops the site
from starting, naming the setting. The module needs a PostgreSQL pool of at least three
connections (`postgres.pool_max_size`) and creates its own tables on first start.

## Audit, logs and metrics

Every call is logged on the target `spot::audit::task` with who, which task, which action, the
outcome class tried and the result code, **refusals included**. A line never carries a document
value, a comment, a form value or a token. Counters carry no label: no task id, user or process
name reaches a metric.

## Failure behaviour

The module is optional. A broken task inbox degrades readiness and the inbox operations answer
`503 server.unavailable`; the site keeps serving documents.
