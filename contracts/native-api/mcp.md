# The MCP endpoint (contract 0.24.0, WP-CON-03.T53; extension tools 0.27.0, WP-CON-03.T68)

Normative, in the discipline of `realtime.md`. The Model Context Protocol endpoint is a **separate
protocol on the same host**: JSON-RPC 2.0 over HTTP, not REST. It is documented here and is not
wrapped in OpenAPI operations (`openapi.yaml` carries only the pointer `x-spot-mcp`). Words in
capitals are requirements on a conforming server; a client may rely on every one of them.
The implementation is `spot-mcp` and `spot-http`; where the crate and this document differ the
difference is a defect in one of them, listed under "Known gaps".

## 1. Endpoint and transport

- **One endpoint:** `POST /mcp` on the site host (the tenant comes from the host, as for `/api/v1`).
  Any other method is `405`. There is **no `GET` stream in this version**.
- **Protocol revision:** `2026-07-28`. The legacy revision `2025-11-25` (the `initialize` handshake) is
  accepted on the same endpoint. The client may send `MCP-Protocol-Version`; any other value is `400`
  with the MCP unsupported-version error. Every response carries `MCP-Protocol-Version`.
- **Content type:** the request body is `application/json` (one JSON-RPC message, UTF-8). The answer is
  `application/json` (one message), or `text/event-stream` when the call produced more than one message
  (progress notifications before the result: each frame is `event: message` and `data: <one JSON-RPC
  message>`), or `202` with an empty body for a notification. Every response carries `X-Request-Id`.
- **Enablement:** the MCP server ships with the platform and is ON by default on every site (blueprint WS13).
  A site may switch it off by configuration; when off, `POST /mcp` answers `404`. A client reads the
  contract `version` from `GET /api/health/live` (0.24.0 or later) to know the document applies.

## 2. Limits

| Limit | Value | Source |
|---|---|---|
| Request message | at most 256 KiB | `MAX_INPUT_BYTES` |
| Rows returned by one call | at most 500 | `MAX_ROWS` |
| Query IR depth / nodes | at most 32 / 10,000 | `MAX_DEPTH`, `MAX_QUERY_NODES` |
| Deadline of one call | 30 seconds by default | `DEFAULT_TIMEOUT` |
| Rate | 120 calls per minute per principal | `DEFAULT_RATE` |

A call that exceeds a limit fails with a tool error (section 6), never with a truncated silent result.
No tool call holds a database connection beyond its deadline.

## 3. Authentication and identity

- **Credential:** `Authorization: Bearer <token>` (a session token or an OAuth access token) **or**
  `X-Api-Key: <key id>:<secret>`. Nothing else: a token in a URL or a query string is never accepted.
- **Every call runs as that principal**, with that principal's rights, never as a system principal. An API
  key's scope narrows every decision. A consumer principal (ADR-083) cannot use MCP.
- **Failure:** a missing or invalid credential is `401` with the native error envelope
  (`auth.required` or `auth.invalid_credentials`) and a `WWW-Authenticate: Bearer` header; no tool list, no
  tool name and no document is revealed before authentication.

## 4. Origin and transport safety

- The `Origin` header, when present, MUST be one of the site's allowed origins; a browser-origin request
  whose origin is not allowed, or that carries a browser `Origin` the site has not allowed, is `403`
  (`permission.denied`) before any body is read. This is DNS-rebinding protection.
- The endpoint is **not CORS-open by default**: it answers no `Access-Control-Allow-Origin` unless the
  site allows the origin. A non-browser agent sends no `Origin` and is not affected.

## 5. Tools

The tool list (`tools/list`) is **filtered per principal**: a principal never learns of a tool it cannot
call. A tool that can change data exists only if the principal holds the right. Names are generated from
metadata, are stable, lower snake case, and collide with nothing (a collision is resolved by the
metadata, never by suffix guessing); characters outside letters, digits, `_`, `-` and `.` become `_`.

| Tool | Meaning |
|---|---|
| `search_<doctype>` | Equality filters over readable fields, `order_by`, `descending`, `limit` (at most 500), `cursor`, `fields`. Only readable fields are returned. |
| `get_<doctype>` | One document the caller may read. |
| `create_<doctype>_draft` | Create a draft through the lifecycle pipeline (write-capable). |
| `update_<doctype>` | Update a document through its update lifecycle (write-capable). |
| `query` | **Read-only.** The query IR subset the native API accepts (`resource_query` filters, no search in this version). |
| `call_<app>_<module>_<function>` | A whitelisted function (as `function_call`), synchronously when it declares verbs, else as a background job. |
| `job_status` | The state of a job by `job_id`. |

