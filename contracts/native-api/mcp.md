# The MCP endpoint (contract 0.24.0, WP-CON-03.T53)

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

## 6. Errors

A native API error appears as an MCP **tool error** (`isError: true`) with the same stable `code`
(section "Errors" of `openapi.yaml`) and a user-safe `explanation`. A document that does not exist and one the
caller may not read are the same not-found as over REST. A protocol error (unknown method, malformed
JSON-RPC, unsupported revision) is the JSON-RPC error object. Messages never carry a secret, a stack or a
value of a protected field.

## 7. Audit

Every call writes an AI audit record (principal, tool, outcome, agent identity and tier), **allowed or
refused**. The record never carries the arguments (they may hold business data or a secret). A failed audit
write **fails a write-capable call closed**; a read-only call may proceed.

## 8. Out of scope

Agent tiers, remedies and memory (behind milestone M9), a server-initiated stream, resources and prompts
beyond what `tools/list` offers.

## Known gaps in `spot-mcp` against this document (for the serving task)

1. The `Origin` check of section 4 is not in `spot-mcp::http` yet (`WP-AI-04`).
2. A missing credential is answered with a transport error code that is not `auth.required`/`401` with
   `WWW-Authenticate`; the adapter must map it as section 3 says.
3. `jobs.cancel` does not exist (not promised).
