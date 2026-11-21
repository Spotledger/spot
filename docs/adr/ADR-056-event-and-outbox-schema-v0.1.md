# ADR-056: Event and outbox schema v0.1

- **Status:** Proposed by the lead, 29 Sep 2026. Awaits the architecture owner; the crate is frozen once this PR merges.
- **Date:** 29 Sep 2026
- **Work package:** WP-CON-04.T1
- **Builds on:** ADR-11, ADR-23, ADR-34, ADR-51, ADR-053 (outbox and change feed), ADR-055 (value encoding, idempotency keys)
- **Code:** `contracts/events/` (crate `spot-events-contract`, version 0.1.0)

## Context

ADR-053 fixed how business events are **captured and delivered**:

- an `OutboxEvent` with an id, a topic, a subject, the subject's version, a `schema_version` and a JSON `payload`;
- publication with safe bookmarks, transaction order and causal order;
- at-least-once delivery.

It deferred the rest to this work package:

- `source_id`, command id and causation id;
- payload schemas;
- the rules for evolving them.

Consumers waiting on this:

- the PostgreSQL outbox relay (WP-STO-08);
- the job engine and its consumer framework (WP-PLT-06);
- the `*_committed` phase of ADR-51;
- integrations and webhooks;
- branch sync (ADR-23).

## Decision

### The storage contract is unchanged
Everything new lives inside the existing `OutboxEvent.payload` text, or is added by the relay when an event leaves its database. The storage contract stays at 0.1.0.

### Payload document
`payload` is always the JSON document `{"meta": EventMeta, "data": {...}}`.

- `meta` carries the same fields for every topic, so a consumer can route, trace and deduplicate without knowing the topic:
  - `occurred_at`: the business transaction's time, RFC 3339 UTC.
  - `actor`: `{"kind": "user", "id"}` or `{"kind": "system", "component"}`.
  - `command_id`: optional. It is the native API's `Idempotency-Key` (ADR-055), or an id a job assigns, so a retried uncertain command is recognisable end to end.
  - `causation_id`: optional; the event whose handler produced this one.
  - `correlation_id`: the root of the chain. A root event's correlation id is its own event id.
  - `trace_id`: optional W3C trace id.
- `data` is a JSON object in the **native API value encoding**: decimals as strings, dates `YYYY-MM-DD`, UTC timestamps with `Z`, canonical UUIDv7 ids. An event payload and an API response therefore never disagree about how a value is written.

### External envelope
Outside the source database, consumers see an `EventEnvelope` with these fixed field names:

- `event_id`, `topic` and `schema_version`;
- `source_id`, `position {generation, seq}`, `transaction` and `ordinal`;
- `subject {entity, id}` and `subject_version`;
- `meta` and `data`.

The relay builds it from an `OutboxRecord`, and it is the only place `source_id` is added.

`source_id` names the producing database: `<site>`, or `<site>#<partition>` for a branch edge (ADR-23, ADR-34). Positions, transaction ids and document versions are **only comparable within one source**. That is the rule ADR-053 set for positions, and it now holds for every ordered field of the envelope. `tenant_id` is not a field, because a tenant is a site (ADR-34).

### Topics
- A topic is a storage name (`[a-z][a-z0-9_]*`, at most 63 bytes), because it is stored in the outbox's `topic` column.
- The prefixes `doc_` and `sys_` are reserved for the platform. App topics are checked for collisions at install (WP-EXT-03).
- The lifecycle topics are the ADR-51 phase-4 events: `doc_created`, `doc_updated`, `doc_posted`, `doc_cancelled`, `doc_deleted`, `doc_renamed` and `doc_updated_posted`.
  - The document engine enqueues one in the same transaction as each successful operation.
  - `*_committed` handlers consume them, and `save_committed` subscribes to both `doc_created` and `doc_updated`.
  - They carry a minimal v1 payload: `name`, `changed_fields` for updates, and `old_name`/`new_name` for renames. Consumers read the document for anything else.
  - The same names are the realtime event kinds of ADR-055.

### Payload schemas and evolution
- **Schemas.** Each topic has an `EventSchema` per **major** version. The major is carried on the wire as `schema_version`; ADR-053 already stores it. A **revision** within the major is recorded only in the registry.
- **Field types** follow the value encoding: bool, int, decimal, text, date, datetime, id, json, enum, list and object.
  - An enum is declared `open` or closed.
  - A closed enum may never gain a value within a major version, because consumers may match it exhaustively.
