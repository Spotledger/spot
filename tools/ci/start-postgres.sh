#!/usr/bin/env bash
# Starts a throwaway PostgreSQL 18 for backend tests (WP-STO-01 onwards), creates the ADR-087
# extensions in its test database and exports its admin URL. SPOT_REQUIRE_PG=1 makes
# PostgreSQL-backed tests fail instead of skipping when the URL is missing, so CI can never pass
# by skipping them. Linux runners only (Docker service images).
#
# ADR-087 section 4.2: PostGIS is required on every real tier, and the parity suite runs its geo,
# range, exclusion and search cases on PostgreSQL only (sections 4.5, 5.5, 7.9), so the server
# this script starts must offer `postgis`, `btree_gist`, `pg_trgm` and `unaccent`, and the test
# database must have them created (WP-STO-10.T4). It must also offer `vector` (pgvector), which
# the spot-agent-memory-pg tests require (SPOT_REQUIRE_PG=1 fails without it). A server that does
# not offer an extension fails here, naming the package to install: nothing is skipped silently.
#
# Local overrides: SPOT_PG_CONTAINER, SPOT_PG_PORT, SPOT_PG_IMAGE, SPOT_PG_MAJOR.
set -euo pipefail

pg_major="${SPOT_PG_MAJOR:-18}"
pgbin="${SPOT_PG_BIN:-/usr/lib/postgresql/$pg_major/bin}"
# `postgis` is the one the ADR makes mandatory; the other three are trusted (F.8) and are what
# `btree_gist` exclusion scopes, trigram similarity and accent folding need.
# `vector` (pgvector) is not a contrib extension: it has its own package.
extensions=(postgis btree_gist pg_trgm unaccent vector)
# Distribution packages, named so a failure says exactly what to install.
postgis_package="postgresql-${pg_major}-postgis-3"
pgvector_package="postgresql-${pg_major}-pgvector"
contrib_package="postgresql-contrib-${pg_major}"
# Test credentials only: a throwaway cluster on a loopback port, deleted with the job.
user="spot"
password="spot"

die() {
  echo "$*" >&2
  exit 1
}

# The package that carries `ext`.
package_of() {
  case "$1" in
    postgis) echo "$postgis_package" ;;
    vector) echo "$pgvector_package" ;;
    *) echo "$contrib_package" ;;
  esac
}

# Writes the environment for the tests: into GITHUB_ENV on CI, to stdout for a local run.
emit_env() {
  local url="$1" dir="${2:-}"
  if [ -n "${GITHUB_ENV:-}" ]; then
    {
      echo "SPOT_TEST_PG_URL=$url"
      echo "SPOT_REQUIRE_PG=1"
      if [ -n "$dir" ]; then echo "SPOT_PG_DIR=$dir"; fi
    } >>"$GITHUB_ENV"
  else
    echo "export SPOT_TEST_PG_URL=$url"
    echo "export SPOT_REQUIRE_PG=1"
    if [ -n "$dir" ]; then echo "export SPOT_PG_DIR=$dir"; fi
  fi
}

