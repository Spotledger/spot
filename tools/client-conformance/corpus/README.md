# Client conformance corpus

Scripted sessions for the sans-IO client core (ADR-088 section 9). Every client implementation
of this behaviour (the Rust crate `spot-client-core`, later the C# client) must produce exactly
the recorded outputs.

A file is `{"topic": "...", "cases": [...]}`. A case is `{"name", "description"?, "steps": [...]}`.
A step is `{"in": <Input>, "out": [<Output>...]}`: feed `in` to a fresh-per-case core and compare
the returned outputs, in order, with `out`. The JSON shapes are those of `Input` and `Output` in
`crates/framework/spot-client-core/src/types.rs` (tagged by `type`, `op`, `kind` and `event`).

Time exists only as `tick` inputs. Request ids (`req_id`) come from the application and transport
ids (`id`) are assigned by the core, starting at 1 and counting up per core.

A case changes only after the specification (the contract, ADR-088 and ADR-092) changes first.

## Files that need the storage feature

A file with a top-level `"requires": ["storage"]` describes behaviour that exists only when the
core is built with persistence (`storage-*.json`). The default-feature runner skips such files and
says how many it skipped; a second runner (`tests/corpus_storage.rs`, `--features storage`) runs
only them. `run.py --leg rust` runs both and fails if either fails. A case in such a file may begin
with `"start": {"site": "s1", "out": [...]}`: the host calls `Core::start(site)` and gets exactly
`out` (the one `storage_read`); `storage_reply` steps then feed the stored text.

The stored queue is `{"v":2,"owner":{"site","user_id"},"calls":[...]}`. Requests to `/api/v1/auth/`,
`/api/v1/events/` and `.../reveal` paths (after normalisation) are sent but never stored. A restored
queue is held apart until a sign-in: for the same owner its calls go out once; otherwise (another
user or site, the old untagged array, unreadable text) each call yields `failed` with
`auth.session_changed`, then one `queue_discarded` event with `count` and `reason`
(`owner_changed`, `untagged` or `unreadable`), then (after `signed_in`) a `storage_write` with
`value: null`.

A write under `/api/v1/resource/{doctype}` that carries a body (or a query) is stored only when the
core has seen that DocType's metadata in a `200` reply to `GET /api/v1/meta/{doctype}` (the host
issues the read like any `GET`; the core sends nothing of its own) and the metadata has no field of
type `secret` (or `bytes`: the server's meta_get sends a secret field as `bytes` today, so a bytes field is treated as a possible secret) and every `table` or `table_multiselect` field names a `child_doctype` whose metadata
has itself been seen and is safe (decided when the call is stored, so the order of the replies does
not matter). Otherwise the call is still sent, retried and held for re-auth from memory, but no
`storage_write` holds it, and a restored call of such a DocType is replayed after the owner's sign-in
but dropped from the next write. A later reply replaces an earlier one (a `secret` field added
later removes the call from the next write; a reply that does not parse forgets the DocType), and a
different user signing in or a sign-out clears everything learned. A call without a body and without
a query (`DELETE`, `post`, `cancel`) and calls outside `/api/v1/resource/` are judged by path only.
`storage-secret-doctypes.json` and `storage-secret-doctypes-2.json` hold the cases.

Run: `python3 tools/client-conformance/run.py --leg rust`.
