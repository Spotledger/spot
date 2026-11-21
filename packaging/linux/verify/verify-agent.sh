#!/usr/bin/env bash
# Static and (where the tools exist) live checks of the spot-agent packaging.
#
#   bash packaging/linux/verify/verify-agent.sh [PATH-TO-spot-agent-priv]
#
# Always runs: the unit lint and the package-file lint (python). When the helper
# binary is given (argument or $SPOT_AGENT_PRIV) it also generates the sudoers
# drop-in with `--print-sudoers`, validates its shape and runs `visudo -cf`.
# When `systemd-analyze` exists it runs `verify` and `security`; the exposure
# score must not exceed the ceiling. Missing tools print SKIP and do not fail.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
agent="$(cd "$here/../agent" && pwd)"
repo="$(cd "$here/../../.." && pwd)"
# Measured 2.4 on systemd 259 (2026-11-12); the host unit's ceiling is 3.0.
CEILING="${SPOT_AGENT_UNIT_CEILING:-3.0}"
helper="${1:-${SPOT_AGENT_PRIV:-}}"
fail=0
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

echo "== lint: files, modes, scriptlets, unit directives"
python3 -m unittest discover -s "$repo/tools/packaging/tests" -p 'test_agent_packaging.py' || fail=1

echo "== sudoers drop-in"
if [ -n "$helper" ] && [ -x "$helper" ]; then
    "$helper" --print-sudoers --user spot-agent >"$tmp/spot-agent.sudoers"
    python3 - "$agent" "$tmp/spot-agent.sudoers" <<'PY' || fail=1
import sys
from pathlib import Path
sys.path.insert(0, sys.argv[1])
import agent_sudoers
names = agent_sudoers.check(Path(sys.argv[2]).read_text())
print("OK sudoers shape:", len(names), "templates, no wildcard")
PY
    if command -v visudo >/dev/null 2>&1; then
        chmod 0440 "$tmp/spot-agent.sudoers"
        visudo -cf "$tmp/spot-agent.sudoers" || fail=1
    else
        echo "SKIP visudo not found"
    fi
else
    echo "SKIP no helper binary given (pass spot-agent-priv or set SPOT_AGENT_PRIV)"
fi

echo "== systemd-analyze"
if command -v systemd-analyze >/dev/null 2>&1; then
    # The build host has no /usr/bin/spot-agent: point ExecStart at an existing binary.
    unit="$tmp/spot-agent.service"
    sed -e 's#^ExecStart=.*#ExecStart=/usr/bin/true#' "$agent/systemd/spot-agent.service" >"$unit"
    systemd-analyze verify "$unit" 2>&1 | grep -v "^$" || true
    out="$(systemd-analyze security --offline=true "$unit" 2>&1 || true)"
    score="$(printf '%s\n' "$out" | sed -n 's/.*exposure level for [^:]*: \([0-9.]*\).*/\1/p' | tail -n 1)"
    if [ -z "$score" ]; then
        echo "SKIP systemd-analyze security gave no score"
    elif awk -v s="$score" -v c="$CEILING" 'BEGIN { exit !(s <= c) }'; then
        echo "OK exposure score $score <= ceiling $CEILING"
    else
        echo "FAIL exposure score $score exceeds ceiling $CEILING"; fail=1
    fi
else
    echo "SKIP systemd-analyze not found"
fi

echo "== built packages (when rpm/dpkg tools and packages are present)"
dist="${SPOT_AGENT_DIST:-}"
if [ -n "$dist" ] && [ -d "$dist" ]; then
    for f in "$dist"/spot-agent-*.rpm; do
        [ -e "$f" ] || continue
        command -v rpm >/dev/null 2>&1 || { echo "SKIP rpm not found"; break; }
        rpm -qp --scripts "$f" | grep -Eq 'systemctl (enable|start)|spot-agent enrol' && { echo "FAIL scriptlet starts or enrols the agent: $f"; fail=1; }
        rpm -qpl "$f" | grep -q '^/etc/sudoers.d/spot-agent$' || { echo "FAIL no sudoers drop-in in $f"; fail=1; }
    done
    for f in "$dist"/spot-agent_*.deb; do
        [ -e "$f" ] || continue
        command -v dpkg-deb >/dev/null 2>&1 || { echo "SKIP dpkg-deb not found"; break; }
        dpkg-deb -c "$f" | awk 'substr($1,1,1) != "l" && substr($1,9,1) == "w" { print "FAIL world-writable: " $0; bad=1 } END { exit bad }' || fail=1
    done
else
    echo "SKIP set SPOT_AGENT_DIST to a directory of built packages to check them"
fi

[ "$fail" -eq 0 ] && echo "verify-agent: OK" || { echo "verify-agent: FAILED"; exit 1; }