- **Validation is strict** for producers and tests: unknown fields, a JSON number for a decimal, or null in a required field are all errors. **Consumers must ignore unknown fields.**
- **Rules** (checked by `compat::check`):
  1. Within a major version only additive changes are allowed:
     - new **optional** fields;
     - deprecation marks;
     - new values of an **open** enum.

     Removing a field, changing its type, or switching it between required and optional is breaking. A new required field is also breaking, because events already stored do not have it. Every change raises the revision.
  2. A field name, once removed, is **retired** for that topic forever and never reused, including across major versions.
  3. A breaking change needs a new major version. During the deprecation window the producer publishes both majors, as separate events with separate ids, so no consumer has to understand two shapes in one event.

## Consequences

- **WP-STO-08** stamps `source_id` and builds envelopes with `EventEnvelope::from_record`. It never rewrites `payload`.
- **After a restore** (a new stream generation, ADR-053), `source_id` stays the same and `position.generation` changes. Consumers resync by generation, not by source.
- **The document engine (WP-DOC-04)** enqueues the lifecycle topics through `PayloadDocument::to_outbox_event`, carrying the request's idempotency key as `command_id`.
- **The job engine's consumer framework (WP-PLT-06)** deduplicates by `event_id`, orders per subject by `subject_version`, and propagates `correlation_id`. When a handler produces an event it sets `causation_id`.
- **App manifests (WP-EXT-04)** declare the topics they publish, with their schemas. Installing an app runs `check` against the registered schemas, so a breaking change in an app update is refused.
- **The schema registry** (topic → majors → revisions) is persisted later, with the manifest work. This contract defines only the types and the checker.

## Alternatives considered

- **Adding metadata fields to `OutboxEvent`.** Rejected. It would reopen the storage contract for data the storage never interprets. Keeping the metadata in the payload document keeps the relay a pure transport, which matters for the later CDC profile (ADR-053).
- **A per-event full semantic version (`1.3.0`) on the wire.** Rejected. Consumers only need to know the shape they must parse, which is the major version. Revisions are compatible by definition.
- **JSON Schema documents for payloads.** Rejected for v0.1. JSON Schema cannot express the value encoding's rules (a decimal must be a string with no exponent) without custom formats, nor evolution rules such as retired names. The Rust types are smaller and are checked by the compiler. An export to JSON Schema for integrators can be added later without changing the rules.
- **Dotted topics (`sales.invoice.posted`).** Rejected, because a topic must be a storage name to live in the outbox column.

## Amendment A1 (6 Oct 2026): identity events (ADR-070, WP-IAM-01)

- **Status:** Proposed by the lead on the architecture owner's direction; the actor change was approved with ADR-070 on 6 Oct 2026. Events contract 0.2.0 (additive).
- **`Actor::User` gains `via`.** `id` is the identity whose rights applied; `via` is the real person when one identity acts as another (impersonation by the support tier, ADR-070 section 9). It is omitted on the wire when absent, so 0.1 payloads still read. Code that builds `Actor::User { id }` adds `via: None`.
- **A third reserved topic prefix, `identity_`.** It joins `doc_` and `sys_`; an app topic cannot use it.
- **The identity topics** (28 in v1, listed in `contracts/events/src/identity.rs`) are the platform's audit stream for sign-in, users, roles, assignments, sessions, keys, emergency access, impersonation, the field access trace and branch-node events. Each is written in the transaction of the change it records (ADR-11).
- **No secret travels in an identity event.** `identity::validate` checks the strict schema and then scans every key at every depth, free-form `Json` fields included, against a deny list (password, passphrase, secret, token, credential, private, recovery_code, key_material, totp, pin_code). A producer that fails the scan has a bug; a harmless field that trips it is renamed.
- **The field access trace never holds a value.** It names the user, DocType, group, field, right, whether the access was authorised and the channel.
- Consumers (journal, sync, audit, reviews) treat `identity_*` like any topic; branch sync routing of these topics is for the sync ADRs (ADR-062 to ADR-067) and ADR-070 rule 12.13.

## Amendment A2 (10 Oct 2026): the identity stream completed (WP-IAM-01.T1)

- **Status:** Proposed by the lead. Events contract 0.3.0 (additive).
- **Seven more topics:** `identity_session_started`, `identity_password_changed`, `identity_second_factor_enrolled`, `identity_second_factor_removed`, `identity_recovery_codes_regenerated`, `identity_sign_in_failed_burst` and `identity_access_review_completed`. Role assignment and revocation are the existing `identity_assignment_*` topics.
- **Delivery:** every identity event is at-least-once (consumers deduplicate on the event id) and an audit record, kept for the audit retention. A failed sign-in storm may be summarised by `identity_sign_in_failed_burst` once a window passes the `threshold` the payload carries.
- **Bounded strings and listed free-form fields:** `identity::validate` rejects any string over 256 characters at any depth, and a contract test fails on any `Text` or `Json` field whose name is not in `identity::FREE_FORM_FIELDS`, so a secret cannot arrive as an innocent-looking string. A free-text `note` is untrusted and `spot-redact` redacts it at emit.
- **`via` is unchanged.** It stays the optional record id of the real person (0.2.0). How an actor authenticated is on `identity_sign_in_succeeded` and `identity_session_started` (`method`), not on the actor.
- Example payloads for every identity topic live in `contracts/events/examples/identity/`.

