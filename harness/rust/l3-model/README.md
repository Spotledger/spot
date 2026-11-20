# spot-l3-model

Model-based lifecycle test (WP-TST-10.T5). A seeded generator makes 20 to 200 lifecycle calls over
a few Time Entries and Tasks; a pure reference model says before each call what must happen; the
runner makes the call through the real `DocumentService` and compares the result **and a full
read-back** of every document; a shrinker reduces a failure to a minimal sequence.

## The model's rules

| Rule | Statement | Source |
| --- | --- | --- |
| R1 | A document starts at version 1; each committed write adds 1; a refusal changes nothing | ADR-42; engine optimistic version check |
| R2 | Refusal order: not found, stale version, state, permission | `engine/ops.rs`; the L3 document cases |
| R3 | Draft, Posted, Cancelled: post only a draft, cancel and correct only a posted one, delete only a draft | ADR-42 |
| R4 | A draft accepts an update; a posted one refuses a fixed field; a cancelled one is invalid_state; an update always commits a version | ADR-42; the engine writes on every accepted update (verify on the first run) |
| R5 | `correct` cancels the original (+1) and creates one draft (v1) with `amended_from`; two lifecycle events | ADR-42 |
| R6 | A Time Entry links a live Task; a Task a Time Entry links cannot be deleted, a cancelled entry included | ADR-51 link checks |
| R7 | Rename keeps the id, changes the title, +1 | ADR-42 |
| R8 | A principal without the right is refused when the call would otherwise commit | the permission model (WP-DOC-06) |

The world has no app controllers, so there is no roll-up on post or cancel. Documents are
**slots**, so dropping calls keeps a sequence meaningful (a call on an uncreated slot is skipped).

## Running

```text
cargo test -p spot-l3-model                                   # 300 sequences on the fake, 60 on PostgreSQL when configured
SPOT_L3_MODEL_SEQUENCES=2000 SPOT_L3_MODEL_SEED=42 cargo test -p spot-l3-model
SPOT_TEST_PG_URL=postgres://... SPOT_REQUIRE_PG=1 cargo test -p spot-l3-model --test postgres
```

The default seed is fixed (`20261113`), so CI is deterministic. A nightly run uses a random seed
that the job prints; replay with `SPOT_L3_MODEL_SEED=<printed> SPOT_L3_MODEL_SEQUENCES=<n>`.
A failure prints the shrunk sequence as a Rust body (`to_rust`) plus the seed.

## Adding a rule

Model first (`model.rs`, with a unit test), then the generator weight (`gen.rs`), then the runner
dispatch if the call is new. A rule where the engine is wrong is recorded as an `#[ignore]`d test
named for the defect, with the shrunk sequence and the owning task id, never by bending the model.
