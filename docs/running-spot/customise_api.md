Part of [Running Spot](../running-spot.md).

# Customising a running site over HTTP

A site can get a new DocType, a custom field or a property override while it runs, with no restart. This page is
the reference of the seven HTTP calls that do it, what each takes and returns, every error a caller can see, and
what the site does at start-up and on readiness when a change could not be applied. Where a customisation comes
from and how a change reaches the other nodes is in [live metadata](metadata.md); this page does not repeat it.
How a client signs in and sends its token is in [the session guide](session.md).

Everything here was read from `crates/framework/spot-cli/src/services/customise.rs` and checked by
`python3 tools/docs-check/check_customise_api.py`. The worked session is taken from
`crates/framework/spot-cli/tests/customise.rs` (a real HTTP session against the test PostgreSQL), not run by hand.

## Who may customise

A caller may customise when they hold the role `Tenant Administrator` or `System Manager` through an
**unscoped** assignment. An assignment restricted to a company or another scope does not count: customising
changes the whole site (`Customise::caller`). The check is one function, `may_customise`; there is no separate
right for it.

- Anyone else gets `403 permission.denied` with `details.customise_code` = `customise.forbidden`, on every
  operation including `list` (`tests/customise.rs`).
- Only a signed-in user can call. Any other caller gets `auth.required`.
- The catalogue text of `customise.forbidden` mentions a "Customise right"; the host on this branch decides by the
  roles above, as the module comment of `customise.rs` says.
- A new DocType starts with **one grant**: `System Manager` may read, write, create and delete it. Everyone else is
  refused until a permission is granted through the site's permission documents (see the permission guide of your
  site; this API has no call for it) (`tests/pipeline.rs`: `a_new_doctype_is_closed_to_everyone_but_the_system_manager_until_granted`).

## The calls

