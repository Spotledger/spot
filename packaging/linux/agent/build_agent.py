#!/usr/bin/env python3
"""Build the spot-agent RPM and DEB from prebuilt binaries (nothing is compiled here).

    python3 packaging/linux/agent/build_agent.py rpm --binary X --helper Y --version 1.2.3 --out dist
    python3 packaging/linux/agent/build_agent.py deb --binary X --helper Y --version 1.2.3 --out dist

The sudoers drop-in is not a file in the repository: it is produced at build time
by ``Y --print-sudoers --user spot-agent`` and checked by ``agent_sudoers.py``.
Versioning, epoch and reproducibility reuse ``tools/packaging/build.py``. A kind
whose tool is missing prints ``SKIP`` and exits 0, like ``build.py``.
"""

from __future__ import annotations

import argparse
import gzip
import hashlib
import json
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[2]
sys.path.insert(0, str(REPO / "tools" / "packaging"))
sys.path.insert(0, str(HERE))

import agent_sudoers  # noqa: E402
import build as host  # noqa: E402

NAME = "spot-agent"
MAINTAINER = "Spotledger Packaging <packaging@spotledger.invalid>"


def _read(path: Path) -> bytes:
    return path.read_bytes()


def _changelog(version: str, release: str, epoch: int) -> str:
    return (
        f"* {host.changelog_date(epoch)} {MAINTAINER} - {version}-{release}\n"
        f"- Automated release build of {version}-{release} from the prebuilt binaries.\n"
    )


def render_spec(version: str, epoch: int, arch: str = "x86_64") -> tuple[str, str, str]:
    host.require_rpm_changelog_epoch(epoch)
    rpm_version, rpm_release = host.rpm_versions(version)
    template = (HERE / "rpm" / "spot-agent.spec").read_text(encoding="utf-8")
    text = (
        template.replace("@VERSION@", rpm_version)
        .replace("@RELEASE@", rpm_release)
        .replace("@ARCH@", arch)
        .replace("@CHANGELOG@", _changelog(rpm_version, rpm_release, epoch))
    )
    return text, rpm_version, rpm_release


def render_preinst(version: str) -> str:
    text = (HERE / "debian" / "preinst").read_text(encoding="ascii")
    return text.replace("@DEB_VERSION@", host.deb_version(version))


def deb_control(version: str, arch: str, installed_kb: int) -> str:
    return (
        f"Package: {NAME}\n"
        f"Version: {host.deb_version(version)}\n"
        f"Architecture: {host.deb_arch(arch)}\n"
        f"Maintainer: {MAINTAINER}\n"
        f"Installed-Size: {installed_kb}\n"
        "Depends: libc6, systemd, sudo\n"
        "Section: admin\n"
        "Priority: optional\n"
        "Homepage: https://spotledger.example/\n"
        "Description: Spotledger host agent\n"
        " The Spotledger host agent runs as an unprivileged account and executes signed\n"
        " management commands. The few privileged operations go through a small helper\n"
        " named by an exact entry in /etc/sudoers.d/spot-agent.\n"
        " .\n"
        " The package never starts, enables or enrols the agent: enrolment is an\n"
        " explicit operator action with a one-time token.\n"
    )


def _inputs(binary: Path, helper: Path) -> dict[str, tuple[bytes, int]]:
    """Every staged source: name -> (bytes, mode). The sudoers text comes from the helper."""
    for path, what in ((binary, "agent binary"), (helper, "helper binary")):
        if not path.is_file():
            raise SystemExit(f"{what} not found: {path}")
    sudoers = agent_sudoers.generate(helper)
    return {
        "spot-agent": (_read(binary), 0o755),
        "spot-agent-priv": (_read(helper), 0o755),
        "spot-agent.service": (_read(HERE / "systemd" / "spot-agent.service"), 0o644),
        "spot-agent.sysusers": (_read(HERE / "systemd" / "spot-agent.sysusers"), 0o644),
        "spot-agent.tmpfiles": (_read(HERE / "systemd" / "spot-agent.tmpfiles"), 0o644),
        "spot-agent.sudoers": (sudoers.encode("ascii"), 0o440),
        "LICENSE": (host._license_bytes(), 0o644),
        "NOTICE": (host._notice_bytes(), 0o644),
    }


