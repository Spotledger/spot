# l8-faults CI

`run.sh` runs the **standard** fault profile and writes one JSON report and one fault-timeline
file per scenario under `$SPOT_L8_FAULTS_OUT` (default `target/l8-faults`). It exits non-zero on
any violation.

```bash
SPOT_PG_BIN=/usr/lib/postgresql/18/bin harness/rust/l8-faults/ci/run.sh
```

The CI host has PostgreSQL 18 binaries at `/usr/lib/postgresql/18/bin` and no root (see
the CI runner notes); the script finds them through `SPOT_PG_BIN` or that default. Each
scenario starts its **own** disposable cluster through `spot-pg-supervisor`, so parallel jobs do
not collide and the shared `SPOT_TEST_PG_URL` server is never touched.

`SPOT_L8_FAULTS_PROFILE=nightly` repeats the scenarios at the larger size; the seeds are fixed in
the test, so a failure can be replayed with the same profile and seed.

Artefacts per scenario:

- `<name>.json`: the full [`FaultReport`](../src/report.rs) — outcome, classification table,
  recovery time, notes and the fault timeline.
- `<name>.timeline.txt`: the timeline alone, for a quick diff between a passing and a failing run.
