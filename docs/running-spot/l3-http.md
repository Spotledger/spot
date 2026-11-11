Part of [Running Spot](../running-spot.md).

# The l3-http suite: adding a case, and writing a delegate

`harness/rust/l3-http` runs the framework's behaviour over the real stack (see its `README.md`
for how a run works). This page is for the task that adds a case or an engine delegate. Neither
needs an edit to a shared list: a new file is found by the build.

## Add a case file

Create `harness/rust/l3-http/src/cases/<name>.rs` (a lower-case module name) that declares, at
column 0, both of these:

```rust
use super::CaseDef;

pub const ORDER: u32 = 30;

pub fn cases() -> Vec<CaseDef> {
    vec![CaseDef::run(
        "a_filtered_export_matches_the_query",       // unique across the whole suite
        "Export",                                     // a heading of CASES.md (new headings are fine)
        "an export under a filter holds the listed rows", // what it pins, one line
        false,                                        // core: true puts it in the quick subset
        |fixture| Box::pin(a_filtered_export_matches_the_query(fixture)),
    )]
}

async fn a_filtered_export_matches_the_query(fixture: &Fixture) -> CaseResult { /* ... */ }
```

* `build.rs` scans `src/cases/*.rs`. A file registers when it declares both `pub const ORDER: u32`
  and `pub fn cases() -> Vec<CaseDef>`. A file with one of the two fails the build and names the
  file; any other file (the six helper modules `auth`, `documents`, `edge`, `lifecycle`,
  `lists`, `permissions`) is left as it is.
* A run executes the hand-written block of `cases/mod.rs` first, in its fixed order, then the
  registered cases sorted by `(ORDER, name)`. Pick an `ORDER` that does not depend on another
  group's data; a case is independent of the others (it makes its own documents and restores what
  it changes).
* The quick subset (`L3_QUICK=1`) runs the cases with `core = true`; a blocked case is reported
  only when the filter admits non-core cases.
* Cases follow the rules in the crate's README (narrowest user, `env.unique`, one behaviour).

## Add a blocked entry, and turn it into a case later

A surface the host does not serve yet is a blocked entry in the same file, not a missing test:

```rust
CaseDef::blocked("events: the_audit_stream_replays_from_a_position", "Events",
    "the stream replays from a position", "WP-INT-02.T4", "no `/api/v1/events/*` route is mounted")
```

The task that serves the surface replaces `CaseDef::blocked(...)` with `CaseDef::run(...)` in the
same file (same name, so the history reads as one case). `export.rs` and `events.rs` already
hold the two blocked entries that map to registry groups. The other blocked entries are still in
the hand-written block of `cases/mod.rs`, with their owner and reason in `LEGACY_BLOCKED`.

## CASES.md is generated

`harness/rust/l3-http/CASES.md` lists every case with the columns Case, Pins and Kind (`case` or
`blocked: <owner>: <reason>`) and counts computed from the index. It does not carry pass or fail:
the run report does. After adding or changing a case:

```bash
cargo run -p l3-http -- --write-cases
```

`cargo test -p l3-http --test cases_index` fails when the committed file differs from the
generated text, and when two cases share a name. **A textual merge conflict in `CASES.md` is
resolved by regenerating it**, never by editing the markers: take either side, run the command,
commit. The command never starts a fixture or a database.

## Fixture limits

One database, one site, one host per run; the users are the Administrator, a Project Manager, a
Team Member, a Viewer, a role-less observer, a lockout account and an invited user. The sign-in
throttle counts per source, so cases that fail sign-ins run last. A case that changes shared state
(metadata, the first-sign-in user) restores it or is the only consumer of it.

## Write an engine delegate

`EngineResources` serves every `ResourceService` method except `call_function` and `run_report`,
which it hands to optional delegates (`spot_api_services::Delegates`). A delegate is a trait object
the host installs:

* `crates/framework/spot-api-services/src/resources/function.rs` holds `FunctionDelegate`, and
  `report.rs` holds `ReportDelegate`; their signatures match the two `ResourceService` methods
  and take the request's own `RequestCtx`, so a delegate runs as the calling user and nothing
  wider.
* `build.rs` declares every `src/resources/<name>.rs` as a module. Another group's engine-side
  implementation (for example the engine-backed implementation of a `spot-http` port trait) is
  one new file there, and a group that needs an `EngineResources` delegate adds a trait file and
  one `Option` field to `Delegates` in `resources/mod.rs`.
* `EngineResources` resolves the request's credential before it calls a delegate, so a request that does not
  resolve answers `auth.required` and never reaches it.
* A delegate returns an `ApiError` for work it cannot do; it never answers success for it. With
  no delegate installed the two operations answer `method.not_found` and `report.not_found`, as
  they always did.
* Installing a delegate in `spot serve` (`EngineResources::with_delegates`) is the host wiring of
  WP-PRD-01.T48.
