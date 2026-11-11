#!/usr/bin/env python3
"""Build Linux distribution artefacts from an already-built release binary.

    build.py rpm|deb|container|tarball --binary PATH --version X.Y.Z [--out DIR]

Standard library only (Python 3.11). Nothing here compiles Rust or calls cargo:
the binary is an input. The tarball is always buildable; rpm, deb and container
steps are skipped with ``SKIP <what> <why>`` (exit 0, JSON summary) when their
tool is absent, so the packaging tests run on any host.

Reproducibility: ``SOURCE_DATE_EPOCH`` is honoured for every timestamp; when it
is unset a fixed epoch is used, so two runs produce byte-identical archives.
Owners are normalised to 0/0, modes are fixed per file kind and archive entries
are sorted.
"""

from __future__ import annotations

import argparse
import gzip
import hashlib
import io
import json
import os
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import time
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
PACKAGING = REPO_ROOT / "packaging" / "linux"
#: The largest epoch gzip, tar and the ar header of a .deb can express.
MAX_EPOCH = 0xFFFFFFFF  # 2106-02-07T06:28:15Z
#: The default build epoch: 2026-01-01T00:00:00Z.
#:
#: 1980-01-01, the earliest mtime a portable tar can carry, was the obvious
#: choice and is what this script used until it turned out to break the RPM:
#: rpm reads the newest ``%changelog`` entry as the package build time, its
#: date parser only accepts a year from 1990 to 2999, and for anything else it
#: logs ``error: bad date in %changelog``, drops the entry and still exits 0,
#: so the package ships with an empty changelog and nothing fails. One fixed
#: modern date is valid for rpm, dpkg-deb and tar at once, and because it is a
#: constant rather than "today" two runs still produce identical bytes.
DEFAULT_EPOCH = 1767225600  # 2026-01-01T00:00:00Z

#: The oldest and newest ``%changelog`` dates rpm accepts, in seconds.
#:
#: rpm's parser (``build/parseChangelog.c``) refuses a year below 1990 or at or
#: after 3000; see :func:`require_rpm_changelog_epoch` for why that has to be a
#: hard error here instead of a warning from rpm. A 32-bit mtime stops at 2106,
#: well below rpm's own ceiling, so :data:`MAX_EPOCH` is the upper bound.
RPM_CHANGELOG_MIN_EPOCH = 631152000  # 1990-01-01T00:00:00Z, "Mon Jan 01 1990"
RPM_CHANGELOG_MAX_EPOCH = MAX_EPOCH

KINDS = ("rpm", "deb", "container", "tarball")
FIXED_MODE = 0o644
EXEC_MODE = 0o755
DIR_MODE = 0o755
CONFIG_MODE = 0o640


def source_date_epoch() -> int:
    """The build epoch: SOURCE_DATE_EPOCH when set and valid, else a fixed date."""
    raw = os.environ.get("SOURCE_DATE_EPOCH", "").strip()
    if raw:
        try:
            value = int(raw, 10)
        except ValueError as exc:
            raise SystemExit(f"SOURCE_DATE_EPOCH is not an integer: {raw!r}") from exc
        if value < 0:
            raise SystemExit(f"SOURCE_DATE_EPOCH must not be negative: {raw!r}")
        if value > MAX_EPOCH:
            raise SystemExit(
                f"SOURCE_DATE_EPOCH does not fit in 32 bits: {raw!r} "
                "(tar and gzip cannot store that mtime)"
            )
        return value
    return DEFAULT_EPOCH


def changelog_date(epoch: int) -> str:
    """The ``%changelog`` date for an epoch, in rpm's ``%a %b %d %Y`` form.

    Always rendered from the epoch argument, never from the clock, so the same
    epoch gives the same text on every host and in every run.
    """
    return time.strftime("%a %b %d %Y", time.gmtime(epoch))


