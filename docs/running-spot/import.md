# Data import

`spot serve` imports a CSV or XLSX file into a DocType, in two phases: a **dry run** that reads,
maps, converts and checks every row and writes nothing, then a **run** that writes the rows. The
routes are `/api/v1/import*` (contract 0.21.0); the engine is `spot-import`; the host half is
`crates/framework/spot-cli/src/services/import.rs`.

## Level and failure

The service is **optional**. If its start-up check fails (no metadata served, no files service,
a spool directory that cannot be created) the site keeps serving documents, readiness says the
import service is degraded, and the import routes answer `503`. Nothing else is affected.

## Configuration

`[services.import]` in `common.toml`. Unknown keys are refused.

| Key | Default | Meaning |
| --- | --- | --- |
| `enabled` | `true` | Off: the routes are not mounted. |
| `max_file_bytes` | 52,428,800 | Largest uploaded file. Over it: `413 import.limit_exceeded`, before any row is read. |
| `max_rows` | 100,000 | Most rows in a file. |
| `max_columns` | 200 | Most columns (at most 500). |
| `max_concurrent_per_site` | 2 | Dry runs and runs working at once. |
| `batch_rows` | 500 | Rows per batch; progress, cancel and the lease are checked between batches. |
| `max_errors` | 100 | Failed rows after which a run stops. |
| `spool_dir` | system temp dir, `spot-import/` | Where the uploaded file is copied for the job. |

`GET /api/v1/import/limits` shows a client the limits before it uploads.

## The two phases

```bash
# 1. upload the file (the files service scans it; a quarantined file is refused)
FILE=$(curl -s -H "Authorization: Bearer $TOKEN" -H 'Content-Type: text/csv' \
  -H 'Content-Disposition: attachment; filename="tasks.csv"' \
  --data-binary @tasks.csv "$BASE/api/v1/files" | jq -r .data.id)

# 2. dry run: nothing is written; the answer carries a digest
JOB=$(curl -s -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -H 'Idempotency-Key: import-1' \
  -d "{\"file_id\":\"$FILE\",\"doctype\":\"task\",\"mode\":\"insert\"}" "$BASE/api/v1/import")
ID=$(echo "$JOB" | jq -r .data.id); DIGEST=$(echo "$JOB" | jq -r .data.digest)

# 3. read the row errors, fix the file and repeat step 1 and 2 if there are any
curl -s -H "Authorization: Bearer $TOKEN" "$BASE/api/v1/import/$ID/errors?limit=50"

# 4. run it: the digest must be the dry run's
curl -s -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d "{\"digest\":\"$DIGEST\"}" "$BASE/api/v1/import/$ID/run"

# 5. poll
curl -s -H "Authorization: Bearer $TOKEN" "$BASE/api/v1/import/$ID"
```

The digest covers the file's bytes, the mapping, the mode, the submit flag and the metadata
version. If any of them changed since the dry run the run is `409 import.digest_changed`: run the
dry run again. A run without a dry run is refused. `POST /import/{id}/cancel` stops a run after the
current batch; rows already written stay and are counted.

## What it does to rows

- Each row is written through the normal lifecycle operation **as the user who started the
  import**, never as the platform. A row the user may not write fails that row only, and the other
  rows go on. The user's rights are checked when the job is created, at the dry run, and again when
  the run starts; a user who is disabled or loses the import right stops the run with
  `import.right_missing`.
- A cell that starts with `=`, `+`, `-` or `@` is stored as text and never evaluated.
- Memory stays bounded: the file is read one parent (with its child rows) at a time.
- A re-run after a crash skips rows already written (the batch label is the job id), so the final
  counts are exact and no row is written twice.

## Reading the errors page

`GET /import/{id}/errors` lists at most 200 errors per page with `row` (from 1), `column`, `code`
and a plain explanation. `cell_text` is the cell as written, cut to 256 bytes, and is shown only for
a field the caller may write and that is in no protection group; otherwise it is absent. Cell text
is never stored: it is read from the file again when the page is served, and it is gone once the job
has ended.

## Security notes

Only the creator sees, runs or cancels a job; anyone else gets the same `404` as for a missing job,
and a file or job of another tenant is `404` too. The file is read only by id through the files
service, never by path. Every refusal (rights, digest, limits, unreadable file) is audited on the
target `spot::audit::import` with who, job, DocType and the rule, never a cell, a file name or a
token.

## Known limits of this version

- Modes `update` and `upsert` need a key index of the target DocType and answer
  `422 import.mapping_invalid` until it exists.
- Job records are held in memory (a restart ends a running job) until the platform declares the
  DocTypes `import_job` and `import_job_error`; the runner is the service's own loop, not yet the
  platform job worker.
