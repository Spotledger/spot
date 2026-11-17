Part of [Running Spot](../running-spot.md).

# The AI audit trail (`[services.ai_audit]`)

Every act of an AI agent on a site is a tamper-evident chain entry: a tool call, an approval, a rejection, an undo. The entry names the agent and the user it acts for, the tool, the outcome and the **masked** arguments. It never holds a prompt, a model output or a tool result. The service also reads the entries back as a document's AI timeline and produces a periodic review digest for the tenant's reviewer.

## What exists, and what this service adds

| piece | state |
|---|---|
| AI action entries on the chain | `services/ai_audit.rs`: `AiAuditPort::append`, a chain of its own (scope `ai:<site>`) on the shared audit store, verified by the same verifier as every chain |
| Document timeline entries | `AiAuditPort::timeline`: the entries of one document, filtered and masked for the viewer |
| Review digest | the service's loop; the newest is `AiAuditPort::latest_digest` |
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

The access comes from the `ReadAccess` port. **Without a provider the host uses `DenyAll`**, so the timeline answers nothing until the engine-backed provider is wired (an open item of WP-AI-11.T8). An error from the access check is a refusal (`ai_audit_host.timeline_refused`), never an allow.

## The review digest

Built from chain entries only: counts of calls, approvals, rejections, undone actions and refused or failed actions for the last complete period, and an anomaly for each agent whose count is above `spike_factor` times the previous period's (and at least `spike_min_actions`). It is appended as an `ai.digest` entry. A notice to the reviewer is not sent yet (no notice port exists).

## Errors

Stable codes in `crates/framework/spot-errors/catalogue/ai_audit_host.toml` (`append_failed`, `record_invalid`, `timeline_refused`, `chain_unavailable`, `digest_failed`). A message never carries an argument value.
