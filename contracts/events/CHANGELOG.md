# spot-events-contract changelog

Every change is an ADR plus a semantic version bump (blueprint §4, contracts-first rule).

## 0.7.7 (draft, awaiting architecture-owner approval)

Additive (WP-CON-04.T7, lead decisions of 2 and 12 Nov 2026). A patch version: gate G6 needs a
VERSION change and 0.7.6 is on `dev`. **Version policy for the 0.7 line:** (1) additive changes
and first schemas for unschemaed topics are patch versions; (2) `0.8.0` is reserved for removing
the three functions deprecated in 0.7.1, with its ADR; (3) **freeze rule:** once a version
carrying the nine `sys_*` schemas of 0.7.2 is on `main`, revision 0 of each is frozen, and a later
change to a shape is an additive revision 1 or, if it removes or retypes a field, a new major of
that topic; (4) enforcement (a producer or consumer calling `system::validate`) is a code change
in the calling crate, not a contract bump, and lands only after WP-CON-04.T8, T9 and T10.
- `sys_job_enqueued` gains two optional fields the job intake reads: `queue` (1 to 64 bytes of
  `[a-z0-9_-]`; absent or null means `default`) and `context` (a JSON object, no float at any
  depth, same key and string bounds as `args`). Still no `site`: the site is the intake's own
  context, so a payload carrying it stays refused. The example now uses both.
- Null for absent: every optional field of every schema accepts an explicit `null` and treats it
  as absent (this was already how the schema engine behaved); `spot-document`'s literal job shape
  (`"args": null`, `"idempotency_key": null`, `"not_before": null`) is now a tested case. The
  `system_platform` module doc gains a table stating, per topic, which fields are optional; a
  test checks it against the schemas.
- The nine schemas are documented as not yet enforced: nothing on the publish or consume path
  calls them, so no payload is refused at run time because of them.
- Compatible: no field removed or retyped, no payload accepted before is refused (a payload with
  `queue` or `context` was refused by the strict 0.7.2 schema and is accepted now).
  `registry.snapshot.json` changes only `contract_version`.

## 0.7.6 (draft, awaiting architecture-owner approval)

Additive (ADR-061 sections 7 and 8 and the answer to question 5; ADR-067; ADR-062 section 3;
WP-JRN-01.T2). A patch version, for the same reason as 0.7.1 to 0.7.5: gate G6 requires a VERSION
change for any contract edit and 0.7.5 is already on `dev`. Five audit topics for hosting the
journal and sync: who can read a tenant's whole history, who holds the `journal_reader` grant,
what an operator or the platform did to the journal itself, what happened to a sync node's
identity, and what decision a sync operator or rule made on data. Nothing existing changes.
- New module `journal`: v1, revision 0 schemas for `sys_journal_access`, `sys_journal_grant`,
  `sys_journal_admin`, `sys_sync_node_admin` and `sys_sync_decision`, all class Audit, audit
  retention, keep-all coalescing, owner `spot-events-contract`, `since` 0.7.6. Same mechanism as
  `system_platform`: a strict schema, then structural rules, run by `system::validate` after its
  generic key scan and 256-character string bound. `system::TOPICS` grows from 12 to 17.
- `sys_journal_access`: one record per read of journal bytes through the WP-JRN-09.T4 access
  surface (`principal`, `grant` — a `journal_reader` grant id or the literal `admin` —, `object`
  closed to `manifest`/`segment`/`tail_chunk`/`snapshot_index`, `generation`, `first_seq` and
  `last_seq` — required unless the object is a manifest —, `outcome` closed to
  `served`/`denied`/`not_found`/`expired`/`failed`, an optional short `reason` code and an
  optional `bytes` count).
- `sys_journal_grant`: the `journal_reader` grant lifecycle (`action` closed to
  `granted`/`renewed`/`revoked`/`expired`/`refused`, `grantee`, `grantor` — a principal id, or the
  literal `system` exactly for an `expired` action —, `grant`, `expires_at` — required for
  `granted` and `renewed`, absent otherwise —, `external` and an optional `reason`).
