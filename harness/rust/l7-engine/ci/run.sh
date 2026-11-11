#!/usr/bin/env bash
# L7 engine suite: the CI entry (WP-TST-07.T2).
#
# It runs the per-commit smoke suite on the in-memory fake and, when SPOT_TEST_PG_URL is set,
# on PostgreSQL, then the `standard` profile on both with PostgreSQL required
# (SPOT_REQUIRE_PG=1). It writes the JSON reports under SPOT_L7_ARTIFACTS (default
# `artifacts/l7-engine`) and exits non-zero on any violation.
set -euo pipefail

cd "$(dirname "$0")/.."

export SPOT_L7_REPORT_DIR="${SPOT_L7_ARTIFACTS:-artifacts/l7-engine}"
mkdir -p "$SPOT_L7_REPORT_DIR"

echo "== L7 engine: smoke (fake, and PostgreSQL when configured) =="
cargo test -p spot-l7-engine

echo "== L7 engine: ${SPOT_L7_PROFILE:-standard} profile on the fake and PostgreSQL =="
# A release run must not pass by skipping PostgreSQL.
export SPOT_REQUIRE_PG=1
export SPOT_L7_PROFILE="${SPOT_L7_PROFILE:-standard}"
cargo test -p spot-l7-engine --test profiles -- --ignored --nocapture

echo "L7 engine suite: OK (reports in $SPOT_L7_REPORT_DIR)"
