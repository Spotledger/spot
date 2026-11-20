Part of [Running Spot](../running-spot.md).

# Live metadata: where it comes from and how a change goes live

ADR-05 says nothing a user changes may need a rebuild or a restart. Five crates deliver it:
`spot-meta` (definitions, merge, validation), `spot-plan` (compiled plans), `spot-meta-live`
(the versioned snapshot and the other nodes), `spot-customise` (the crash-safe change path) and
`spot-pg-migrate` (the schema half). This page tells the one story. A script
(`tools/docs-check/check_metadata_guide.py`) keeps its codes, paths and rules equal to the source.

## Where metadata comes from

| Source | What it is | Who may create it |
| --- | --- | --- |
| App definitions | TOML files in an app's `doctypes/` directory; they ship with the app | the app's authors (an app update replaces them) |
| Custom DocType | a DocType a site creates while running; it lives in the module `custom` | a caller holding the platform `Customise` right (System Manager always holds it) |
| Custom field | a field a site adds to any DocType; its name starts with `custom_` | the same |
| Property setter | an override of one property of a field or of a DocType | the same |

The **effective metadata** is made by `spot-meta`'s `Registry::build`: take the app definitions,
append the custom fields, apply the property overrides in the order they were made (a later
override of the same property wins), validate the result (every problem is reported at once), then
derive what the engine needs (engine fields on posted DocTypes, inherited protection groups). The
effective metadata is immutable. A reader keeps the one it took until it is done, so a request that
started on version N finishes on N.

```text
app definitions + custom fields + property overrides
        |  merge and validate (all errors at once)
        v
effective metadata (version V)  ->  compiled plans (spot-plan)  ->  the engine
        |
        +-> storage catalogue  ->  schema plan (spot-pg-migrate)  ->  the database
```

<!-- safety-rules -->
## Rules a site may not break

`spot-customise` enforces these before anything is persisted. Each refusal has a stable code:

- A custom DocType name may not **collide** with an app DocType or another custom one
  (`customise.name_collision`), and may not use a **reserved** platform name
  (`customise.reserved_name`).
- A custom DocType lives in the module `custom`; only a platform administrator may pick another
  (`customise.module_not_allowed`).
- A custom field may not collide with an existing field of the DocType (`customise.field_collision`).
- A mandatory custom field on a DocType that holds records needs a **default** to fill them with
  (`customise.needs_default`).
- A security property may only be **tightened**, never loosened (`customise.security_loosened`).
- Removing a field that something refers to is refused, with the **referrers** listed
  (`customise.referenced`).
- A new DocType is **closed** to everyone except the single default grant (System Manager read,
  write, create, delete) until more access is granted.
- A change that needs a destructive or lossy schema step is refused (`customise.destructive_change`):
  a runtime change only adds.

<!-- end-safety-rules -->

## The version

The metadata version is the number every layer agrees on. <!-- version-rule: next_change_version -->
It rises by exactly one per accepted change on a node that started from stored metadata, and it
never returns to 1: the starting snapshot of a fresh node already reports version 1, so the first
local change mints version 2 (`next_change_version` in `spot-meta-live` holds that one rule, and
both `LiveMeta::apply` and `spot-customise` use it). A refused change moves nothing: the old
snapshot keeps serving, the version does not change.

The version is the `ETag` of `GET /api/v1/meta/<doctype>` (a request with a matching `If-None-Match`
gets "not modified"). It is **not** the plan cache's key: two separately built metadata values can
report the same version with different content, so `PlanCache::refresh` compares the **plan hash**
of every DocType and reuses the old plan for each one whose hash did not change. A refresh of
unchanged metadata rebuilds nothing, and a plan a request holds stays valid.

## Life of a change

`CustomiseService` runs every change through one pipeline. The previous snapshot serves until the
last step:

1. **Validate** in a scratch copy of the metadata. Every problem is reported with its path
   (`customise.invalid`, `customise.metadata_invalid`, `customise.plan_invalid`). Nothing is touched.
2. **Plan** the schema for the resulting catalogue and refuse a destructive step
   (`customise.destructive_change`).
3. **Persist** the records as `pending`, atomically. This is the intent marker.
4. **Apply** the additive schema plan to the database (`customise.schema_failed` rolls the change
   back to `failed` with the reason, and nothing is live).
5. **Mark and swap**: mark the records `applied`, build the new snapshot, adopt it on this node,
   publish it on the invalidation bus and tell the host (`customise.swap_failed` when that fails).
6. **Announce** the lifecycle events and write the audit entry.

A crash between any two steps is recovered by `CustomiseService::recover` (the start of every later
call does it too). It finds records still `pending` or `removing`, re-plans from the records (the
plan is idempotent) and either finishes them or rolls them back to `failed` with the reason. A
removed custom field keeps its column in the database (a runtime change never drops data), so its
record stays as `removed` and the schema plan keeps seeing the column.