- `sys_journal_admin`: an operation on the journal itself (`action` closed to thirteen values:
  `policy_changed`, `snapshot_taken`, `snapshot_failed`, `rebuild_planned`, `rebuild_started`,
  `rebuild_completed`, `rebuild_failed`, `fork_opened`, `audit_run`, `reship_started`,
  `reship_completed`, `key_rotated`, `subject_erased`; `actor`; an optional `target` — a position,
  a policy name or a snapshot id, refused when it parses as a record id —; `outcome` closed to
  `ok`/`refused`/`failed`; an optional catalogue `code`, allowed only when the outcome is not
  `ok`; `dry_run`).
- `sys_sync_node_admin`: a sync node's lifecycle (ADR-062 section 3): `action` closed to
  `enrol_started`, `enrolled`, `certificate_rotated`, `suspended`, `resumed`, `revoked` and
  `role_changed`; `node`; `actor`; `outcome`; an optional `code`; an optional `fingerprint`, the
  SHA-256 hex of the pinned certificate (public; never a key).
- `sys_sync_decision`: a sync decision an operator or a rule made on data (ADR-067): `action`
  closed to `resend_requested`, `reship_requested`, `contact_notified`, `quarantine_released`,
  `quarantine_discarded`, `hold_released`, `adoption_forced`, `policy_applied`; optional `node`
  and `entity`; optional `count`; `actor`; `outcome`; an optional `code`. Counts and names only,
  never a record id list or content.
- `actor`/`grantor` is a principal id, or the platform itself (`system`); `target` is bounded free
  text and refused when it parses as a record id, so no record id or document content ever rides
  in it; `code` is a bounded `namespace.reason` catalogue code and only present when the outcome
  is not `ok`, the same pattern `sys_doc_audit`'s `error_class`/`error_code` already set.
- `system::FREE_FORM_FIELDS` gains the six Text field names the five topics need (`grant`,
  `grantor`, `target`, `code`, `entity`, `fingerprint`); none looks like a secret.
- The registry gains the five entries (`registry.snapshot.json` and `REGISTRY_LEN` updated); the
  registry test's schemaless list is still `doc_edge` alone.
- Ten examples in `examples/sys/` (one `ok` and one refused or failed per topic), and contract
  tests: the registered schemas, every example, strictness (unknown field, missing `outcome`), an
  action outside its closed set, the defence-in-depth check that none of the five schemas
  declares a field named `key`, `token`, `secret`, `password` or `credential`, and the cross-field
  rules of each topic (the manifest exception, the grant/expiry pairing, the `system` grantor, the
  target-is-never-a-record-id rule, the code-only-when-not-ok rule).
- Out of scope (left to later tasks): emitting these events (the hosting tasks of WP-RTB-01 to
  WP-RTB-04 and WP-JRN-09), the publish-path refusal of an unregistered topic (WP-JRN-01.T3), and
  anything in `crates/`.

## 0.7.5 (draft, ADR-056 amendment A7; awaiting architecture-owner approval)

