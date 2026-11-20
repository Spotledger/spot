Part of [Running Spot](../running-spot.md).

# The MCP endpoint

`spot serve` answers the Model Context Protocol on `POST /mcp` of every site's host, so an agent (a
person's own Claude, a script, a support tool) can read and draft documents **as the signed-in user,
with that user's rights and no more**. It is on by default. The protocol is documented in
`contracts/native-api/mcp.md`; this guide is for the operator.

Level: **optional**. If its start-up check fails (the site's audit store is not prepared) the site
keeps serving documents, `/api/health/ready` says `degraded: mcp (...)` and `/mcp` answers
`503 server.unavailable` for every request. There is no unaudited path.

## Settings

`[services.mcp]` in `common.toml`. Every key is optional.

| Key | Default | Meaning |
| --- | --- | --- |
| `enabled` | `true` | `false` answers `404` on `/mcp`. |
| `allowed_origins` | `[]` | Browser origins allowed besides the site's own, `https://app.example` (scheme, host and port only). |
| `call_timeout_seconds` | `30` | Deadline of one call, 1 to 300. A call past it is cancelled and holds no connection. |
| `rate_per_minute` | `120` | Calls per minute per principal. |
| `source_rate_per_minute` | `600` | Requests per minute per source address, counted before the credential is checked. |
| `site_requests_per_minute` | `6000` | Requests per minute for the whole site on this route (the host's open-path limit). |
| `max_request_bytes` | `262144` | Largest request body; 1,024 up to the contract's 256 KiB. Larger is `413`. |

A change needs a restart. An unknown key or an out-of-range value stops the site start and names the
key (never the value).

## Credentials and origins

- `Authorization: Bearer <session token or OAuth access token>`, or `X-Api-Key: <key id>:<secret>`.
  Nothing else; a token in the URL is never read. Both headers at once are refused.
- Missing, malformed, expired, revoked, locked-out and other-site credentials are all `401` with
  `WWW-Authenticate: Bearer` and the same body shape, so the answer tells an attacker nothing. A user who
  has not yet chosen a password gets `403`.
- A request with an `Origin` header is allowed only when it is the site's own host or in
  `allowed_origins`; otherwise `403` before anything else is read (DNS-rebinding protection). Agents that
  send no `Origin` are unaffected. Only `POST` is answered (`405` otherwise); there is no stream.

An API key acts with its owner's rights, narrowed by the key's recorded scope when a scope store is
installed (see the hooks below). Create the key for a service user whose own rights are already narrow:

1. Create a service user with only the roles the agent needs.
2. Issue it a key (`spot` admin tools or the keys screen) and keep the `spk_...` text; it is shown once.
3. Give the agent `X-Api-Key: <id>:<secret>`, where `<id>` and `<secret>` are the two parts after `spk_`
   (`spk_<id>_<secret>`).

## A sample session

```sh
curl -s https://acme.example/mcp \
  -H 'Authorization: Bearer '"$TOKEN" -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}'
curl -s https://acme.example/mcp \
  -H 'Authorization: Bearer '"$TOKEN" -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"get_task","arguments":{"id":"<record id>"}}}'
```

The first lists only the tools this user may use; the second reads one document, or answers the same
not-found a REST read would.

## Audit and limits

Every call is appended, allowed or refused, to the `mcp-<site>` audit chain of the site's database with
the principal, the tool, the outcome, the agent identity and tier and the key id; never the arguments. If
the append fails, a call that can change data fails closed and a read goes on while an alert event and the
`mcp_audit_failures_total` metric record it. Verify the chain with the audit tools as for any other.

Rows (500), query depth (32) and nodes (10,000), the request size and the per-call deadline are the
contract's; exceeding one is a tool error, never a silently shorter result.

## For other services

The module provides a port, `McpHandle` (in `crates/framework/spot-cli/src/services/mcp/seams.rs`), so
other hosting modules can plug in without editing this one: `install_directory` (the agent tier service:
an agent's tier is its stored registration, never a header or an argument), `install_admission` (AI
isolation: calls are admitted or shed with `429` and `Retry-After`; with `required`, an absent admission
makes `/mcp` answer `503`) and `install_key_scopes` (a store of API key scopes).

## Known limits

- The function registry served here is empty until the `function` module exposes its registry, so there
  are no `call_*` tools yet. Document tools, `query` and `job_status` work.
- On the in-memory job store (`services.jobs.store = "memory"`) long operations cannot be run; the
  service says so in the log at start.
