Part of [Running Spot](../running-spot.md).

# Process triggers

The trigger intake (`crates/framework/spot-cli/src/services/process_triggers.rs` and its
submodules, `ORDER = 1130`) is what starts a Case. `spot-bpm-triggers` matches a source event
against the published process versions and starts exactly one Case per
`(process, trigger node, source event)`; this module feeds it from three places and hosts it over
the process engine (`process`, `ORDER = 1100`). It is an **optional** service: a broken intake
degrades readiness and the processes it would start, never document serving.

## What starts a Case

| Source | How | One Case per |
| --- | --- | --- |
| Document event (`doc_created`, `doc_updated`, `doc_deleted`, `doc_posted`, `doc_cancelled`, `doc_renamed`, `doc_updated_posted`) | The lease holder reads the site's outbox from a durable position (the table `spot_bpm_trigger_mark`) | event id |
| Schedule | A tick every 15 s plans the due slots of every published schedule trigger | slot key |
| Webhook | `POST /api/v1/process/hook/{trigger}`, the one open path of this module | delivery id, else the body hash |

Manual and API starts are the process API's (`process_api`), not this module's.

## When the trigger set changes

The module provides itself as a port (`TriggerReload`). A publish or a retire in `process_versions`
calls it as soon as the version is stored, so a new, changed or retired trigger takes effect when
that call returns; the 15 s pass runs the very same reload and is only the backstop (a site where
`process_versions` is off, a failed reload, a node that did not take the call). A reload that finds
the published set unchanged does nothing; a reload that fails leaves the previous set in force and
is returned to the caller. Reloading twice never starts a second schedule slot (the slot key is the
dedup key), and one site's reload never reads or changes another site's triggers.

Every Case id is derived from the start key, so a redelivery, a second node or a restart finds the
Case that is already there and starts nothing.

## Document events

- The document values the trigger judges are **read from the platform** (as the platform
  itself), never taken from the thin event payload. A document the platform cannot show denies
  the start (rule `document_unreadable`); a read that fails is retried a few passes with the
  outbox position held, then denied.
- **State completeness.** The set of doctypes with a lifecycle state is taken from the metadata
  served now, for every event, so a doctype added by a metadata reload is covered without a
  restart. Metadata that cannot be read denies every document start (`metadata_unreadable`).
- A document start is judged as the event's actor; a non-user actor is the platform.

### No recursion

A Document node writes with the command id `<case>:<node>:<attempt>`, which the outbox event
carries. A trigger never starts on an event whose origin Case runs **the same process version**:
the start is refused and audited (`recursion`). A process that updates the document it triggers on
therefore runs once.

### Start rate

At most `start_rate_per_minute` new Cases per process per clock minute. A start over the limit is
refused and audited (`rate_limited`), and readiness carries a warning for five minutes.

## Schedules

A schedule seen for the first time is marked and fires from its next slot. The missed-run policy
is `spot-scheduler`'s (`RunOnce` for a trigger node today). Two nodes ticking one slot start one
Case. A newly published version is picked up on the next pass; no restart is needed.

## Webhooks

The request is judged in this order and every refusal is the same `WebhookRejected` after the same
work (the MAC is always computed, with a decoy key when no secret is usable), so status, body and
headers do not tell a caller why:

1. the trigger name must be `^[a-z0-9][a-z0-9_-]{0,63}$` and be configured and published;
2. the secret is opened from the site's sealed secrets **by reference**;
3. the signature (`X-Spot-Signature`, `t=<unix>,v1=<hex>`) is verified over the **raw body**
   before anything is parsed, and a timestamp outside `webhook_tolerance_secs` is refused;
4. the delivery id (header, 1 to 128 of `[A-Za-z0-9._:-]`) is validated, and the body must be a
   JSON object.

A trigger with no configured secret is **disabled**, and readiness says so.

## Configuration

```toml
[services.process_triggers]
enabled = true                  # false: inert
start_rate_per_minute = 100     # 1 to 100000
batch = 100                     # outbox events per read, 1 to 500
webhook_tolerance_secs = 300    # 30 to 3600
webhook_rate_per_minute = 600   # the open path's per-site limit

[services.process_triggers.webhooks.intake-1]   # the trigger node's `path`
secret = "intake-secret"                        # a reference, never the secret

[services.process_triggers.secrets.intake-secret]
sealed = "spsec:v1:..."         # sealed under [services.secrets]
aad = "process_triggers:intake-1"
```

Unknown keys are refused at start-up.

## Audit and logs

The log target `spot::audit::process_triggers` records, for every start and every refusal, the
site, the source (`document`, `schedule`, `webhook`), the outcome, the rule, the process, the
node and who (`user`, `platform`, `caller`). It never records a document value, a body, a
signature, a secret or a delivery id.

## Health

`process_triggers` is `Optional`. It reports: published versions whose trigger does not load,
webhook triggers without a usable secret, a recent rate limit hit, and a failing intake pass.
