# spot-l7-invariants (WP-TST-07.T1)

L7 of the test layers, storage level: invariants the platform promises after every run, a seeded
multi-worker workload driver, and the **seeded-corruption proofs** that show the invariants fail
when, and only when, their promise is broken. Part 2 (WP-TST-07.T2) runs the same driver through
the document engine.

## Run it

```bash
cargo test -p spot-l7-invariants                          # smoke: 8 workers, 2,000 operations
SPOT_TEST_PG_URL=postgres://spot:spot@localhost:5432/postgres \
  cargo test -p spot-l7-invariants                        # also on PostgreSQL + corruption proofs
cargo test -p spot-l7-invariants -- --ignored             # standard and nightly profiles
cargo run -p spot-l7-invariants --bin spot-l7 -- run --backend pg --profile standard \
  --seed 7 --report out.json --history out.jsonl
```

Without `SPOT_TEST_PG_URL` the PostgreSQL tests print `SKIPPED`; with `SPOT_REQUIRE_PG=1` they
fail instead. Profiles (`SPOT_L7_PROFILE` or `--profile`): `smoke` 8 workers / 2,000 operations,
`standard` 32 / 50,000, `nightly` 200 / 1,000,000.

## Framework invariants

`seq-gapless`, `doc-version_continuity`, `outbox-complete`, `child-integrity`, `link-integrity`,
`unique-keys`, `lost_update-free`, `doc-count_matches_oracle`, `read-snapshot_consistent`.
Each states exactly what it proves in its `describe()`. `child-integrity`, `link-integrity` and
`unique-keys` read the catalog, so they cover any app; the others compare the store with the
oracle of the platform workload. Apps add theirs with `InvariantRegistry::register`.

## Honest limits

- Replaying a seed repeats the operation **schedule**; the interleaving of workers on real threads
  is not deterministic.
- Checks assume a quiescent store (the workload has stopped). Table-derived checks read one
  repeatable-read snapshot; the feed checks read the feeds afterwards.
- The corruptors are PostgreSQL-only (the fake's state is private); on the fake the same
  invariants are proven by the weakened backends of `faulty` and by `burn_sequence_number`.
- The history format (`history.rs`, version 1) is public: WP-TST-08 reads it.
- The in-memory fake copies its state on every commit, so its cost grows with the data: `smoke`
  takes seconds, 10,000 operations about a minute, and `standard` and `nightly` are meant for
  PostgreSQL (10,000 operations on PostgreSQL take about 40 s with 32 workers).
- Under `RepeatableRead` and `Serializable` the hot counters and series make PostgreSQL abort
  many transactions; an operation that runs out of its 8 attempts is recorded as `Failed`
  (`exhausted`), never as a success, and the invariants still hold.
