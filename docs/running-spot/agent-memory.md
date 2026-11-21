Part of [Running Spot](../running-spot.md).

# Agent memory (`[services.agent_memory]`)

Optional, **off by default, per tenant**: nothing is created unless `enabled = true` **and** the site
is listed in `tenants`. Hosts the agent's memory (episodes: what happened; procedures: how to fix it;
deployment facts: what the installation is) in a **database of its own**, with a retention schedule,
a recall API for agents and a write filter that keeps business data out.

## The boundary is a database

At first use the tenant's memory gets a **separate database** (`spot_memory_db_<derived>`) and a
**separate role** (`spot_memory_role_<derived>`) on the control server, recorded in the control store.
The role owns only its database and is granted nothing on any business database; PostgreSQL has no
cross-database query, so memory code holds no path to a business table. The `vector` extension is
created in the memory database only. Provisioning is idempotent: a restart, or a resume after a crash,
converges instead of failing. Note: unless you also revoke `PUBLIC CONNECT` on your business
databases, the memory role can *connect* to one; what stops it reading is that it has no table
privilege there (`42501`), which the tests assert.

The memory role's password comes from `password_file` (16 to 128 characters of letters, digits, `.`,
`_`, `~`, `-`; it is placed in a statement that takes no parameter, so nothing else is accepted). The
control connection that creates the database and role is the site configuration's administrative one.

## What it does

- **Write filter**, on every write: the sanitiser again (a secret or a financial value is refused,
  personal data is refused rather than stored), and a refusal of any text that contains a field value
  (6 characters or more) of the record the episode is about, read as the platform and never stored.
  Every verdict is counted and audited (`spot::audit::agent_memory`, and an AI audit entry when that
  chain is hosted: a write the chain cannot record is not made). A refusal names the class
  (`secret`, `financial`, `pii`, `document_field_value`), never the match.
- **Recall for agents**: similarity ranked in the database with the structured filters and a recency
  boost. The caller must be an agent; its tier comes from the stored registration (the agent
  directory), never from an argument. A procedure learned above the caller's tier is not returned,
  counted or hinted at. An agent never writes above its own tier. Memory never crosses a tenant.
- **Retention**: every `retention_interval_seconds` the node holding the lease
  `agent_memory_retention` plans, then applies exactly the plan (TTLs, caps, compaction of old
  episodes into statistics). Legal holds in `holds_file` (`spot-data-policy` holds TOML) suspend it
  for what they cover; **a hold file that cannot be read stops the pass**. The job
  `platform:agent_memory.retention` runs one pass on demand.
- **Learned remedies**: a candidate is usable only after CI promoted it and recorded its run; the
  table's own `CHECK` refuses a promotion with no CI run.

## Settings

| Setting | Meaning |
| --- | --- |
| `enabled` | default false |
| `tenants` | the sites agent memory is on for (default none) |
| `password_file` | the file holding the memory role's password |
| `retention_interval_seconds` | 60 to 86,400 (default 3,600) |
| `holds_file` | legal holds, read at every pass |
| `max_recall_k` | the most hits one recall returns, 1 to 100 (default 20) |
| `[retention]` | `compaction_min_age_days`, and `episode`, `procedure`, `fact` each with `ttl_days` and `max_entries` |

Unknown keys and out-of-range values stop the site from starting, naming the key.

## Level, and what a failure does

Optional. The site serves whatever happens here; readiness says `degraded: agent_memory (...)` and
nothing is written or recalled, when:

- the server has no `vector` extension: the text says to **install the pgvector package** on the
  server (creating an extension is a privileged step), then restart the site;
- `password_file` is unset, unreadable or refused; the control connection cannot create databases or
  roles; the database requires TLS (this store does not open TLS);
- no embedding provider port exists (turn the LLM service on and configure an embedding provider) or
  the coordinator lease port is absent.

## The `recall_memory` MCP tool

The service installs `recall_memory` on the MCP service's `McpHandle` (contract `mcp.md`, section 5,
"Extension tools") when the site is built and takes it off at shutdown. It is listed only for a
registered agent (a user principal never sees it) and only once the start-up check has provisioned the
tenant's memory. Arguments: `query` (at most 512 bytes), `k` (1 to `max_recall_k`, default 5) and `kinds`
(`episode`, `procedure`, `fact`). The answer is `{"entries": [...], "provenance": "untrusted"}`; each
entry has `kind`, `id`, `text`, `score_bp`, `confidence_bp`, `source`, `tier`, `created_ms` and
`remedies`. The agent's tier comes from the authenticated principal and gates the answer: an entry
learned above it is neither returned, counted nor hinted at. A failure is a tool error with
`agent_memory.query_too_long`, `agent_memory.embed_failed` or `agent_memory.store_failed`.

## Limits to know

- Embeddings are 64 wide (`EMBEDDING_DIM`), the width of the memory tables.
- Aggregate facts written by compaction carry deployment epoch 0 until the deployment model is hosted.
- Moving a candidate through CI is the CI integration's job (`PromotionPort`); this module stores the
  result and answers `remedy_usable`.
