#!/usr/bin/env bash
# Runs the L8 fault scenarios (standard profile), writes a JSON report and the fault timeline per
# scenario, and exits non-zero on any violation.
#
#   SPOT_PG_BIN=/usr/lib/postgresql/18/bin harness/rust/l8-faults/ci/run.sh
#
# Artefacts land in $SPOT_L8_FAULTS_OUT (default: target/l8-faults).
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
crate_dir="$(dirname "$here")"
out="${SPOT_L8_FAULTS_OUT:-target/l8-faults}"

export SPOT_L8_FAULTS_PROFILE="${SPOT_L8_FAULTS_PROFILE:-standard}"
export SPOT_L8_FAULTS_OUT="$out"

echo "l8-faults: profile=$SPOT_L8_FAULTS_PROFILE out=$out"
if [ -z "${SPOT_PG_BIN:-}" ] && [ ! -x /usr/lib/postgresql/18/bin/initdb ]; then
  echo "l8-faults: no PostgreSQL binaries; setting SPOT_REQUIRE_PG=1 would fail, but CI sets SPOT_PG_BIN" >&2
fi

# The scenarios write their own report files when SPOT_L8_FAULTS_OUT is set; the test harness
# prints the timeline to stdout, captured here.
mkdir -p "$out"
cargo test -p spot-l8-faults -- --ignored --nocapture --test-threads=1 2>&1 | tee "$out/run.log"

# Any violation makes cargo test fail; this is belt and braces.
if grep -q "VIOLATIONS" "$out/run.log"; then
  echo "l8-faults: a scenario reported violations" >&2
  exit 1
fi
echo "l8-faults: OK (log: $out/run.log)"
