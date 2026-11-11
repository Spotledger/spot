# Changelog

All notable changes to Spotledger Runtime are recorded in this file. The layout follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and version numbers follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). Until 1.0, any release may change
storage formats and APIs; each entry says what changed and what is not ready yet.

## [Unreleased]

Nothing yet.

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
