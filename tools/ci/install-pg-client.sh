#!/usr/bin/env bash
# Puts the PostgreSQL 18 client tools (pg_dump, pg_restore, psql) on a GitHub-hosted Linux runner.
#
# Why: `start-postgres.sh` runs the server in a Docker container, so the host has only the clients
# the runner image ships. On `ubuntu-latest` those are an older major version behind the
# `postgresql-common` wrapper (`/usr/bin/pg_dump` is a symlink to `pg_wrapper`), and an older
# `pg_dump` refuses to dump a newer server. The backup and restore tests run the real client
# tools, so they need the same major version as the server. The tools are installed under
# `/usr/lib/postgresql/<major>/bin`, a root-owned directory the backup code accepts, and the job's
# environment points the tests at it (SPOT_TEST_PG_BIN) and puts it first on PATH.
#
# Nothing to do when the tools are already there (a self-hosted runner, a re-run). Local override:
# SPOT_PG_MAJOR.
set -euo pipefail

major="${SPOT_PG_MAJOR:-18}"
root="/usr/lib/postgresql/$major"
bin="$root/bin"

die() {
  echo "$*" >&2
  exit 1
}

if [ ! -x "$bin/pg_dump" ]; then
  command -v apt-get >/dev/null 2>&1 || die "$bin/pg_dump is missing and apt-get is not available: install postgresql-client-$major"
  sudo=""
  if [ "$(id -u)" -ne 0 ]; then
    command -v sudo >/dev/null 2>&1 || die "$bin/pg_dump is missing and sudo is not available: install postgresql-client-$major"
    sudo="sudo"
  fi
  # The PostgreSQL project's own repository, through the script its `postgresql-common` package
  # ships (the runner image has that package).
  setup=/usr/share/postgresql-common/pgdg/apt.postgresql.org.sh
  [ -x "$setup" ] || die "$setup is missing: cannot add the PostgreSQL apt repository; install postgresql-client-$major by hand"
  $sudo "$setup" -y >/dev/null
  $sudo apt-get install -y -qq --no-install-recommends "postgresql-client-$major" ||
    die "cannot install postgresql-client-$major"
fi
[ -x "$bin/pg_dump" ] || die "postgresql-client-$major is installed but $bin/pg_dump is not executable"
"$bin/pg_dump" --version

if [ -n "${GITHUB_ENV:-}" ]; then
  echo "SPOT_TEST_PG_BIN=$root" >>"$GITHUB_ENV"
  echo "$bin" >>"${GITHUB_PATH:?GITHUB_PATH is not set}"
else
  echo "export SPOT_TEST_PG_BIN=$root"
  echo "export PATH=$bin:\$PATH"
fi
