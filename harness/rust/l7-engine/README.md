# spot-l7-engine (WP-TST-07.T2)

L7 of the test layers, **framework cut, part 2**: the storage-level driver of
`spot-l7-invariants` (WP-TST-07.T1) run through the **document engine** and the **job engine**.
Part 1 proved the storage promises under parallel load; this part proves the platform's own
promises one level up, on the sample app's DocTypes (WP-DX-05.T1) with no business scenario.

## What it proves

The invariants are T1's `Invariant`s, registered on T1's `InvariantRegistry` and checked through
T1's `CheckContext`; the engine facts they need beyond the store travel in each invariant's own
state.

| Invariant | Promise |
| --- | --- |
| `naming-gapless` | every series the naming workload drove is dense `1..=n`, the next allocation is `n+1`, and each committed allocation left its marker row in the same transaction |
| `naming-unique` | no two live committed documents share a human-readable name |
| `lifecycle-state_machine` | stored versions and states equal the oracle's, and every committed transition is reachable in the ADR-42 machine |
| `events-per_operation` | every committed write produced exactly one lifecycle event, once, with the right subject, version, actor and order; none for a refusal |
| `audit-no_secret_in_events` | no planted protected value appears in any serialised outbox payload |
| `links-checked` | every link a committed document holds points at a document that existed |
| `perm-no_effect_on_denial` | every permission refusal left no effect, and the engine's denial log matches |
| `jobs-exactly_one_effect_per_idempotency_key` | no key took effect twice; every succeeded job has one effect |
| `jobs-no_lost_job` | every enqueued job reached a terminal state |
| `jobs-retry_bounds` | no job exceeded its maximum attempts; a dead job spent exactly its bound |

T1's catalog invariants (`child-integrity`, `link-integrity`, `unique-keys`) are registered
alongside them.

## The world

Built entirely from the sample app: its six DocType definitions, its three-role permission model
and its `confidential` field group, plus the two platform fixtures (`user`, `file`) its links
name and one whitelisted method (`l7:touch`). Six principals cover the permission surface: the
`Project Manager`, the `Team Member`, the `Viewer`, `ghost` (no role), the `Tenant
Administrator` and the platform `system`.

## Run it

```bash
cargo test -p spot-l7-engine                              # smoke on the fake
SPOT_TEST_PG_URL=postgres://spot:spot@localhost:5432/postgres \
  cargo test -p spot-l7-engine                            # also on PostgreSQL
cargo test -p spot-l7-engine -- --ignored                 # the standard/nightly profiles
./harness/rust/l7-engine/ci/run.sh                        # the CI entry
```

`SPOT_L7_PROFILE=smoke|standard|nightly` selects the profile; `SPOT_L7_SEED` selects the seed;
`SPOT_L7_REPORT_DIR` writes the JSON reports. See `ci/README.md` for what runs per commit,
nightly and before a release.

## Honest limits

- Replaying a seed repeats the operation **schedule**; the interleaving of workers on real
  threads is not deterministic.
- The document engine has no naming integration yet (WP-MET-03 wires the naming plan into the
  lifecycle), so `naming-gapless` is proven by a dedicated naming workload that drives
  `spot-naming` at the transaction level with the same series keys the app's DocTypes name.
- The in-memory fake keeps its state private, so most engine corruptions are applied to the run's
  oracle (what a weakened engine would make the store look like); PostgreSQL additionally gets
  raw-SQL corruptions. The `enqueue_orphan_event` corruption touches the store itself, through
  the contract, on both backends.
- The job workload uses the in-memory store; the PostgreSQL job store joins in a later task.
- A worker "kill" is modelled by abandoning a claim: the lease expires and `reap_expired`
  returns the job to `Retrying`, exactly as the real worker's lease handling does.
