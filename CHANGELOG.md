# Changelog

All notable changes to Spotledger Runtime are recorded in this file. The layout follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and version numbers follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). Until 1.0, any release may change
storage formats and APIs; each entry says what changed and what is not ready yet.

## [Unreleased]

Nothing yet.

## [0.1.15] - 2026-11-21

Continuous integration fix after 0.1.14, no new features.

### Fixed

- The Rust test shard script of 0.1.14 failed to parse (an apostrophe in a Python comment inside
  a quoted shell string), so all four shards failed at once. The script is fixed, and the workflow
  lint now runs `bash -n` on every `run:` block (rule `shell-syntax`) so it cannot recur.

## [0.1.14] - 2026-11-21

Continuous integration fixes after 0.1.13, no new features.

### Fixed

- The sandbox classifies the three tables of the LLM service (`spot_llm_send`, `spot_llm_tenant`,
  `spot_llm_usage`) as empty, so a sandbox never copies sealed keys or send records.
- The Rust test shards weigh `spot-cli`, which has about 250 test binaries and takes about 50
  minutes on its own, so it gets a shard of its own; each test chunk runs under a timeout and
  prints its crates and times, so a hung chunk can be named from the log.

## [0.1.13] - 2026-11-21

Continuous integration fixes and two batches of runtime work, both compiled and tested on
GitHub-hosted runners before this release (cargo check of the whole workspace, clippy with
warnings denied, and the tests of the touched crates on PostgreSQL 18).

### Added

- Scheduler: manifest schedules fire in `spot serve` under their app name, with durable
  high-water marks and a minimum grace of 60 seconds; the weekly `edge_verify` job.
- Realtime caps and metrics, the site cache, S3 and Azure file providers (features `files-s3` and
  `files-azure`), mail-intake attachments, three nodes on one PostgreSQL without Redis.
- Secrets KMS wiring (`[services.secrets.kms]`), extension seams and overrides listing,
  sandbox registry and destroy in the library crate.
- AI hosting, all off or inert by default: the LLM service with write-only sealed keys
  (`[services.llm]`), the remedy catalogue and fix pipeline, retrieval over pgvector
  (`spot-retrieval-pg`) and agent memory. Known gaps: the remedy host has no world adapter yet,
  and retrieval and agent memory need an embedding provider.
- Test infrastructure (Python): scenario format v1, runner, Spotledger and ERPNext adapters,
  scenario map, divergence registry and the differential engine.

### Fixed

- Manifest cron accepts day-of-week 0; the embedded platform app carries the `file` DocType;
  `spot restore apply --confirm` accepts a token that starts with a dash.
- Public continuous integration: balanced Rust test shards, the hardware-bound `l9-scale` runs
  moved out of hosted runs, a throwaway IMAP certificate, unique L3 site names, the L3 case index
  regenerated, and the step-up and reveal tests report a clean BLOCKED line while the host serves
  no such route.

## [0.1.12] - 2026-11-20

Continuous integration fixes after 0.1.11, no new features.

### Fixed

- Security advisories: `lettre` 0.11.24 (RUSTSEC-2026-0560 to 0565) and `wasmtime` 49.0.6
  (RUSTSEC-2026-0566 to 0573).
- The Rust test shards no longer fill the runner disk: each shard builds, runs and deletes test
  executables ten crates at a time, and incremental builds are off.
- The exported tree ships `apps/scripting`, which its tests read, and the two runbook tests
  format cleanly under the public paths.
- The python suites find the workflow files, the release workflow and the guide where the public
  tree has them.

## [0.1.11] - 2026-11-19

Continuous integration (the last red lint gate and the nightly profiles) and a large batch of
runtime work, written in one pass and compiled, fixed and tested together on PostgreSQL 18: about
150 tasks across the platform framework, among them the maintenance gate and its reload, the
sandbox registry, scheduled scripts, the extension commands, customisation with several nodes,
the SCIM, SSO and SAML administration surfaces, the journal rebuild and sync, the migration
operations, the process (BPM) triggers and insight, and the identity, field-security and
mail-intake layers. The layer tests (l3 end to end, l9 volume at 100 000 rows, l10 liveness) pass.
Not ready: the compliance evidence service is not part of the public tree; contract changes and
the new public API are listed in the sections below.

