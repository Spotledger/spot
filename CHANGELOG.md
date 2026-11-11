# Changelog

All notable changes to Spotledger Runtime are recorded in this file. The layout follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and version numbers follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). Until 1.0, any release may change
storage formats and APIs; each entry says what changed and what is not ready yet.

## [Unreleased]

Nothing yet.

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
