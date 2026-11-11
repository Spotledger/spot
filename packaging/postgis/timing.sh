#!/usr/bin/env bash
# Measure what WP-PRD-02.T7 promises to measure (ADR-087 section 11.3 spike S2): the time to
# initialise a cluster, the time `CREATE EXTENSION postgis` takes on it, and the time one
# radius query needs — the three numbers `timing.md` records.
#
# Not a CI gate: it needs a real PostgreSQL with the PostGIS package installed, which the
# test runners do not have. Run it on the target machine (or the closest one available) and
# paste the output into `timing.md` with the platform named.
#
# Usage
# -----
#   PG_BIN=/usr/lib/postgresql/18/bin PSQL="psql -U postgres" ./packaging/postgis/timing.sh
#
# Variables (all optional, sensible defaults):
#   PG_BIN      directory with initdb   (default: first `initdb` on PATH, else /usr/lib/postgresql/*/bin)
#   PSQL        the psql command        (default: `psql`)
#   DATABASE    database to measure in  (default: spot_timing; created, dropped and recreated)
#   ROWS        points in the probe table (default: 200000)
#   RADIUS_M    radius of the measured query, metres (default: 5000)
#   WORK_DIR    scratch directory for the initdb probe (default: mktemp -d)
#
# Every number is printed as `name=value` (`*_ms` in whole milliseconds), so the output can be
# pasted into the timing record unchanged.
set -euo pipefail

psql_cmd="${PSQL:-psql}"
database="${DATABASE:-spot_timing}"
rows="${ROWS:-200000}"
radius_m="${RADIUS_M:-5000}"
work_dir="${WORK_DIR:-$(mktemp -d)}"

pg_bin="${PG_BIN:-}"
if [ -z "$pg_bin" ]; then
  if command -v initdb >/dev/null 2>&1; then
    pg_bin="$(dirname "$(command -v initdb)")"
  else
    pg_bin="$(ls -d /usr/lib/postgresql/*/bin 2>/dev/null | sort -V | tail -1 || true)"
  fi
fi
[ -n "$pg_bin" ] || { echo "no initdb found: set PG_BIN" >&2; exit 1; }

now_ms() { echo $(( $(date +%s%N) / 1000000 )); }
say() { echo "$1=$2"; }

echo "platform=$(uname -srm)"
say "pg_bin" "$pg_bin"
say "psql" "$psql_cmd"
say "rows" "$rows"
say "radius_m" "$radius_m"

# 1. initdb on a scratch directory (the installer does this once per machine). `initdb`
#    refuses to run as root, so under a root shell (a container) it runs as the `postgres`
#    user when that user exists.
rm -rf "$work_dir/initdb-probe"
mkdir -p "$work_dir/initdb-probe"
start=$(now_ms)
if [ "$(id -u)" = "0" ] && id postgres >/dev/null 2>&1; then
  chown postgres "$work_dir/initdb-probe"
  su postgres -c "'$pg_bin/initdb' -D '$work_dir/initdb-probe/data' --no-instructions -E UTF8" \
    >"$work_dir/initdb.log" 2>&1 || { echo "initdb failed:" >&2; cat "$work_dir/initdb.log" >&2; exit 1; }
else
  "$pg_bin/initdb" -D "$work_dir/initdb-probe/data" --no-instructions -E UTF8 \
    >"$work_dir/initdb.log" 2>&1 || { echo "initdb failed:" >&2; cat "$work_dir/initdb.log" >&2; exit 1; }
fi
say "initdb_ms" "$(( $(now_ms) - start ))"

# 2. CREATE EXTENSION postgis, three times on a fresh database (the supervisor does it once,
#    idempotently, on every later start).
$psql_cmd -q -c "DROP DATABASE IF EXISTS \"$database\"" -c "CREATE DATABASE \"$database\""
for run in 1 2 3; do
  start=$(now_ms)
  $psql_cmd -d "$database" -q -c "CREATE EXTENSION postgis"
  say "create_extension_ms_run$run" "$(( $(now_ms) - start ))"
  $psql_cmd -d "$database" -q -c "DROP EXTENSION postgis"
