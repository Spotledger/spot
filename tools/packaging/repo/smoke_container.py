#!/usr/bin/env python3
"""Smoke test in a container: install from a ``file://`` repository with every check on, then flip one byte.

    python3 tools/packaging/repo/smoke_container.py rpm --repo repo --package spot-repo-keys
    python3 tools/packaging/repo/smoke_container.py deb --repo repo --package spot-repo-keys

``--repo`` is the ``--out`` tree of ``build_repo.py`` (it holds ``spot-release.asc``).
Rootless podman is used; without it the command exits 77 (a skip, like
``tools/packaging/verify/run.py``). It passes only when (1) the untouched
repository installs the package with gpgcheck/repo_gpgcheck (rpm) or Signed-By
(deb) on, and (2) the same repository with one flipped byte in the package, and
then in the signed metadata, is REFUSED. The repository is mounted read-only; the
tampering happens on a copy inside the container. Not run in CI by the task that
wrote it (no container runtime there): the bundle or the packaging runner runs it.
"""

from __future__ import annotations

import argparse
import shutil
import subprocess
import sys
from pathlib import Path

IMAGES = {"rpm": "registry.fedoraproject.org/fedora:44", "deb": "docker.io/library/debian:13"}

RPM_SCRIPT = r"""
set -eu
cp -r /repo /work && cd /work
cat >/etc/yum.repos.d/smoke.repo <<'CFG'
[smoke]
name=smoke
baseurl=file:///work/rpm/CHANNEL/
enabled=1
gpgcheck=1
repo_gpgcheck=1
localpkg_gpgcheck=1
gpgkey=file:///work/spot-release.asc
CFG
flip() { python3 - "$1" <<'PY'
import sys
p = sys.argv[1]; d = bytearray(open(p, 'rb').read()); d[len(d)//2] ^= 1; open(p, 'wb').write(bytes(d))
PY
}
dnf -y --refresh install PACKAGE
echo "OK clean install"
dnf -y remove PACKAGE >/dev/null
flip "$(ls /work/rpm/CHANNEL/Packages/PACKAGE-*.rpm | head -n 1)"
if dnf -y --refresh install PACKAGE; then echo "FAIL tampered package installed"; exit 1; fi
echo "OK tampered package refused"
cp /repo/rpm/CHANNEL/Packages/*.rpm /work/rpm/CHANNEL/Packages/
flip /work/rpm/CHANNEL/repodata/repomd.xml
if dnf -y --refresh install PACKAGE; then echo "FAIL tampered metadata accepted"; exit 1; fi
echo "OK tampered metadata refused"
"""

DEB_SCRIPT = r"""
set -eu
export DEBIAN_FRONTEND=noninteractive
apt-get update >/dev/null 2>&1 || true
apt-get install -y --no-install-recommends gnupg ca-certificates python3 >/dev/null
cp -r /repo /work && cd /work
gpg --batch --no-tty --dearmor --output /work/ring.gpg /work/spot-release.asc
cat >/etc/apt/sources.list.d/smoke.sources <<'CFG'
Types: deb
URIs: file:///work/deb
Suites: CHANNEL
Components: main
Signed-By: /work/ring.gpg
CFG
flip() { python3 - "$1" <<'PY'
import sys
p = sys.argv[1]; d = bytearray(open(p, 'rb').read()); d[len(d)//2] ^= 1; open(p, 'wb').write(bytes(d))
PY
}
apt-get update
apt-get install -y PACKAGE
echo "OK clean install"
apt-get remove -y PACKAGE >/dev/null
f="$(ls /work/deb/pool/CHANNEL/main/*/*/PACKAGE_*.deb | head -n 1)"
flip "$f"
if apt-get install -y PACKAGE; then echo "FAIL tampered package installed"; exit 1; fi
echo "OK tampered package refused"
cp "$(ls /repo/deb/pool/CHANNEL/main/*/*/PACKAGE_*.deb | head -n 1)" "$f"
flip /work/deb/dists/CHANNEL/InRelease
if apt-get update 2>&1 | grep -q 'NO_PUBKEY\|BADSIG\|not signed\|invalid'; then echo "OK tampered metadata refused"; else echo "FAIL tampered metadata accepted"; exit 1; fi
"""


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("kind", choices=("rpm", "deb"))
    parser.add_argument("--repo", type=Path, required=True)
    parser.add_argument("--package", required=True)
    parser.add_argument("--channel", default="stable")
    parser.add_argument("--image", default=None)
    args = parser.parse_args(argv)
    podman = shutil.which("podman")
    if podman is None:
        print("SKIP podman not installed")
        return 77
    if not (args.repo / "spot-release.asc").is_file():
        print("ERROR spot-release.asc not found in --repo", file=sys.stderr)
        return 2
    script = (RPM_SCRIPT if args.kind == "rpm" else DEB_SCRIPT)
    script = script.replace("CHANNEL", args.channel).replace("PACKAGE", args.package)
    cmd = [podman, "run", "--rm", "--security-opt", "label=disable", "-v", f"{args.repo.resolve()}:/repo:ro",
           args.image or IMAGES[args.kind], "/bin/bash", "-c", script]
    run = subprocess.run(cmd, check=False)
    return 0 if run.returncode == 0 else 1


if __name__ == "__main__":
    raise SystemExit(main())