## Amendment A3 (19 Oct 2026): the document audit and graph edge payloads (WP-CON-04.T2)

- **Status:** Proposed by the lead for the architecture owner's approval. Events contract 0.4.0 (additive: no existing topic, payload or schema version changes; a contract test pins the 7 lifecycle and 35 identity schemas by fingerprint).
- **`sys_doc_audit` v1** is the document audit event `spot-doc-audit` already writes (`AUDIT_TOPIC`). The contract adapts to the crate's wire form: operation (a name, or `{"method": name}` for a whitelisted method), `doctype` and `id`, the `version` the write produced, actor (with `on_behalf_of` and `component`), `source`, `at` (Unix microseconds), `changed_fields`, the masked `diff` and `hidden_changes`, `command_id`, and `truncated` with `diff_digest`. A diff value is `"null"`, `{"bool"}`, `{"int"}`, `{"text"}` (at most 1,024 characters) or `{"digest": {len, digest}}`; a diff holds at most 1,000 entries. `truncated` and `diff_digest` are set together. Masking itself stays the producer's duty (ADR-070 section 12 rule 9); the contract bounds what can be carried and rejects secret-like keys at any depth.
- **`sys_graph_edge` v1** carries one change to one edge: `op` (`add`, `remove`, `update`; a closed set), `graph`, `edge_type`, `from` and `to` (doctype and id, storage orientation), an optional `owner` (relation edges only), `properties` (after the event, tagged typed values: bool, int, decimal, text, date, time, datetime, id; never a float; none on `remove`) and `holder_version`.
- **Sequence.** The payload has no sequence field. `EdgeEvent::sequence` is the sequence of the outbox record's feed position, and the graph's tenant is the record's source. A position is comparable only within one source and stream generation (ADR-053); a new generation or a detected hole rebuilds the projection.
- **Delivery and retention.** Both are at-least-once (consumers deduplicate on the event id). The audit event is an audit record (audit retention); the edge event is a projection input (outbox retention).
- **Bounded strings and listed free-form fields,** as in A2: `system::validate` rejects any string over 256 characters (a diff `text` over 1,024) and a contract test fails on any `Text` or `Json` field not in `system::FREE_FORM_FIELDS`.
- Example payloads live in `contracts/events/examples/sys/`.
- **Open points for the architecture owner** are listed in the PR: the previous version is not in the audit payload; the outbox sequence is shared across topics, so a hole rule needs a decision; JSON and byte edge properties are not representable.

## Amendment A4 (26 Oct 2026): the secret field audit payload (WP-CON-04.T3)