def require_rpm_changelog_epoch(epoch: int) -> None:
    """Refuse an epoch rpm would reject in ``%changelog``, before rpmbuild runs.

    rpm takes the newest changelog entry as the package build time, and its date
    parser accepts only a year from 1990 to 2999. Outside that it logs
    ``error: bad date in %changelog``, drops the entry and still exits 0, so the
    package is built with no changelog and no step fails; the message it prints
    says nothing about the variable the caller set. Failing here names it.
    """
    if RPM_CHANGELOG_MIN_EPOCH <= epoch <= RPM_CHANGELOG_MAX_EPOCH:
        return
    raise SystemExit(
        f"SOURCE_DATE_EPOCH={epoch} renders the changelog date "
        f"{changelog_date(epoch)!r}, which rpm refuses: it drops the entry and "
        f"builds a package with no changelog. rpm accepts "
        f"{changelog_date(RPM_CHANGELOG_MIN_EPOCH)!r} to "
        f"{changelog_date(RPM_CHANGELOG_MAX_EPOCH)!r}. Unset SOURCE_DATE_EPOCH "
        f"for the fixed default ({DEFAULT_EPOCH}), or set it inside that window."
    )


def _split_release(version: str) -> tuple[str, str]:
    """Split ``X.Y.Z[-PRE]`` into the upstream base and a pre-release tag.

    A leading ``v`` is accepted because release tags often carry one. Anything
    that does not look like a dotted numeric base is rejected rather than
    silently packaged under a surprising version.
    """
    value = version.strip()
    if value.startswith("v"):
        value = value[1:]
    base, sep, pre = value.partition("-")
    if not re.fullmatch(r"\d+(?:\.\d+){0,2}", base):
        raise SystemExit(f"invalid version {version!r}: expected X.Y.Z[-pre]")
    if sep and not pre:
        raise SystemExit(f"invalid version {version!r}: empty pre-release tag")
    if pre and not re.fullmatch(r"[0-9A-Za-z][0-9A-Za-z.-]*", pre):
        raise SystemExit(f"invalid pre-release tag in {version!r}")
    return base, pre


def rpm_versions(version: str) -> tuple[str, str]:
    """Map a release version to RPM ``Version`` and ``Release``.

    RPM forbids ``-`` in Version, so a pre-release moves into Release with a
    leading zero: ``2.0.0-rc.1`` -> ``("2.0.0", "0.rc.1")``. The zero sorts the
    pre-release before the final ``2.0.0-1`` build, which is what we want.
    """
    base, pre = _split_release(version)
    if not pre:
        return base, "1"
    return base, "0." + pre.replace("-", ".")


def deb_version(version: str) -> str:
    """Map a release version to a Debian upstream version.

    A pre-release uses ``~`` so it sorts before the final release:
    ``2.0.0-rc.1`` -> ``2.0.0~rc.1``.
    """
    base, pre = _split_release(version)
    return f"{base}~{pre}" if pre else base


def package_versions(version: str) -> dict[str, str]:
    """All package-version spellings for one release version (used by tests)."""
    rpm_version, rpm_release = rpm_versions(version)
    return {
        "version": version,
        "rpm_version": rpm_version,
        "rpm_release": rpm_release,
        "deb_version": deb_version(version),
    }


def deb_arch(arch: str) -> str:
    """Map a Rust target architecture to a Debian architecture name."""
    return {"x86_64": "amd64", "aarch64": "arm64"}.get(arch, arch)


def normalise_version(version: str) -> str:
    """Drop a leading ``v`` and return the canonical ``X.Y.Z[-pre]`` spelling."""
    base, pre = _split_release(version)
    return f"{base}-{pre}" if pre else base


def tarball_name(version: str, arch: str) -> str:
    return f"spot-{version}-linux-{arch}.tar.gz"


def _read(path: Path) -> bytes:
    return path.read_bytes()


def _license_bytes() -> bytes:
    """The project licence, or an Apache-2.0 notice when the repo has no file."""
    for candidate in ("LICENSE", "LICENSE.txt", "LICENSE.md", "COPYING"):
        path = REPO_ROOT / candidate
        if path.is_file():
            return _read(path)
    return (
        "Copyright (c) 2026 Spotledger (SMC-PRIVATE) Limited.\n"
        "Licensed under the Apache License, Version 2.0; see https://www.apache.org/licenses/LICENSE-2.0\n"
    ).encode()