def build_rpm(binary: Path, helper: Path, version: str, arch: str, out: Path, epoch: int) -> dict:
    host.require_rpm_changelog_epoch(epoch)
    tool = shutil.which("rpmbuild")
    if tool is None:
        return host._skip("rpm", version, arch, epoch, "rpmbuild not found")
    spec_text, rpm_version, rpm_release = render_spec(version, epoch, arch)
    with tempfile.TemporaryDirectory(prefix="spot-agent-rpm-") as tmp:
        top = Path(tmp) / "rpmbuild"
        sources = top / "SOURCES"
        for directory in ("BUILD", "BUILDROOT", "RPMS", "SOURCES", "SPECS", "SRPMS"):
            (top / directory).mkdir(parents=True, exist_ok=True)
        for name, (data, mode) in _inputs(binary, helper).items():
            host._write(sources / name, data, mode)
        spec = host._write(top / "SPECS" / "spot-agent.spec", spec_text.encode(), 0o644)
        env = dict(os.environ, SOURCE_DATE_EPOCH=str(epoch))
        command = [
            tool, "-bb",
            "--define", f"_topdir {top}",
            "--define", f"_sourcedir {sources}",
            "--define", "_buildhost reproducible",
            "--define", "use_source_date_epoch_as_buildtime 1",
            str(spec),
        ]
        run = subprocess.run(command, capture_output=True, text=True, env=env, check=False)
        if run.returncode != 0:
            sys.stderr.write(run.stdout + run.stderr)
            raise SystemExit(f"rpmbuild failed with exit {run.returncode}")
        rpms = sorted((top / "RPMS").rglob("*.rpm"))
        if not rpms:
            raise SystemExit("rpmbuild produced no .rpm")
        target = host._write(out / rpms[-1].name, _read(rpms[-1]), 0o644)
    return host._summary("rpm", version, arch, str(target), epoch,
                         {"rpm_version": rpm_version, "rpm_release": rpm_release,
                          "sha256": hashlib.sha256(_read(target)).hexdigest()})


def build_deb(binary: Path, helper: Path, version: str, arch: str, out: Path, epoch: int) -> dict:
    tool = shutil.which("dpkg-deb")
    if tool is None:
        return host._skip("deb", version, arch, epoch, "dpkg-deb not found")
    src = _inputs(binary, helper)
    with tempfile.TemporaryDirectory(prefix="spot-agent-deb-") as tmp:
        root = Path(tmp) / "root"
        staged = [
            ("usr/bin/spot-agent", "spot-agent"),
            ("usr/libexec/spot-agent/spot-agent-priv", "spot-agent-priv"),
            ("usr/lib/systemd/system/spot-agent.service", "spot-agent.service"),
            ("usr/lib/sysusers.d/spot-agent.conf", "spot-agent.sysusers"),
            ("usr/lib/tmpfiles.d/spot-agent.conf", "spot-agent.tmpfiles"),
            ("etc/sudoers.d/spot-agent", "spot-agent.sudoers"),
            ("usr/share/doc/spot-agent/LICENSE", "LICENSE"),
            ("usr/share/doc/spot-agent/NOTICE", "NOTICE"),
        ]
        for dest, name in staged:
            data, mode = src[name]
            host._write(root / dest, data, mode)
        host._write(root / "usr/share/doc/spot-agent/copyright",
                    _read(HERE / "debian" / "copyright"), 0o644)
        changelog = _changelog(host.deb_version(version), "1", epoch).encode()
        host._write(root / "usr/share/doc/spot-agent/changelog.gz",
                    gzip.compress(changelog, compresslevel=9, mtime=epoch), 0o644)
        (root / "var/lib/spot-agent").mkdir(parents=True, exist_ok=True)
        os.chmod(root / "var/lib/spot-agent", 0o700)
        host._write(root / "DEBIAN/control",
                    deb_control(version, arch, host._tree_size_kb(root)).encode(), 0o644)
        host._write(root / "DEBIAN/preinst", render_preinst(version).encode(), 0o755)
        for script in ("postinst", "prerm", "postrm"):
            host._write(root / "DEBIAN" / script, _read(HERE / "debian" / script), 0o755)
        host._set_tree_times(root, epoch)
        filename = f"{NAME}_{host.deb_version(version)}_{host.deb_arch(arch)}.deb"
        tmp_deb = Path(tmp) / filename
        env = dict(os.environ, SOURCE_DATE_EPOCH=str(epoch))
        run = subprocess.run([tool, "--root-owner-group", "--build", str(root), str(tmp_deb)],
                             capture_output=True, text=True, env=env, check=False)
        if run.returncode != 0:
            sys.stderr.write(run.stdout + run.stderr)
            raise SystemExit(f"dpkg-deb failed with exit {run.returncode}")
        target = host._write(out / filename, _read(tmp_deb), 0o644)
    return host._summary("deb", version, arch, str(target), epoch,
                         {"deb_version": host.deb_version(version),
                          "sha256": hashlib.sha256(_read(target)).hexdigest()})


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("kind", choices=("rpm", "deb"))
    parser.add_argument("--binary", type=Path, required=True, help="prebuilt spot-agent")
    parser.add_argument("--helper", type=Path, required=True, help="prebuilt spot-agent-priv")
    parser.add_argument("--version", required=True)
    parser.add_argument("--arch", default="x86_64")
    parser.add_argument("--out", type=Path, default=Path("dist"))
    args = parser.parse_args(argv)
    epoch = host.source_date_epoch()
    args.out.mkdir(parents=True, exist_ok=True)
    try:
        func = build_rpm if args.kind == "rpm" else build_deb
        summary = func(args.binary, args.helper, args.version, args.arch, args.out, epoch)
    except agent_sudoers.SudoersError as exc:
        raise SystemExit(f"sudoers drop-in refused: {exc}") from exc
    print(json.dumps(summary, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
