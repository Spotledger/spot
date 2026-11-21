Part of [Running Spot](../running-spot.md).

# The AI audit trail (`[services.ai_audit]`)

Every act of an AI agent on a site is a tamper-evident chain entry: a tool call, an approval, a rejection, an undo. The entry names the agent and the user it acts for, the tool, the outcome and the **masked** arguments. It never holds a prompt, a model output or a tool result. The service also reads the entries back as a document's AI timeline and produces a periodic review digest for the tenant's reviewer.

## What exists, and what this service adds

| piece | state |
|---|---|
| AI action entries on the chain | `services/ai_audit.rs`: `AiAuditPort::append`, a chain of its own (scope `ai:<site>`) on the shared audit store, verified by the same verifier as every chain |
| Document timeline entries | `AiAuditPort::timeline`: the entries of one document, filtered and masked for the viewer |
| Review digest | the service's loop; the newest is `AiAuditPort::latest_digest` |
| The timeline route | `GET /api/v1/resource/{doctype}/{id}/ai-timeline` (`resource_ai_timeline`, contract 0.28.0), served when the service is on: see "The timeline route" |
| Document-change lanes (`spot-doc-audit`) | **not hosted by this service**: the lane appender has no host in `spot serve` yet (WP-SEC-06.T8) |

## Settings

```toml
[services.ai_audit]
enabled = false                 # off by default
digest_interval_seconds = 3600  # how often the loop looks for a period needing its digest; at least 60
digest_period_seconds = 86400   # the length of one period; at least 3600
spike_factor = 5                # an agent above this multiple of its previous period is an anomaly; at least 2
spike_min_actions = 20          # the fewest actions that can be an anomaly; at least 1
dedupe_window = 10000           # recent event ids remembered for exactly-once; 100 to 1,000,000
```

An unknown key or an out-of-range value is refused when the site starts.

## Level and failure

The service is **optional**: a failing start-up check or a stopped loop degrades the site and `/api/health/ready` says so. The append is different: `append` returns an error when the entry is not durable and **the agent runtime must then refuse the action**, so nothing the AI does goes unrecorded.

## Exactly once

Every action has an event id. An id already among the last `dedupe_window` entries answers `Duplicate` and writes nothing, so a retry after a restart or a timeout is safe. The digest of a period is the entry with event id `digest:<period>`, so it is written once.

## Masking

Arguments pass through the `ArgumentMask` before anything is written: a hidden value is a marker, a value over 128 bytes is its length and SHA-256, and the default mask hides any argument whose name contains `password`, `secret`, `token`, `key`, `credential`, `auth`, `iban` or `card`. A timeline then masks again for the viewer: the viewer's access is `None` (an empty answer, exactly like a missing document), `Masked` (every value shows as `***`) or `Full`.

The access comes from the `ReadAccess` port. A provider another module installs wins; otherwise the host uses the **engine-backed** one (`ai_audit/read_access.rs`, WP-AI-11.T9), which decides as the **viewer**, never as the platform:

| Access | When | The timeline shows |
|---|---|---|
| `None` | the viewer may not read the document, the document does not exist, the id is not a record of this site (another tenant's included), the DocType is unknown, or the viewer is not a user or agent id | nothing: an empty list, the same answer in every one of these cases |
| `Masked` | the viewer may read the document but the field guard does not show at least one protected field as its real value (it is masked or hidden) | the entries with every argument value hidden (`***`) |
| `Full` | the viewer may read the document and every protected field | the entries as recorded (values the write-time mask hid stay hidden) |

The viewer text is `<user id>`, `user:<user id>` or `agent:<id>`. An **agent is judged by its own roles**: the id after `agent:` is the agent's own user record, so an agent never borrows the rights of the person it acts for. The document is loaded through the live engine with the viewer's own invocation (role grants, user permissions, shares and conditions of the decision core served now), then dropped unread; the field decision is the guard's on the version-history channel. Only the decision is kept: no field value is read into an answer, a log line or an error, and a permission revoked a moment ago applies to the next timeline.

An engine that is not running, a store or guard failure, or a DocType the guard has no facts for makes the access check fail with a fixed sentence, and an error from the access check is a refusal (`ai_audit_host.timeline_refused`), never an allow and never a list of entries. Only a site whose name is not a valid site id keeps the `DenyAll` access.

## The timeline route

`GET /api/v1/resource/{doctype}/{id}/ai-timeline?limit=N` answers the AI entries of one document for the **caller**, newest first. It exists on a site only while `[services.ai_audit] enabled = true`; with the service off the operation is unserved, like any operation of a service that is not on.

**Who sees what.** The route has no role or permission of its own. The viewer is the authenticated caller and nobody else: the bearer is authenticated on every call, and the viewer is that user's record id (an API key authenticates as its owner; a user of type agent keeps the spelling `agent:<id>` and is judged by its own roles). Nothing in the path, the query or a body can name another viewer. Only the staff and partner audiences are served; a consumer, a credential still restricted to choosing a password and a request for another site are `403 permission.denied`, a missing or invalid bearer is `401`. The access itself is the engine-backed `ReadAccess` above, so the route adds no access logic.

| What the caller may do | The answer |
|---|---|
| read the document and every protected field | `200` with the entries as recorded |
| read the document but not every protected field | `200` with the entries, every argument value `***` (names, kind, times, agent, user and outcome still show) |
| not read the document, or the document, the DocType or the id does not exist or is malformed | `200` with `{"data": {"entries": []}}`: the same answer in every case, never a `400` |
| (the access cannot be decided, or the chain cannot be read) | `503 server.unavailable` with `Retry-After`: never entries, never an empty list that would read as "no access" |

The three `200` answers are told apart by the entries only: the answer carries no count, no hint of hidden entries and no access level. `limit` is 1 to 100 (default 50); anything else, a repeated `limit` or any other query parameter is `400 api.invalid_request`, refused before the audit service is asked. There is no cursor: a client that needs more reads the audit chain through the audit export. Every answer, errors included, is `Cache-Control: private, no-store`.

An entry has the contract's `seq`, `at` (the chain's instant as RFC 3339 UTC), `kind`, `agent`, `acting_user`, `tool`, `outcome` and at most 32 `arguments` of a name and a value (a value over 128 characters is shown as `***`). It never carries a prompt, a model output, a tool result or a value of the document. An entry whose instant cannot be written as RFC 3339 makes the answer a `503`, not a clipped list. Nothing logged carries a document id, a user, an argument name or a value: only the stable `ai_audit_host.*` code.

## The review digest

Built from chain entries only: counts of calls, approvals, rejections, undone actions and refused or failed actions for the last complete period, and an anomaly for each agent whose count is above `spike_factor` times the previous period's (and at least `spike_min_actions`). It is appended as an `ai.digest` entry. A notice to the reviewer is not sent yet (no notice port exists).

## Errors

Stable codes in `crates/framework/spot-errors/catalogue/ai_audit_host.toml` (`append_failed`, `record_invalid`, `timeline_refused`, `chain_unavailable`, `digest_failed`). A message never carries an argument value.
