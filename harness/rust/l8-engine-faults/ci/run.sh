#!/usr/bin/env bash
# L8 engine-level fault injection (WP-TST-08.T6). Needs the PostgreSQL 18 binaries.
set -euo pipefail
export SPOT_REQUIRE_PG=1
export SPOT_L8_FAULTS_PROFILE="${SPOT_L8_FAULTS_PROFILE:-smoke}"
export SPOT_PG_BIN="${SPOT_PG_BIN:-/usr/lib/postgresql/18/bin}"
cargo test -p spot-l8-engine-faults -- --nocapture
