#!/usr/bin/env bash
# Checks of the control plane packaging.
#
#   bash packaging/linux/verify/verify-control-plane.sh
#
# Always: the python lint/tests (unit hardening against the host unit, config template against the
# service's own key list, spec and DEB scripts, Containerfile, compose file, dev certificate script).
# When `systemd-analyze` exists: `verify` and `security`, and the exposure score must not exceed the
# score of the HOST unit (packaging/linux/systemd/spot.service) measured the same way. When
# `podman` exists and SPOT_CP_BUILD_IMAGE=1: a build smoke test of the image. Missing tools print
# SKIP and do not fail.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cp="$(cd "$here/../control-plane" && pwd)"
host_unit="$(cd "$here/.." && pwd)/systemd/spot.service"
fail=0
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

echo "== lint and tests"
python3 -m unittest discover -s "$cp/tests" -v 2>&1 | tail -n 5 || fail=1
python3 -m unittest discover -s "$cp/tests" >/dev/null 2>&1 || { echo "FAIL control plane packaging tests"; fail=1; }

echo "== shell syntax"
bash -n "$cp/compose/gen-dev-certs.sh" || fail=1

echo "== systemd-analyze"
score_of() { # unit file -> exposure score on stdout
    local unit="$tmp/$(basename "$1")"
    sed -e 's#^ExecStart=.*#ExecStart=/usr/bin/true#' -e '/^ExecStartPre=/d' "$1" >"$unit"
    systemd-analyze security --offline=true "$unit" 2>&1 | sed -n 's/.*exposure level for [^:]*: \([0-9.]*\).*/\1/p' | tail -n 1
}
if command -v systemd-analyze >/dev/null 2>&1; then
    unit="$tmp/spot-control-plane.service"
    sed -e 's#^ExecStart=.*#ExecStart=/usr/bin/true#' "$cp/systemd/spot-control-plane.service" >"$unit"
    systemd-analyze verify "$unit" 2>&1 | grep -v '^$' || true
    mine="$(score_of "$cp/systemd/spot-control-plane.service")"
    theirs="$(score_of "$host_unit")"
    if [ -z "$mine" ] || [ -z "$theirs" ]; then
        echo "SKIP systemd-analyze security gave no score"
    elif awk -v m="$mine" -v t="$theirs" 'BEGIN { exit !(m <= t) }'; then
        echo "OK exposure score $mine <= host unit score $theirs (the ceiling)"
    else
        echo "FAIL exposure score $mine exceeds the host unit score $theirs"; fail=1
    fi
else
    echo "SKIP systemd-analyze not found"
fi

echo "== built packages (when SPOT_CP_DIST names a directory of them)"
dist="${SPOT_CP_DIST:-}"
if [ -n "$dist" ] && [ -d "$dist" ]; then
    for f in "$dist"/spot-control-plane-*.rpm; do
        [ -e "$f" ] || continue
        command -v rpm >/dev/null 2>&1 || { echo "SKIP rpm not found"; break; }
        if rpm -qp --scripts "$f" | grep -Eq 'systemctl (enable|start)'; then echo "FAIL scriptlet starts the service: $f"; fail=1; fi
        if rpm -qpl "$f" | grep -Eq '\.(pem|key|crt)$'; then echo "FAIL a key or certificate is packaged: $f"; fail=1; fi
    done
    for f in "$dist"/spot-control-plane_*.deb; do
        [ -e "$f" ] || continue
        command -v dpkg-deb >/dev/null 2>&1 || { echo "SKIP dpkg-deb not found"; break; }
        if dpkg-deb -c "$f" | grep -Eq '\.(pem|key|crt)$'; then echo "FAIL a key or certificate is packaged: $f"; fail=1; fi
    done
else
    echo "SKIP set SPOT_CP_DIST to check built packages"
fi

echo "== container image build"
if command -v podman >/dev/null 2>&1 && [ "${SPOT_CP_BUILD_IMAGE:-}" = "1" ]; then
    SPOT_CP_BUILD_IMAGE=1 python3 -m unittest discover -s "$cp/tests" -k test_image_builds_and_runs_non_root_read_only || fail=1
else
    echo "SKIP podman not found or SPOT_CP_BUILD_IMAGE is not 1"
fi

[ "$fail" -eq 0 ] && echo "verify-control-plane: OK" || { echo "verify-control-plane: FAILED"; exit 1; }
