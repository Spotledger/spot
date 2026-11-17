# sync_loops

The periodic sync loops, hosted. Each step has its own interval, examines a bounded number of
subjects per run (`max_items`, default 200), and an error or panic in one never stops the others.
Conditions are raised through the conditions service (which keeps its debounce and flap guard);
where the catalogue ladder says "person" the condition stays open with its remedy.

| Step | Where | Condition (ADR-067 id) | Responds on its own |
| --- | --- | --- | --- |
| obligations | hub | `OBLIGATION_DUE_SOON`, `OBLIGATION_OVERDUE`, `OBLIGATION_RED`; the ladder notifies the contact before it suspends the node | yes |
| liveness | hub | `NODE_SILENT`, `LAG_HIGH` | no |
| freshness and stamps | edge | `FRESHNESS_STALE`; documents posted while a class is stale carry the stale stamp | stamps |
| reconcile | edge | `DIVERGENCE` | no |
| healing | edge | `HEAL_BUDGET_EXHAUSTED` (red) once the attempt budget (3) is spent | mends |
| closure repair | edge | `MISSING_DEP` for repair-list items that stay stuck | pulls |
| quarantine | edge | `QUARANTINED` for open items that age (it retries and closes nothing: the queue owns that) | no |
| certificate | hub | `CERT_EXPIRING` (amber inside 30 days, red inside 3 or past) | no |
| series | both | none: samples numbers into the health view | no |

Every automatic response (a stamp, a mend, a repair pull, a notify or a suspension) writes one
`sys_sync_decision` (step, action word, subject identity, time, ok). Nothing in it is a record's
content, a key, a code or a payload.

## Two homes

The edge runs `sync_loops` (`ROLES = ["edge"]`). The hub's home is the `sync_hub_loops` row of the
role table; its one-file module calls `sync_loops::register_hub`.

## A step is wired when its source is provided

Each step reads a source the module that owns the store provides (the ledger, the registry, the
repair list, the freshness tracker). A step with no source is **left out** and listed on the
readiness line (`not wired: freshness, healing`): nothing is faked, and `calm` means the wired steps
found nothing. The one exception is reconcile, which is always added: the wire has no digest request
kind yet (the kind it needs is named `digest` in `spot_sync_loops::reconcile`), so without a provider
it fails every run with `not available` and raises an information-level `RECONCILE_UNAVAILABLE`; it
never reports a clean result it did not compute.

## Readiness

`sync_loops: calm` or `sync_loops: 2 amber, 0 red`, followed by `; reconcile: not available` for a
step that failed in the last tick and `; not wired: ...` for the steps left out. The line is always
ok: open conditions are information, not a failure.

```toml
[services.sync_loops]
max_items = 200            # 1 to 10000
reconcile_scopes = ["branch_01"]
```
