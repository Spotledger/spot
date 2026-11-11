# L7 engine suite: CI

`run.sh` is the entry point. It runs:

1. **Per commit** — `cargo test -p spot-l7-engine`: the smoke suite on the in-memory fake, and
   the same tests on PostgreSQL when `SPOT_TEST_PG_URL` is set (they print `SKIPPED` otherwise).
   This is exactly what a developer runs.
2. **`standard` profile** — `cargo test -p spot-l7-engine --test profiles -- --ignored` with
   `SPOT_REQUIRE_PG=1` and `SPOT_L7_PROFILE=standard`: the larger workload on the fake (bounded
   by `SPOT_L7_FAKE_OPERATIONS`, default 20,000, because the fake's cost grows with its data) and
   on PostgreSQL. `SPOT_REQUIRE_PG=1` makes a missing server a failure, so a release run cannot
   pass by skipping.

JSON reports are written under `SPOT_L7_ARTIFACTS` (default `artifacts/l7-engine`) as
`engine-fake-<profile>.json`, `engine-pg-<profile>.json` and `naming-pg-<profile>.json`.

## What runs when

| When | What | How |
| --- | --- | --- |
| every commit | `smoke` on the fake (8 workers, 2,000 operations) | `cargo test -p spot-l7-engine` |
| nightly | `nightly` (200 workers, 1,000,000 operations) on PostgreSQL | `SPOT_L7_PROFILE=nightly SPOT_TEST_PG_URL=... SPOT_REQUIRE_PG=1 cargo test -p spot-l7-engine --test profiles -- --ignored` |
| before a release | `nightly` **three times with different seeds** | the command above, with `SPOT_L7_SEED` or by editing the test seeds, three times |

The seed is printed in every report and violation; pass it again to replay the schedule.
