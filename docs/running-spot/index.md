Part of [Running Spot](../running-spot.md).

# Running Spot: all pages

Every page of the operator guide, by topic. The entry page, [Running Spot](../running-spot.md),
explains the host and its services; the pages below go deeper into one thing each.

## Getting started and operating

- [Adding a `spot` command](commands.md): how a subcommand is added to the binary, and the two kinds there are.
- [`spot console`](console.md): an interactive or scripted session over one site's documents, as a named user, for inspecting and repairing a site from the server.
- [Your own sessions](session.md): how a signed-in user lists and ends the sessions they hold.
- [Step-up](step_up.md): `POST /api/v1/auth/step-up`, how a signed-in person re-authenticates for the operations that need a recent step-up.
- [Field reveal](field_reveal.md): `POST /api/v1/resource/{doctype}/{id}/reveal`, reading one masked or protected field with a reason, audited, never cached.
- [Service ports of the jobs module](service-ports.md): the narrow ports the `jobs` module gives to modules registered after it.
- [The node role model](node_role.md): the `[services.node_role]` settings that make a process standalone, a hub or an edge node.
- [Workload routing](workload_routing.md): hosting the policy that decides where a request runs and where its reads go.
- [Files on disk](files.md): the layout of the configuration root, a site's folder and its files.

## Data and metadata

- [Conditions](conditions.md): the durable, per-site conditions that say something needs attention.
- [The capture invariant](capture_invariant.md): the continuous check that every committed write left a journal event.
- [`spot audit verify`](audit-verify.md): checking the hash chain of a site's identity audit trail (read only).
- [The event stream](events.md): the named consumers served at delivery level D1 (opt-in).
- [Bulk export and incremental extraction](export.md): moving a DocType out in bounded chunks, and extracting what changed since a watermark.
- [Reports](report.md): listing, describing and running the reports the installed apps ship.

## Processes

- [Process engine](process.md): the durable business-process engine over PostgreSQL.
- [The process and run API](process_api.md): the process and run operations the host serves.
- [Process triggers](process_triggers.md): how document events, schedules and webhooks start process cases.
- [Document nodes](process_documents.md): the process step that works on documents.
- [Integration nodes](process_integration.md): the HTTP, e-mail and outgoing webhook handlers.

## Security and secrets

- [The secrets service](secrets.md): the key file, rotation and what never to do with sealed values.
- [`spot support`](support_bundle.md): diagnostic bundles, crash and telemetry consent, offline help.

## Networking and discovery

- [LAN discovery](discovery.md): advertising a server on a LAN and finding one without typing an address.
- [The `cluster-redis` profile](cluster_redis.md): the optional plugin for running against Redis or Valkey.
- [Realtime](realtime.md): the WebSocket that carries document and room updates.
- [Adding a served HTTP operation](http-routes.md): how a new `/api/v1` operation group is added to the server.
- [The l3-http suite](l3-http.md): adding a behaviour case or an engine delegate to the end-to-end suite.

## Extending

- [Functions](function.md): calling a function an installed app declares in its manifest.
- [Durable jobs and coordination](jobs.md): the job queue, its intake and the leases that coordinate nodes.
- [Mail](mail.md): the mail job, the site's account and the sinks.
- [Print](print.md): rendering a document to PDF as the caller, with the site's print templates.
- [File uploads](file-uploads.md): uploads and downloads through `/api/v1/files` and the file provider.

## AI hosting

- [The LLM service](llm.md): models under the tenant's policy: bring-your-own keys, the allowed-endpoint list, budgets and the send record.
- [Remedies](remedies.md): the catalogue an agent repairs a site with, dry run and undo as audited operations.
- [The document lifecycle port](document_lifecycle.md): view, cancel, amend, recompute and repost a document as the remedy's actor, for the `document.*` remedies.
- [The fix pipeline](fix-pipeline.md): the one path by which an agent changes a site: rehearse, apply, verify, roll back.
- [Retrieval](retrieval.md): the permission-aware vector index and the `retrieve` tool, per tenant and off by default.
- [Agent memory](agent-memory.md): the separate per-tenant memory database, its write filter and its retention.

## The app store

- [The app store server](store-server.md): the operator guide for `spot-store-server`, a separate deployable.

## Keeping this index current

A task that adds a page to `docs/running-spot/` adds one line here, in the group it
belongs to. The check `python3 tools/docs-check/check_running_spot_pages.py` warns about a page
the index does not list (and `--strict` fails on it), and fails on a dead link, a missing header,
a path that does not exist or a TOML example that does not parse.
