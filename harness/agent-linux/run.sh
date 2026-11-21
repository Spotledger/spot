#!/usr/bin/env bash
# The Linux module conformance harness entry point (WP-ADM-04.T3). See README.md.
#
# Runs the real spot-agent-linux / spot-agent-linux-sys suite inside Fedora, a
# RHEL-compatible distribution and Ubuntu LTS containers, and prints the matrix. It
# refuses to run for real unless SPOT_HARNESS_DISPOSABLE=1 and the host root is a
# container or a VM; that check lives in agent_linux_harness/orchestrator.py, so it is
# unit-tested rather than duplicated here. `--list` and `--dry-run` need no guard.
#
# Usage:
#   SPOT_HARNESS_DISPOSABLE=1 ./run.sh                 # the whole container matrix
#   ./run.sh --list                                    # show the matrix, no guard needed
#   ./run.sh --dry-run                                 # show what would run, no guard needed
#   SPOT_HARNESS_DISPOSABLE=1 ./run.sh --target fedora-44 --target ubuntu-2404
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

PYTHON="${PYTHON:-python3}"
if ! command -v "$PYTHON" >/dev/null 2>&1; then
  echo "error: $PYTHON not found; set PYTHON to a Python 3.11+ interpreter" >&2
  exit 127
fi

export PYTHONPATH="$HERE${PYTHONPATH:+:$PYTHONPATH}"
exec "$PYTHON" -m agent_linux_harness.cli "$@"
