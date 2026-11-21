#!/usr/bin/env bash
# Nightly L7/L8 profiles (WP-TST-08.T3): starts PostgreSQL with the existing script when
# SPOT_TEST_PG_URL is not already set, then runs nightly_profiles.py. Reports and the seed land in
# target/nightly-profiles/ (override with --out). Exits non-zero on any violation.
#
#   bash tools/ci/nightly-profiles/run.sh [--run-id N] [--seeds 5] [--out DIR] [--dry-run]
set -euo pipefail
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
cd "$repo_root"

export SPOT_REQUIRE_PG=1
if [[ " $* " != *" --dry-run "* && -z "${SPOT_TEST_PG_URL:-}" ]]; then
  # Outside CI the script prints `export ...` lines; on CI it wrote them to GITHUB_ENV for later
  # steps, so the job should start PostgreSQL in its own step (see the nightly.yml block).
  eval "$(GITHUB_ENV= bash tools/ci/start-postgres.sh)"
fi
exec python3 tools/ci/nightly-profiles/nightly_profiles.py "$@"