- **Status:** Proposed by the lead for the architecture owner's approval. Events contract 0.5.0 (additive: no existing topic, payload or schema version changes; a contract test pins the 44 schemas 0.4.0 published — the 7 lifecycle, the 35 identity and the two `sys_*` — by fingerprint, and the 0.4.0 proof test still passes unchanged).
- **`sys_secret_audit` v1** is the audit event `spot-document` already writes for every secret-field call (`TOPIC_SECRET_AUDIT`, the `audit_event` of the secret vault). The contract adapts to the crate's wire form: `op` (`set`, `open`, `find`, `rewrap`, `is_set`; a closed set, and `create_with_secrets` writes one `set` per field), `doctype`, `field` (a secret field, or `"*"` for one row of a rewrap batch), `record` (only when the call named one), `caller` (`login_service`, `seed`, `test`; a closed set), `site`, `outcome` (`ok` or `refused`), `error` (the catalogue code a refusal ended with) and `state` (`<set>`, `<unset>`, `<none>`). The actor is the event's own `meta.actor`; it is not a payload field, and the subject of the outbox record is the record the call named, or a fresh id when it named none.
- **No value, sealed blob, blind index or hex form can be carried.** The payload is closed: there is no `value`, `sealed`, `bidx`, `iv`, `key` or `ciphertext` field, and an event carrying one is refused as an unknown field. `doctype` and `field` must be **names** ([a-z][a-z0-9_]*, at most 63 characters), and `system::validate` additionally refuses a name that ends in a derived-column suffix (`__sealed`, `__bidx`) or that holds a hex run of 16 characters or more, because those are the shapes a value, a blob or a digest has and a name never does. `error` must be a catalogue code (`namespace.reason`, two or three lower-case segments), never a message; `outcome` and `error` agree — a refusal names its code, a success carries none. A contract test proves each rule, including an extra field and a secret-looking `field`.
- **Why a closed set for `op`, `caller` and `state`.** Consumers switch on them, and an open value is a hole through which an arbitrary string (a value) could arrive bounded only by 256 characters. A new operation, caller kind or state is a change to the producer's own enum, so it is a new major version — the same rule as `sys_graph_edge`'s `op`.
- **Delivery and retention.** At-least-once (consumers deduplicate on the event id), and an audit record (audit retention), as for `sys_doc_audit`.
- Example payloads live in `contracts/events/examples/sys/`: a successful `set`, a refused `find` and a rewrap batch row, all validated by a test.
- **Producer caveat (open point for the architecture owner).** `spot-document` clips a caller-supplied `doctype` and `field` to 128 characters but does not otherwise sanitise them, so a refusal that names a malformed DocType or field (for example a field name with a space) writes a name this schema refuses. The contract states what may be carried; the producer should substitute a placeholder for a string that is not a name before it writes the event. That change is in `crates/framework/spot-document/src/engine/secrets.rs`, outside this task's owned paths, so it is recorded here rather than made.

## Amendment A5 (26 Oct 2026): the typed principal reference (events contract 0.6.0, ADR-083)

- **Status:** Proposed by the lead for the architecture owner's signature; the architecture owner approved writing it on 26 Oct 2026 (ADR-083 open question 10, answered yes). Additive on the wire; a source break only for an exhaustive `match` on `Actor`.
- **Why.** A consumer (ADR-083 section 2.2) is a separate principal in the consumer plane, not a `User` row, so a record it causes needs an actor that is not a user id. Today `Actor` is `User { id, via }` or `System { component }`.
- **Decision.** `Actor` gains `Consumer { id }` (no `via`: a consumer is never impersonated). The text of a principal is the typed reference `user:<id>` or `consumer:<id>` (`system` for the platform), canonical and strictly parsed (`PrincipalRef`, `PrincipalKind`, `Actor::principal_text`). The audit actor text of `sys_doc_audit` was already `user:<id>` or `system`; the schema is unchanged and now validates that the text is a reference, that `on_behalf_of` is a `user:` reference, and that only a `user:` actor has one. The request grant of `spot-bpm-request-authority` (`rg1.<payload>.<tag>`, payload field `u=<actor>`) already carried `actor_text` of the invocation; it now refuses to issue or decode an actor that is not a typed reference (new code `bpm_request_authority.actor_not_typed`), so an untyped id can never be read as either kind. A grant issued to `consumer:X` never verifies for `user:X`: the texts differ.
- **Not changed.** The grant format version (`v=1`, prefix `rg1.`), the HMAC, the expiry rules and the `process_only` guard's decision; no event topic or payload schema; `via` and impersonation of users.
- **Consequences.** Every exhaustive match on `Actor` gained an arm that fails closed (a consumer resolves to a principal with no assignment). Work not done here and tasked later (ADR-083 6.1 tasks 14 and 18): the consumer plane, the consumer entry points that put `Actor::Consumer` into an invocation, and the owner-scope check on `consumer_ref`.

## Amendment A6 (27 Oct 2026): the audit event's schema 2 fields (WP-CON-04.T4)