done
$psql_cmd -d "$database" -q -c "CREATE EXTENSION postgis"
say "postgis_version" "$($psql_cmd -d "$database" -Atc "select extversion from pg_extension where extname = 'postgis'")"
say "database_size_after_extension" "$($psql_cmd -d "$database" -Atc "select pg_size_pretty(pg_database_size(current_database()))")"

# 3. One radius query on a geometry(Point,4326) column with a GiST index (ADR-087 section
#    4.4's construct). Two index shapes are measured, because only one of them can be used:
#    a plain GiST index on the geometry supports bounding-box operators, while the
#    metre-radius `ST_DWithin` on `geography` needs a geography index.
$psql_cmd -d "$database" -q <<SQL
DROP TABLE IF EXISTS spot_geo_probe;
CREATE TABLE spot_geo_probe (id integer PRIMARY KEY, geom public.geometry(Point,4326));
INSERT INTO spot_geo_probe (id, geom)
SELECT g, ST_SetSRID(ST_MakePoint(67.0 + random() * 0.5, 24.8 + random() * 0.5), 4326)
FROM generate_series(1, $rows) AS g;
SQL
start=$(now_ms)
$psql_cmd -d "$database" -q -c "CREATE INDEX spot_geo_probe_geom_gix ON spot_geo_probe USING gist (geom)"
say "gist_geometry_build_ms" "$(( $(now_ms) - start ))"
say "gist_geometry_size" "$($psql_cmd -d "$database" -Atc "select pg_size_pretty(pg_relation_size('spot_geo_probe_geom_gix'))")"
$psql_cmd -d "$database" -q -c "ANALYZE spot_geo_probe"

for run in 1 2 3 4 5; do
  lon=$(awk -v seed="$run" 'BEGIN { srand(seed); printf "%.6f", 67.0 + rand() * 0.5 }')
  lat=$(awk -v seed="$run" 'BEGIN { srand(seed + 100); printf "%.6f", 24.8 + rand() * 0.5 }')
  start=$(now_ms)
  found=$($psql_cmd -d "$database" -Atc "select count(*) from spot_geo_probe where ST_DWithin(geom::geography, ST_SetSRID(ST_MakePoint($lon, $lat), 4326)::geography, $radius_m)")
  say "radius_ms_geography_cast_run$run" "$(( $(now_ms) - start ))"
  say "radius_rows_geography_cast_run$run" "$found"
done

start=$(now_ms)
$psql_cmd -d "$database" -q -c "CREATE INDEX spot_geo_probe_geog_gix ON spot_geo_probe USING gist ((geom::geography))"
say "gist_geography_build_ms" "$(( $(now_ms) - start ))"
say "gist_geography_size" "$($psql_cmd -d "$database" -Atc "select pg_size_pretty(pg_relation_size('spot_geo_probe_geog_gix'))")"
$psql_cmd -d "$database" -q -c "ANALYZE spot_geo_probe"

for run in 1 2 3 4 5; do
  lon=$(awk -v seed="$run" 'BEGIN { srand(seed); printf "%.6f", 67.0 + rand() * 0.5 }')
  lat=$(awk -v seed="$run" 'BEGIN { srand(seed + 100); printf "%.6f", 24.8 + rand() * 0.5 }')
  start=$(now_ms)
  found=$($psql_cmd -d "$database" -Atc "select count(*) from spot_geo_probe where ST_DWithin(geom::geography, ST_SetSRID(ST_MakePoint($lon, $lat), 4326)::geography, $radius_m)")
  say "radius_ms_geography_index_run$run" "$(( $(now_ms) - start ))"
  say "radius_rows_geography_index_run$run" "$found"
done

echo "plan_geography_cast:"
$psql_cmd -d "$database" -c "EXPLAIN (ANALYZE, BUFFERS, TIMING OFF) SELECT count(*) FROM spot_geo_probe WHERE ST_DWithin(geom::geography, ST_SetSRID(ST_MakePoint(67.2, 25.0), 4326)::geography, $radius_m)" | sed 's/^/  /'

rm -rf "$work_dir/initdb-probe"
echo "done"