def _readme_install(version: str) -> bytes:
    return f"""Spotledger {version} - installation
========================================

Files in this tarball
  bin/spot              the release binary
  etc/spot.toml         safe default configuration (edit before starting)
  etc/environment       empty environment file for the service
  systemd/spot.service  hardened systemd unit
  systemd/spot.sysusers service account definition
  systemd/spot.tmpfiles state/log/runtime directories

Install (as root)
  1. install -m 0755 bin/spot /usr/bin/spot
  2. install -m 0644 systemd/spot.sysusers /usr/lib/sysusers.d/spot.conf
     install -m 0644 systemd/spot.tmpfiles /usr/lib/tmpfiles.d/spot.conf
     install -m 0644 systemd/spot.service  /usr/lib/systemd/system/spot.service
  3. systemd-sysusers && systemd-tmpfiles --create
  4. install -d -m 0755 /etc/spot
     install -m 0640 etc/spot.toml    /etc/spot/spot.toml
     install -m 0640 etc/environment  /etc/spot/environment
  5. edit /etc/spot/spot.toml and provision a site
  6. systemctl daemon-reload && systemctl enable --now spot.service

The service is never started by an installer. Logs go to the journal:
  journalctl -u spot.service

This archive is reproducible; see packaging/linux/README.md.
""".encode()


class Source:
    """One staged file: archive name, bytes and mode."""

    def __init__(self, name: str, data: bytes, mode: int = FIXED_MODE) -> None:
        self.name = name
        self.data = data
        self.mode = mode


def _sources(binary: Path, version: str) -> list[Source]:
    if not binary.is_file():
        raise SystemExit(f"binary not found: {binary}")
    return [
        Source("spot", _read(binary), EXEC_MODE),
        Source("spot.service", _read(PACKAGING / "systemd" / "spot.service")),
        Source("spot.sysusers", _read(PACKAGING / "systemd" / "spot.sysusers")),
        Source("spot.tmpfiles", _read(PACKAGING / "systemd" / "spot.tmpfiles")),
        Source("spot.toml", _read(PACKAGING / "etc" / "spot.toml"), CONFIG_MODE),
        Source("environment", _read(PACKAGING / "etc" / "environment"), CONFIG_MODE),
        Source("LICENSE", _license_bytes()),
        Source("README.install", _readme_install(version)),
    ]


def _tarinfo(name: str, size: int, mode: int, epoch: int, is_dir: bool = False) -> tarfile.TarInfo:
    info = tarfile.TarInfo(name)
    info.size = 0 if is_dir else size
    info.mode = mode
    info.type = tarfile.DIRTYPE if is_dir else tarfile.REGTYPE
    info.mtime = epoch
    info.uid = 0
    info.gid = 0
    info.uname = ""
    info.gname = ""
    return info


def tarball_bytes(binary: Path, version: str, arch: str, epoch: int) -> bytes:
    """The reproducible ``.tar.gz`` bytes for one release."""
    files = _sources(binary, version)
    entries: list[tuple[str, int, int, bytes]] = []
    for directory in ("bin", "etc", "systemd"):
        entries.append((directory + "/", DIR_MODE, 0, b""))
    layout = {
        "spot": "bin/spot",
        "spot.toml": "etc/spot.toml",
        "environment": "etc/environment",
        "spot.service": "systemd/spot.service",
        "spot.sysusers": "systemd/spot.sysusers",
        "spot.tmpfiles": "systemd/spot.tmpfiles",
        "LICENSE": "LICENSE",
        "README.install": "README.install",
    }
    for source in files:
        entries.append((layout[source.name], source.mode, len(source.data), source.data))
    entries.sort(key=lambda item: item[0])

    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w", format=tarfile.USTAR_FORMAT) as tar:
        for name, mode, size, data in entries:
            is_dir = name.endswith("/")
            info = _tarinfo(name, size, mode, epoch, is_dir)
            tar.addfile(info, None if is_dir else io.BytesIO(data))
    # gzip.compress writes a fixed header when mtime is supplied; no filename,
    # so the output does not depend on the archive path or the clock.
    return gzip.compress(raw.getvalue(), compresslevel=9, mtime=epoch)


