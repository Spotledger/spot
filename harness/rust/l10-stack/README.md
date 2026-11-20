# spot-l10-stack

L10 on the real stack (WP-TST-11.T2). `StackHost` implements `spot_l10_liveness::LivenessHost` over
`CustomiseService`, the document engine and a PostgreSQL scratch database behind `PgSchema` (the
online migration engine). `apply(change)` maps the L10 `Change` onto the service; `meta()` is the
live snapshot; the probes:

- **Plan**: the version the engine's swap listener last applied (the compiled layer the engine
  runs on) against the metadata version, and agreement on the touched field.
- **Schema**: a dry run of `PgSchema::plan` over the catalogue of the current metadata: an applied
  change leaves an **empty plan**; a refused destructive change leaves the live schema untouched.
- **View**: the effective metadata's DocType describes the touched field at the metadata version.

The comparisons are pure functions (`src/probes.rs`) with unit tests that need no database.
A probe never mutates state.

## Scenarios

`tests/stack.rs` runs the 16 scenarios of `spot_l10_liveness`; none may skip a layer. The two
`app_update_*` scenarios replace the standard definitions wholesale, which the service (base fixed
at build) cannot do: they stay failing and `#[ignore]`d with the reason; see the report.

```text
SPOT_TEST_PG_URL=postgres://... SPOT_REQUIRE_PG=1 cargo test -p spot-l10-stack
```

Per-change timing figures (apply, plan visible) are printed by the scenarios' own notes.

## Per-change budget

The 200-change scenario gates each change at `StackHost::per_change_budget`: **2 s**, not the
shared 500 ms `SEQUENTIAL_BUDGET` of the in-memory registry. A change on this stack plans, applies
(the migration engine plans again under its lock), backfills and swaps the engine; measured 500 to
570 ms per change on an idle host (about 160 ms planning, 350 ms applying, 40 ms converging). It
stays a gross liveness bound; real performance budgets belong to L9.