Two callers are added to the closed `caller` set of the `sys_secret_audit` v1 payload: `webhooks` (the write path of a webhook's signing secret, WP-INT-01.T5) and `operator` (the `spot secrets` command line, so that a key rotation sweep is no longer recorded as `seed`, WP-SEC-03.T12). A patch version, for the same reason as 0.7.1 to 0.7.3: gate G6 requires a VERSION change for any contract edit, and the lint treats a 0.x minor bump as breaking. Wire-additive: no topic, no field, no type and no other value changes, every 0.5.0 payload still validates, and the registry revision of `sys_secret_audit` v1 rises from 0 to 1 (`system::SECRET_AUDIT_REVISION`); `schema_version` on the wire stays 1.
- `system::SECRET_CALLERS` is `["login_service", "seed", "webhooks", "operator", "test"]` (was three). Still closed: a name outside the set is refused, and a further caller is a further amendment. `test` still exists only in a build with the `testing` feature.
- The compatibility checker reports a closed enum that gains a value as `EnumClosedChanged`; that is the one finding for this change and it is the exception the amendment asks the architecture owner to approve (ADR-056 A4 had said a new caller kind is a new major version). A consumer that switches exhaustively on `caller` must learn the two names; the repository has none (the audit chain check in `spot-doc-audit` uses the producer's own `SecretCaller::from_name`).
- Examples `examples/sys/sys_secret_audit.webhooks.json` and `.operator.json`, validated by the existing example test.
- Contract tests: all five callers validate and near spellings are refused; a written-out 0.5.0 schema of the event (three callers) is the subject of the new proof: same fields, types and required-ness, only the `caller` set differs, `check` reports exactly `EnumClosedChanged("caller")`, and the revision rose by one. The other 44 schemas are pinned by the same fingerprint as before, now with the 0.5.0 secret audit schema written out instead of read from the registry.
- Producers: `spot-document` adds `SecretCaller::Webhooks` and `SecretCaller::Operator` (WP-INT-01.T5, WP-SEC-03.T12); both tasks stay gated until this change merges.

## 0.7.3 (draft, awaiting architecture-owner approval)

Additive (WP-JRN-01.T1). A patch version: gate G6 requires a VERSION change for any contract edit, and 0.7.2 is already on `dev`. The `doc_state` payload schema. Nothing existing changes.
- `doc_state` payload schema (WP-JRN-01.T1). New module `state`: v1, revision 0, `data` has exactly `op` (closed: `insert`, `update`, `delete`), `entity_schema` (integer, at least 1: the catalogue version the image was written under) and `image` (an object: the whole document in the native value encoding, every declared field and child row; for `delete` the last committed version). Document identity is the envelope's `subject`, the version its `subject_version`; `state::validate_envelope` refuses a `doc_state` envelope without `subject_version` (the capture invariant compares by it). `image` is opaque to the contract (no string bound, no secret-key scan: its fields belong to the DocType), except that an object carrying `$pii` (sealed personal field of `spot-journal-pii`: `$pii`, `subject`, `ct`, `nonce`, `aad_hash`) or `$erased` (erasure placeholder: `$erased`, `purpose`, optional `erased_at`) must be a complete one, at any depth. Five examples in `examples/doc/`. The registry entry (class State, retention, owner, `since` 0.7.1) is unchanged; the registry test's schemaless list is now `doc_edge` alone. `delete` requires an image although the rebuild executor tolerates its absence: the capture step has the last version, and one shape is simpler for the three consumers.
- Reduction of WP-JRN-01: the class attribute is the registry (WP-CON-04.T5), the `doc_state` schema is this task (WP-JRN-01.T1), the refusal of an unregistered topic at the publish path is WP-JRN-01.T3, the audit topics of the journal and sync hosting are WP-JRN-01.T2. Nothing else remains of the WP.
- ADR obligations: none new. ADR-061 section 3 (State events) is implemented as amended on 2026-10-31; the `$pii` form is the one ADR-061 section 6 and `spot-journal-pii` already define.

## 0.7.2 (draft, awaiting architecture-owner approval)

Additive (ADR-056; WP-CON-04.T6, the follow-up to WP-CON-04.T5). A patch version, for the same reason as 0.7.1: gate G6 requires a VERSION change for any contract edit. Payload schemas for the nine `sys_*` topics that 0.7.1 registered without one. No existing topic, class, retention, owner, `since`, payload or fingerprint changes; the registry snapshot is untouched.
- New module `system_platform`: v1, revision 0 schemas for `sys_job_enqueued`, `sys_user_notice`, `sys_operation_receipt`, `sys_operation_receipt_effect`, `sys_partition_range_low`, `sys_bpm_governance`, `sys_bpm_case_migrated`, `sys_condition_raised` and `sys_condition_cleared`, each written from what its producing crate puts in the outbox (`spot-document`, `spot-partition`, `spot-bpm-governance`, `spot-bpm-versioning`, `spot-conditions`). Same mechanism and strictness as the three older `sys_*` schemas: unknown fields refused, no secret-looking key at any depth, strings of at most 256 characters, and a structural check of every `Json` and `Text` field. The two app-filled payloads (`args` of a job, `payload` of a notice) are bounded in shape and refuse a float; the notice follows the realtime bridge's boundary (2 KiB, scalars and arrays of scalars).
- `system::TOPICS` lists all twelve `sys_*` topics (it was three; the type is now `[&str; 12]`), so `system::schemas()`, `system::schema_of` and `system::validate` cover the nine. The deprecated `system::retention_of` and `system::is_audit` answer for them too, through the registry.
- `system::FREE_FORM_FIELDS` gains the names the nine need (`job`, `args`, `idempotency_key`, `not_before`, `payload`, `name`, `step`, `class`, `store_refs`, `digest`, `state`, `series`, `subject`, `company`, `policy`, `reason`, `detail`, `case`, `hash`, `plan_digest`, `id`, `params`).
- Examples in `examples/sys/` for each of the nine (12 files); the registry test's schemaless list is now `doc_state` and `doc_edge`, neither a `sys_*` topic.
- Known mismatches between producers and consumers, reported rather than papered over (see the task report): the job intake decodes a `site`, `queue` and `context` that the engine never writes, and `sys_bpm_case_migrated` has no producer that serialises its payload yet, so its wire form is inferred from `MigrationAudit`.
- Source compatibility: code that names the type of `system::TOPICS` as `[&str; 3]` must change it; nothing in this workspace does. A 0.7.1 consumer is otherwise unaffected.

## 0.7.1 (draft, awaiting architecture-owner approval)

Additive (ADR-061 section 2; WP-CON-04.T5, decisions B1 to B9 of 31 Oct 2026). The event class registry. It is a patch version because gate G6 requires a VERSION change for any contract edit once 0.7.0 is on `main`, and the lint treats a 0.x minor bump as breaking (it would need an ADR file in this PR); the change is additive. Additive: no topic, payload, schema or fingerprint changes; the three
old functions stay for this minor version, marked `#[deprecated]`, and are removed in 0.8.0. The
lead approved this under the architecture owner's authority of 31 Oct 2026; it is listed in
the contract change log (item 6).
- New module `registry`: `Class` (`State`, `Domain`, `Audit`), `Retention` (moved here from
  `system`, re-exported there), `Coalescing`, `Entry { topic, class, retention, coalescing, owner,
  since, retired }`, `REGISTRY` (56 entries), `lookup`, `class_of`, `retention_of`, `is_audit`,
  `check_publishable`.
- Classes (B2, B6 to B8): all 35 `identity_*` topics are Audit; `sys_partition_range_low` is
  Domain; `sys_condition_raised` and `sys_condition_cleared` are Audit; `sys_graph_edge` is the
  one name of the graph edge fact and `doc_edge` is registered only as `retired`; the seven
  `doc_*` lifecycle topics, `sys_graph_edge`, `sys_job_enqueued` and `sys_user_notice` are Domain;
  `doc_state` is State; `sys_doc_audit`, `sys_secret_audit`, `sys_operation_receipt`,
  `sys_operation_receipt_effect`, `sys_bpm_governance` and `sys_bpm_case_migrated` are Audit.
  Nine `sys_*` topics and `doc_state` were emitted without being registered; they are registered
  now without a payload schema (their producers own the payload until a later amendment).
- Rules (B3, B4): a class never changes after release and an entry is never removed (`retired`
  instead), enforced by a test against the committed `registry.snapshot.json`.
  `check_publishable` refuses an unregistered platform topic (`doc_*`, `sys_*`, `identity_*`) and
  returns `Warn` for an unregistered app topic for one minor version. The publish path adopts it
  in a follow-up; nothing calls it yet.
- A rebuild applies State and Audit events and makes Domain events available without applying them
  (ADR-061 amendment A.0.1). The rebuild executor still uses its prefix classifier until
  WP-JRN-05.T9 switches it to this registry.
- Deprecated, removed in 0.8.0: `identity::is_audit`, `system::retention_of`, `system::is_audit`.
- Lint `tools/lints/event_topics.py` (B5): fails when a platform topic constant is not registered
  or when an entry's `owner` crate does not contain the topic.
- Source compatibility: a caller of the three old functions sees a deprecation warning; nothing
  else in this workspace calls them. A 0.6.0 consumer is otherwise unaffected.

## 0.7.0 (draft, ADR-056 amendment A6)

Teaches the `sys_doc_audit` v1 payload the seven optional fields of the audit participant's schema
2 (WP-SEC-06.T3), which the producer already writes (WP-CON-04.T4). Additive: no topic, no
existing field and no `via` changes, and a schema 1 payload validates unchanged. Within v1 the
payload's registry revision rises from 0 to 1 (`DOC_AUDIT_REVISION`); the wire's `schema_version`
stays 1.
- `sys_doc_audit` v1 gains, all optional: `method` (a declared method name, 1 to 64 bytes of
  `[a-z0-9_]`, and the `operation` names the same method), `correction_half` (`cancel` or `amend`,
  only with operation `correct`), `via` (`step` or `method`; a closed set), `node` (a deployment
  node name of ADR-062), `outcome` (the single value `denied`), and `error_class` and `error_code`
  (bounded `namespace.reason` catalogue codes, allowed only with `outcome = "denied"`).
- A denial records the attempt, never a change: it carries no diff, no changed field, no
  `truncated`, no `diff_digest` and no message. There is no field for a message, and an unknown
  extra field is still refused.
- A method-made write names its method twice, in `method` and in `operation`, and the contract
  requires the two to agree: the operation can never be the only carrier of a name.
- Example payloads in `contracts/events/examples/sys/`: a method call, a correction half, a
  node-stamped write and a denial, all validated by a test.
- Compatibility proof: a test pins the 0.5.0 `sys_doc_audit` shape by fingerprint (as a written-out
  schema), proves `compat::check` accepts the live schema as an additive revision of it, proves the
  seven additions are the only difference and that all seven are optional, and pins the other 44
  schemas 0.5.0 published (7 lifecycle, 35 identity, `sys_graph_edge`, `sys_secret_audit`) by
  fingerprint. The 0.4.0 and 0.5.0 proofs still pass unchanged.
- Contract tests: each field's bound, every cross-field rule (a denial with a diff, a digest or a
  truncation; a code without a denial; a correction half without `correct`; a method without the
  operation), the four examples, the schema 1 examples that must keep validating, and an unknown
  field on a write and on a denial.

## 0.6.0 (draft, ADR-056 amendment A5; ADR-083, the architecture owner approved this contract amendment on 26 Oct 2026)

The typed principal reference: an event, an audit record and a request grant can name a consumer
as their actor (ADR-083 section 2.2 item 4). Wire-additive: no topic, payload, schema or
fingerprint changes (a test still pins the 42 existing schemas), and every 0.5.0 payload reads
unchanged.
- `Actor` gains `Consumer { id }`, wire `{"kind": "consumer", "id": "<record id>"}`. It has no
  `via`: a consumer is never impersonated. `deny_unknown_fields` still holds, so a 0.5.0 reader
  refuses the new kind rather than misreading it (see Compatibility).
- New `PrincipalKind` (`User`, `Consumer`; `#[non_exhaustive]`) and `PrincipalRef`: the text
  `user:<id>` or `consumer:<id>`. The text is canonical (lower-case kind, canonical lower-case
  hyphenated UUID) and `FromStr` is strict, so one principal has one spelling. A bare id, an
  unknown kind (`agent:`), another case or another id spelling is an error.
  `PrincipalKind::of_text` is the shape check an authority runs without parsing the id.
- `Actor::principal_ref()` (`None` for the platform) and `Actor::principal_text()` (`user:<id>`,
  `consumer:<id>` or `system`, the text an audit record and a request grant carry);
  `SYSTEM_PRINCIPAL`.
- `sys_doc_audit` v1: `actor.actor` must be a principal reference or `system`, and
  `actor.on_behalf_of` a `user:` reference, allowed only on a `user:` actor. The producer
  (`spot-doc-audit`) already wrote exactly these texts, so no producer changes; the schema
  (field names, types) is unchanged, which is why the fingerprint holds. New example
  `examples/sys/sys_doc_audit.consumer.json`.
- Contract tests: the wire form and the no-`via` rule, the strict reference grammar, the
  actor-to-text mapping (and that one id under two kinds gives two texts), an `EventMeta`
  round trip with a consumer actor, and the audit actor rules.
- `CONTRACT_VERSION` brought to 0.6.0.

Compatibility: **additive on the wire, a source break only for an exhaustive `match` on `Actor`.**
A consumer of this contract that matches `Actor` exhaustively must add an arm; the compiler finds
every one (the same precedent as `via` in 0.2.0). The ten places in this workspace that do are
changed in the same PR and each fails closed: a consumer is mapped to a principal with no role
and no assignment, never to a user. A 0.5.0 reader meeting `kind: "consumer"` fails to decode the
actor (it does not misread it as a user); consumer-caused events exist only once the consumer
plane exists, so no 0.4.0 reader meets one before it is upgraded. `Actor` is deliberately **not**
`#[non_exhaustive]`: a security decision on an actor must not fall into a wildcard arm.

## 0.5.0 (draft, ADR-056 amendment A4)

Registers the `sys_secret_audit` payload that `spot-document` already writes for every
secret-field call (WP-CON-04.T3). Additive: no existing topic, payload or schema version changes
(a test pins the 44 schemas 0.4.0 published — the 7 lifecycle, the 35 identity and the two
`sys_*` — by fingerprint, and the 0.4.0 test still passes unchanged).
- `sys_secret_audit` v1: one secret-field call (`set`, `open`, `find`, `rewrap`, `is_set`), the
  DocType and the field name (`"*"` for one row of a rewrap batch), the record when the call named
  one, the caller kind (`login_service`, `seed`, `test`), the site, the outcome (`ok`, or
  `refused` with the catalogue code the call ended with) and the state the engine recorded
  (`<set>`, `<unset>`, `<none>`). The actor is the event's metadata, never a payload field.
  At-least-once, audit retention.
- No value travels in it: `doctype` and `field` must be names — never a sealed blob (`__sealed`),
  a blind index (`__bidx`) or a hex run of 16 characters or more — `error` must be a
  `namespace.reason` catalogue code, and the payload is closed: a `value`, `sealed`, `bidx`, `iv`
  or `key` field is an unknown field and is refused. A test proves each of these.
- Example payloads in `contracts/events/examples/sys/`: a `set` that succeeded, a refused `find`
  and one rewrap batch row, all validated by a test.
- Contract tests: the three closed sets, the error-code rule, the required fields, and the
  compatibility proof (0.5.0 adds one topic and moves nothing else).

## 0.4.0 (draft, ADR-056 amendment A3)

Registers two `sys_*` payloads that platform crates already emit or decode (WP-CON-04.T2).
Additive: no existing topic, payload or schema version changes (a test pins the 42 existing
schemas by fingerprint).
- `sys_doc_audit` v1: the document audit event of `spot-doc-audit` (operation, subject doctype and
  id, the version the write produced, actor, source, the masked diff as a bounded list of typed
  values, and `truncated` with the SHA-256 `diff_digest` of the full diff). At-least-once, audit
  retention.
- `sys_graph_edge` v1: one change to one graph edge (`add`, `remove`, `update`), the edge type, the
  two ends in storage orientation, an optional `owner` for relation edges, the properties after the
  event as tagged typed values (no floats), and the holder's document version. At-least-once,
  outbox retention. There is no sequence field: the sequence is the position of the outbox record.
- New module `system`: `schemas`, `validate` (schema, structural rules of the `Json` fields, size
  bounds and the secret-key scan at every depth), `FREE_FORM_FIELDS`, `Retention`.
- Example payloads in `contracts/events/examples/sys/`, validated by a test.
- Contract tests: a `Text` or `Json` field outside `system::FREE_FORM_FIELDS` fails, as does a
  field named like a secret; strings, diff size and property count are bounded.

## 0.3.0 (draft, ADR-056 amendment A2)

Completes the identity stream (WP-IAM-01.T1). Additive: no existing topic, payload or `via` changes.
- Seven new `identity_*` topics, 33 in all (35 with the node topics): `session_started`,
  `password_changed`, `second_factor_enrolled`, `second_factor_removed`,
  `recovery_codes_regenerated` (a count, never the codes), `sign_in_failed_burst` (the rate
  summary an emitter may write instead of one `sign_in_failed` per attempt above its documented
  `threshold`) and `access_review_completed`.
- Role assignment and revocation stay `identity_assignment_created` / `_ended` (0.2.0); no
  duplicate `role_assigned` / `role_revoked` topics.
- `identity::validate` also refuses any string longer than `MAX_STRING_CHARS` (256) at any depth.
- `identity::FREE_FORM_FIELDS`: the only `Text` and `Json` field names an identity schema may use;
  a contract test fails on any other, and on a `key` field that is not an id.
- Delivery: every identity event is `at-least-once` and an audit record (`AT_LEAST_ONCE`,
  `is_audit`).
- Example payloads for every identity topic in `contracts/events/examples/identity/`, validated by
  a test so they cannot drift.

## 0.2.0 (draft, ADR-070 amends ADR-056)

Identity and access events (WP-IAM-01). Additive; nothing that compiled before stops compiling
except struct-literal construction of `Actor::User` (add `via: None`):
- `Actor::User` gains `via: Option<RecordId>`, the real person when one identity acts as another
  (impersonation, ADR-070 section 9). Absent on the wire when `None`, so 0.1 payloads still read.
- The `identity_` topic prefix is reserved for the platform, like `doc_` and `sys_`
  (`Topic::RESERVED_PREFIXES` has three entries; an app topic may not use it).
- `identity.rs`: the v1 schemas of 28 `identity_*` topics (users, sign-in, lockout, recovery,
  sessions, API keys, roles and duties, assignments, SSO policy, segregation-of-duties
  overrides, emergency access, impersonation, the field access trace, node and snapshot events).
- `identity::validate`: strict schema validation plus a scan of **every key at every depth**,
  including free-form `Json` fields, against `FORBIDDEN_KEY_PARTS`. An identity event never
  carries a credential, token, secret, key material or recovery code.
- Ten new contract tests (24 in total).

## 0.1.0 (draft, awaiting architecture-owner approval)

First version (WP-CON-04.T1, ADR-056). It covers:
- topic, source and command-id names;
- the payload document `{meta, data}` carried in the storage outbox's `payload`, with actor, command, causation, correlation and occurrence time;
- the external `EventEnvelope`, stamped with `source_id` by the relay;
- payload schemas, and payload validation in the native API value encoding;
- the compatibility checker and the evolution rules;
- the platform lifecycle topics (`doc_*`) with their v1 schemas.
