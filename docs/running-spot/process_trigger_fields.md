Part of [Running Spot](../running-spot.md).

# Process trigger fields

A process definition's trigger node can state who may start the process, what the start input
must look like, when a document event counts, and how a schedule behaves. This page is for the
person who writes the definition. The fields are part of the definition text, so they travel with
the process between sites and are checked when the process is **published**, not when the first
start arrives. The runtime side is described in [process triggers](process_triggers.md) and the
[process API](process_api.md).

| Field | On which trigger | Meaning |
| --- | --- | --- |
| `roles` | `manual`, `api` | A caller must hold one of these roles to start the process. |
| `input_schema` | `manual`, `api` | The start input must satisfy this JSON Schema (written as JSON text). |
| `condition` | `document_event` | The event starts a Run only when this expression is true for the document. |
| `zone` | `schedule` | The IANA time zone the cron expression is read in (default `UTC`). |
| `missed` and `catch_up_max` | `schedule` | What the schedule does with firings it missed. |

A field on a trigger type that does not read it is refused at publish
(`bpm_model.trigger_field_misplaced`), so a typo about where a setting belongs is caught.

## `roles`

```toml
[[nodes]]
id = "start"
kind = "trigger"
trigger = "manual"
roles = ["purchaser"]
```

A caller who holds none of the roles gets the same answer as for a process that does not exist
(`process.not_found`), so the list of processes in a site is not revealed to people who may not
start them. The refusal is audited (target `spot::audit::process`, outcome `process.not_found`).
No Case is created. Leave `roles` out and any signed-in person may start the process. At most 32
roles, each 1 to 128 bytes with no whitespace, no duplicates.

## `input_schema`

```toml
[[nodes]]
id = "start"
kind = "trigger"
trigger = "manual"
input_schema = '{"type":"object","required":["amount","supplier"],"additionalProperties":false,"properties":{"amount":{"type":"integer","minimum":1,"maximum":1000000},"supplier":{"type":"string","minLength":1,"maxLength":64}}}'
```

Why the schema is **JSON text** and not a TOML table: a TOML table cannot carry JSON `null`, and
the model keeps one canonical byte form for hashing. Write the JSON between single quotes, which
TOML reads literally.

Rules the model checks at publish (`bpm_model.trigger_field_invalid`): at most 16 KiB, a JSON
object, nested at most 32 levels, and **no floating-point number anywhere** (a number with a
fraction or an exponent is refused; platform rules keep money, quantities and time out of
floats). Write bounds as whole numbers.

Keywords the runtime enforces: `type`, `required`, `properties`, `additionalProperties` (`true` or
`false`), `enum`, `items`, `minimum`, `maximum` (whole numbers), `minLength` and `maxLength`
(characters). `pattern` and every other keyword are **not** enforced, so a schema that uses one is
refused at publish (`trigger_invalid`) rather than silently accepting any input.

A start with bad input is refused with `process.input_invalid` listing every field problem, not
only the first: the field path, a stable code (`process.input_required`,
`process.input_unknown_field` or `process.input_invalid`) and a plain sentence. The message never
repeats what the caller sent. No Case is created.

## `condition`

```toml
[[nodes]]
id = "start"
kind = "trigger"
trigger = "document_event"
doctype = "Sales Invoice"
event = "post"
condition = "doc.total > 1000"
```

The condition is an expression in the shared expression language over the document as the
platform reads it (`doc`). It is parsed at publish; a condition that does not parse is refused
with the node id, never the text. An event whose document does not meet
the condition starts nothing and is not a refusal. A redelivery of an event that did start a Run
starts nothing more.

## `zone`

```toml
[[nodes]]
id = "start"
kind = "trigger"
trigger = "schedule"
cron = "0 0 9 * * *"
zone = "Asia/Karachi"
```

The cron expression (six fields, seconds first) is read in this zone, so the example fires at
09:00 in Karachi, five hours before 09:00 UTC. Without `zone` it is UTC. A name that is not an
IANA zone is refused at publish (`bpm_triggers.schedule_invalid`).

## `missed` and `catch_up_max`

```toml
missed = "catch_up"
catch_up_max = 5
```

`missed` says what a schedule does with firings that fell while no node was leading it:
`run_once` (the default) runs once for all of them, `catch_up` runs each missed firing up to
`catch_up_max` (1 to 1000, required with `catch_up`, refused with anything else). `skip` is
**refused**: the scheduler's planning drops the whole window under `skip`, on-time firings
included, so a `skip` schedule would never fire.

## A webhook's signing secret is not part of the definition

A webhook trigger names only its `path`. The signing secret lives in site configuration
(`[services.process_triggers.webhooks.<path>]`), because a definition travels between sites and a
secret must not. Publishing a process whose webhook has no configured secret is refused.

## What the author sees at publish

| Mistake | Refusal |
| --- | --- |
| `input_schema` is not JSON, not an object, too big, too deep or has a float | `bpm_model.trigger_field_invalid` at the trigger node |
| `condition` does not parse | `bpm_model.trigger_field_invalid` at the trigger node |
| `roles`, `condition`, `zone` on a trigger type that does not read them | `bpm_model.trigger_field_misplaced` |
| A schema keyword the runtime cannot enforce (for example `pattern`), an unknown zone, `missed = "skip"` | `trigger_invalid` |

A refused publish stores nothing and changes nothing: the live version keeps serving. Republish a
corrected text to replace the live triggers; a role removed in a new version takes effect at once
for the next start.
