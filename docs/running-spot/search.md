# Search

`spot serve` can search the DocTypes that declare a `[search]` projection (ADR-087 section 7): a
generated full-text column kept in step with the document in the same transaction, accent
folding, optional typo tolerance (trigram similarity, which includes Urdu), ranked paging,
facets and snippets. The request is the `search` clause of `POST /api/v1/resource/{doctype}/query`
(contract 0.22.0); the host half is `crates/framework/spot-cli/src/services/search.rs`.

## Level and failure

The service is **optional**. A site whose database cannot serve search keeps serving documents;
`/api/health/ready` names the `search` service degraded with the exact fix, and a search answers
`501 search.unsupported`. Results are never silently weaker: a database that cannot build Urdu
trigrams is degraded, not "working without typo tolerance".

## Configuration

`[services.search]` in `common.toml`. Unknown keys are refused.

| Key | Default | Meaning |
| --- | --- | --- |
| `enabled` | `true` | Off: no search service, no port; searches answer `501`. |
| `language` | `en` | The tenant's default text language; a DocType's `[search] language` wins. |
| `max_rebuild_rows_per_second` | 5000 | Planned throttle of a rebuild (see Limits). |
| `snippet_max_chars` | 200 | The longest snippet a request may ask for. |
| `lag_bound_ms` | 5000 | A projection older than this refuses a search as `503 search.projection_behind`. |

## Prerequisites (what the start-up check tells you)

- `pg_trgm` installed in the tenant database (for `typo_tolerance`), and `unaccent` (for accent
  folding). Install them with the audited `extension.install` operation of the Admin Center;
  the platform never installs an extension itself.
- A Unicode ctype on the tenant database. A `C` ctype builds no trigrams for Urdu, so the typo and
  Urdu-morphology fallback would silently return nothing. `create_database` creates tenant
  databases with `C.UTF-8`; a database made earlier must be recreated with one and restored.

## The rebuild job

`platform:search.rebuild` with `{"doctype": "<name>"}` recreates one DocType's projection (for
example after a metadata change that alters a searchable field). It runs in one transaction:
searches keep answering from the old projection until it commits, and a failure rolls back and
leaves the old projection intact. It is started through the job interface by an administrator.

## What leaves through search

The control holds in every channel the data leaves by: a field in a protection group never enters
the projection, so it cannot match or rank; naming a protected (or unknown) field in `search.fields`,
`facets` or `snippets` is `403 permission.field_denied`; results, facet counts, totals and snippets
are computed over rows the caller may read only; snippets are HTML-escaped with `<mark>` as the only
markup. A search term is never logged or used as a metric label.

## Limits of this version

- Paging is by position in the (rank, keys, id) order, bound to the query and the caller: a rank is
  never a value, so a keyset over rank cannot be written.
- The rebuild is one transaction (not throttled or resumable by id range); the throttle setting is
  reserved for the chunked rebuild.
- The metrics `search_query_seconds`, `search_projection_rows` and `search_rebuild_running` are not
  exported yet.
- The host must call `services::search::install_hook` where it builds the site's backend; until it
  does, every search answers `501 search.unsupported` and the service reports itself degraded when a
  DocType declares search.