All calls are `POST /api/v1/method/platform/customise/<operation>` with `Authorization: Bearer <session token>`
and a JSON body that wraps the arguments: `{"args": { ... }}`. `list` also answers `GET`. Calling any other
operation with `GET` is refused with `400 api.invalid_request` ("`<operation>` changes the site; call it with
POST."). A successful answer is `{"data": ...}`.

### `create_doctype`

Creates one or more DocTypes. A parent and its child table are created together in one change by passing both.

| Argument | Type | Required | Meaning |
| --- | --- | --- | --- |
| `definition` | text | one of the two | One DocType definition, in TOML text. |
| `definitions` | list of text | one of the two | Several TOML texts, created together, so a parent and the child table it names are valid at once. When `definitions` is present it wins; each item must be text. |

Result: `{"version": n}`. Refusals: `customise.invalid` (with the issue codes `customise.name_collision`,
`customise.reserved_name`, `customise.bad_definition`, `customise.metadata_invalid`, `customise.plan_invalid`,
`customise.module_not_allowed`), `customise.destructive_change`, `customise.forbidden`, and the failures of the
pipeline below.

### `update_doctype`

Replaces the definition of a custom DocType with a new TOML text. Only additive changes are applied.

| Argument | Type | Required | Meaning |
| --- | --- | --- | --- |
| `name` | text | yes | The DocType to change. |
| `definition` | text | yes | The new TOML text. |

Result: `{"version": n}`. A change that would drop or retype a field or table is refused as
`customise.destructive_change` and nothing is changed.

### `add_custom_field`

Adds a field to a DocType (an app DocType or a custom one).

| Argument | Type | Required | Meaning |
| --- | --- | --- | --- |
| `doctype` | text | yes | The DocType to add the field to. |
| `field` | object | yes | The field, written like a field of a definition: at least `name` and `type`, for example `{"name": "custom_site", "type": "text", "max_chars": 40}`. An unknown key, or a value of the wrong kind, is `400 api.invalid_request` ("`field` is not a field definition: ..."). The name must start with `custom_`. |
| `insert_after` | text | no | The field to place it after. An empty text counts as absent. |

Result: `{"version": n}`. A mandatory custom field on a DocType that holds records needs a default
(`customise.needs_default`); a name the DocType already uses is `customise.field_collision`; a name without the
`custom_` prefix is `customise.metadata_invalid`.

### `remove_custom_field`

Removes a custom field that this site added.

| Argument | Type | Required | Meaning |
| --- | --- | --- | --- |
| `doctype` | text | yes | The DocType. |
| `name` | text | yes | The custom field to remove. |

Result: `{"version": n}`. A field that something else still refers to is refused with `customise.referenced`, and
the referrers are named in the message. On this branch the host supplies no index, view or rule referrers
(`SiteHost::referrers` returns none), so the referrers you can see over HTTP are the fields of the same DocType
that point at it, for example through `fetch_from` (`spot-customise/tests/pipeline.rs`:
`removing_a_field_that_something_refers_to_lists_the_referrers`).

### `set_property`

Overrides one property of a DocType, or of one of its fields when `field` is given (for example the `label`).

| Argument | Type | Required | Meaning |
| --- | --- | --- | --- |
| `doctype` | text | yes | The DocType. |
| `field` | text | no | The field. Absent or empty: the property of the DocType itself. |
| `property` | text | yes | The property name, for example `label`. |
| `value` | text | yes | The new value, always as text. |

Result: `{"version": n}`. A security property (classification, permission level, protection group) can only be
tightened: loosening it is `customise.security_loosened`.

### `clear_property`

Removes a property override, so the app's value applies again.

| Argument | Type | Required | Meaning |
| --- | --- | --- | --- |
| `doctype` | text | yes | The DocType. |
| `field` | text | no | The field. Absent or empty: the DocType itself. |
| `property` | text | yes | The property to clear. |

Result: `{"version": n}`. Clearing the override of a security property of a standard field is a loosening and is
refused as `customise.security_loosened`.

### `list`

Lists every customisation the site holds, whatever its state. It takes no arguments and answers to `GET` or `POST`.

Result:

```json
{"data": {"entries": [
  {"kind": "custom_doctype", "doctype": "ticket", "field": null, "name": "ticket", "state": "applied",
   "reason": null, "intent": 1, "actor": "..."}
]}}
```

`kind` is `custom_doctype`, `custom_field` or `property_setter`; `state` is one of `pending`, `applied`, `failed`,
`removing` and `removed` (`applied` is live; `pending` and `removing` are a change a crash left half done, until
recovery settles it); `reason` explains a failed or rolled-back change.

## The result and the version

`{"version": n}` is the effective metadata version after the swap: the number that `GET /api/v1/meta/<doctype>`
now serves. A refused change leaves the version and the served metadata exactly as they were
(`tests/pipeline.rs`: `a_destructive_change_is_refused_and_leaves_nothing_behind`). To confirm a change is live,
read `GET /api/v1/meta/<doctype>`: the DocType (or field, or label) is there, and the `ETag` changes
(`tests/customise.rs`: `a_label_override_changes_the_meta_etag`). On a cluster, the other nodes follow within one
poll of the version store (`[services.metadata] poll_ms`).

## Errors

Every failure is an API error with the code below, the text of the problem in `error.message`, the customisation
code in `error.details.customise_code`, and, for a validation failure, one entry per problem in `error.fields`:
`{"field": "<doctype>" or "<doctype>.<field>", "code": "...", "message": "..."}`.

| Situation | Status and API code | `details.customise_code` |
| --- | --- | --- |
| A required argument is missing or is not text ("`name` is required and must be text.") | 400 `api.invalid_request` | none |
| `field` is not a field definition; `definitions` is not a list of text | 400 `api.invalid_request` | none |
| A changing operation sent with `GET` | 400 `api.invalid_request` | none |
| No customise operation of that name | 404 `method.not_found` | none |
| The caller may not customise | 403 `permission.denied` | `customise.forbidden` |
| The change does not validate (all problems at once) | 422 `document.validation_failed` | `customise.invalid` |
| The change would need a destructive schema step | 422 `document.validation_failed` | `customise.destructive_change` |
| The customisation to change does not exist | 404 `meta.unknown_doctype` | `customise.not_found` |
| This node cannot reproduce the metadata a peer announced | 503 `server.unavailable` | `customise.divergence` |
| The database refused the schema step (rolled back) | 500 `http.internal` | `customise.schema_failed` |
| The customisation records could not be read or written | 500 `http.internal` | `customise.store` |
| The new snapshot could not be built, adopted or published | 500 `http.internal` | `customise.swap_failed` |
| A test fault hook stopped the pipeline | 500 `http.internal` | `customise.interrupted` |

A validation failure has one `fields` entry per issue; the issue's own code (for example
`customise.name_collision`) is the entry's `code`. A destructive change has one entry per refused step, with the
path `<entity>` or `<entity>.<field>`, the code `customise.destructive_change` and the reason as the message.

