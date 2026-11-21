Part of [Running Spot](../running-spot.md).

# Retrieval (`[services.retrieval]`)

Optional, **off by default, per tenant**: nothing is created unless `enabled = true` **and** the site
is listed in `tenants`. Finds documents by meaning for agents (and any caller of the `retrieve` tool)
through a vector index kept in the site's own database, and **never shows a user a document that user
cannot read**.

## What it does

- **Indexer.** The node that holds the coordinator lease `retrieval_indexer` reads the site's change
  feed from a durable cursor, loads each changed document (as the platform: the indexing path has no
  calling user), chunks the fields the policy names, embeds the chunks through the **LLM service** and
  replaces the document's chunks. A delete removes them. The cursor moves only after the store
  accepted the batch, so a crash replays a range and a replay adds no duplicate.
- **Index policy** (`index_policy`, inline TOML). A DocType with no entry is **not indexed**. A field
  the permission rules protect (protection group, classification tag, mask) is left out of the index
  and so out of every embedding request, unless the policy lists it under `restricted_ok` **and**
  `index_protected = true`. An empty policy indexes nothing.
- **The `retrieve` tool.** Query text in, hits out: `doctype`, `id`, `version`, `score`, `snippet`.
  Every hit is re-checked **at read time, as the calling user**, against the live permission core
  (a permission-checked load) and against the field guard on the `Search` channel (the `report`
  right) before it leaves. A permission revoked a moment ago applies to the very next call, with no
  re-index. There is no total count and no "hidden" figure: a query that matches only unreadable
  documents answers exactly what a query that matches nothing answers. The answer is marked
  untrusted data.
- **Backfill.** Submit the job `platform:retrieval.backfill`: the indexer resets its cursor to the
  start of the feed and replays it (after a policy or protection change, or when the embedding model
  and `dimensions` change together with a new table).

## Settings

| Setting | Meaning |
| --- | --- |
| `enabled` | default false |
| `tenants` | the sites retrieval is on for (default none) |
| `provider` | the embedding provider name the LLM service resolves for the tenant |
| `dimensions` | the width of the provider's vectors, 1 to 2000; **fixed for the life of the table** |
| `batch_size`, `page`, `interval_ms` | texts per embedding call, changes per pass, pause between passes (32, 200, 1000) |
| `index`, `ivfflat_lists` | `"hnsw"` (default) or `"ivfflat"` and its list count |
| `index_policy` | inline TOML: `[doctypes.<name>] fields = [...]`, `restricted_ok = [...]`, optional chunking |
| `index_protected` | whether a `restricted_ok` protected field is indexed (default false) |
| `max_document_chars`, `max_query_chars`, `max_hits`, `snippet_chars` | bounds (1,000,000, 1,000, 20, 240) |

Unknown keys and out-of-range values stop the site from starting, naming the key.

## Level, and what a failure does

Optional. The site serves whatever happens here; readiness says `degraded: retrieval (<code>: <fix>)`,
the `retrieve` tool answers `mcp.not_supported`, and nothing is indexed, when:

- `retrieval.pgvector_missing`: the PostgreSQL server has no `vector` extension. Install the pgvector
  package on the server, then restart the site (creating the extension is a privileged step).
- `retrieval.embedder_missing`: no embedding port exists. Turn the LLM service on and configure an
  embedding provider for the tenant.
- `retrieval.ports_missing`: the coordinator lease or the document reader port is absent.
- `retrieval.store_unavailable`: the table or the pool could not be created (the database requires
  TLS, which this store does not open, is one cause; see the log).

A running indexer that is failing (`retrieval.embed_failed`, `retrieval.store_failed`,
`retrieval.position_failed`) reports `degraded` as well, keeps its cursor and retries with backoff.
A refused index policy indexes nothing until it is fixed (`retrieval.policy_*`).

## Where things are

- The table `spot_retrieval_chunk` in the site's database: `tenant`, `doctype`, `id`, `chunk_no`,
  `version`, `fields`, `body`, `embedding vector(n)`. It holds **no permission data**: the index is a
  copy and decides no access.
- The cursor: `sites/<site>/retrieval.cursor`, node-local. A node that takes the lease over starts
  from its own last position (or the start); the store replaces by `(document, version)`, so that is
  safe.
- Embedding calls leave the system: they go through the LLM service, so its endpoint allow-list, its
  budget and its record of what was sent apply. This module calls no model or URL itself.

## Limits to know

- The `retrieve` tool is not yet in the MCP tool list: the MCP service needs a hook to install an
  extra tool. Until it exists the tool is a port of this module (`RetrieveTool`).
- One embedding model per table: changing `dimensions` is a re-index into a new table, and the
  service refuses to start against a table of another width.
