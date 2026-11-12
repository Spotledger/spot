# Native API contract changelog

Every change is an ADR plus a semantic version bump (blueprint §4, contracts-first rule). Additive changes bump the minor version and keep the `/api/v1` prefix; breaking changes need a new major version and prefix (see `openapi.yaml`, "Versioning").

## 0.12.0 (draft, awaiting architecture-owner approval)

Additive (WP-CON-03.T20; the architecture owner gave the lead authority over contract changes on 31 Oct 2026); `/api/v1` and every existing path, operation, property and error code are unchanged. Fourteen operations (60 in all) and thirteen error codes (75) are new. The process engine, the task inbox and the trigger intake existed as libraries with no operation in the native API; this version gives clients the operations and changes no server. The wire says **run** and **request**, never "Case" (ADR-080).
- **Processes and runs** (tag `process`, paths under `/api/v1/process`): `process_list` (`GET /process`: the processes whose latest version has a manual or API trigger the caller's roles allow; others are absent), `run_start` (`POST /process/{process}/runs`; `Idempotency-Key` required; `201`, a replay `200` with the same run; a process the caller may not start is the same `404 process.not_found` as an unknown one; bad input is `422 process.input_invalid` with field paths only), `run_list` (visible to the starter, to an assignee or supervisor of one of its tasks, and to a holder of `process.supervise`; filtered before it is paged, no total), `run_get` (no variables, no outputs, no error text beyond `failure_code`; documents by `doctype` and `id` only), `run_events` (the audit trail, no payload, `after` and `limit`), `run_cancel` (the starter or a `process.supervise` holder; `409 process.run_not_active` once ended), and `process_webhook` (`POST /process/hook/{trigger}`, **the only public operation of this group**: a signature header `X-Spot-Signature` over the raw body, verified before the body is parsed; `202 {accepted: true}` that never says whether a run started; every refusal is the same `401 process.webhook_rejected`; rate class `auth`, the strictest existing class).
- **Task inbox** (tag `task`, paths under `/api/v1/task`): `task_list` (`view=mine|team|overdue`), `task_get` (the item, the outcomes the caller may choose, the form fields by name and type, the events; `ETag` is the version), `task_claim`, `task_release` (`If-Match` required), `task_complete` (`If-Match` and `Idempotency-Key` required; the actor is always the caller, the body cannot name one, and `system:sla` is not reachable), `task_delegate` (`to`, `until?`, `reason?`) and `task_reassign` (`to`, `reason?`, supervisors only). An unknown or inactive target is the same `422 task.target_invalid`.
- **Error codes (13):** `process.not_found`, `process.input_invalid`, `process.run_not_found`, `process.run_not_active`, `process.webhook_rejected`, `task.not_found`, `task.already_claimed`, `task.not_claimed_by_caller`, `task.outcome_invalid`, `task.segregation`, `task.delegation_invalid`, `task.target_invalid`, `task.not_active`; none is retryable. `task.target_invalid` is not in the task brief's list but its text names it for delegate and reassign. A stale `If-Match` is the existing `412 document.stale_version`.
- **Security rules** (each operation states them): authentication on every operation but the webhook; no existence oracle (an invisible resource is the same `404` as a missing one, `403` only where the caller already knows the resource exists); lists are filtered by the caller's rights before they are paged; no secret, token, variable or hidden document value in any answer; bounded, closed request bodies; failures echo field paths, never values; every write is audited, a refusal included. Rate classes: reads `interactive`, writes the class `resource_update` runs in, the webhook `auth`.
- `check.py` rule 18 (with 18 self-test cases) and the conformance pins described above. Process names `runs` and `hook` are reserved by the paths. The Rust registry in `spot-http` gains the thirteen codes. The operations are not served yet (computed as not-yet-served by `conformance.rs`); the serving tasks that follow add the route modules.
- **compatibility.** A 0.11.1 client is unaffected: it does not call the new operations and ignores an error code it does not know. A client that wants the process or inbox features reads `version` from `GET /api/health/live` (0.12.0 or later). `conformance.rs` pins `0.12.0`, 75 error codes and the shape of the fourteen operations.

## 0.11.1 (draft, awaiting architecture-owner approval)

Patch, documentation only (decision by the architecture owner on 2026-11-10); `/api/v1` and every path, operation, property and error code are unchanged (46 operations, 62 codes). `auth_session` now says in its description that an API key is refused with `403 permission.denied`, whatever its expiry, as the other session operations refuse it. `permission.denied` is already in the registry.
- **compatibility.** Before the matching code change a key with no expiry got `500 http.internal` and a key with an expiry read the session; a client that used a key to probe the session must probe with a signed-in session or another authenticated read. `conformance.rs` pins `0.11.1`.

## 0.11.0 (draft, awaiting architecture-owner approval)

Additive, documentation only (WP-PLT-17.T5); `/api/v1` and every existing path, operation,
property and error code are unchanged (46 operations, 62 codes). `function_call` and
`function_call_get` (WP-PLT-17.T2) now spell out the five error answers a client can meet, in
the order the dispatcher checks them, as `crates/framework/spot-api-services/src/resources/
function.rs::function_error` maps them:
- `404 method.not_found` (`function_code` `function.not_found`): no function with this key. An
  anonymous caller asking for a non-guest function gets **exactly** this answer too, never a
  different one, so the endpoint never reveals which functions exist to a guest;
- `405 api.method_not_allowed` (`function_code` `function.verb_not_allowed`, `details.allow`
  lists the answered verbs);
- `401 auth.required` (`function_code` `function.auth_required`: a guest called a verb that
  changes things);
- `429 api.rate_limited` (`function_code` `function.rate_limited`, `details.retry_after` in
  whole seconds, rounded up, at least 1);
- `400 api.invalid_request` (`function_code` `function.bad_arguments`).

None of the five is a new registry code: each already named the status in the registry
(`method.not_found`, `api.method_not_allowed`, `auth.required`, `api.rate_limited`,
`api.invalid_request`); `function_code` is a response detail, not a registry entry, so it
carries the finer code without adding one.
- `check.py` rule 9 (already general-purpose) checks every `"<status> <code>"` this version adds
  to the two operations' descriptions against the existing registry; no new self-test case is
  needed because no new code is registered. Operations (46) and error codes (62) are unchanged;
- **compatibility.** A 0.10.0 client is unaffected: the wire shape of every function error is the
  same `error.code` and `details` it already got (`function_code` already rode in `details` since
  WP-PLT-17.T2); this version only names it in the contract text.
- **note on `crates/framework/spot-errors/catalogue/function.toml`.** The catalogue lists
  `function.not_found`, `function.verb_not_allowed`, `function.auth_required`,
  `function.rate_limited` and `function.bad_arguments` (and the install-time `function.duplicate`,
  `function.override_unknown`, `function.override_duplicate`, `function.override_chain`,
  `function.verb_empty`, `function.guest_verbs`, `function.foreign_app`), agreeing with the five
  dispatch codes documented here. It does not carry a `function.failed` entry, and no code in
  `crates/framework/spot-functions` or `spot-api-services::resources::function::function_error`
  raises one. The task text expected a sixth code, `function.failed`; it exists nowhere on `dev`
  (not in PR 2883 either), so this version documents the five that do exist. Whether a sixth
  code is wanted (a catch-all for a failed dispatch) is the architecture owner's call and would
  be a later additive change made together with the catalogue entry and the mapping.

## 0.10.0 (draft, awaiting architecture-owner approval)

Additive (WP-CON-03.T13; the architecture owner gave the lead authority over contract changes on 31 Oct 2026; listed in the contract change log, items 7 and 8); `/api/v1` and every existing path, operation, property and error code are unchanged. Two operations (46 in all) and one error code (62) are new.
- `partition.not_owner` (403, retryable false): "this node does not own the partition for this write" (decision F3). A client branches on it instead of treating the answer as a plain permission failure. The Rust registry in `crates/framework/spot-http/src/api/registry.rs` is not edited here; the follow-up core task WP-CON-03.T14 adds the code there (`Code::all()` is 61 until then).
- `GET /api/v1/notifications?after=<event_id>&limit=` (`notification_list`, decision H2): the caller's own notices after `after`, oldest first, at most 100 per page, `200 {items: [{event_id, event, data, created_at}], next}`; always the caller's own, so there is no existence oracle; rate class `interactive`; any signed-in user. It is the durable side of the realtime `notification` (ADR-085 stores the `notification_delivery` row it reads).
- `POST /api/v1/auth/consumer/renew` (`auth_consumer_renew`, L1 of ADR-092 section 10.1): consumer session renewal. The only credential is the rotating, one-use renewal cookie `__Host-spot_consumer_renew` (`HttpOnly`, `Secure`, `SameSite=Strict`, `Path=/`, 30 days); the new short-lived bearer token comes back in the body, never in a URL. A replayed cookie revokes the whole family and answers `401 auth.session_expired`, as does any missing, malformed, unknown or expired cookie. The cookie is revoked on sign-out and on a password or device change. ADR-088 section 7.2 and ADR-083 are the sources; the amendment sentence to ADR-055 is still owed (this task may not edit an ADR).
- `realtime.md` (decision H1): the keys inside a `user_notice` `data` object: `title`, `body`, `severity` (`info`, `warning`, `error`; absent means `info`), `doctype`, `id` (only with `doctype`) and `kind`; all optional scalars, unknown keys ignored, the same 2 KiB and 256-byte limits. The "offline" paragraph points at the feed.
- `check.py` rule 17 (with ten self-test cases): the notice keys and severities are named, `partition.not_owner` is a registered 403, `notification_list` takes only `after` and `limit` (limit at most 100), `auth_consumer_renew` has no body, no bearer and no query, and states the cookie attributes. Operations (46) and error codes (62).
- **compatibility.** A 0.9.0 client is unaffected: it does not call the new operations, ignores an error code it does not know (as for any other), and reads the same realtime `notification`. A client that wants the feed or the renewal reads `version` from `GET /api/health/live` (0.10.0 or later). `conformance.rs` pins `0.10.0` and 62 codes, and lists the two operations as not yet served.

## Metadata note (no version change)

`info.license` in `openapi.yaml` now reads Apache-2.0 (it read `LicenseRef-Proprietary`), matching the repository licence (2026-11-09). No path, operation, property or error code changes, so the version stays 0.9.0.

## 0.9.0 (draft, awaiting architecture-owner approval)

Additive (ADR-055 amendment, ADR-068 amendment A4, ADR-056 amendment A5; ADR-083, approved in principle by the architecture owner on 26 Oct 2026); `/api/v1` and every existing path, operation, property and error code are unchanged (44 operations, 61 codes). It carries three things the app API 0.7.0 and the events contract 0.6.0 introduce:
- `FieldMeta.create_only` (boolean, absent means false): the field can be set only at creation. Afterwards a request that changes it fails with the existing `document.read_only_field` on that field and writes nothing; the value it already has is accepted. The `resource_update` description says so. No new error code;
- `FieldType` gains `consumer_ref`: the consumer's id, encoded like a link's value but not a link (no `link_to`, no lookup, no navigation). A staff client shows it as read-only text, and a client that does not know the type treats it as text;
- `EventEnvelope.meta.actor.kind` gains `consumer` (`user`, `consumer`, `system`). The schema documents the actor's properties: `id` for `user` and `consumer` (the same id under two kinds is two principals), `via` only on `user`, `component` on `system`;
- `check.py` rule 16 (with three self-test cases beyond the rule's own): `FieldMeta.create_only` is a boolean, `FieldType` names `consumer_ref`, and the actor kinds equal the variants of `Actor` in `contracts/events` (read from the Rust source, so the two cannot drift). The `spot-http` conformance suite gains a schema test for the new properties; the registry and operations are unchanged;
- **compatibility.** A client that does not know `create_only` ignores it and finds out only when a server refuses a write, as it does for any other refused field; a client that does not know `consumer_ref` shows the value as text; a client that meets an event actor kind it does not know ignores the actor. A server older than 0.9.0 sends none of them, so a client that wants them reads `Live.version` from `/api/health/live` first. Nothing is removed, renamed or reshaped, so a 0.8.0 client talking to a 0.9.0 server is unaffected. The consumer-facing listener and its contract (the surface definition) are not part of this version.

## 0.8.0 (draft, awaiting architecture-owner approval)

Additive (WP-CON-03.T10, ADR-055 amendment; approved in principle by the architecture owner on 24 Oct 2026); `/api/v1` and every existing path, operation, property and error code are unchanged (44 operations, 61 codes). It names the vocabulary of the realtime `notification` kind, which the contract listed but never defined, before WP-PLT-08.T6 sends it.
- `notification` payload: `{event_id, event, data}`. `event` is a short lowercase token; `"user_notice"` is the only value this version defines. `event_id` is stable across a redelivery (at-least-once: clients deduplicate on it). `data` is an object of at most 2 KiB serialised, whose values are scalars, record ids or arrays of those, with no nesting below the top level and no string (key or value) over 256 bytes; it is plain data, never markup or script, and a doorbell (ADR-25), so the client refetches what it needs;
- delivery: a `user_notice` goes only to the addressee's own `user:<id>` room (that user's own connections), the addressee being the user the committed event names and never a field of `data`; never a room broadcast. A notice raised while the user is offline is replayed only within the room's replay buffer (`resync_required` beyond it);
- not in this version, on purpose: **no `left` message** (the architecture owner decided on 24 Oct 2026 that the server does not acknowledge a leave), no notice acknowledgement, no notice history. No message type, close code, room kind or limit changes; an existing `kind` is untouched;
- `check.py`: rule 15 (with self-test cases): `realtime.md` names `user_notice`, both size limits (2 KiB and 256 bytes), and defines no `left` message type. Operations (44) and error codes (61) are unchanged; `openapi.yaml` changes only its version string;
- **compatibility.** A client that does not know `event: "user_notice"` ignores the event (and a `notification` with any `event` it does not know), so a 0.7.0 client talking to a 0.8.0 server is unaffected. A server older than 0.8.0 never sends it, so a client that wants notices reads `Live.version` from `/api/health/live` first and treats them as unavailable below `0.8.0`. Nothing is removed, renamed or reshaped.

## 0.7.0 (draft, awaiting architecture-owner approval)

Additive (WP-CON-03.T9, ADR-055 amendment); `/api/v1` and every existing path, operation, property and error code are unchanged (44 operations, 61 codes). It gives a client the one thing `spot-print` renders and the contract never named: a document as a PDF.
- printing: `GET /api/v1/print/{doctype}/{id}` (`print_get`), optional query `format` (a print format's name; omitted means the DocType's default). Success is `200 application/pdf`, `Content-Disposition: inline; filename="<doctype>-<id>.pdf"` built from the two validated path names and never from document content, and `Cache-Control: private, no-store`. No `ETag`, no `If-None-Match`, no `Range` and no `304` in this version: a print is a fresh render, not a byte-stable resource;
- permission is `read` on the document, and anything else — a missing document, a forbidden one, an unknown DocType — is the same `404 document.not_found`, checked before the format is resolved, so the answer is no existence oracle (the rule `file_get` follows). The page shows what the caller may read and no more: a hidden field is absent as it is from `resource_get`, a masked field prints masked, and the real value of a masked field never reaches a page (`field_reveal` remains the only path);
- `format` is the only query parameter: no template source, paper size, language, copy count or flag, so a client cannot ask for a template the site did not define;
- two error codes: `print.format_unknown` (404, the DocType defines no print format with that name — a name belonging to another DocType answers the same) and `print.render_failed` (500, retryable false, and neither the message nor `details` carries template text or document content) — 61 codes in all. A site with no print service answers `service.not_configured` with status 501, as the other unconfigured optional services already do (it stays outside the registry, as it is for files);
- rate class `interactive`, the class `report_run` runs in (`report_list` and `report_describe` state it for their own operations); no new class;
- `check.py` rule 14 (with a self-test case per part): the operation is a read-only `GET` (no other method, no request body), its responses are `application/pdf` only, `format` is the only query parameter, and no response header or property names a path or a storage key. The `spot-http` conformance suite lists `print_get` in `NOT_YET_SERVED` until WP-PLT-13.T2 serves it, and the Rust registry gains the two codes;
- **compatibility.** A server that does not implement this operation answers `404` (unknown route) or `501` (no print service), so a client reads `Live.version` from `/api/health/live` first and treats printing as unavailable below `0.7.0`: it hides the print action and keeps every other operation, which are byte-for-byte the 0.6.0 ones. Nothing is removed, renamed or reshaped, so a 0.6.0 client talking to a 0.7.0 server is unaffected.

## 0.6.0 (draft, awaiting architecture-owner approval)

Additive (WP-CON-03.T7, ADR-068 amendment A3); `/api/v1`, every path, operation, existing property and error code are unchanged (43 operations, 59 codes). It gives the client the rule expressions it needs to run field rules instantly (ADR-28), each decided by the architecture owner on 15 Oct 2026:
- rule expressions: `FieldMeta.depends_on`, `mandatory_depends_on`, `read_only_depends_on` and `default_expr`, optional strings of at most 1,024 characters in the shared expression language, over `doc` (and `parent` on a child row). A client evaluates them for instant feedback; the server enforces them on save;
- `DocTypeMeta.expr_version` (integer, at least 1): the language version the strings are written for, present whenever any field carries an expression. A client that does not know the version ignores every expression of the DocType and uses the static properties;
- lookup filters stay on the server: `lookup_search` applies the field's filter over the `doc` and `parent` snapshots the request already carries. `LookupRequest` stays closed (no `filter`, no map type, no change to the expression language);
- provisional fills: `LookupRow.provides`, an optional map keyed by the form's receiving field, holding the values the form's `fetch_from` fields would copy from the row, only values the asker may see and never a masked or reveal-gated one. The client may fill them at once as provisional; `form_event` replaces them and the server recomputes on save. Client-side caching of lookup pages needs no contract change;
- left out on purpose: no view schema (the client keeps definition order and `hidden`; views are a later bump with the form designer) and `fetch_from` is not on the wire (the server fills it in `form_event`);
- `check.py` rule 13 (with a self-test case for the filter and one for the missing version): expression properties are bounded strings, `expr_version` is present whenever an example carries an expression, `LookupRequest` stays closed without a filter, `LookupRow.provides` exists. The `spot-http` conformance suite gains a schema test for the new properties; the registry is unchanged.

## 0.5.0 (draft, awaiting architecture-owner approval)

Additive (WP-CON-03.T6, ADR-068 amendment A2); `/api/v1`, every path, operation, existing property and error code are unchanged. It carries the field-presentation inputs that the control catalogue (WP-UI-08) needs, each decided by the architecture owner on 15 Oct 2026:
- masked values: the server forms the masked text and never sends the real value before `field_reveal`, so no mask format is added to any metadata. `auth_permissions` gains `fields.revealable` (the masked fields this user may reveal); the reveal right is a permission, not a field property;
- currency symbol: `Document._currency` maps each `currency`-format field path to `{code, symbol}`, resolved by the server from the field's `currency_field` on that document. Presentation only;
- time zone: no new property. Instants show in the user's zone (`LocaleContext.timezone`), business dates are never converted and wall-clock values carry their own zone, as ADR-077 decides; a field-level zone setting is deliberately not added;
- attachment limits: `auth_permissions` gains `attach_limits`, per `attach` field `{max_bytes, accept}`, already resolved for the caller from the global default and any narrower override. `file_upload` enforces it; `files.max_upload_bytes` stays the ceiling;
- create from a lookup: no new property. A client offers "create" for a link when `rights.create` is true for the `link_to` DocType in the same permission summary;
- control hint and multi-select: `FieldHints.control` (a closed enumeration: `switch`, `checkbox`, `dropdown`, `radio`, `segmented`, `autocomplete`, `chips`, `textarea`); `FieldMeta.multiple` makes a `select` hold an array of its values; `LookupPage.columns` and `LookupRow.cells` give the picker several display columns;
- `CurrencyInfo` schema; no new operation and no new error code, so the registry and the `spot-http` conformance suite are unchanged (59 codes, 43 operations);
- `check.py` rule 12: no mask format reaches a client schema, `FieldHints.control` is closed, and the carriers named above exist.

## 0.4.0 (draft, awaiting architecture-owner approval)

Additive (WP-CON-03.T5, ADR-055 amendment, ADR-078 section 4); `/api/v1` and every existing path, schema property and error code are unchanged. It makes four library services reachable: reports beyond `report_run`, field reveal, bulk movement of data, and the named-consumer event stream:
- `GET /api/v1/report` (`report_list`) and `GET /api/v1/report/{report}` (`report_describe`): the reports the caller may see, and one report's typed parameters (with the options source of `select` and `link`), the columns *this caller* may see (never one removed for them) and `max_rows`. `report_run` is unchanged and now documents `422 report.bad_argument` and that an unauthorised caller gets an empty result, never a 403;
- `POST /api/v1/resource/{doctype}/{id}/reveal` (`field_reveal`): `{field, reason}` in, `{value}` out, for one field of one document; reason 1 to 500 characters; always audited; `Cache-Control: no-store` and never replayed from the idempotency store; one `403 permission.reveal_denied` for a missing document, a forbidden one and a caller without the right. There is no batch form and `reveal_bulk` is not exposed;
- `POST /api/v1/export` (`export_start`), `POST /api/v1/export/next` (`export_next`): CSV or NDJSON in bounded chunks, the signed cursor in the `X-Export-Cursor` response header and in the next request's body (never a URL), `X-Export-Rows`, and `X-Export-Watermark` for gap-free follow-on; `include_children` answers `multipart/mixed`;
- `POST /api/v1/export/extract` (`export_extract`): incremental extraction `{upserts, deletes, new_watermark, has_more}` with tombstones; `410 bulk.watermark_expired` means run a full export. New rate class `bulk`;
- `PUT|GET|DELETE /api/v1/events/consumers/{name}`, `GET /api/v1/events/consumers`, `POST .../pull`, `.../ack`, `.../restart` (`event_consumer_put`, `_get`, `_delete`, `_list`, `_pull`, `_ack`, `_restart`) and `GET /api/v1/events/stream` (`event_stream`, `text/event-stream`): the named consumer at delivery level D1 (ADR-078 section 6). One connection per consumer through a lease in `X-Consumer-Lease`; ack-before-advance; `EventEnvelope` has exactly the property names of `contracts/events`;
- `ReportSummary`, `ReportParameter`, `ReportDescription`, `RevealRequest`, `RevealResponse`, `ExportSpec`, `ExportNextRequest`, `ExtractRequest`, `ExtractRow`, `ExtractPage`, `EventPosition`, `ConsumerSpec`, `ConsumerStart`, `EventConsumer`, `PullRequest`, `PullBatch`, `EventEnvelope`, `AckRequest`, `RestartRequest` and `Checkpoint` schemas;
- thirteen error codes added: `report.bad_argument` (422), `permission.field_denied` (403, named in the contract for the first time; it already existed in the catalogue), `permission.reveal_denied` (403), `bulk.cursor_invalid` (422), `bulk.watermark_expired` (410), `bulk.limit` (429, retryable), `eventstream.consumer_not_found` (404), `eventstream.consumer_busy` (409, retryable), `eventstream.resync_required` (410), `eventstream.window_full` (429, retryable; carried in `messages` of a 200 pull), `eventstream.position_invalid` (422), `eventstream.ack_refused` (409), `eventstream.spec_invalid` (422) — 59 codes in all;
- `check.py` rules 9 to 11: a "status code" an operation description names is registered with that status; `EventEnvelope` agrees with `contracts/events`; no query parameter is a token, cursor or watermark (the document list's paging cursor is the one named exception), no response property is a database or storage key, and the reveal request is one field of one document.

## 0.3.0 (draft, awaiting architecture-owner approval)

Additive (WP-CON-03.T4, ADR-055 amendment); `/api/v1` and every existing path, schema field and error code are unchanged:
- `POST /api/v1/resource/{doctype}/form-event` (`form_event`): run the server's `field_changed` steps on an unsaved draft; a document diff in merge-patch form comes back, with `messages` and the resolved rule state for the changed region when the server computed it. Writes nothing, idempotent, rate class interactive; the diff never shows a field the caller cannot read;
- `GET /api/v1/auth/permissions` (`auth_permissions`, repeated `?doctype=`): per DocType the document rights as booleans and the field summary as `hidden`, `read_only` and `masked` lists — a UI hint, never authority; names no protection group and no other user's rights; carries the permission snapshot `etag`;
- `POST /api/v1/files` (`file_upload`): octet-stream or multipart, client-named by `Content-Disposition`, registry upload limit, the `file.scan` seam named in the description; the response is the record `{id, name, size, content_type, digest, private}` and never a path; `GET /api/v1/files/{id}` (`file_get`) streams with `Content-Type`, `Content-Disposition`, `ETag` and RFC 9110 single-range support; permission follows the attached document, or the uploader;
- `POST /api/v1/auth/password` (`auth_password_change`): change the current user's password, policy-checked (`details.rules` on failure), audited, other sessions ended (ADR-070 §5.4);
- `GET /api/v1/auth/sessions` (`auth_session_list`) and `DELETE /api/v1/auth/sessions/{id}` (`auth_session_end`): the user's own sessions, method/source/device shown, token never;
- `Live` gains the optional `version` property: a client reads the contract version from `/api/health/live` and degrades against an older server (0.2.0 servers answer without it);
- seven error codes added: `form.field_unknown` (400), `file.too_large` (413, retryable), `file.store_failed` (500, retryable), `file.scan_failed` (502), `file.virus_detected` (422), `auth.password_policy` (422), `auth.session_not_found` (404) — 46 codes in all.

## 0.2.0 (draft, awaiting architecture-owner approval)

Additive (WP-CON-03.T3, ADR-068, ADR-055 amendment); `/api/v1` and every existing path, schema field and error code are unchanged:
- `POST /api/v1/lookup/{doctype}/{field}` (`lookup_search`): the candidates for a field. The server picks the seam; the request cannot name one;
- `GET /api/v1/meta/{doctype}` carries the complete field: `label`, `read_only`, `default`, `lookup`, `hints`, `format` with `format_params`, `select` values, `link_type_field`, `link_field`, `max_chars` and `precision`;
- `FieldType` gains `long_text`, `attach`, `secret` and `table_multiselect`;
- new error code `lookup.seam_failed` (502, with an explanation), 39 codes in all;
- `check.py` rule 8: the lookup request is closed and names no seam.

## 0.1.0 (draft, awaiting architecture-owner approval)

First version (WP-CON-03.T1, ADR-055):
- health probes;
- session login with TOTP;
- DocType metadata;
- resource create, get, merge-patch, delete and query, with keyset paging;
- the lifecycle actions post, cancel, correct and rename;
- document methods and module functions;
- reports;
- the realtime WebSocket protocol;
- one value encoding (decimals as strings);
- `If-Match`/`ETag` concurrency and idempotency keys;
- one error envelope with a registry of 38 codes.