### Changed

- A maintenance gate that cannot be read answers `503 server.unavailable` with a `Retry-After`
  detail, not `server.maintenance`; a reload keeps the running gate instead of layering a fresh
  one that stayed closed.
- A projected column the caller may not see is answered as an unknown field (422), like a name
  the DocType does not have, so a refusal does not reveal that the field exists.
- A removed custom field keeps its column, marked retired, and `set_meta` no longer refuses; two
  nodes customising at once no longer overwrite each other's pending change.
- `spot_job_lease_idx` is a partial index on `lease_expires_at IS NOT NULL`, so the per-site job
  cap count no longer scans every running job (the schema is updated on `prepare`).
- New Rust API: `SchemaBackend::retire_column`, `maintenance_gate::refuse`,
  `spot_pack::check_app_with`; the entry `ctx` of a TypeScript function is frozen and a function
  result is refused.

### Fixed

- The query lint allows the one host-side file that provisions the `spot_ai` role to name the
  database driver's error and pool types (0.1.10 failed its lint gates on it).
- The nightly full Rust job runs in four shards per operating system, as the main CI does, and
  builds on the larger runner disk, so it no longer runs out of space; the Windows build of the
  developer-tools DAP test is free of an unused-mutability warning.
- The L7 checks on PostgreSQL reopen their read snapshot once when the server has ended its idle
  connection (the long final checks of a 1M-operation run); no check is weakened.

## [0.1.10] - 2026-11-18

Continuous integration, third pass: the hosted run of 0.1.9 left five Rust test failures, two
Python packaging tests, the `cargo deny` advisory check and the dotnet build.

### Fixed

- The `spot_ai` database role is provisioned under an advisory lock and retried, so sites that
  start together no longer race on it (one of them could refuse `/mcp` with a 503).
- `cargo deny` ignores RUSTSEC-2026-0542 (`humantime`, unmaintained through `chrono`; no
  vulnerability).
- Tests that carried wrong premises on a hosted runner: a short password was searched in every
  output, the fuel limit was burned before the wall clock, and the support-bundle `--since` test
  dated its old log from the wall clock. The packaging tests skip bytecode files and accept a
  `sudo` that no longer knows `requiretty`.
- The dotnet flow designer build and the Windows build of the backup volume target are clean.

## [0.1.9] - 2026-11-18

Continuous integration again: 0.1.8 still failed on the hosted runners, in places the product was
right and the harness, the export or the environment was not.

### Fixed

- The lint gates, the Python job, `cargo deny` (rustix 1.1.6, RUSTSEC-2026-0486) and the dotnet
  client build (the webhook signing-secret request body is one closed object now, native API
  0.26.1; the wire format is unchanged) pass.
- The export no longer ships the error catalogues of the compliance service it leaves out.
- CI Postgres starts with `max_connections=400` and the other settings the suites assume, so the
  journal-sync end-to-end tests no longer exhaust it; their node pools are smaller.
- Hosted runners install the PostgreSQL 18 client tools the backup tests run; the refusal message
  of an unsafe tools directory names the file and its mode.
- The occupancy stress test treats a statement timeout as a refusal under load, as it already
  did a lock timeout (the double-booking check is unchanged); the control-plane dependency test
  reads `Cargo.lock`, not an offline `cargo metadata`; the scaffold licence tests read the root
  README; the job plan test no longer depends on equal lease times.
- The 100,000-row import budget (120 s) applies to release builds and the private end-to-end run,
  not to debug builds on a hosted runner.

## [0.1.8] - 2026-11-17

A large batch of runtime work (about 270 tasks, integrated and compiled together), and the hosted
continuous-integration failures of 0.1.7 fixed. Not ready yet: the hub-side sync assembly keeps
its documented limits (tenant push sinks, policy-generation swap needs a reload), the sign-in
throttle's source-side counter is not written in production, and the webhook worker cannot be
switched on by a reload.

### Added

