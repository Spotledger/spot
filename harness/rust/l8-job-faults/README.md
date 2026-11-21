# spot-l8-job-faults

Fault injection for the whole job path on PostgreSQL (WP-TST-08.T7): a document operation commits
and enqueues a job in its own transaction (outbox), the intake moves it to `PgJobStore`, 6 workers
run a handler whose effect is one row in `effects` (`ON CONFLICT DO NOTHING`, own transaction), and
the database is killed at the worst moments.

## Scenarios

`kill_before_intake`, `kill_between_intake_and_claim`, `kill_while_running` (handlers sleep 50 ms
before their effect), `kill_during_effect_commit` (the proxy cuts the handler's commit: the effect
is committed, the completion is not), `repeated_kills`. Profiles: `smoke` runs `kill_while_running`
with 100 operations, `standard` all five with 300, `nightly` five seeds.

## Checks (`src/checks.rs`, pure and tested without PostgreSQL)

(a) every committed operation's job succeeded and has exactly one effect row; (b) nothing exists for
a refused or uncommitted operation; (c) no job is left pending, running or retrying; (d) attempts
are within `max_attempts`; (e) one job per distinct committed key (the intake did not double
insert). The leases are 2 s and the reap tick 500 ms.

## Truth

Each operation creates a Time Entry with a client-chosen id; after a lost reply the row's existence
is read back (no ledger needed). The job's idempotency key is that id.

## Run

```text
ci/run.sh
SPOT_L8_FAULTS_PROFILE=standard ci/run.sh
cargo test -p spot-l8-job-faults --test scenarios -- --ignored --nocapture   # the honesty demonstration
```

SIGKILL is not power loss (WP-PRD-05). Replay: the kill moment is seeded; the interleaving is not.
Nightly wiring is a separate lead change (the command above with the nightly profile).