`tests/customise.rs` sends `add_custom_field` on `ticket` with a name the DocType already has and asserts a `422` with a
non-empty `fields` list; the body has this shape (the message text is the code's, shortened here):

```json
{"error": {"code": "document.validation_failed",
           "message": "the change is not valid: customise.field_collision at `ticket.subject`: `ticket` already has a field named `subject`",
           "details": {"customise_code": "customise.invalid"},
           "fields": [{"field": "ticket.subject", "code": "customise.field_collision", "message": "`ticket` already has a field named `subject`"}]}}
```

### Every customisation code

The remedies are quoted from `crates/framework/spot-errors/catalogue/customise.toml`.

| Code | Where you see it | Meaning and remedy |
| --- | --- | --- |
| `customise.forbidden` | `details.customise_code` | The caller does not hold the platform Customise right. Ask a platform administrator for a role that holds the Customise right, or have one make the change. |
| `customise.invalid` | `details.customise_code` | The change does not validate; every problem found is listed. Fix each listed problem (the paths name the DocType and field) and submit the change again. |
| `customise.destructive_change` | `details.customise_code` and a field entry | The change would need a destructive or lossy schema step, which a runtime change never applies. Make the change additive (add, do not drop or retype), or use the expand and contract migration path. |
| `customise.schema_failed` | `details.customise_code` | The database refused the schema step; the change was rolled back and nothing is live. Check the storage backend's health and the reason on the record, then retry the change. |
| `customise.store` | `details.customise_code` | The customisation records could not be read or written. Check the storage backend's health and retry. |
| `customise.swap_failed` | `details.customise_code` | The new metadata snapshot could not be built, adopted or published. Read the metalive code in the message, fix the cause and run recovery. |
| `customise.divergence` | `details.customise_code` | This node cannot reproduce the metadata another node announced and refuses to serve metadata until it can. Reconcile this node from the shared records (run recovery) before it serves requests. |
| `customise.not_found` | `details.customise_code` | The customisation to change does not exist or is not applied yet. List the customisations and name one that is applied. |
| `customise.interrupted` | `details.customise_code` | The pipeline was stopped by a test fault hook after a step. Run recovery; the hook exists only in tests. |
| `customise.bad_definition` | a field entry | A definition, field spec or property value is not valid text. Correct the named definition, field spec or property and submit it again. |
| `customise.name_collision` | a field entry | A custom DocType name is already taken by an app DocType or another custom DocType. Choose a different DocType name. |
| `customise.reserved_name` | a field entry | A custom DocType name is reserved by the platform. Choose a name that does not start with a platform prefix and is not a platform DocType. |
| `customise.module_not_allowed` | a field entry | Only a platform administrator may place a custom DocType outside the custom module, and a module cannot change. Leave the module as custom, or have a platform administrator make the change. |
| `customise.field_collision` | a field entry | A custom field collides with a standard or existing field of the DocType. Pick a field name that the DocType does not use; custom fields start with custom_. |
| `customise.metadata_invalid` | a field entry | The metadata registry refused the result of the change. Fix the problem named in the message; the registry reports every one at once. |
| `customise.plan_invalid` | a field entry | The plan compiler refused the result of the change, for example a cycle among defaults and fetches. Remove the cycle or the bad rule named in the message. |
| `customise.needs_default` | a field entry | A mandatory custom field on a DocType that holds records needs a default to fill them with. Give the field a default, or make it optional. |
| `customise.security_loosened` | a field entry | A site may only tighten a security property, never loosen it. Keep or raise the protection; to lower it, change the standard through an app update. |
| `customise.referenced` | a field entry | The field is still referred to by an index, a rule, a view, a permission or another field. Remove or change the listed referrers first, then remove the field. |

The metadata layer has its own codes, which can appear inside a `customise.swap_failed` message or on readiness:
`metalive.divergence` (two nodes hold the same version number with different content), `metalive.build` (building
the next snapshot failed; the previous one is still serving), `metalive.unknown_custom_field`, `metalive.store`,
`metalive.encode` and `metalive.exhausted`. Their meanings and remedies are in
`crates/framework/spot-errors/catalogue/metalive.toml` and in [live metadata](metadata.md).

## Start-up and readiness

**At start-up**, before the site accepts traffic, the `customise` service runs its check: it recovers a change that a
crash left half done between two pipeline steps (a change is first written as `pending`, then the schema is changed,
then it is marked applied and swapped in), and it retries a swap that did not reach the document engine. The
service is required (`running-spot.md` lists it, order 400), and a required service whose check fails stops that
site: it answers `503 server.unavailable` while the other sites keep serving. A crash between any
two steps converges on restart (`tests/customise.rs`; `spot-customise/tests/recovery.rs`).

**While running**, every node polls the version store; when another node announced a newer version, or the same
version with different content, it rebuilds from the shared customisation records, which are the truth (the version is
only a claim about them), and it retries a failed swap on the next poll.

**On readiness**, `GET /api/health/ready` answers `503` with the check `metadata:<site>` failed when:

- a swap could not reach the engine: the detail reads `metadata version <n> could not be served: <reason>`; the
  previous metadata keeps serving what it can, and the next poll retries;
- or this node **diverged**: it cannot reproduce what another node announced (`metalive.divergence`); the detail reads
  `metadata diverged: ...`. While it lasts, requests that need the metadata answer `503 server.unavailable` with
  `details.reason`, and the node does not serve metadata it could not verify.

What to do: read the detail text on the readiness check; look at the customisation records with `list` (a change in
`pending` or a recorded `reason`); wait one poll (the poll interval is `[services.metadata] poll_ms`); if it persists,
restart the node, whose start-up recovery rebuilds from the shared records. These are the only recoveries this code
supports.

Three things never happen (`spot-customise/tests/cross_node.rs`, `recovery.rs`, `pipeline.rs`): a half-applied change
is not served (the swap happens only after the records and the schema step are done); an old version is not served
as if it were new after a failed swap (the failure is reported on readiness and retried); and a node does not serve
metadata it could not verify (`a_node_that_cannot_reproduce_the_announced_metadata_refuses_to_serve_it`).

## A worked session

This is the session of `tests/customise.rs`, with the request shapes it sends. Replace the host with your site's
and `TOKEN` with the session token you got from signing in (the test signs in as a user holding `System Manager`).

1. Sign in:

   ```sh
   curl -s -X POST https://acme.example.invalid/api/v1/auth/login \
        -H 'Content-Type: application/json' \
        -d '{"user": "manager@example.com", "password": "EXAMPLE-PASSWORD"}'
   # {"data": {"token": "TOKEN", ...}}
   ```

2. Create a DocType and its child table together (the TOML texts are those of the test: `ticket` with
   `subject`, `priority` and a `notes` table of `ticket_note`):

   ```sh
   curl -s -X POST https://acme.example.invalid/api/v1/method/platform/customise/create_doctype \
        -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
        -d '{"args": {"definitions": ["name = \"ticket\"\n...", "name = \"ticket_note\"\nis_child = true\n..."]}}'
   # 200 {"data": {"version": 2}}
   ```

   The very next request works with no restart: `GET /api/v1/meta/ticket` is `200`, and `POST /api/v1/resource/ticket`
   with `{"subject": "Printer is on fire", "notes": [{"text": "first note"}]}` is `201` with the default
   `"priority": "normal"`.

3. Add a custom field and use it:

   ```sh
   curl -s -X POST https://acme.example.invalid/api/v1/method/platform/customise/add_custom_field \
        -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
        -d '{"args": {"doctype": "ticket", "field": {"name": "custom_site", "type": "text", "max_chars": 40}}}'
   # 200 {"data": {"version": 3}}
   ```

   (The version numbers in these comments are illustrative; the test only asserts that it is greater than zero.)

4. Override a label, then clear it (the test does this on the standard DocType `project`, and the `/meta` ETag
   changes each time):

   ```sh
   ... customise/set_property  -d '{"args": {"doctype": "project", "field": "title", "property": "label", "value": "Project name"}}'
   ... customise/clear_property -d '{"args": {"doctype": "project", "field": "title", "property": "label"}}'
   ```

5. List what the site holds:

   ```sh
   curl -s https://acme.example.invalid/api/v1/method/platform/customise/list -H "Authorization: Bearer $TOKEN"
   # {"data": {"entries": [{"kind": "custom_doctype", "state": "applied", ...}, {"kind": "custom_field", "state": "applied", ...}]}}
   ```

6. Remove the field:

   ```sh
   ... customise/remove_custom_field -d '{"args": {"doctype": "ticket", "name": "custom_site"}}'
   ```

## Troubleshooting

| Symptom | Cause | What to do |
| --- | --- | --- |
| `403 permission.denied` with `customise.forbidden` | The user has neither `Tenant Administrator` nor `System Manager`, or only through an assignment limited to a company | Use a user with one of the roles assigned without a scope. |
| `400 api.invalid_request`, "`definition` is required and must be text." | The body is not wrapped in `{"args": ...}`, or the argument is missing | Send `{"args": {"definition": "..."}}`. |
| `422` with `customise.name_collision` | The DocType name is already an app or custom DocType | Choose another name; every collision is listed at once. |
| `422` with `customise.reserved_name` | The name is a platform DocType or starts with a platform prefix | Choose a name that is neither. |
| `422` with `customise.needs_default` | A required custom field on a DocType that has records | Add a `default`, or make the field optional. |
| `422` with `customise.security_loosened` | The override lowers a classification, permission level or protection group, or clears a security override | Keep or raise the protection; lowering needs an app update. |
| `422` with `customise.referenced` | Another field (or a view, rule or index, where supplied) still refers to the field | Remove the listed referrers first, then remove the field. |
| `422` with `customise.destructive_change` | An `update_doctype` text drops or retypes a field | Keep the field and add instead; the message names the step. |
| `422` with `customise.field_collision` or a name without the `custom_` prefix | The DocType already has a field of that name, or the prefix is missing | Pick a free name that starts with `custom_`. |
| A change is not visible on a second node yet | The node follows on its poll | Wait one `poll_ms`; if it never appears, read the node's readiness detail. |
| `/api/health/ready` is `503` after a crash | The start-up recovery or a swap failed, or the node diverged | Read the `metadata:<site>` detail, wait one poll, and restart the node if it persists (see Start-up and readiness). |
| `503 server.unavailable` on a request, with `details.reason` | The node refuses metadata it cannot verify (divergence) | Same as the previous row; the next successful poll clears it. |
