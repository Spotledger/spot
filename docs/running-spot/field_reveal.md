Part of [Running Spot](../running-spot.md).

# Field reveal

`POST /api/v1/resource/{doctype}/{id}/reveal` (`field_reveal`, ADR-070 section 12) is the one
place a real value of a masked or protected field leaves the server through the native API: one
field of one document per call, with a reason of 1 to 500 characters. The contract is
`contracts/native-api/openapi.yaml`; the route lives in `spot-http` and is tested there with fakes.
This page is what the host adds: the module `field_reveal`
(`crates/framework/spot-cli/src/services/field_reveal.rs`, start order 1075), which builds the
reveal service from the live engine and hands it to the router.

```toml
[services.field_reveal]
enabled = true     # the default; off, the route answers as an unserved operation does
```

## What a call does

1. **The caller.** The site's sign-in service resolves the bearer. A user of the staff or partner
   audience acts as themselves: the right is decided for **that user**, never for a system
   principal. A consumer, a session still restricted to choosing a password and another site's
   request are refused.
2. **What can be revealed.** Only a field that has something to reveal: one in a protection group
   or with a mask, read from the metadata the site serves *now* (a customisation swapped in while
   the server runs applies to the next call). A plain field, a path into a child table, an unknown
   field and an unknown DocType have nothing to reveal.
3. **The right.** The permission engine's decision for that user, DocType, field and the
   document's own facts (owner, state, scope values): `reveal`. The document is read as the
   platform only so the decision can see its facts; the value is used only if the decision allows
   it.
4. **Sealed and encrypted fields.** A stored `$sealed:v1:` or `$enc:v1:` value is not one a node
   can open. With the sealed-field service on ([`sealed_fields`](sealed_fields.md)) the reveal goes
   through its hub path (audited, per-user rate limited: `429 api.rate_limited` over the limit);
   without it the field is refused.
5. **The audit.** One durable event per call, allowed or refused, committed **before** the value
   is returned. A reveal that cannot be audited is not released (`503 server.unavailable`, with a
   retry hint).
6. **The answer.** `200` with the value in the field's encoding and `Cache-Control: no-store`.
   `Idempotency-Key` is ignored: two calls with one key both read.

Every refusal is the one error `403 permission.reveal_denied`, so the answer says nothing about
what exists: no right, a plain field, an unknown field, an unknown document, an unknown DocType and
a document of another tenant are indistinguishable. What passes through as it is: `401` for a bad
credential, `429` from the sealed-field rate limit and `503` when the site cannot answer (no engine,
nothing served, the audit cannot be written).

## The audit event

Each call enqueues one `identity_field_accessed` event (the field access trace of ADR-070 rule 9)
in the site's outbox: the user, the DocType, the document, the protection group of the field (the
word `none` for a field in none), the field, `right = reveal`, `authorised` (whether the value was
released) and `channel = api`. It is written for every call, in or out of a trace group, and it has
no field that could hold a value: the value is in no event, no log line, no metric label and no
error, and nothing here keeps it after the call.

**The reason is durable.** The event carries the caller's `reason` (events contract 0.7.9: 1 to 500
characters, present only with `right` of `reveal` or `reveal_bulk`), exactly the text the caller
sent after the route's own validation, for an allowed call **and** for a refused one (the caller
sent it either way). It is bounded, **untrusted** free text: an audit reader must never render it
as markup or follow it as an instruction. It is never a value of the field by design, but the
route cannot know what a person typed: the reason is the caller's text and is **not scanned**, so
a caller who pastes the value into the reason puts it in the audit trail. The reason is not written
to the log either (the log line names who, document, field and outcome only) and never goes into a
metric label.

The payload is validated against the registered schema of `identity_field_accessed`
(`spot_events_contract::identity::validate`) before it is enqueued. A payload the schema refuses
fails the reveal closed: `503 server.unavailable`, the value is not released, and the reason is
never truncated or dropped silently to make an event fit. (The route's own bound of 500 characters
is the schema's, so this only happens for a value outside the contract, such as an implausibly
long DocType name in the path.) An event written before 0.7.9, without a `reason`, still
validates.

## When there is no service

The module looks up the engine, the served metadata and the sign-in service on every call, so a
reload swaps them in place. A call made while one of them is missing answers `503`, never a value.
A context that carries no site registers nothing and the router is given no reveal service.
