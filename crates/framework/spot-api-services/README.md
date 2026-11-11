# spot-api-services

Adapters from `spot-http`'s service interfaces to a document host and an immutable effective
metadata snapshot. Authentication is deliberately not implemented here.

`EngineResources::new` takes an `Arc<S>`, matching `EffectiveMeta`, a `Clock`, and a caller's
`ActorResolver`. The resolver must return the already authenticated actor for the request's
credential **and site**, or fail. No random user id, fallback system identity or anonymous
write is created by the adapter. Invocation time is taken once per operation.

Document writes go through `DocumentService`; reads load through the same service. Values
stay typed, including exact decimals and child-row identifiers. Errors use
`ErrorClass::api_code` and retain field errors, details and explanation. `MetaResources`
uses the effective metadata version for the HTTP layer's version ETag and preserves field
formats, format parameters, child/table-multiselect targets, defaults, lookup and hints.

## Form events (WP-UI-01.T1)

`EngineFormEvents` serves one `POST /api/v1/resource/{doctype}/form-event` over the engine's
`DocumentService::field_changed`. A request is the unsaved draft, the `changed` field path and
the optional `base_version`; the answer is a document diff in the merge-patch form
`resource_update` takes (`{"data": {"patch": …}, "messages": …}`).

- The draft decodes through the resource-write codec, so field names, value types and child
  rows are checked exactly as a create body is. The contract's row `idx` is accepted and
  checked against the row's position.
- The operation is a pure function of (draft, metadata, principal): it opens no transaction,
  takes no lock, writes no outbox row and answers the same bytes for the same request. A row
  the draft does not identify gets an id derived from the draft, never from a clock or a
  random source.
- The caller needs `read` on the DocType. The diff names no field the caller cannot read, a
  masked field only appears in its masked form, and a message about a hidden field is dropped.
  A `changed` path the caller cannot read is refused without naming the field.
- `base_version` is validated (integer at least 1) and is not compared with storage: the
  contract's operation does not re-read the document, so a stale base for a pure derivation is
  not an error. The save's `If-Match` remains the stale-write check.
- `Limits` bounds the body and the draft's child rows and sets a per-user in-process token
  bucket; a `deadline` from the request turns a runaway step into `504 http.timeout`, and
  dropping the future leaves nothing behind.
- `form.field_unknown` needs to know which `field_changed:<point>` steps exist. The seam
  registry is the production source; `FieldChangedPlan` is the small seam, and the default
  `EveryField` answers every field (an empty diff rather than the error) for hosts that do not
  wire one yet.

## The permission summary (WP-UI-01.T1)

`AccessControl` is the permission view the form-event and summary adapters share: the decision
core's snapshot, the actor→principal source, the effective metadata, the snapshot version and
the adapter's clock and actor resolver. `PermissionResources` implements `PermissionService`:
per requested DocType (or every DocType the user holds a right on) the eight document rights,
the field summary (`hidden`, `read_only`, `masked`, `revealable`) and the version whose quoted
form is the `ETag`. Document-dependent grants are summarised optimistically — the question is
"could any assignment grant this?" — so owner, scope and condition grants count; the server
still decides every real operation. The summary names no protection group and no other user.


## Queries

`DocumentService` currently has no query method. The local `QueryService` seam exposes the
existing storage IR; its Engine implementation compiles the supplied metadata catalog and
uses the engine's public backend read path. This mirrors the engine's present policy: neither
path integrates permission evaluation. **Production mounting requires caller-enforced site
binding and read authorization**, or a `QueryService` wrapper that compiles row/column policy.
It is not safe to share one unguarded engine across tenant contexts.

The smallest upstream addition is a permission-filtered
`DocumentService::query(&Invocation, &Query) -> BoxFuture<AppResult<Rows>>`, like the existing
`Repository::query`. No contract changes are made in this task.

Filters convert through the HTTP scalar codec to typed IR (including decimal/date/link
values). Queries explicitly select record id regardless of requested projection. Sort keys
plus record id form a lexicographic continuation predicate with explicit null behavior; no
numeric offsets are used. Cursors are opaque base64url JSON, bound to site, actor, DocType,
metadata version and the query. They are validated input, not an authorization credential or
a cryptographically signed token. Totals count all matches before cursor application.

These are live keyset pages, not a repeatable-read snapshot across requests: inserts/deletes
before the boundary do not shift an offset, but changing an existing record's sort key can
move it across the boundary. Count and loaded documents also are separate reads.

## Upstream blocker (not completed conformance)

The current `spot-document::Engine` returns `platform.unsupported` with class Internal for
post, cancel, correct, rename and whitelisted methods (WP-DOC-04.T2). The adapters delegate
those calls as required, so real HTTP requests return the corresponding 500 error without
mutations. This crate cannot implement those lifecycles without editing unowned engine code.

The real-engine oneshot tests cover every requested operation, but the five unsupported
operations test error propagation, **not successful lifecycle behavior**. The response-schema
suite validates the responses actually produced against the native OpenAPI contract. The task
must remain blocked until the engine supports those operations and success cases can replace
the blocker regression tests. Reports/module functions remain typed not-found fallbacks;
`DocumentService` does not expose either subsystem. Auth and lookup are independent services,
not tested as if this adapter implemented them.
