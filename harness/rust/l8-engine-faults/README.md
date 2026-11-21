# spot-l8-engine-faults

Engine-level fault injection (WP-TST-08.T6): the document engine under `kill -9` of a real
PostgreSQL and under lost commit replies, with command ids on every call, judged by the L7 engine
invariants (`spot-l7-engine`) and the engine durability audit in `src/audit.rs`.

## What is covered

The engine's promises: one lifecycle event per committed operation, a re-sent command applied once
(the command ledger, WP-DOC-04.T8), a posted document never half posted, a correction (cancel plus
amended draft) both or neither, and one committed-step job per committed post (WP-DOC-04.T4).
Not covered: power loss and torn pages (WP-PRD-05), the job engine itself over PostgreSQL
(WP-TST-08.T7).

## Classification of what the engine returns (`src/classify.rs`)

| Engine result | Class |
| --- | --- |
| `Ok` | committed |
| `Unavailable` (commit outcome unknown, connection lost, timeout) | indeterminate: re-send with the same command id |
| `StaleVersion`, `InvalidState`, `NotFound`, `Duplicate`, `Linked`, `Validation`, `MethodNotFound`, `UnknownDocType` | refused |
| `PermissionDenied` | denied |
| `Internal` | **unclassified: the run fails** |

A method call has no ledger row, so an indeterminate method call is settled by a read-back (its
worker is the only writer of its Task) and never re-sent when it may have committed.

## Scenarios (`src/scenarios.rs`)

`crash_during_engine_load`, `commit_reply_lost`, `crash_between_cancel_and_amend`,
`crash_with_committed_steps`, `repeated_crashes`. Profiles: `smoke` runs the first two (4 workers,
200 calls, 20 cuts), `standard` all five, `nightly` all five over five seeds.

```text
ci/run.sh                                            # smoke
SPOT_L8_FAULTS_PROFILE=standard ci/run.sh
SPOT_L8_FAULTS_PROFILE=nightly SPOT_PG_BIN=... cargo test -p spot-l8-engine-faults --test scenarios
```

Replay a failure: the plan of every worker is a pure function of the seed (printed in the report);
the fault timing is not, so a replay may need a few runs. Wiring into `nightly.yml` is a separate
lead change; the command is the nightly line above.

## The audit

Rules a to f and the two failures are listed in `src/audit.rs`. Honesty tests
(`tests/machinery.rs`): an engine without the ledger fails rule e, a decorator that answers
committed for a call it never sent fails rule a, and a workload that never re-sends reports
unresolved attempts.