- Batched writes: `Engine::create_batch` creates many documents of one actor in one transaction
  (a savepoint per document, each with its own validation, permission checks, events and
  ledger row), and the import host uses it, up to eight batches at once for DocTypes whose naming
  and unique keys do not depend on write order. A 100,000-row import takes about a minute.
- `Engine::load_many` and a batched native list path: a list of 20 rows no longer resolves the
  caller's roles once per row.
- The audit-chain appender and the feed publisher work in groups, without a lock convoy.
- `postgres.pool_wait_ms` (100 to 60,000 ms, default 5,000): how long a request waits for a
  database connection before it is refused with `storage.unavailable`.
- Events contract 0.7.8 (command ids up to 320 characters).

### Fixed

- The graph drift check no longer starves request serving: it reads in short transactions, pages
  by key and pauses between pages.
- A process-version migration, role-named task members and the process run list work end to end.
- Hosted continuous integration: the l3 harness's root sweep no longer races with itself, the
  Windows build no longer fails on helpers only the Unix build uses, the browser publish uses
  the pinned .NET SDK, the dotnet analyzer level is pinned, the release-workflow test is skipped
  where its workflow is absent, and the nightly L8 isolation run uses 25,000 transactions per run
  (at 100,000 its checker needed 19 GB and the hosted runner was shut down).

## [0.1.7] - 2026-11-12

Continuous integration and packaging checks: 0.1.6 still failed on the hosted runners.

### Fixed

- The Debian package check refused every file of a correct package: its allowed-prefix test
  compared paths against prefixes written with a trailing slash, so nothing matched. The check
  only runs where `dpkg-deb` exists, which is why it was first seen on the hosted runner.
- `spot-agent-linux` compiles for Windows (the process-group call is Unix-only), so the Windows
  workspace build no longer stops at that crate.
- The formatting of the 0.1.6 lock-test fix.

## [0.1.6] - 2026-11-12

Continuous integration, and one test: the first scheduled nightly run on the hosted runners failed
in four places, none of them in the product.

### Fixed

- The Linux-lock test in `spot-agent-linux-sys` waits for the release instead of assuming it is
  instant: a sibling test's forked child holds the lock's open file description until its `exec`.
- The nightly and weekly workflows check formatting on Linux only (Windows refuses the one long
  command line `cargo fmt --all` builds) and the nightly Rust tests use `--no-fail-fast`.
- The Windows .NET jobs check the sources out without line-ending conversion and leave out the
  suites whose baselines are recorded on Linux (screenshots, accessibility) and the file-name
  refusal tests that need names Windows cannot create.
- A red L7 nightly now keeps the PostgreSQL log and prints the cluster's state.

## [0.1.5] - 2026-11-12

Continuous integration, and nothing else: 0.1.4 failed two of the lint gates.

### Fixed