- **Status:** Proposed by the lead for the architecture owner's approval. Events contract 0.7.0 (additive: no topic, no existing field and no `via` changes; a contract test pins the `sys_doc_audit` shape 0.5.0 and 0.6.0 published and the other 44 schemas by fingerprint, and the 0.4.0 to 0.6.0 proofs still pass unchanged).
- **Why.** The audit participant's schema 2 (`spot-doc-audit`, WP-SEC-06.T3) already writes seven fields this contract did not know: a method-made write, the half of a correction, how a write came about, the deployment node, and a refusal with its class and code. Because validation is strict, a real schema 2 event was refused by the contract's own validator — the contract would have rejected the audit trail's denial records, which are the ones an investigation needs most.
- **The seven fields, all optional, all bounded.** `method` is the declared method of a method-made write: 1 to 64 bytes of `[a-z0-9_]`, the shape the producer's `is_recordable_method` enforces (it drops a name it cannot bound, so a name that reaches the contract is always of that shape). `correction_half` is `cancel` or `amend` (ADR-42) and accompanies operation `correct` only. `via` is `step` (a lifecycle step made the write) or `method` (a method write whose name was dropped) — a **closed** set, because a consumer switches on it and an open value is a way to carry an arbitrary string under the 256-character bound. `node` is the deployment node of ADR-062 that made the write, as a name. `outcome` has exactly one value, `denied`. `error_class` and `error_code` are the refusal's native API class and its stable catalogue code.
- **A method-made write names its method twice, and the two agree.** The operation is `{"method": name}` and the payload also carries `method`; the contract requires them to be equal. Without that rule the operation would be the only carrier of a method name, and today it may hold up to 128 characters of anything. The producer always writes the same name in both places, so the rule refuses nothing real.
- **A denial records the attempt, never a change.** `outcome = "denied"` forbids a non-empty `diff`, a non-empty `changed_fields`, `truncated = true` and a `diff_digest`: a refused operation has no before and after image, and neither a digest nor a truncation flag can be derived from a diff that does not exist. There is **no field for a message, a reason or an explanation** anywhere in the payload, so there is nothing to bound and nothing to redact; an unknown extra field is refused as it always was, on a write and on a denial alike. `error_class` and `error_code` are allowed only with `outcome = "denied"`, and both are catalogue codes: lower-case `[a-z0-9_]` segments separated by dots, at most 63 characters in all, with the segment count left open because the catalogue already has two-, three- and four-segment codes (`offline_identity.quarantine.tier1_after_revocation`) and the producer substitutes the single-segment `unknown` for a code it cannot parse.
- **Attacker reach, reviewed.** Every new string is bounded: a method name by 64 bytes of a restricted alphabet, a node name by the platform's identifier rule (63 bytes), and the two codes by 63 bytes of dotted identifiers. Nothing here is prose, and the one field that could have become prose — the refusal's message — does not exist. A schema 1 reader is unaffected: all seven are optional, a payload without them validates exactly as before, and the two schema 1 examples are unchanged and still validated by a test.
- **Versioning.** The payload stays **v1** on the wire (`schema_version` is 1) and its registry revision rises from 0 to 1 (`system::DOC_AUDIT_REVISION`), which is what ADR-056's additive rule requires for a v1 payload that gained optional fields. 0.7.0 also pins the `sys_doc_audit` v1 shape 0.5.0 and 0.6.0 published by fingerprint, and proves with `compat::check` that the live schema is an additive revision of it rather than a replacement.
- **Open points for the architecture owner** are listed in the PR: whether a denial should *require* `error_class` and `error_code` (the producer always writes both) rather than merely allow them; and whether a deployment node name may carry a hyphen, which the identifier rule this amendment reuses would refuse.

## Amendment A7 (7 Nov 2026): two more secret audit callers (events contract 0.7.5)

- **Status:** Proposed by the lead for the architecture owner's approval. Events contract 0.7.5 (additive on the wire: no topic, field or type changes, and every payload 0.5.0 published still validates; a contract test pins the other 44 schemas by fingerprint and proves the secret audit differs from its 0.5.0 shape only by its `caller` set).
- **What.** The closed `caller` set of `sys_secret_audit` v1 gains `webhooks` and `operator`. `webhooks` is the capability that writes and reads a webhook's sealed signing secret (WP-INT-01.T5). `operator` is the operator command line (`spot secrets`): the key rotation sweep used `seed`, so an audit reader could not tell a rotation from a seeding (WP-SEC-03.T12). Both are audit labels, not permissions.
- **Why now.** Both tasks are blocked: each needs a `SecretCaller` variant, and the audit chain check refuses a caller the contract does not list.
- **The set stays closed, and A4's rule is relaxed in one respect.** A4 said a new caller kind is a new major version. The compatibility checker agrees (a closed enum gaining a value is `EnumClosedChanged`). A7 treats a new caller kind as a minor revision of v1 instead, recorded as `SECRET_AUDIT_REVISION` 1, because the producer's enum and the contract's set are one list kept by the same lead, and a major bump would orphan every journalled v1 record for a label. A caller kind is still never open: an unknown name is refused, so no arbitrary string can arrive under the 256-character bound. The cost is that a consumer that switches exhaustively on `caller` must learn the two names; none exists in the repository.
- **Risks reviewed.** Audit validation still refuses an unknown caller; no payload field is added, so no value, blob or index can be carried; nothing is renamed or removed, so journalled records read unchanged. A build before 0.7.5 refuses a `webhooks` or `operator` event, which is why the two producers are gated on this change landing first.
- **Revert.** Remove the two names, restore revision 0 and VERSION, and keep the two producers unmerged.
