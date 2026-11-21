Part of [Running Spot](../running-spot.md).

# Edge integrity

The platform keeps a **graph edge** for every link between documents: each Link, Dynamic Link,
Table and Table Multiselect field of a document is one edge, written in the same transaction as the
document by the edge participant. The invariant is: **the set of edges equals the set of link
fields**, always. This page is how an operator checks that and, when it does not hold, repairs it.

## How drift can happen

- a restore from a snapshot taken between a document write and its edge write on another node;
- a manual SQL fix to a document or to `spot_graph_edge`;
- a bug in an app's own writes that bypassed the document lifecycle;
- rows seeded before the edge participant existed (a fresh site's first scan may find them; one
  repair makes them consistent).

A drift is a **data finding, not an outage**: the site stays ready and keeps serving.

## The weekly job

The platform app schedules the job `platform:edge_verify` every Sunday at 03:30 UTC on the `long`
queue with a 4 hour budget (`apps/platform/app.toml`). The host registers its handler on every
site (`[services.edge_integrity] enabled = true`, the default). Each run:

1. takes the coordinator lease `edge_verify:<site>`; a second run while one is going skips with a
   debug line, so two scans never overlap;
2. builds the scan from the metadata the site serves **now**, so a reload or a custom field changes
   what is checked;
3. writes a drift report under the key `scheduled-<UTC date>-<job id>` (or the job's `report_key`
   argument) and publishes the gauges below; a scan that cannot run is retried with its stable
   code (`edge_pg.unavailable`, `edge_pg.write_failed`).

`[services.edge_integrity]` also has `verify_page_size` (documents per page, default 500, 1 to
10 000). Unknown keys are refused.

### Metrics

Every series carries the `site` label.

| Metric | Meaning | Alert |
| --- | --- | --- |
| `spot_edge_drift_entries{kind}` | entries of the last report by kind: `missing`, `extra`, `changed` | any kind above 0 for a day |
| `spot_edge_verify_last_run_seconds` | when the last scan finished (Unix seconds) | older than 8 days |
| `spot_edge_verify_documents_checked` | documents the last scan read | a sudden fall |
| `spot_edge_verify_runs_total{result}` | `clean`, `drift` or `error` | `error` rising |

A drift also logs one `warn` with the counts and the report key, never a document id or a value.
The readiness line of the service reads `edges: clean at <time>` or
`edges: <n> drift entries (report <key>)`.

## The commands

```
spot edges verify --site S [--page-size N]
spot edges repair --site S --report KEY [--apply] [--batch N]
```

`verify` scans now (not through the job queue), writes a report and prints
`checked <n> documents, <m> drift entries, report <key>`, then at most the first 50 entries, one per
line: `<kind> <doctype> <record id> <edge type>`. Identifiers only; no field value is printed.
`missing` means a link has no edge, `extra` an edge has no link, `changed` the edge's properties
differ.

`repair` without `--apply` prints what the named report would change (counts and the same first 50
lines) and **writes nothing**. With `--apply` it applies the report in audited batches (default 200
changes per batch), each with one row in `spot_edge_repair_audit`, and prints one line per batch with
its audit key. It never touches an edge the report does not name.

| Exit | Meaning |
| --- | --- |
| 0 | done; for `verify`, the site is clean |
| 1 | usage error (`--site` missing, a number that is not a whole number of 1 or more) |
| 2 | `verify` found drift; or `repair` refused (unknown report, report older than 7 days) |
| 3 | could not run (database unreachable, tables missing: run `spot migrate`, or a repair stopped part-way) |

### The repair workflow

1. `spot edges verify --site S`: read the counts and the kinds.
2. Look at the lines. A burst of `missing` on one DocType after a restore is the usual shape.
3. `spot edges repair --site S --report KEY`: the dry run.
4. `spot edges repair --site S --report KEY --apply`.
5. `spot edges verify --site S` again: it must say 0 drift entries and exit 0.

A report older than 7 days is refused: the data has moved on, run `verify` again.

## Troubleshooting

| Symptom | Cause | What to do |
| --- | --- | --- |
| a clean site reports drift right after a restore | the snapshot is between a document write and its edge write, or the edge tables came from another point in time | repair from a fresh report; the restore runbook should end with `verify` |
| `refused: ... more than 7 days ago` | the report is old | run `verify` again and repair from the new key |
| `refused: there is no report` | a mistyped key, or the report was written on another site | copy the key from the `verify` output |
| the repair stopped part-way (exit 3) | the database failed between batches | batches that committed stay, each with its audit row; run `verify` and repair from the new report |
| drift returns after a repair | something still writes documents without the edge participant (a script, SQL) | find the writer; the report's DocType and edge type name where it happens |
| a custom DocType appears in the report | its Link fields are checked like any other | repair as usual; a custom field added at run time is seen by the next scan without a restart |
| exit 3 with `edge_pg.write_failed` on a new site | the edge tables are not there | run `spot migrate --site S --apply` |