# Creates every extension this suite needs in the test database, and fails naming the package
# when the server does not offer one of them. Runs `psql_run` for the path that started the
# server.
ensure_extensions() {
  local offered missing=() ext
  offered="$(psql_run -Atc "select name from pg_available_extensions")" ||
    die "could not read pg_available_extensions from the test server"
  for ext in "${extensions[@]}"; do
    case $'\n'"$offered"$'\n' in
      *$'\n'"$ext"$'\n'*) ;;
      *) missing+=("$ext") ;;
    esac
  done
  if [ ${#missing[@]} -gt 0 ]; then
    for ext in "${missing[@]}"; do
      echo "the PostgreSQL server does not offer the extension \`$ext\`: install the package $(package_of "$ext") (required on every tier, so this is a failure, not a skip; ADR-087 for PostGIS)" >&2
    done
    exit 1
  fi
  for ext in "${extensions[@]}"; do
    psql_run -c "CREATE EXTENSION IF NOT EXISTS \"$ext\"" >/dev/null ||
      die "could not create the extension \`$ext\` in the test database: check the package $(package_of "$ext")"
  done
  psql_run -Atc "select string_agg(extname, ', ' order by extname) from pg_extension where extname = any (string_to_array('$(IFS=,; echo "${extensions[*]}")', ','))" ||
    die "the extensions were created but cannot be read back"
}

if ! command -v docker >/dev/null 2>&1 && [ -x "$pgbin/initdb" ]; then
  # Self-hosted runner without Docker (the CI runner notes): a throwaway cluster owned by
  # the runner user, on a free port, under the job's temp dir. Parallel runners cannot collide,
  # and the cluster is deleted with RUNNER_TEMP. fsync off: it is disposable.
  dir="$(mktemp -d "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/spot-pg.${GITHUB_RUN_ID:-local}.XXXXXX")"
  echo "$password" >"$dir/pw"
  "$pgbin/initdb" -D "$dir/data" -U "$user" --pwfile="$dir/pw" --auth=scram-sha-256 >/dev/null
  # A runner without root cannot install an extension package. PostgreSQL 18 can load extensions
  # from a directory the runner user owns: unpack the package there with
  # `apt-get download <package> && dpkg-deb -x <deb> "$SPOT_PG_EXT_ROOT"` (default
  # $HOME/pgext/root) and the throwaway cluster searches it after the system directories. The
  # `$system` and `$libdir` macros are PostgreSQL's own, so they are written to the config file,
  # where no shell expands them.
  ext_root="${SPOT_PG_EXT_ROOT:-$HOME/pgext/root}"
  if [ -d "$ext_root/usr/share/postgresql/$pg_major" ]; then
    {
      echo "extension_control_path = '\$system:$ext_root/usr/share/postgresql/$pg_major'"
      echo "dynamic_library_path = '\$libdir:$ext_root/usr/lib/postgresql/$pg_major/lib'"
    } >>"$dir/data/postgresql.conf"
    # Shared libraries the unpacked packages need (PostGIS: GEOS, PROJ, TIFF...) live in the same
    # tree; the server is a child of this script, so it inherits the search path.
    if [ -d "$ext_root/usr/lib/x86_64-linux-gnu" ]; then
      export LD_LIBRARY_PATH="$ext_root/usr/lib/x86_64-linux-gnu${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
    fi
    if [ -d "$ext_root/usr/share/proj" ]; then
      export PROJ_DATA="$ext_root/usr/share/proj"
    fi
    # The PostGIS package names its control file postgis-3.control and adds the postgis.control
    # link in its post-install step, which a plain unpack skips.
    ext_dir="$ext_root/usr/share/postgresql/$pg_major/extension"
    if [ -f "$ext_dir/postgis-3.control" ] && [ ! -e "$ext_dir/postgis.control" ]; then
      ln -s postgis-3.control "$ext_dir/postgis.control" 2>/dev/null || true
    fi
    echo "extensions also searched in $ext_root"
  fi
  # The port is chosen by binding port 0 and releasing it, so another process (a parallel runner
  # on this host) can take it before the server binds. `pg_ctl -w` then fails; try again with a
  # new port instead of failing the job.
  port=""
  for attempt in 1 2 3 4 5; do
    port="$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1])')"
    if "$pgbin/pg_ctl" -D "$dir/data" -l "$dir/log" -w -t 60 \
      -o "-p $port -k $dir -c listen_addresses=127.0.0.1 -c fsync=off -c log_line_prefix=%m[%p]" start >/dev/null; then
      break
    fi
    echo "PostgreSQL did not start on port $port (attempt $attempt); log tail:" >&2
    tail -n 20 "$dir/log" >&2 || true
    port=""
  done
  [ -n "$port" ] || die "PostgreSQL did not start after 5 attempts (log: $dir/log)"
  psql_run() {
    PGPASSWORD="$password" "$pgbin/psql" -h 127.0.0.1 -p "$port" -U "$user" -d postgres \
      -v ON_ERROR_STOP=1 "$@"
  }
  # `pg_ctl -w` returns once the postmaster reports ready; poll a real query anyway so a server
  # still replaying WAL ("the database system is starting up") never reaches the tests.
  ready=""
  for _ in $(seq 1 60); do
    if psql_run -Atc "select 1" >/dev/null 2>&1; then
      ready=yes
      break
    fi
    sleep 0.5
  done
  if [ -z "$ready" ]; then
    echo "PostgreSQL does not answer queries on port $port; log tail:" >&2
    tail -n 40 "$dir/log" >&2 || true
    exit 1
  fi
  psql_run -Atc "select version()"
  # The extensions come from the host's distribution packages, so a missing PostGIS is the
  # host's package and `ensure_extensions` names it.
  ensure_extensions
  emit_env "postgres://$user:$password@127.0.0.1:$port/postgres" "$dir"
  exit 0
fi

command -v docker >/dev/null 2>&1 ||
  die "neither docker nor a PostgreSQL $pg_major install ($pgbin/initdb) is available: install docker or the packages $postgis_package and $pgvector_package"

container="${SPOT_PG_CONTAINER:-spot-pg}"
port="${SPOT_PG_PORT:-5432}"
image="${SPOT_PG_IMAGE:-docker.io/library/postgres:$pg_major}"
# This script owns the container it names: a leftover one from an interrupted job would make
# `docker run` fail on the name, so it is removed first. Nothing else is ever removed.
docker rm -f "$container" >/dev/null 2>&1 || true
docker run -d --name "$container" \
  -e POSTGRES_USER="$user" -e POSTGRES_PASSWORD="$password" \
  -p "127.0.0.1:$port:5432" "$image" >/dev/null
ready=""
for _ in $(seq 1 60); do
  if docker exec "$container" pg_isready -U "$user" -d postgres >/dev/null 2>&1; then
    ready=yes
    break
  fi
  sleep 1
done
if [ -z "$ready" ]; then
  echo "PostgreSQL did not become ready in 60 s" >&2
  docker logs "$container" >&2 || true
  exit 1
fi
psql_run() {
  docker exec -e PGPASSWORD="$password" "$container" \
    psql -U "$user" -d postgres -v ON_ERROR_STOP=1 "$@"
}
# The official `postgres` image ships the contrib extensions but not PostGIS or pgvector (their
# documented install path is each its own package). Install the ones the server does not offer;
# a runner without the package fails below, naming it.
to_install=()
for ext in postgis vector; do
  if ! psql_run -Atc "select 1 from pg_available_extensions where name = '$ext'" | grep -q 1; then
    to_install+=("$(package_of "$ext")")
  fi
done
if [ ${#to_install[@]} -gt 0 ]; then
  echo "the $image server does not offer every extension; installing ${to_install[*]} inside $container"
  docker exec --user root "$container" apt-get update -qq ||
    die "cannot reach the distribution package index inside $container; the runner must provide ${to_install[*]}"
  docker exec --user root "$container" apt-get install -y -qq --no-install-recommends "${to_install[@]}" ||
    die "cannot install the packages ${to_install[*]} inside $container"
fi
psql_run -Atc "select version()"
ensure_extensions
emit_env "postgres://$user:$password@127.0.0.1:$port/postgres"