## Other nodes

A node that makes a change publishes `(version, content hash)` on the invalidation bus. The
single-node default is an in-process broadcast; a cluster writes the version and hash to the
database (`PollingBus`) and every node reads them on its own schedule, the host calling
`Follower::poll_once` when `PollSchedule` says it is due. A follower that is told of version N loads
the definitions of N, builds and swaps. It reconciles from the store on a bus gap and at start-up,
and it **never goes backwards**: an older version is ignored.

What the test proves (`spot-customise/tests/cross_node.rs`): a second node serves a new DocType
within one poll, a service on a second node catches up from the shared records, and a node that
cannot reproduce what was announced refuses to serve metadata. On the same node the change is
visible to the next request (the L10 suite, `harness/rust/l10-liveness`, 16 scenarios).

**Divergence.** `metalive.divergence` means two nodes hold the **same version number with different
content** (the hash is independent of the version, so a version-only comparison cannot hide it).
The node keeps serving its own snapshot and, through `customise.divergence`, refuses to serve
metadata it cannot reproduce rather than guess. The operator stops sending requests to that node,
runs recovery so it reconciles from the shared records, and reports it if it persists: two nodes
minting the same version with different content is a defect.

## What an app author must know

- A definition change shipped in an app update goes through the same path (`ReplaceDefinitions`
  in the L10 suite, `SetDefinitions` in the code): customisations stay and apply on top, and an
  update that would collide with a site's custom field is refused naming it.
- The typed views of the generator (`spot-typed-views`) are fixed at build time from the app's
  definitions. A custom field is not in generated code; read it dynamically by name through the
  document (`Document::get`).
- Changes the runtime path refuses (a drop, a rename, a type change, narrowing) need the expand and
  contract path; for the schema side see `spot migrate` in `schema-migration.md` when it exists on
  your base.

## Troubleshooting

| Symptom or code | Cause | What to do |
| --- | --- | --- |
| a change is visible on one node and not on another | the other node has not polled yet, or its bus missed the event | wait one poll interval; if it persists, run recovery on that node and check `metalive.store` in its log |
| `customise.forbidden` | the caller does not hold the `Customise` right | have a System Manager make the change or grant a role the right |
| `customise.field_collision` | the DocType already has a field of that name | pick another `custom_` name |
| `customise.needs_default` | a mandatory custom field on a DocType that holds records | give a default, or make the field optional |
| `customise.security_loosened` | the change lowers a protection | keep or raise it; to lower it, change the standard through an app update |
| `customise.referenced` | an index, rule, view, permission or field still refers to it | remove or change the listed referrers first |
| a plan did not refresh | the plan hash did not change, so the old plan is reused on purpose | check the DocType really changed; `PlanCache::refresh` lists the DocTypes it rebuilt |
| a change is stuck as `pending` after a crash | the process stopped between persist and mark | run recovery; it finishes or rolls the record back to `failed` with a reason |
| `metalive.divergence` or `customise.divergence` | same version number, different content | stop traffic to the node, run recovery, report if it persists |
| the version did not rise after a change | the change was refused (a refused change moves nothing) or was a no-op | read the refusal code; nothing was applied |
| a custom DocType nobody can open | a new DocType is closed except for the default System Manager grant | grant roles access to it |
| `customise.schema_failed` | the database refused the schema step | check the storage backend's health and the reason on the record, then retry |
| `metalive.build` | the next snapshot did not build; the previous one is still serving | fix the named metadata or plan errors and apply again |

## Every code of the live metadata crates

| Code | Meaning |
| --- | --- |
| `metalive.divergence` | two nodes hold the same version with different content |
| `metalive.build` | building the next snapshot failed; the previous one serves |
| `metalive.unknown_custom_field` | a change tried to remove a custom field the site does not have |
| `metalive.store` | the version store could not be read or written |
| `metalive.encode` | the definitions could not be encoded for the content hash (a bug) |
| `metalive.exhausted` | the version space is exhausted (cannot happen in practice; a bug) |
| `customise.forbidden` | the caller does not hold the Customise right |
| `customise.invalid` | the change does not validate; every problem is listed |
| `customise.destructive_change` | the change needs a destructive or lossy schema step |
| `customise.schema_failed` | the database refused the schema step |
| `customise.store` | the customisation records could not be read or written |
| `customise.swap_failed` | the new snapshot could not be built, adopted or published |
| `customise.divergence` | this node cannot reproduce the announced metadata |
| `customise.not_found` | the customisation to change does not exist or is not applied yet |
| `customise.interrupted` | the pipeline was stopped by a test fault hook (tests only) |
| `customise.bad_definition` | a definition, field spec or property value is not valid text |
| `customise.metadata_invalid` | the registry refused the result of the change |
| `customise.plan_invalid` | the plan compiler refused the result, for example a cycle |
