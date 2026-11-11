# l3-http — the framework's behaviour over the real stack

`l3-documents` (WP-TST-10.T1) proves document behaviour on the app API stub. This crate proves
the same behaviour through the **real stack**: the product host that `spot serve` runs, real
HTTP over a loopback socket, real sessions, the real permission engine and a real PostgreSQL
database for a freshly created site with the platform and sample apps installed and seeded.
Nothing here is mocked: every case talks to a running host as a signed-in user.

```
harness/rust/l3-http/
├── src/fixture.rs   one run: database, root, site, seeded apps, users per role, host, teardown
├── src/client.rs    a typed HTTP client: sessions, error envelopes, pages, lifecycle actions
├── src/report.rs    case outcomes, a JUnit-style document, the blocked-behaviour list
├── src/cases/       the cases: one module per section (auth, documents, lifecycle, lists,
│                    permissions, edge) and one file per registered group (`export.rs`,
│                    `events.rs`, ...); `mod.rs` holds `CaseDef` and the legacy index
├── src/index.rs     the case index and the text of `CASES.md`
├── src/main.rs      `cargo run -p l3-http -- --write-cases` regenerates `CASES.md`
├── build.rs         finds the registered case files
├── CASES.md         generated: every case, what it pins, whether it is blocked
├── tests/l3.rs      the suite: one fixture, every case, one report
├── tests/parallel.rs the 200-session proof and its end-state invariants
└── README.md
```

## Run it in one command

From the repository root, on a machine with PostgreSQL (the binary is not needed: the fixture
starts the host through the library API, so there is no separate build step):

```bash
SPOT_TEST_PG_URL=postgres://spot:spot@127.0.0.1:5432/postgres \
SPOT_REQUIRE_PG=1 \
cargo test -p l3-http -- --nocapture
```

`SPOT_REQUIRE_PG=1` is what CI sets: without it a missing database makes the fixture print
`SKIPPED: set SPOT_TEST_PG_URL` and the suite passes vacuously, which is the one failure mode a
gate must not have. `tools/ci/e2e/run.sh` wraps the same command for CI, starts the repository's
throwaway PostgreSQL when the job does not export one, and writes the JUnit document and the
logs:

```bash
tools/ci/e2e/run.sh --quick     # core cases, the pull-request job
tools/ci/e2e/run.sh             # every case, nightly and before merging to main
```

On Windows: `tools\ci\e2e\run.ps1 -Quick` (the runner must export `SPOT_TEST_PG_URL`; see
the CI runner notes).

## What one run does

`Fixture::start()` (see the module docs in `src/fixture.rs`):

1. creates a scratch database with `spot_postgres::testing::TestDb` — its name carries a UUIDv7,
   the **run id**, and creating it drops databases an earlier run leaked;
2. writes a temp root with `common.toml` and a private copy of `apps/sample`, so a case may
   change a definition and reload without touching the repository;
3. creates the site host-wide (`spot_cli::new_site_of`) and runs `install-app sample --apply
   --seed`, so the demo projects, tasks, labels and draft time entries exist;
4. sets the Administrator's password through the operator API (the seeded user is `invited`, and
   the sample app's `owner` lookup only accepts an active user) and creates the users — a
   Project Manager, a Team Member, a Viewer, a role-less observer, a role-less account the
   lockout cases may lock, and an invited user — before the host starts, so no reload races the
   setup;
5. starts the host with `spot_cli::start_with` on `127.0.0.1:0`, waits for
   `/api/health/ready`, and signs the Administrator in;
6. creates the three sample roles as site records and assigns them.

Teardown is a drop guard: the host is stopped, the root removed and the database dropped even
when a case panics. Before the root goes, the run's log is copied to the artifacts directory
(`L3_ARTIFACTS`; default a `spot-l3-artifacts/server-logs/` directory under the system temp
directory, never the repository), so a failing run keeps it; `L3_KEEP=1` keeps the database and
the root instead. A start failure removes the root it created, and a root an earlier run left
behind is swept after an hour, like the databases.