def _write(path: Path, data: bytes, mode: int = FIXED_MODE) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(data)
    path.chmod(mode)
    return path


def _summary(kind: str, version: str, arch: str, artifact: str | None,
             epoch: int, extra: dict | None = None) -> dict:
    result = {
        "kind": kind,
        "status": "ok",
        "version": version,
        "arch": arch,
        "source_date_epoch": epoch,
    }
    if artifact is not None:
        result["artifact"] = artifact
    if extra:
        result.update(extra)
    return result


def _skip(kind: str, version: str, arch: str, epoch: int, reason: str) -> dict:
    result = _summary(kind, version, arch, None, epoch)
    result.update({"status": "skipped", "reason": reason})
    print(f"SKIP {kind} {reason}")
    return result


def build_tarball(binary: Path, version: str, arch: str, out: Path, epoch: int) -> dict:
    data = tarball_bytes(binary, version, arch, epoch)
    target = _write(out / tarball_name(version, arch), data, FIXED_MODE)
    return _summary("tarball", version, arch, str(target), epoch,
                    {"sha256": hashlib.sha256(data).hexdigest(), "bytes": len(data)})


def _changelog(version: str, release: str, epoch: int) -> str:
    return (
        f"* {changelog_date(epoch)} Spotledger Packaging <packaging@spotledger.invalid> - {version}-{release}\n"
        f"- Automated release build of {version}-{release} from the prebuilt binary.\n"
    )


def render_spec(version: str, epoch: int, arch: str = "x86_64") -> tuple[str, str, str]:
    """Render the RPM spec template. Returns (text, rpm_version, rpm_release)."""
    require_rpm_changelog_epoch(epoch)
    rpm_version, rpm_release = rpm_versions(version)
    template = (PACKAGING / "rpm" / "spot.spec").read_text(encoding="utf-8")
    text = (
        template.replace("@VERSION@", rpm_version)
        .replace("@RELEASE@", rpm_release)
        .replace("@ARCH@", arch)
        .replace("@CHANGELOG@", _changelog(rpm_version, rpm_release, epoch))
    )
    return text, rpm_version, rpm_release


def build_rpm(binary: Path, version: str, arch: str, out: Path, epoch: int) -> dict:
    # Before the tool check and before rpmbuild: a caller epoch rpm refuses must
    # fail as a bad SOURCE_DATE_EPOCH, not as rpm's "bad date in %changelog".
    require_rpm_changelog_epoch(epoch)
    tool = shutil.which("rpmbuild")
    if tool is None:
        return _skip("rpm", version, arch, epoch, "rpmbuild not found")
    spec_text, rpm_version, rpm_release = render_spec(version, epoch, arch)
    with tempfile.TemporaryDirectory(prefix="spot-rpm-") as tmp:
        top = Path(tmp) / "rpmbuild"
        sources = top / "SOURCES"
        for directory in ("BUILD", "BUILDROOT", "RPMS", "SOURCES", "SPECS", "SRPMS"):
            (top / directory).mkdir(parents=True, exist_ok=True)
        for source in _sources(binary, version):
            _write(sources / source.name, source.data, source.mode)
        spec = _write(top / "SPECS" / "spot.spec", spec_text.encode(), FIXED_MODE)
        env = dict(os.environ, SOURCE_DATE_EPOCH=str(epoch))
        command = [
            tool, "-bb",
            "--define", f"_topdir {top}",
            "--define", f"_sourcedir {sources}",
            "--define", f"_buildhost reproducible",
            "--define", f"use_source_date_epoch_as_buildtime 1",
            str(spec),
        ]
        run = subprocess.run(command, capture_output=True, text=True, env=env)
        if run.returncode != 0:
            sys.stderr.write(run.stdout)
            sys.stderr.write(run.stderr)
            raise SystemExit(f"rpmbuild failed with exit {run.returncode}")
        rpms = sorted((top / "RPMS").rglob("*.rpm"))
        if not rpms:
            raise SystemExit("rpmbuild produced no .rpm")
        target = _write(out / rpms[-1].name, _read(rpms[-1]), FIXED_MODE)
    data = _read(target)
    return _summary("rpm", version, arch, str(target), epoch,
                    {"rpm_version": rpm_version, "rpm_release": rpm_release,
                     "sha256": hashlib.sha256(data).hexdigest()})