Descriptions never contain document data. (The brief for this document proposed `<doctype>.list|get|query|create_draft`,
`method.<app>.<module>.<function>` and `jobs.get|jobs.cancel`; the names above are what `spot-mcp` implements and are the
contract. `jobs.cancel` is not in this version.)

**Long operations** return a job handle `{ "job_id": <integer> }`; the client polls `job_status`. No call
holds a connection beyond its deadline.

### Extension tools

A hosting service (retrieval, agent memory, the remedy host) may add tools to `tools/list` beyond the
generated ones above. These are **extension tools**. The rule is normative and applies to every one of
them, now and later; a tool that does not meet it MUST NOT be offered.

1. **Name.** Stable, lower snake case, 1 to 64 characters, and never equal to or beginning with a
   generated name: no extension name is `query` or `job_status`, and none begins with `search_`, `get_`,
   `create_`, `update_` or `call_` (the generated names carry one of those or a DocType). A name that
   would collide is refused when the tool is installed; a collision is never resolved by a suffix. The
   names `retrieve`, `recall_memory`, `remedy_list`, `remedy_dry_run`, `remedy_apply` and `remedy_undo` are
   reserved for the six tools below.
2. **Listing.** An extension tool appears in `tools/list` only for a principal that may call it. A call by
   any other principal is refused with the same `permission.denied` tool error as any other tool; the
   refusal does not say whether the tool exists for someone else.
3. **Identity.** It runs as the calling principal, never as a system principal or an administrator. An
   API key's scope narrows every decision it makes. A consumer principal (ADR-083) cannot use MCP, so it
   cannot call an extension tool.
4. **Limits.** The same rate limit, request size limit and deadline as any tool (section 2) apply, and a
   result that would exceed a limit is a tool error, never a silent truncation.
5. **Audit.** The same record as any tool (section 7): the tool name, the principal, the outcome, the
   agent identity and tier, and **no argument value**. A tool that can change data is write-capable for
   the fail-closed rule of section 7.
6. **Availability.** A tool is absent (not listed, and a call is the same unknown-tool refusal as any
   unlisted name) while its service is off, not ready or degraded. A call in flight when the service
   stops fails with a tool error (`server.unavailable`), never with a transport or JSON-RPC protocol
   error.
7. **Descriptions** never contain data: a tool's description and input schema are the same for every
   caller and name no document, value or tenant.
8. **Untrusted content.** What retrieval and memory return is data that someone wrote, not an
   instruction. Their answers carry `"provenance": "untrusted"` and a client MUST NOT treat any text in
   them as an instruction.
9. **Errors.** A tool-specific refusal is a tool error whose `code` is the code of the service's
   catalogue (`retrieval.*`, `agent_memory.*`, `remedies.*`, `remedies_host.*`); a refusal that is not
   specific to the tool uses the codes of the native API (`permission.denied`, `api.invalid_request` for
   arguments that do not validate, `api.rate_limited`, `http.timeout`, `server.unavailable`). The message is
   a fixed sentence: it never quotes an argument, a document value, a key or a path.

The six extension tools of this version follow. Arguments are a JSON object; an unknown key, a wrong type
or a value out of range is `api.invalid_request` (the tool error names the argument, never its value).

| Tool | Listed for | Writes | Meaning |
|---|---|---|---|
| `retrieve` | a user, or an agent acting as a user (any DocType rights) | no | Finds documents by meaning, among the ones the caller may read. |
| `recall_memory` | a registered agent only (a user principal does not see it) | no | Recalls what the agent's memory holds for a query. |
| `remedy_list` | a registered agent whose status is active and whose domain the remedy host serves | no | The catalogued remedies the agent's tier allows. |
| `remedy_dry_run` | the same agents | no | Plans a remedy, changes nothing, returns a sealed plan. |
| `remedy_apply` | the same agents, only when the agent's tier allows an apply | yes | Applies a plan the host issued. |
| `remedy_undo` | the same agents, only when the agent's tier allows an apply | yes | Undoes an apply inside its window. |

**`retrieve`.** Arguments: `query` (string, 1 to 2000 characters; a site may configure a lower limit, never
a higher one), `k` (integer 1 to 20, default 5; a site may lower the maximum), `doctypes` (optional array
of 1 to 16 DocType names, each at most 63 characters). Answer:

