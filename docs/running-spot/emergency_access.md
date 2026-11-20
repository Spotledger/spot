Part of [Running Spot](../running-spot.md).

# Emergency access (`[services.emergency_access]`)

The firefighter pattern (ADR-070 section 9): a locked-out business can still get an elevated role, for a
bounded time, on a reason, with a second person's approval, a recorded session and a review afterwards. The
whole life of a grant is a pure state machine in `spot-iam-questions` (`EmergencyGrant`); this module makes it
durable on a site. It is **off by default**.

## The flow

1. **Request.** A user asks for an elevated role *for themselves*, with a reason, an approver and a number of
   minutes. The request is refused (and the refusal audited, never the text the user typed) when the duration is
   over the policy, the role is not on `emergency_roles`, the approver is not one of `approvers`, or the approver
   is the user.
2. **Approval** is a Process on the process engine, started once per grant (the grant id is the business key, so a
   retry never starts a second Case). The named approver (or, after the SLA timer, the escalation target)
   approves or rejects with `spot access emergency approve|reject` or through the in-process port
   `EmergencyAccess`. The user can never approve their own request. Approving twice grants once.
3. **Activation** runs the segregation-of-duties gate (`GrantGuard::recheck_approved`, the one every grant goes
   through). A conflict refuses the grant; the policy may allow an override (`allow_sod_override`), and then the
   approver must submit a reason and a mitigating control, which the gate records. The assignment has source
   `emergency`, `valid_from` and `valid_to`, and is written with its `identity` topic event in one transaction. If
   the journal, the gate or the process host is missing, nothing is granted and readiness says which.
4. **The session.** Every action the user takes under the grant is recorded with the grant id
   (`EmergencyAccess::record_action`, called by the request path). The actions are kept in the grant record.
5. **The end.** At `valid_to` the sweep ends the record and the assignment and archives the recording through the
   break-glass archive (sealed, written once, digest on the audit chain); with no archive the grant's recording
   status is `unavailable` and the actions stay in the record and the audit trail. The permission engine refuses an
   assignment after its `valid_to` by itself on every request: the job is cleanup, not the control. A user, the
   approver or a controller may end a session early (`spot access emergency end`).
6. **The review.** A controller who is not the user (the first of `controllers` who is not) must confirm or flag
   the session. When that controller is also the approver and `second_check` is set, a second, distinct person must
   confirm too. An unreviewed task escalates to `escalate_to` at `review_within_hours`, then every
   `escalate_after_hours`, at most `max_escalations` times; each escalation is audited.

## Settings

| Setting | Default | Meaning |
| --- | --- | --- |
| `enabled` | false | Off: nothing is provided. |
| `emergency_roles` | none | The roles that may be requested. Required (never "any role"). |
| `approvers` | none | User ids or `role:<name>` who may be named approver. Required. |
| `controllers` | none | Who reviews ended sessions. Required. |
| `escalate_to` | none | Where an unanswered approval or review escalates: a user id or `role:<name>`. Required; blank or with a control character refuses the start. |
| `max_duration_minutes` | 60 | 1 to 1440. |
| `approval_sla_minutes` | 60 | The approver's time. |
| `review_within_hours` | 24 | 1 to 720. |
| `escalate_after_hours` | 24 | Between escalations. |
| `max_escalations` | 3 | 1 to 10. |
| `second_check` | true | A controller who is the approver needs a second check. |
| `allow_sod_override` | false | The approver may override a conflict with a reason and a control. |
| `sweep_seconds` | 5 | How often the sweep runs (1 to 60). |

A setting that is missing or out of range stops the site with `emergency_access_host.settings_invalid`.

## Commands

`spot access emergency request|list|status|approve|reject|end|review --site SITE --as USER [--json]`. A command
runs on the host against the site's database as the named operator, goes through the same code as the server and
is refused with the same stable codes. What needs the running server (starting the approval Case, the gate, the
archive) is done by the server's sweep within seconds. Exit codes: 0 done, 1 usage, 2 refused, 3 failed.

## What the reviewer sees

The grant id, the user, the role, the reason, the approver, when the session ended, the number of actions, the
recording status and its digest, and the review state. The actions themselves are in the recording the
customer-side break-glass view lists. The "what changed" question lists every state change in order: requested,
approval started, approved, the assignment created, ended (with its cause), the recording, the review opened and
confirmed or flagged, each escalation.

## Storage and restarts

One table, `spot_emergency_access (tenant, key, value)`, one record per grant, written with a compare-and-swap.
A record carries a format version, its site and id and a digest; one that does not match is refused loudly and
never repaired (`state_invalid`, `state_version`). The digest catches an accident or an edit that forgot it; it is
not a MAC, and the hash-chained access-change journal is the tamper evidence.

On a **crash mid-session** nothing is lost and nothing is doubled: the activation instant is written into the
record before the assignment, the assignment's record id is derived from the grant id (a second write is a
no-op), the journal event is written before the effect (a duplicate is possible, a lost one is not), and the
recording is rebuilt from the actions in the record, so an ended session whose archive did not finish is archived
by the next sweep. A restart finds the active grant, the pending approval and the open review in the table and
carries on; the escalation timers live in memory and are armed again at every sweep.

## Recording what the session does

The door that honours an emergency assignment calls the port `EmergencyActions::record(user, action,
target_type, target_id)` for every action. The recorder finds the user's active grant and records the
action under it: the action's name and `type:id`, never a payload. A user with no active grant records
nothing. A recording that fails goes to a retry queue (1,024 entries, the oldest dropped and counted
first) that every sweep pass drains.

The review timer and the end of the session are stored with the grant (due time, state, `valid_to`),
not only in memory: the sweep runs when the service starts, arms every pending review again, escalates
the overdue ones and ends the grants past their time, so a restart in the middle of a grant loses
neither the review nor the revocation.