Environment switches: `L3_QUICK=1` (core cases), `L3_KEEP=1` (keep the database and root),
`L3_JUNIT=<path>` (write the JUnit document), `L3_ARTIFACTS=<dir>` (where a failing run's log is
copied; CI sets it to `artifacts/e2e`, see `tools/ci/e2e/run.sh`), `L3_VERBOSE=1` (echo the
fixture's notes).

## Adding a case

A case is one behaviour, named like the L3 suite, returning a `CaseResult`:

```rust
/// A Viewer may not create a Task (permissions: `Viewer` holds `read` and `select` only).
async fn viewer_cannot_create_a_task(env: &Fixture) -> CaseResult {
    let (project, _, _) = env.project("Viewer denied").await?;
    let viewer = env.sign_in(VIEWER).await?;
    viewer
        .create("task", &json!({ "subject": "nope", "project": project }))
        .await?
        .expect_error("permission.denied")?;
    Ok(())
}
```

A new case lives in its own file `src/cases/<name>.rs` and is registered by `ORDER` and `cases()`, with no edit to `mod.rs`; see [`docs/running-spot/l3-http.md`](../../../docs/running-spot/l3-http.md). Rules the suite keeps:

* one behaviour per case, and the case name says what it proves;
* every case signs in as the narrowest user that can observe the behaviour, so a permission
  regression cannot pass by running everything as the Administrator;
* data is named with `env.unique(prefix)`, so cases never collide on unique fields;
* a case that cannot run because the stack does not serve the surface yet is **blocked**, not
  deleted: it returns `Status::Blocked { owner, reason }` (or stays `#[ignore = "WP-…"]` when it
  is a whole test), so the report names the gap and its work package every run.

## When it fails

1. The run prints one line per case with its duration, then every failure with the observation
   (status, error code, body) — start there.
2. `L3_JUNIT` (CI: `artifacts/e2e/junit.xml`) has the same information in JUnit form.
3. Each fixture writes `<root>/l3-http.log` (a line per setup step and per note). The suite
   copies a failing run's log to `<L3_ARTIFACTS>/server-logs/<run id>.log` before teardown (also
   on a panic) — CI sets `L3_ARTIFACTS=artifacts/e2e` so the runner's `find` fallback agrees with
   it, and locally with no override that is a `spot-l3-artifacts/server-logs/` directory under
   the system temp directory. Locally the run prints the copy's path, and `L3_KEEP=1` leaves the
   root in place with the database.
4. To look at the data itself, run with `L3_KEEP=1` and connect:
   `psql "<SPOT_TEST_PG_URL with the printed database name>"`.
5. A leaked database or root is named with the run id; the next run's `TestDb::create` sweeps
   databases older than an hour.

## Status on `origin/dev`

The seed blocker this crate was written against (`user.password_hash__sealed` missing) is fixed
by WP-SEC-03.T3 (7782a0d), so the fixture starts and the suite runs. On that tree the run is:

```text
55 cases: 37 passed, 0 failed, 18 blocked
```

The 18 blocked behaviours are not hidden: each names the work package that owns the missing
surface, and every one is listed in [`CASES.md`](CASES.md) with the observation that proves the
gap. The largest is WP-PRD-01.T4 (`spot serve` does not register the installed apps'
controllers or mount files, reveal, export, events or the feed), which is why the Time Entry
roll-up, the hours range and the reveal case are blocked. The parallel proof still runs; it
prints which invariants it could not enforce today instead of passing silently.

Every case that is not blocked passes against the real host, the real permission engine and
PostgreSQL. A blocked case is a review item for the owning work package, never a deleted test:
when that surface lands, the case runs without an edit.

## The parallel safety proof

`tests/parallel.rs` signs in 200 sessions over one site, runs creates, updates, submits and
lists concurrently for a 30 s budget, and checks the end state:

* **no lost update** — every acknowledged task write is what the task reports now, and the
  contended task's version is one plus the number of successful patches;
* **no duplicate name** — no two created tasks share a subject;
* **no negative roll-up** — and, where `logged_hours` is set, it equals the sum of the submitted
  entries. Today it is always null: the sample controller is not registered (WP-PRD-01.T4), and
  the test prints that instead of pretending the invariant held;
* **the outbox** — when `spot_outbox` exists, every created task has an event and no published
  row is orphaned. Today the host does not wire the feed (WP-PRD-01.T4), so the table does not
  exist and the test says so.