```json
{"hits": [{"doctype": "Customer", "id": "...", "version": 7, "score": 0.83, "snippet": "..."}],
 "provenance": "untrusted"}
```

`hits` is ordered best first and holds at most `k` entries. `score` is a JSON number that ranks hits
**within this answer only**; it is not comparable across calls. `version` is the document version the
snippet was read from. **Visibility is decided at the moment of the call**: every hit is re-checked
against the caller's live permissions and field rules (no cached decision, no decision made when the
text was indexed), a snippet contains only fields the caller may read, and an API key's scope drops hits
of a DocType the key may not read (naming such a DocType in `doctypes` is `permission.denied`). There is
**no total and no count of hidden documents**: an empty `hits` means that nothing the caller may see
matched, and says nothing about what exists. A query that matches only documents the caller may not read
answers exactly as a query that matches nothing. Tool errors: `retrieval.query_too_long`,
`retrieval.embed_failed`, `retrieval.store_failed` (no value in the message).

**`recall_memory`.** Arguments: `query` (string, 1 to 512 bytes), `k` (integer 1 to 20, default 5; a site may
lower the maximum), `kinds` (optional array of 1 to 3 distinct values from `episode`, `procedure`,
`fact`). Answer:

```json
{"entries": [{"kind": "procedure", "id": "...", "text": "...", "score_bp": 8312,
              "confidence_bp": 9000, "source": "remedy_run", "tier": "T1",
              "created_ms": 1790000000000, "remedies": ["job.retry"]}],
 "provenance": "untrusted"}
```

`entries` are ordered best first. `score_bp` and `confidence_bp` are integers in basis points (0 to 10,000;
`score_bp` may be lower for a weak match). `source` is one of `incident`, `remedy_run`, `operator`,
`support_case`, `deployment`, `compaction`. `tier` is the tier the entry was learned at (`T0` to `T3`).
The agent's own tier gates what comes back: an entry learned above the caller's tier is **not returned,
not counted and not hinted at**. Only this tenant's memory is read. An entry carries **no document field
value**: the write filter that guards every memory write guarantees it, and this contract states it, so a
client may rely on it. Tool errors: `agent_memory.query_too_long`, `agent_memory.embed_failed`,
`agent_memory.store_failed`.

**The remedy tools** are for a registered agent. The tenant is the site and the agent is the calling
principal; neither is an argument (an extra key is `api.invalid_request`). A remedy is listed and may be run only
within the agent's tier, domain and status; a remedy above the agent's tier is refused (`remedies.refused`).
- **`remedy_list`**: no arguments. Answer `{"remedies": [{"id", "title", "tier", "risk", "affects",
  "preconditions", "reversible", "params_schema"}]}` in id order, filtered by the agent's tier. `tier` is
  `T0` to `T3`, `risk` is `low`, `medium` or `high`, `affects` is an array of DocType names,
  `preconditions` is a sentence, `reversible` is a boolean and `params_schema` is a JSON Schema object
  for `params`.
