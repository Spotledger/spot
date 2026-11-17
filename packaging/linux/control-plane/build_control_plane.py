#!/usr/bin/env python3
"""Build the spot-control-plane RPM and DEB from a prebuilt binary (nothing is compiled here).

    python3 packaging/linux/control-plane/build_control_plane.py rpm --binary X --version 1.2.3 --out dist
    python3 packaging/linux/control-plane/build_control_plane.py deb --binary X --version 1.2.3 --out dist

Versioning, the reproducible epoch and the helpers are those of ``tools/packaging/build.py``.
A kind whose tool is missing prints ``SKIP`` and exits 0, like ``build.py``. No key, certificate
or password is staged: the package ships the unit, the sysusers/tmpfiles data, the licence
files and two templates (``control-plane.toml.example``, ``environment``).
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

import build as host  # noqa: E402

NAME = "spot-control-plane"
MAINTAINER = "Spotledger Packaging <packaging@spotledger.invalid>"


def _read(path: Path) -> bytes:
    return path.read_bytes()


def _changelog(version: str, release: str, epoch: int) -> str:
    return (f"* {host.changelog_date(epoch)} {MAINTAINER} - {version}-{release}\n"
            f"- Automated release build of {version}-{release} from the prebuilt binary.\n")


def render_spec(version: str, epoch: int, arch: str = "x86_64") -> tuple[str, str, str]:
    host.require_rpm_changelog_epoch(epoch)
    rpm_version, rpm_release = host.rpm_versions(version)
    template = (HERE / "rpm" / "spot-control-plane.spec").read_text(encoding="utf-8")
    text = (template.replace("@VERSION@", rpm_version).replace("@RELEASE@", rpm_release)
            .replace("@ARCH@", arch).replace("@CHANGELOG@", _changelog(rpm_version, rpm_release, epoch)))
    return text, rpm_version, rpm_release


def deb_control(version: str, arch: str, installed_kb: int) -> str:
    return (
        f"Package: {NAME}\nVersion: {host.deb_version(version)}\nArchitecture: {host.deb_arch(arch)}\n"
        f"Maintainer: {MAINTAINER}\nInstalled-Size: {installed_kb}\nDepends: libc6, systemd\n"
        "Recommends: postgresql-18\nSection: admin\nPriority: optional\nHomepage: https://spotledger.example/\n"
        "Description: Spotledger control plane\n"
        " The Spotledger control plane: tenant registry, licensing, usage, fleet health\n"
        " and links, served on two mutual-TLS listeners.\n"
        " .\n"
        " The service is installed but never started or enabled, and no key,\n"
        " certificate or password is shipped.\n")


def _inputs(binary: Path) -> dict[str, tuple[bytes, int]]:
    if not binary.is_file():
        raise SystemExit(f"binary not found: {binary}")
    return {
        NAME: (_read(binary), 0o755),
        f"{NAME}.service": (_read(HERE / "systemd" / f"{NAME}.service"), 0o644),
        f"{NAME}.sysusers": (_read(HERE / "systemd" / f"{NAME}.sysusers"), 0o644),
        f"{NAME}.tmpfiles": (_read(HERE / "systemd" / f"{NAME}.tmpfiles"), 0o644),
        "control-plane.toml.example": (_read(HERE / "etc" / "control-plane.toml.example"), 0o640),
        "environment": (_read(HERE / "etc" / "environment"), 0o600),
        "LICENSE": (host._license_bytes(), 0o644),
        "NOTICE": (host._notice_bytes(), 0o644),
    }


def build_rpm(binary: Path, version: str, arch: str, out: Path, epoch: int) -> dict:
    host.require_rpm_changelog_epoch(epoch)
    tool = shutil.which("rpmbuild")
    if tool is None:
        return host._skip("rpm", version, arch, epoch, "rpmbuild not found")
    spec_text, rpm_version, rpm_release = render_spec(version, epoch, arch)
    with tempfile.TemporaryDirectory(prefix="spot-cp-rpm-") as tmp:
        top = Path(tmp) / "rpmbuild"
        sources = top / "SOURCES"
        for directory in ("BUILD", "BUILDROOT", "RPMS", "SOURCES", "SPECS", "SRPMS"):
            (top / directory).mkdir(parents=True, exist_ok=True)
        for name, (data, mode) in _inputs(binary).items():
            host._write(sources / name, data, mode)
        spec = host._write(top / "SPECS" / f"{NAME}.spec", spec_text.encode(), 0o644)
        env = dict(os.environ, SOURCE_DATE_EPOCH=str(epoch))
        run = subprocess.run([tool, "-bb", "--define", f"_topdir {top}", "--define", f"_sourcedir {sources}",
                              "--define", "_buildhost reproducible", "--define", "use_source_date_epoch_as_buildtime 1",
                              str(spec)], capture_output=True, text=True, env=env, check=False)
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


def build_deb(binary: Path, version: str, arch: str, out: Path, epoch: int) -> dict:
    tool = shutil.which("dpkg-deb")
    if tool is None:
        return host._skip("deb", version, arch, epoch, "dpkg-deb not found")
    src = _inputs(binary)
    with tempfile.TemporaryDirectory(prefix="spot-cp-deb-") as tmp:
        root = Path(tmp) / "root"
        staged = [
            (f"usr/bin/{NAME}", NAME),
            (f"usr/lib/systemd/system/{NAME}.service", f"{NAME}.service"),
            (f"usr/lib/sysusers.d/{NAME}.conf", f"{NAME}.sysusers"),
            (f"usr/lib/tmpfiles.d/{NAME}.conf", f"{NAME}.tmpfiles"),
            (f"etc/{NAME}/control-plane.toml.example", "control-plane.toml.example"),
            (f"etc/{NAME}/environment", "environment"),
            (f"usr/share/doc/{NAME}/LICENSE", "LICENSE"),
            (f"usr/share/doc/{NAME}/NOTICE", "NOTICE"),
        ]
        for dest, name in staged:
            data, mode = src[name]
            host._write(root / dest, data, mode)
        os.chmod(root / "etc" / NAME, 0o750)
        (root / "var/lib" / NAME).mkdir(parents=True, exist_ok=True)
        os.chmod(root / "var/lib" / NAME, 0o700)
        host._write(root / f"usr/share/doc/{NAME}/copyright", _read(HERE / "debian" / "copyright"), 0o644)
        host._write(root / f"usr/share/doc/{NAME}/changelog.gz",
                    gzip.compress(_changelog(host.deb_version(version), "1", epoch).encode(), compresslevel=9, mtime=epoch), 0o644)
        host._write(root / "DEBIAN/control", deb_control(version, arch, host._tree_size_kb(root)).encode(), 0o644)
        host._write(root / "DEBIAN/conffiles", _read(HERE / "debian" / "conffiles"), 0o644)
        for script in ("postinst", "prerm", "postrm"):
            host._write(root / "DEBIAN" / script, _read(HERE / "debian" / script), 0o755)
        host._set_tree_times(root, epoch)
        filename = f"{NAME}_{host.deb_version(version)}_{host.deb_arch(arch)}.deb"
        tmp_deb = Path(tmp) / filename
        run = subprocess.run([tool, "--root-owner-group", "--build", str(root), str(tmp_deb)], capture_output=True,
                             text=True, env=dict(os.environ, SOURCE_DATE_EPOCH=str(epoch)), check=False)
        if run.returncode != 0:
            sys.stderr.write(run.stdout + run.stderr)
            raise SystemExit(f"dpkg-deb failed with exit {run.returncode}")
        target = host._write(out / filename, _read(tmp_deb), 0o644)
    return host._summary("deb", version, arch, str(target), epoch,
                         {"deb_version": host.deb_version(version), "sha256": hashlib.sha256(_read(target)).hexdigest()})


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("kind", choices=("rpm", "deb"))
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--arch", default="x86_64")
    parser.add_argument("--out", type=Path, default=Path("dist"))
    args = parser.parse_args(argv)
    epoch = host.source_date_epoch()
    args.out.mkdir(parents=True, exist_ok=True)
    func = build_rpm if args.kind == "rpm" else build_deb
    print(json.dumps(func(args.binary, args.version, args.arch, args.out, epoch), indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
