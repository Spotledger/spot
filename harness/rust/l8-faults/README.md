# spot-l8-faults (WP-TST-08.T2)

L8 fault injection, framework cut. L8 promises *recovery after crash, partition, power loss*; the
platform's central safety claims all depend on it. This crate attacks those claims on a real
PostgreSQL that the harness starts, kills and restarts itself, using the L7 invariants and the
isolation checker as the judges.

## What it covers

- **Crash during load.** [`ScratchCluster`] runs its own disposable cluster; `kill9` `SIGKILL`s
  the postmaster *and its children*, `restart_after_kill` waits for crash recovery and records
  how long it took. The scenario kills the server mid-run, lets the pool reconnect, and then
  checks every acknowledged commit is present, every unacknowledged operation is wholly present
  or wholly absent, sequences have no gap or duplicate, and the outbox and change feed are exact.
- **Repeated crashes.** Five kills at seeded random moments in one run.
- **Killed connections.** `pg_terminate_backend` on the workload's backends mid-run; the pool
  recovers and no lock is left stuck.
- **Commit-ack loss.** [`FaultProxy`] resets the connection right after the commit reaches the
  server but before the client sees the reply; the driver gets `indeterminate` and resolves each
  by reading its marker event back from the outbox.
- **Partition.** The proxy blackholes the server's replies for 5 s, then heals; statement and
  lock timeouts fire as declared, and the driver's history is consistent with the isolation
  checker before and after.
- **Slow storage.** 500 ms of added latency for a short window: throughput degrades, correctness
  holds, and the lock-wait path times out cleanly.
- **Job engine.** Killed workers, expired and re-taken leases, idempotency keys that take effect
  exactly once and bounded retries, judged by WP-TST-07.T2's job invariants.

Every scenario runs the L7 framework invariants **and** the [`DurabilityAudit`], which partitions
the operations into acknowledged / refused / failed / indeterminate-resolved-committed /
indeterminate-resolved-absent and asserts the durability rules.

## What it does not cover

- **Power loss.** A `SIGKILL` of the postmaster is not a power cut: the OS page cache survives,
  so a `fsync = off` server does not lose acknowledged commits on a process kill the way it would
  on a real power loss. Power loss, disk faults and torn pages belong to **WP-PRD-05** and a
  physical rig. The one `#[ignore]`d test that starts a server with `fsync = off` and
  `synchronous_commit = off` exists only to show that the audit *can* fail; it is never part of
  the green suite.
- **The PostgreSQL job store.** The job-engine scenario runs over the engine's in-memory store
  (the PostgreSQL store is a later task), so it proves the engine's lease, dedup and retry
  behaviour, not a PostgreSQL job table surviving a server kill.
- **The engine-level workload.** The fault scenarios run the storage-level `LoadDriver` of
  WP-TST-07.T1, the driver whose history and oracle carry the per-attempt markers the durability
  audit needs. The engine-level (`l7-engine`) workload keeps its own invariants in WP-TST-07.T2.
- **Windows.** Killing a cluster is Linux-only; elsewhere the crash methods compile away and the
  caller prints `SKIP`. The Windows crash work belongs to WP-PRD-05.

## Run it

```bash
# The smoke pair, in cargo test (skipped with a notice when PostgreSQL is absent):
SPOT_PG_BIN=/usr/lib/postgresql/18/bin cargo test -p spot-l8-faults

# The full set (standard profile):
SPOT_PG_BIN=/usr/lib/postgresql/18/bin cargo test -p spot-l8-faults -- --ignored

# One JSON report and the fault timeline per scenario:
harness/rust/l8-faults/ci/run.sh
```

Without `SPOT_PG_BIN` (or with PostgreSQL binaries absent) the scenarios print `SKIPPED` and pass;
`SPOT_REQUIRE_PG=1` makes the absence a failure instead, so CI can never pass by skipping.

## Reproducing a failure

Every fault plan is a pure function of its seed and the fault timeline is recorded with times. To
replay a failure, run the same scenario with the same seed:

```bash
SPOT_L8_FAULTS_PROFILE=standard SPOT_PG_BIN=/usr/lib/postgresql/18/bin \
  cargo test -p spot-l8-faults -- --ignored --nocapture
```

The report's `fault_timeline` names every fault that fired, on which connection, in which
direction and at what microsecond, so a run can be compared against the failing one.

## Honesty

The suite's honesty is itself tested (`tests/honesty.rs`, offline on the in-memory fake):

- a [`LossyClient`](src/honesty.rs) that reports success for a commit it never sent is caught by
  the audit's `acknowledged_effect_absent`;
- a driver that treats `indeterminate` as success is caught the same way.

A suite that passed on those would prove nothing.