def deb_control(version: str, arch: str, installed_kb: int) -> str:
    return (
        "Package: spot\n"
        f"Version: {deb_version(version)}\n"
        f"Architecture: {deb_arch(arch)}\n"
        "Maintainer: Spotledger Packaging <packaging@spotledger.invalid>\n"
        f"Installed-Size: {installed_kb}\n"
        "Depends: libc6, systemd\n"
        "Recommends: postgresql-18\n"
        "Section: admin\n"
        "Priority: optional\n"
        "Homepage: https://spotledger.example/\n"
        "Description: Spotledger site host\n"
        " Spotledger is a single-binary site host on PostgreSQL. This package installs\n"
        " the daemon, a hardened systemd unit with its sysusers and tmpfiles data, and\n"
        " a safe default configuration under /etc/spot.\n"
        " .\n"
        " The service is installed but deliberately not started or enabled: an\n"
        " administrator provisions a site first and then starts it.\n"
    )


def _set_tree_times(root: Path, epoch: int) -> None:
    for path in sorted(root.rglob("*")):
        os.utime(path, (epoch, epoch), follow_symlinks=False)
    os.utime(root, (epoch, epoch), follow_symlinks=False)


def _tree_size_kb(root: Path) -> int:
    total = sum(path.stat().st_size for path in root.rglob("*") if path.is_file())
    return max(1, (total + 1023) // 1024)


def build_deb(binary: Path, version: str, arch: str, out: Path, epoch: int) -> dict:
    tool = shutil.which("dpkg-deb")
    if tool is None:
        return _skip("deb", version, arch, epoch, "dpkg-deb not found")
    sources = {source.name: source for source in _sources(binary, version)}
    debian = PACKAGING / "debian"
    with tempfile.TemporaryDirectory(prefix="spot-deb-") as tmp:
        root = Path(tmp) / "root"
        staged = [
            ("usr/bin/spot", sources["spot"].data, EXEC_MODE),
            ("usr/lib/systemd/system/spot.service", sources["spot.service"].data, FIXED_MODE),
            ("usr/lib/sysusers.d/spot.conf", sources["spot.sysusers"].data, FIXED_MODE),
            ("usr/lib/tmpfiles.d/spot.conf", sources["spot.tmpfiles"].data, FIXED_MODE),
            ("etc/spot/spot.toml", sources["spot.toml"].data, CONFIG_MODE),
            ("etc/spot/environment", sources["environment"].data, CONFIG_MODE),
            ("usr/share/doc/spot/copyright", _read(debian / "copyright"), FIXED_MODE),
            ("usr/share/doc/spot/README.install", sources["README.install"].data, FIXED_MODE),
        ]
        for name, data, mode in staged:
            _write(root / name, data, mode)
        changelog = _changelog(deb_version(version), "1", epoch).encode()
        _write(root / "usr/share/doc/spot/changelog.gz",
               gzip.compress(changelog, compresslevel=9, mtime=epoch), FIXED_MODE)

        control = deb_control(version, arch, _tree_size_kb(root)).encode()
        _write(root / "DEBIAN/control", control, FIXED_MODE)
        _write(root / "DEBIAN/conffiles", _read(debian / "conffiles"), FIXED_MODE)
        for script in ("postinst", "prerm", "postrm"):
            _write(root / "DEBIAN" / script, _read(debian / script), EXEC_MODE)
        _set_tree_times(root, epoch)
        filename = f"spot_{deb_version(version)}_{deb_arch(arch)}.deb"
        tmp_deb = Path(tmp) / filename
        env = dict(os.environ, SOURCE_DATE_EPOCH=str(epoch))
        run = subprocess.run(
            [tool, "--root-owner-group", "--build", str(root), str(tmp_deb)],
            capture_output=True, text=True, env=env,
        )
        if run.returncode != 0:
            sys.stderr.write(run.stdout)
            sys.stderr.write(run.stderr)
            raise SystemExit(f"dpkg-deb failed with exit {run.returncode}")
        target = _write(out / filename, _read(tmp_deb), FIXED_MODE)
    data = _read(target)
    return _summary("deb", version, arch, str(target), epoch,
                    {"deb_version": deb_version(version),
                     "sha256": hashlib.sha256(data).hexdigest()})


def build_container(binary: Path, version: str, arch: str, out: Path, epoch: int) -> dict:
    tool = shutil.which("podman") or shutil.which("buildah")
    if tool is None:
        return _skip("container", version, arch, epoch, "podman not found")
    with tempfile.TemporaryDirectory(prefix="spot-container-") as tmp:
        context = Path(tmp)
        _write(context / "spot", _read(binary), EXEC_MODE)
        _write(context / "spot.toml", _read(PACKAGING / "etc" / "spot.toml"), CONFIG_MODE)
        runner = "podman" if Path(tool).name == "podman" else "buildah"
        if runner == "podman":
            command = [
                tool, "build", "--format", "oci",
                "--build-arg", f"VERSION={version}",
                "--build-arg", f"SOURCE_DATE_EPOCH={epoch}",
                "-t", f"spot:{version}",
                "-f", str(PACKAGING / "container" / "Containerfile"),
                str(context),
            ]
        else:
            command = [
                tool, "bud", "--format", "oci",
                "--build-arg", f"VERSION={version}",
                "--build-arg", f"SOURCE_DATE_EPOCH={epoch}",
                "-t", f"spot:{version}",
                "-f", str(PACKAGING / "container" / "Containerfile"),
                str(context),
            ]
        env = dict(os.environ, SOURCE_DATE_EPOCH=str(epoch))
        run = subprocess.run(command, capture_output=True, text=True, env=env)
        if run.returncode != 0:
            sys.stderr.write(run.stdout)
            sys.stderr.write(run.stderr)
            raise SystemExit(f"{runner} build failed with exit {run.returncode}")
    image = f"spot:{version}"
    return _summary("container", version, arch, image, epoch, {"type": "image"})


def build(kind: str, binary: Path, version: str, out: Path, arch: str) -> dict:
    epoch = source_date_epoch()
    version = normalise_version(version)
    out.mkdir(parents=True, exist_ok=True)
    # Validate the version once, so every kind fails the same way.
    package_versions(version)
    if kind == "tarball":
        return build_tarball(binary, version, arch, out, epoch)
    if kind == "rpm":
        return build_rpm(binary, version, arch, out, epoch)
    if kind == "deb":
        return build_deb(binary, version, arch, out, epoch)
    if kind == "container":
        return build_container(binary, version, arch, out, epoch)
    raise SystemExit(f"unknown kind {kind!r}")


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("kind", choices=KINDS)
    parser.add_argument("--binary", required=True, type=Path, help="prebuilt spot binary")
    parser.add_argument("--version", required=True, help="release version, e.g. 1.2.3 or 1.2.3-rc.1")
    parser.add_argument("--out", type=Path, default=Path("dist"), help="output directory")
    parser.add_argument("--arch", default="x86_64", help="target architecture (default x86_64)")
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    binary = args.binary.expanduser().resolve()
    out = args.out.expanduser().resolve()
    summary = build(args.kind, binary, args.version, out, args.arch)
    print(json.dumps(summary, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
