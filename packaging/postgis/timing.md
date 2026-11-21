# Timing record: PostGIS on the desktop install

The numbers ADR-087 section 11.3 (spike S2) asks for and the WP-PRD-03 acceptance needs ("a
clean-machine install under 5 minutes"): how long `initdb` takes, how long
`CREATE EXTENSION postgis` takes on the result, what the extension and its indexes cost in
space, and that one radius query is answered afterwards.

This is a record, not a CI gate: it needs a real PostgreSQL with the PostGIS package, which
the test runners do not have.

## Procedure

[`timing.sh`](timing.sh) performs the whole measurement and prints one `name=value` line per
number, so its output can be pasted below unchanged.

```bash
# On the target machine (or the closest available), with psql and initdb reachable:
PG_BIN=/usr/lib/postgresql/18/bin PSQL="psql -U postgres" ./packaging/postgis/timing.sh
```

What it measures, in order:

1. `initdb` on a scratch data directory — the installer's one-off cluster creation, and the
   step `spot-pg-supervisor::ensure_initialised` performs at first start.
2. `CREATE EXTENSION postgis` three times on a fresh database (`DROP EXTENSION` in between) —
   the step the supervisor's first-start `extension.install` performs once.
3. The size the extension adds to an empty database.
4. A `geometry(Point,4326)` table of `ROWS` points in a 0.5° x 0.5° box around Karachi
   (67.0–67.5 E, 24.8–25.3 N), a GiST index on the geometry, then the ADR-087 section 4.4
   radius query (`ST_DWithin(… ::geography, … , 5000)` metres) at five random centres.
5. The same with a GiST index on `(geom::geography)`, which is the index shape the metre
   query can actually use, plus `EXPLAIN (ANALYZE, BUFFERS)` of the query.

## Measured 29 Oct 2026 — Linux container, not the Windows bundle

Environment: Fedora 44 host, `postgres:18` container (`PostgreSQL 18.6 (Debian
18.6-1.pgdg13+2)`), PostGIS **3.6.4** from `postgresql-18-postgis-3` — note the version
difference from the pinned Windows bundle (3.6.2). `ROWS=200000`, `RADIUS_M=5000`.

```
platform=Linux 7.2.11-200.fc44.x86_64 x86_64
pg_bin=/usr/lib/postgresql/18/bin
rows=200000
radius_m=5000
initdb_ms=3964
create_extension_ms_run1=502
create_extension_ms_run2=590
create_extension_ms_run3=576
postgis_version=3.6.4
database_size_after_extension=17 MB
gist_geometry_build_ms=465
gist_geometry_size=8048 kB
radius_ms_geography_cast_run1=468
radius_rows_geography_cast_run1=5697
radius_ms_geography_cast_run2=406
radius_rows_geography_cast_run2=4286
radius_ms_geography_cast_run3=414
radius_rows_geography_cast_run3=5542
radius_ms_geography_cast_run4=399
radius_rows_geography_cast_run4=5423
radius_ms_geography_cast_run5=385
radius_rows_geography_cast_run5=5593
gist_geography_build_ms=1622
gist_geography_size=14 MB
radius_ms_geography_index_run1=99
radius_rows_geography_index_run1=5697
radius_ms_geography_index_run2=104
radius_rows_geography_index_run2=4286
radius_ms_geography_index_run3=97
radius_rows_geography_index_run3=5542
radius_ms_geography_index_run4=94
radius_rows_geography_index_run4=5423
radius_ms_geography_index_run5=127
radius_rows_geography_index_run5=5593
```

The plan for the cast form, from the same run:

```
Finalize Aggregate  (cost=65159.21..65159.22 rows=1 width=8) (actual rows=1.00 loops=1)
  Buffers: shared hit=2826
  ->  Gather  (cost=65158.99..65159.20 rows=2 width=8) (actual rows=2.00 loops=1)
        Workers Planned: 1
        ->  Parallel Seq Scan on spot_geo_probe
              Filter: st_dwithin((geom)::geography, '…'::geography, '5000'::double precision, true)
              Rows Removed by Filter: 97240
```

with the `(geom::geography)` index in place it becomes a `Parallel Bitmap Heap Scan` over that
GiST index, `Rows Removed by Filter: 1510`.

### What these numbers say

- `initdb` plus `CREATE EXTENSION postgis` is about **4.6 s** on this machine (3.96 s + 0.5–0.6 s),
  so the extension's own cost is far inside the WP-PRD-03 five-minute budget; the install time
  is dominated by unpacking the bundle, not by PostGIS.
- The extension adds **17 MB** to an empty database (its catalogue objects and default
  `spatial_ref_sys` rows), and a GiST index on 200,000 points costs **8 MB** and 0.47 s to
  build.
- **The index is only used when the indexed expression matches the query's expression.** With
  a plain GiST index on `geometry`, `ST_DWithin(geom::geography, …)` cannot use it and scans
  every row: 385–468 ms for 5,400 matching rows out of 200,000. With a GiST index on
  `(geom::geography)` the same query takes 94–127 ms. This is a finding for the query
  compiler (WP-STO-02.T9, "compile the four expression families") and the measurement task
  (ADR-087 section 13 task 14); this record only states it, it does not choose the index shape.
- Numbers on Windows, on the EDB build, will differ. They are not extrapolated here.

## Not measured yet — Windows, the real bundle, the installer

Nothing below was measured on this task, and no number is guessed. Each names the command
that fills it in.

| Number | Why it is missing | How to get it |
| --- | --- | --- |
| Clean-machine install wall time (WP-PRD-03 acceptance: under 5 minutes) | no Windows runner in this task's environment, no MSI yet (WP-PRD-03) | time `start /wait msiexec /i spot.msi /qn` on a clean VM, from process start to the service answering; record with `Get-Date` deltas around the MSI and around the service start |
| `initdb` + first-start `extension.install` on Windows | same | `timing.sh` with `PG_BIN=C:\Program Files\Spot\pgsql\bin` and `PSQL="psql -U postgres -p 5433"` against the installed bundle; or read the `ExtensionProvisioned` event timestamp against the service start |
| Installed size delta of the bundle | the payload is measured (63.5 MB, see `docs/packaging/desktop-postgis.md`), the MSI's compressed contribution is not | compare `msiexec` install of the product without and with the `packaging/postgis` file set; record both `Get-ChildItem -Recurse` totals |
| Radius query on the Windows bundle (PostGIS 3.6.2) | no Windows PostgreSQL here | `timing.sh` against the installed bundle; the differences from 3.6.4 are expected to be nil for these code paths, but that is an expectation, not a measurement |
| Bundle extraction time (two zips, 467,845,337 bytes) | only the archives' sizes were verified, not extracted on Windows | `Measure-Command { Expand-Archive … }` per archive on the target machine |

## Known gaps a later task must close

- The supervisor's provisioning call is measured by the tests, not by the clock: the
  integration tests use the fake `extension.install` adapter, so a real `CREATE EXTENSION`
  over `spot-pg-migrate` should be timed once WP-PRD-03 wires the adapter.
- `PROJ_LIB` must be set for the child (see `MANIFEST.toml`); a wrong value does not change
  these numbers, it makes coordinate transforms fail, which is a separate test.