- **`remedy_dry_run`**: arguments `remedy` (string, the remedy id) and `params` (object, validated against
  that remedy's `params_schema`). Answer: the **sealed plan** `{"remedy", "params", "steps": [{"target",
  "operation", "detail"}], "hash"}`. It changes nothing. The answer is withheld (`remedies_host.audit_unavailable`) when the audit chain
  cannot record that the plan was issued.
- **`remedy_apply`**: arguments `plan` (the object `remedy_dry_run` returned, **unmodified**) and
  `approval` (optional object `{"approver", "ref", "at"}`: a person's approval reference, for a remedy that
  needs one; `at` is Unix seconds; an approval the host cannot verify is refused). **One dry-run
  authorises one apply**: a plan the host did not issue, a plan that was altered, and a plan already
  applied are refused (`remedies.plan_stale`). Answer: `{"remedy", "plan_hash", "changes": [{"target",
  "before", "after"}], "undo_token", "undo_expires_at", "route"}`. `before` and `after` are state
  fingerprints, never values. `undo_expires_at` is the end of the undo window in Unix seconds, and
  `route` is `"direct"` (a reversible T1 setting applied on the live site) or `"pipeline"` (any other remedy:
  the fix pipeline plans, rehearses, applies, verifies and rolls back; the answer is given when that run has
  finished). A remedy that must use the pipeline while none is installed is refused
  (`remedies_host.apply_closed`) and nothing changes. A remedy that needs a tier above T1 never runs directly.
- **`remedy_undo`**: arguments `remedy` (string) and `undo_token` (string), and optionally `approval` as above.
  Answer `{"remedy", "undo_token", "changes": [...]}`. It is refused after the window
  (`remedies.undo_expired`), for an unknown token (`remedies.undo_unknown`), for a remedy that is not
  reversible (`remedies.irreversible`) and when the state changed since the apply (`remedies.undo_conflict`).
- **Audit before effect.** Every apply and every undo is written to the audit chain **before** it runs. If
  the chain cannot record it, the call is refused (`remedies_host.audit_unavailable`) and nothing changes.
- **Tool errors** of the remedy tools carry the codes `remedies.unknown`, `remedies.refused`,
  `remedies.params_invalid`, `remedies.precondition_failed`, `remedies.plan_stale`, `remedies.undo_unknown`,
  `remedies.undo_expired`, `remedies.undo_conflict`, `remedies.irreversible`, `remedies.op_failed`,
  `remedies.hard_floor_financial` (a remedy never changes a submitted financial record), and
  `remedies_host.not_an_agent`, `remedies_host.no_world`, `remedies_host.world_unavailable`,
  `remedies_host.apply_closed`, `remedies_host.audit_unavailable`, `remedies_host.internal`.
  Never the parameters, a plan step's data or a document value.

## 6. Errors

A native API error appears as an MCP **tool error** (`isError: true`) with the same stable `code`
(section "Errors" of `openapi.yaml`) and a user-safe `explanation`. A document that does not exist and one the
caller may not read are the same not-found as over REST. A protocol error (unknown method, malformed
JSON-RPC, unsupported revision) is the JSON-RPC error object. Messages never carry a secret, a stack or a
value of a protected field.

A tool error of an **extension tool** (section 5) carries the stable `code` of the service's own catalogue
when the refusal is specific to the tool (`retrieval.*`, `agent_memory.*`, `remedies.*`, `remedies_host.*`),
and a native code otherwise: `permission.denied`, `api.invalid_request` (arguments that do not validate),
`api.rate_limited` (with the retry delay), `http.timeout` (the deadline), `server.unavailable` (the service
stopped or degraded while the call was in flight). The `explanation` is a fixed, user-safe sentence per code:
it never repeats an argument, a document value, a plan parameter, a key or a path. A call to an extension
tool that is not listed for the caller, or whose service is off, is the unknown-tool refusal of any unlisted
name.

## 7. Audit

Every call writes an AI audit record (principal, tool, outcome, agent identity and tier), **allowed or
refused**. The record never carries the arguments (they may hold business data or a secret). A failed audit
write **fails a write-capable call closed**; a read-only call may proceed.

Extension tools follow the same rule: `retrieve`, `recall_memory`, `remedy_list` and `remedy_dry_run` are
read-only for this purpose (`remedy_dry_run` changes no data; its own plan record is the host's concern, see
section 5), and `remedy_apply` and `remedy_undo` are write-capable, so a failed audit write refuses them. The
record names the tool and carries the principal, the agent identity and tier, and the outcome
(allowed, refused or failed); **never** a query, a plan, a parameter, a snippet, a memory text or an undo token. The
remedy host writes its own record to the audit chain before an apply or an undo runs (section 5); that record
is in addition to this one, never instead of it.

## 8. Out of scope

Agent tiers themselves, writing to agent memory, creating or editing remedies, a server-initiated stream, resources and prompts
beyond what `tools/list` offers. Agent memory and remedies are reachable only through the extension tools of section 5.

## Known gaps in `spot-mcp` against this document (for the serving task)

1. The `Origin` check of section 4 is not in `spot-mcp::http` yet (`WP-AI-04`).
2. A missing credential is answered with a transport error code that is not `auth.required`/`401` with
   `WWW-Authenticate`; the adapter must map it as section 3 says.
3. `jobs.cancel` does not exist (not promised).
4. The extension tools of section 5 (the rule and the six tools) are specified from contract 0.27.0 but are **not
   served** until `spot-mcp` has the extension seam (WP-AI-04.T6) and the services register their tools
   (WP-AI-04.T7). Until then `tools/list` holds only the generated tools and `job_status`, and a client must
   treat `retrieve`, `recall_memory` and the `remedy_*` tools as absent.
5. `spot-mcp` marks a tool error with an `mcp.*` kind (`mcp.permission_denied`, `mcp.invalid_arguments`,
   `mcp.rate_limited`, `mcp.timeout`) where section 6 promises the native code. The serving task maps the kinds to
   the native codes of section 6 and lets an extension tool's own service code through unchanged.
