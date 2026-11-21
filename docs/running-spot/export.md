Part of [Running Spot](../running-spot.md).

# Bulk export and incremental extraction

`/api/v1/export*` moves a DocType out in bulk: a permission-filtered, field-guarded export to CSV
or NDJSON in bounded chunks (`export_start`, `export_next`), and an incremental extract of what
changed since a watermark, with deletions as tombstones (`export_extract`). The contract is
`contracts/native-api/openapi.yaml`. The rules live in `spot-bulk-data`; this page is what the
host adds and what an operator can turn.

Bulk export is the platform's largest data-exfiltration surface. Everything below is checked on
every request, cursors included, against the permission snapshot served *now*.

## Who may export what (default deny)

- **The `export` right.** `export` is a DocType right of a role's grants (`rights = ["export"]`
  in an app's `permissions/grants.toml`). `Tenant Administrator` holds it by name. An `if_owner`
  grant of it does not count: an export has no single owner to test. A caller without the right
  gets `403 permission.denied`, never an empty success, and so does a caller who lost the right
  between two chunks. A session that has not chosen its password yet may do nothing here.
- **Rows.** Only rows the caller may read: the read filter (roles, user permissions, `if_owner`)
  is part of every page query. A filter or sort on a field the caller may not use unmasked is
  `403 permission.field_denied` naming the field (no inference).
- **Columns.** The Export channel of the field guard decides each column. With no `columns` the
  default is every column the caller may export. A requested column the caller may not export,
  or that the DocType does not have, is an error naming it (`403 permission.field_denied`,
  `422 document.unknown_field`), never silently dropped. A column that may leave only masked
  comes out masked. A `Secret` field never leaves, and cannot be filtered or sorted on.
- **Child tables.** `include_children` adds each child table as its own part; every child row
  carries its parent's id in `parent`. A child row is exported only when its parent is a row the
  export selects. The response is `multipart/mixed`, **one table per chunk** (the parent first):
  a chunk holds the part its cursor was in, with `name` in its `Content-Disposition`.

## Using it

```
POST /api/v1/export          {"doctype":"sales_invoice","columns":["name","grand_total"],
                              "filter":{"field":"status","op":"eq","value":"Draft"},
                              "order":[{"field":"name"}],"format":"ndjson"}
POST /api/v1/export/next     {"cursor":"<X-Export-Cursor of the last chunk>"}
POST /api/v1/export/extract  {"doctype":"sales_invoice","watermark":"<X-Export-Watermark>","limit":1000}
```

- **A chunk** is at most `rows_per_chunk` rows and `bytes_per_chunk` bytes. `X-Export-Rows` and
  `X-Export-Watermark` are on every chunk; `X-Export-Cursor` is on every chunk but the last. The
  last chunk is **followed by one more request**: the end is known only when a read finds
  nothing, so the request after the final rows answers an empty body and no cursor (CSV: no
  header again). A client loops while `X-Export-Cursor` is present.
- **Paging is by keyset**, never by offset, and no transaction is held between chunks. Rows
  inserted behind the cursor are not delivered, rows changed ahead of it arrive in their current
  state, and a repeated request with the same cursor returns the same bytes.
- **`max_rows`** stops the export after that many parent rows (at most `max_rows_cap`; above it is
  `400 api.invalid_request` with `details.max_rows` naming the cap). It cannot be combined with
  `include_children`.
- **Formats.** Decimals are exact strings with their scale, instants are UTC RFC 3339, `null` is
  empty in CSV. A text cell starting with `=`, `+`, `-`, `@`, tab or CR gets a leading `'` in CSV
  so a spreadsheet does not run it; NDJSON is not altered.
- **The watermark** of a full export is a position in the change feed at or before the export's
  start; `export_extract` from it follows the export without a gap (a row that changed during the
  export arrives again, which is harmless: apply by `id` and `version`). An extract page holds at
  most `limit` documents (1 to 5,000) with their `id`, `version` and the requested columns, and
  the ids deleted (tombstones). A document that is no longer visible to the caller is reported as
  a delete. Store `new_watermark` after applying the page; `has_more` says to call again. An
  extract scans a bounded stretch of the feed per call (a page may be empty and still have
  more).
- **A watermark or cursor is signed and bound** to the caller, the site and (watermark) the
  DocType. A forged, edited, malformed or foreign one is `422 bulk.cursor_invalid`, the same
  answer for each. `410 bulk.watermark_expired` means the feed no longer holds the position or it
  predates a restore: run a full export and start again.

## Limits and what happens when one is reached

`[services.export]` in `common.toml` (the table is named after the module). Unknown keys are
refused, and a value is never echoed in an error.

| Key | Default | Meaning |
| --- | --- | --- |
| `enabled` | `true` | Serve bulk export at all. Off: the routes are not mounted. |
| `rows_per_chunk` | `1000` | Most rows in one chunk (1 to 10,000). |
| `bytes_per_chunk` | `4194304` | Most bytes in one chunk (1 KiB to 64 MiB). |
| `max_concurrent_per_site` | `4` | Most exports in flight on the site. |
| `max_concurrent_per_user` | `2` | Most in flight for one caller (at most the site's). |
| `rows_per_second` | `20000` | The site's row budget per second (at least `rows_per_chunk`). |
| `max_rows_cap` | `10000000` | The policy cap on a request's `max_rows`. |

Going over a concurrency bound or the row budget is `429 bulk.limit` with `Retry-After`. A slot is
held for the length of **one request**, not for a whole export: an export holds nothing between
chunks, so a client that walks away leaves nothing to release. The bounds are per process; in a
cluster each node enforces its own.

**Level: optional.** A failed start-up check (nothing served, or the change feed cannot be read)
degrades the site (`/api/health/ready` says `degraded: export`) and the routes answer
`503 server.unavailable`; the rest of the site serves.

## Cursors and keys

Cursors and watermarks are signed with two per-site keys made from the operating system's random
source when the service starts. They are **not stored** (the platform has no site secret store
yet, WP-SEC-10; sessions' salt is made the same way). The consequence is honest: a restart of the
process, or a request that reaches another node, ends every cursor and watermark with
`422 bulk.cursor_invalid`. Start the export again; an extract restarts from the watermark of a new
full export. A cursor carries the request it continues (the normalised columns, filter, order and
format), so a request whose normalised form is longer than about 900 bytes, or whose cursor would
pass the contract's 2,048 characters, is refused up front with `400 api.invalid_request`.

## Audit and what is never logged

Target `spot::audit::bulk_export`: a line for every start (a refused one too, with its reason
code and nothing else), one when an export finishes or fails, and the same for an extract: who,
the DocType, the number of columns, a digest that names the filter without its values, and row
counts. Never a filter value, a cell, a cursor, a watermark, a key or a token. The field guard's
own trace records (`spot::audit::field_access`) name field paths, never values.

The records go to the process log. They are not yet entries of the identity audit's hash chain,
which a hosting module cannot reach.

## Honest limits

- The extract reports what the change feed records. A row that stops being visible to the caller
  through a change that writes no document (a role or user-permission edit) is not reported until
  its next change; run a full export to resynchronise after such an edit.
- Several nodes: a cursor and a watermark belong to the node that made them (see above).
- The export reads pages as the platform with the caller's read filter in the query; a rule that
  needs a document's own values beyond the filter's (a share on one document) is not applied.
