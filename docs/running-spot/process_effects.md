Part of [Running Spot](../running-spot.md).

# Request effects, undo and commit points

A request (a Run of a Process) changes records and can cause things outside the system: a mail, a
bank call, a webhook, a message to another organisation. `spot-bpm-effects`, `spot-effect-hold`
and `spot-bpm-commit` describe what a request did, how to take it back and when it becomes final.
This module (`crates/framework/spot-cli/src/services/process_effects.rs`, start order `1170`)
hosts them for each site.

## Configuration

```toml
[services.process_effects]
tick_secs = 30             # how often the lease holder resumes interrupted undos (5 to 3600)
stale_secs = 120           # a running undo untouched this long is taken over (30 to 3600)
max_resume_per_tick = 10   # 1 to 100
supervisor_roles = ["Process Supervisor", "System Manager"]
```

Unknown keys and out-of-range values refuse the site's start, naming the key and never its value.

## The request record file

Every operation that leaves the system writes a **receipt** to `spot_bpm_eff_log`, keyed by
`(request, node, attempt, operation)`, before it is recorded in memory. The table is
**append-only** (the database refuses an `UPDATE` or a `DELETE`) and every row carries the site. A
receipt holds ids, counts and digests only: no secret, no payload body, no document value. A
retried write changes nothing. After a restart the file is rebuilt by replaying the log.

## Undo and cancel

Cancelling a request, or taking an undo edge, compensates what the request did, in reverse
dependency order, **as the person who acts** (never as the system, never widened):

1. Who may start it: the person who started the request, or a holder of one of `supervisor_roles`.
   Anybody else (and a permission source that cannot answer) gets exactly the answer an unknown
   request gets, and the refusal is audited.
2. The effects the request still **holds** (they never left) are discarded first, and that is
   recorded.
3. Each reversal runs through the record type's own lifecycle, idempotent per receipt: its request
   key `undo:<request>:<entry>:<reversal>` is written to the journal (`spot_bpm_eff_done`) as soon as
   the call completes. After a restart the journal returns what is already done instead of reversing
   again.
4. The business graph decides whether a record has dependants. A graph that cannot answer **denies
   the undo** (`bpm_effects.graph_unavailable`). With no graph hosted, only the links inside the
   request's own file count, and readiness says so (`process_effects_graph`).

How an undo ends (`spot_bpm_eff_undo.state`):

| State | Meaning |
| --- | --- |
| `done` | everything after the target is reversed |
| `needs_attention` | a reversal was refused (a dependant, a closed period, an irreversible effect, a partner that refuses). The sequence stopped, **nothing was skipped or forced**, and a supervisor notice was raised. A person picks one of the offered alternatives or abandons; asking again stops at the same place |
| `interrupted` | a port was unavailable. The lease holder resumes it on the next tick, as the person who started it |
| `abandoned` | a person gave up; what was reversed stays reversed |

A refusal never carries a message from a peer: only a stable code.

## Effect hold and commit points

External effects of the nodes before a commit point are **held** under the request and released at
the commit point, in their original order and each with its original key. A request cancelled or
undone first discards them. A process with no commit point holds nothing. A class that is never held
(a fiscal transmission) leaves at once and its node is a point of no return.

A site with no dispatcher registered refuses every dispatch: a held effect stays held. A live
migration hold wins: a commit point reached during one releases nothing and reports it.

## Partner

Compensation messages to another organisation go through the conversations module when it provides
a partner port. Without it they are refused with `bpm_effects.cancellation_failed` and the undo
stays resumable.

## The tables

| Table | Holds |
| --- | --- |
| `spot_bpm_eff_log` | the request record file (append-only) |
| `spot_bpm_eff_request` | who started each request |
| `spot_bpm_eff_done` | the reversal journal |
| `spot_bpm_eff_undo` | the state of each undo |
| `spot_bpm_eff_audit` | every undo, decision, refusal and hold transition: who, what, which rule |

The hold's own ledger is `spot-effect-hold-pg`'s tables. All are created if absent when the site
starts.

## What an operator sees

- `process_effects` in readiness: degraded when the tables are not usable.
- `process_effects_graph`: degraded when no business graph is hosted.
- The counter `spot_bpm_effects_needs_attention_total` and one warning line per request that needs
  attention (the request id, never a value).

## Known limits

- The hold's region plan, the dispatcher, the declarations registry and the grants an undo of a
  governed record needs are provided by other modules. Until they provide them, nothing is held, a
  release stops, an effect with no declaration is a point of no return, and the reversal of a
  governed record is refused by governance.
