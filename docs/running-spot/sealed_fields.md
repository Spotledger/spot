Part of [Running Spot](../running-spot.md).

# sealed_fields

Hosts the node side of sealed and encrypted protected fields (ADR-070 rule 12.13): the rules built
from the sync-class table, the plaintext check on every document write, and reveal through the hub
with a per-user rate. Off unless `[services.sealed_fields]` has `enabled = true`. It reads the
`protected_keys` module's class table, so it starts after it (order 1070).

```toml
[services.sealed_fields]
enabled = true
reveal_per_minute = 30
```

Needs, from the ports of earlier modules: the class table, the served protection facts and the hub's
public sealing key; optional: the node's group keys (without them an `encrypted` group refuses the
start) and the hub port (without it reveal is offline). The service is required.

## What a field is stored as

| Class | Stored form | Who reads it back |
| --- | --- | --- |
| `sealed` | `$sealed:v1:` and a box to the hub's key, bound to tenant, DocType, field and record | nobody on a node; the hub through the audited reveal |
| `encrypted` | `$enc:v1:` and the group-key ciphertext | a holder of a live group key lease |
| `everywhere`, `home_only` | unchanged (home-only stays at the hub through sync) | as today |

A child-table column is bound to the parent record **and the row**. The stored form is text: a
sealed or encrypted field must be a text field, and a typed value there is refused
(`sealed_fields_host.field_not_text`). A derived field in several groups takes the strictest class.

## What the site does today

Every write that still holds a plaintext in a sealed or encrypted field is refused
(`sealed_fields_host.plaintext_refused`) before anything is stored. The engine's participant seam
lends the write read-only, so the transform that replaces the value (`SealedFields::seal_document`)
has no caller until the engine gets the seam below; until then a protected field cannot be written
on a site that has this module on. That is the safe failure and is reported in the readiness text.

### Seams still needed (outside this module)

1. **Write transform.** A hook the engine calls after `check` and before the participants'
   `before_write` with `&mut Document`, for example `Engine::set_field_transform(Arc<dyn FieldTransform>)`
   (`fn before_store(&self, doc: &mut Document, invocation: &Invocation) -> AppResult<()>`), whose
   host implementation calls `SealedFields::seal_document`.
2. **Read mask.** A hook on every read of a document (`get`, list rows, report rows, export, print,
   version history, journal view, search, realtime payload) replacing a stored sealed envelope by
   the masked form, so even a user with the `read` right never receives the box. The field guard
   already answers for the channels it covers; the engine's own read path needs the same call.
3. **Reveal route.** A route in the registry (a port trait file and a route file in `spot-http`)
   that calls `Revealer::reveal` and answers `sealed_fields.reveal_offline` offline.
4. **Node-key encryptor.** A provider of `dyn GroupEncryptor` over the node's `NodeKeyStore` (newest held epoch of the group, `EncryptedStore::write` with a `FieldAd` of tenant, DocType, field, record and epoch); until a module provides it an `encrypted` group refuses the start.
5. **Participant install.** `participants::build` takes the `dyn TxParticipant<PgTransaction>`
   port this module provides and places it before edge sync.

## Reveal

Reveal is online-only. Offline the answer is `sealed_fields.reveal_offline` and no cached plaintext
is ever served. Each call counts against the user's `reveal_per_minute` before the hub is asked (a
refused call answers `sealed_fields_host.rate_limited` and is offered to the audit adapter). The hub
audits every reveal it answers (field name and document, never the value).

No key, sealed blob or value appears in an error, a log line or an audit record: field names, codes
and counts only.
