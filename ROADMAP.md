# Roadmap

This is an honest status page, not a promise. We give no dates. Items move between sections as they are built and proven.

## How we are building

Order of work (decision ADR-060): **the runtime and its services first, rock solid; then the user interfaces; then the business applications built on top.** This repository holds the first layer, the Platform. Foundation and ERP are separate products and are not part of it.

## Exists in this repository

Present and tested to the extent the test suites in this repository show; interfaces may still change before 1.0.

- The `spot` binary and its server (`new-site`, `serve`, `check`, `reload`), with sites, health endpoints and metrics.
- The versioned contracts: storage, events, native API (0.11.1), app API, UI extension.
- Business Process Engine crates: process model, durable execution, tasks, triggers, cross-organisation conversations, undo, versioning, insight.
- PostgreSQL backends for the framework's store interfaces.
- The Avalonia desktop client.


## In development

- Browser client (React) and mobile client (React Native) on one shared Rust client core (ADR-088, ADR-091).
- Visual process designer, partner simulator and load profiles for simulating processes.
- Sync between hubs and edges, tenant links and exchange between organisations: implemented as crates, being hardened (ADR-058, ADR-062 to ADR-067).
- Wiring the durable PostgreSQL stores for replay protection and HTTP idempotency keys into the server, management and agent hosts, and refusing the in-memory stores in a production profile.
- Sandboxed extensions and connectors, and the app review pipeline.
- Server-hosted UI components (ADR-084, proposed).

## Planned

- The AI layer beyond its building blocks (`spot-mcp`, `spot-self-model`, `spot-ai-isolation` exist): support and operations agents, and a worked example of an agent building a complete application end to end.
- Published benchmark results against the scale tiers defined in the blueprint.
- An independent security review.
- Tagged, signed releases with a stability policy.

## Not in this repository

Foundation (organisation structure, parties, products, currency, numbering) and the ERP suite are separate products.

## What we do not claim

No certifications, no reference deployments, no benchmark results yet, and no stability guarantee for storage formats or APIs between pre-release versions.