- Seven stable error codes added in 0.1.4 (the capture-invariant findings, the console's
  unexpected-reply code and the job worker's final-attempt-hook failure) are now in the error
  catalogue, so the error-code completeness gate passes.
- The query gate no longer flags the console's test file, which reads the audit chain through the
  test database pool and feeds a refused `drop table` input to the console's command parser.

## [0.1.4] - 2026-11-12

Runtime changes since 0.1.3: an administrator console, lease metrics, role-conditioned services and
several host wiring steps. Still pre-release; see [ROADMAP.md](ROADMAP.md) for what is not ready.

### Added

- `spot console`: an administrator's interactive and scripted session over the document engine,
  run as a named user. Commands get, list, create, update and delete documents, call a function
  and run a report. Every command goes through the real permission path and is audited; there
  is no raw SQL.
- Lease metrics on `/metrics`: `spot_lease_events_total` and `spot_lease_held`, recorded by a
  decorator around the coordinator that the host builds.
- Role-conditioned services: the services registry can enable or disable a service module by node
  role (standalone, hub, edge). Each role starts, and lists in readiness, exactly its own set;
  hub-only routes answer 501 on a node that is not a hub.
- The cluster profile can provide the cache, the coordinator and the invalidation bus from
  Redis or Valkey when configured. It fails closed when the coordinator is lost.
- The files service is installed as a step of the document engine: saving a document with an
  `attach` field marks the file attached, replacing the value orphans the old file, and deleting
  the document releases it.
- The mail sender is registered as the job worker's final-attempt hook, so a mail that fails its
  last attempt produces one bounce per recipient.
- A capture-invariant check that runs against the real change feed and the real document-state
  events. It runs continuously in bounded memory on the scheduler, raises and clears a mismatch
  alarm, and is also a CI gate.
- The Windows-specific code of the agent crates compiles and lints on Windows runners, and a
  weekly Windows build catches regressions.
- A host-level test against PostgreSQL of the open-path allow-list: a normal route answers 401
  without a session, a declared open path is reachable, and undeclared or reserved paths cannot
  be opened.

### Changed

- The job worker claims a job only while the node's coordinator lease is current. The lease check
  is cached for at most 500 ms, so a worker that has lost its lease stops claiming within that
  bound instead of waiting for the next renewal.

### Known limitations

- The request log may still include concrete paths on open routes; a change to stop this is
  planned.

## [0.1.3] - 2026-11-11

Continuous integration on GitHub-hosted runners, and nothing else.

### Fixed

- The help-coverage test of the unattended-support crate checked a process note that the public
  repository does not ship, so shard 2 of the Rust job failed. It now checks a note that ships
  with the repository.

## [0.1.2] - 2026-11-11

Continuous integration on GitHub-hosted runners, and nothing else.

### Fixed

- The test harnesses create development sites that accept the unsigned sample app: new sites
  require signed apps by default, which made the harness tests fail on installing it.
- The client conformance corpus (`tools/client-conformance/`) ships with the repository, so the
  client core's corpus test finds its cases.
- A test run in CI now reports every failing test of a shard instead of stopping at the first.

## [0.1.1] - 2026-11-11

Continuous integration on GitHub-hosted runners, and nothing else.

### Fixed

- The Rust tests run in four parallel shards: the full workspace test build did not fit on one
  hosted runner.
- The UI performance budgets, which were measured on a reference machine, are recorded as
  warnings on shared hosted runners instead of failing the run; the strict unit tests of the
  budget code no longer depend on that setting.

## [0.1.0] - 2026-11-11

The first public release. Spotledger Runtime is pre-release software: it is not yet ready to run
a live public service, and there are no certifications, reference deployments or benchmark
results. See [ROADMAP.md](ROADMAP.md) for what exists and what is still being built, and
[SECURITY.md](SECURITY.md) before running it anywhere that matters.

### Added

- The `spot` binary with the commands `new-site`, `serve`, `install-app`, `list-apps`,
  `migrate`, `add-user`, `set-password`, `reset-admin`, `reload` and `check`: create a site,
  serve it with health endpoints and metrics, and manage its apps and users.
- Versioned contracts in `contracts/`: storage, events, the native API, the app API and UI
  extension points. The native API contract is at version 0.11.1 (a draft; it will change
  before 1.0).
- Signed app bundles: `spot install-app` checks a bundle's signature, the publisher certificate,
  the site's trust roots and its revocation list before anything is written, and a refused
  bundle changes nothing on disk.
- Offline identity for branch and offline nodes: a signed permission snapshot with a lease that
  fails closed when it lapses, so a branch can keep working without the hub for a bounded time.
- Outgoing e-mail and model-provider (LLM) clients that connect only through the outbound network
  policy: a connection goes only to addresses the policy allowed, names are not resolved a
  second time, and every redirect is checked again.
- The Business Process Engine crates: process model, durable execution, tasks, triggers,
  processes across organisations, undo, versioning and insight.
- PostgreSQL implementations of the framework's store interfaces.
- The Avalonia desktop client.
- The README, the roadmap, the security policy, the contribution policy and the trademark
  policy.

### Not ready yet

- Durable stores for replay protection and HTTP idempotency keys exist, but the server and agent
  hosts do not use them everywhere yet. Where a host still uses an in-memory store, these
  protections do not hold across restarts or across several nodes.
- There are no signed release artefacts yet; build from source.
- The browser and mobile clients, the AI layer beyond its building blocks, and published
  benchmark results are still in development or planned.
- No independent security review has been done yet.
