#!/usr/bin/env python3
"""Build ``spot-repo-keys`` (RPM and DEB): the public keys and the repository configuration.

    python3 tools/packaging/repo/build_keys_package.py rpm --key new.asc --key old.asc \\
        --version 1.0.0 --base-url https://packages.example --out dist

Only public keys are accepted (a secret key makes the build fail). Several ``--key``
options are the dual-key rotation window: the configuration trusts every key listed.
The ``stable`` channel is enabled, ``beta`` is present and disabled. The shipped
configuration has every signature check on; nothing here can turn one off.
"""

from __future__ import annotations

import argparse
import gzip
import os
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[2]
sys.path.insert(0, str(HERE))
sys.path.insert(0, str(REPO / "tools" / "packaging"))

import build as host  # noqa: E402
import signing  # noqa: E402

CONF = REPO / "packaging" / "linux" / "repo"
CHANNELS = ("stable", "beta")
DEFAULT_BASE_URL = "https://packages.spotledger.example"
NAME = "spot-repo-keys"
KEYRING = "/usr/share/keyrings/spot-archive-keyring.gpg"


def key_basename(public_key: bytes) -> str:
    return "RPM-GPG-KEY-spot-" + signing.key_fingerprint(public_key)[-16:].lower()


def check_public(public_key: bytes, label: str) -> None:
    if b"PRIVATE KEY BLOCK" in public_key:
        raise SystemExit(f"{label}: a private key was given where a public key is required")


def render_rpm_repo(base_url: str, key_names: list[str]) -> str:
    template = (CONF / "rpm" / "spot.repo.in").read_text(encoding="utf-8")
    keys = " ".join(f"file:///etc/pki/rpm-gpg/{n}" for n in key_names)
    sections = []
    for channel in CHANNELS:
        text = (template.replace("@CHANNEL@", channel).replace("@BASEURL@", base_url.rstrip("/"))
                .replace("@GPGKEYS@", keys).replace("@ENABLED@", "1" if channel == "stable" else "0"))
        sections.append(text)
    return sections[0] + "\n" + "\n".join(
        "\n".join(l for l in s.splitlines() if not l.startswith("#")) + "\n" for s in sections[1:])


def render_deb_sources(base_url: str) -> str:
    template = (CONF / "deb" / "spot.sources.in").read_text(encoding="utf-8")
    sections = []
    for channel in CHANNELS:
        text = (template.replace("@CHANNEL@", channel).replace("@BASEURL@", base_url.rstrip("/"))
                .replace("@ENABLED@", "yes" if channel == "stable" else "no"))
        if sections:
            text = "\n".join(l for l in text.splitlines() if not l.startswith("#")) + "\n"
        sections.append(text)
    return "\n".join(sections)


def render_spec(version: str, epoch: int, key_names: list[str]) -> tuple[str, str, str]:
    host.require_rpm_changelog_epoch(epoch)
    rpm_version, rpm_release = host.rpm_versions(version)
    template = (CONF / "rpm" / "spot-repo-keys.spec").read_text(encoding="utf-8")
    sources = "\n".join(f"Source{i + 1}:        {n}" for i, n in enumerate(key_names))
    install = "\n".join(f"install -m 0644 %{{SOURCE{i + 1}}} %{{buildroot}}%{{_sysconfdir}}/pki/rpm-gpg/{n}"
                        for i, n in enumerate(key_names))
    files = "\n".join(f"%{{_sysconfdir}}/pki/rpm-gpg/{n}" for n in key_names)
    changelog = (f"* {host.changelog_date(epoch)} Spotledger Packaging <packaging@spotledger.invalid>"
                 f" - {rpm_version}-{rpm_release}\n- Repository keys and configuration.\n")
    text = (template.replace("@VERSION@", rpm_version).replace("@RELEASE@", rpm_release)
            .replace("@KEYSOURCES@", sources).replace("@KEYINSTALL@", install)
            .replace("@KEYFILES@", files).replace("@CHANGELOG@", changelog))
    return text, rpm_version, rpm_release


def build_rpm(keys: list[bytes], version: str, base_url: str, out: Path, epoch: int) -> Path:
    tool = shutil.which("rpmbuild")
    if tool is None:
        raise signing.ToolMissing("rpmbuild not found")
    names = [key_basename(k) for k in keys]
    spec, _, _ = render_spec(version, epoch, names)
    with tempfile.TemporaryDirectory(prefix="spot-keys-rpm-") as tmp:
        top = Path(tmp) / "rpmbuild"
        for d in ("BUILD", "BUILDROOT", "RPMS", "SOURCES", "SPECS", "SRPMS"):
            (top / d).mkdir(parents=True)
        host._write(top / "SOURCES" / "spot.repo", render_rpm_repo(base_url, names).encode(), 0o644)
        for name, key in zip(names, keys):
            host._write(top / "SOURCES" / name, key, 0o644)
        spec_path = host._write(top / "SPECS" / "spot-repo-keys.spec", spec.encode(), 0o644)
        env = dict(os.environ, SOURCE_DATE_EPOCH=str(epoch))
        run = subprocess.run([tool, "-bb", "--define", f"_topdir {top}", "--define", f"_sourcedir {top / 'SOURCES'}",
                              "--define", "_buildhost reproducible", "--define", "use_source_date_epoch_as_buildtime 1",
                              str(spec_path)], capture_output=True, text=True, env=env, check=False)
        if run.returncode != 0:
            raise SystemExit(f"rpmbuild failed with exit {run.returncode}")
        rpms = sorted((top / "RPMS").rglob("*.rpm"))
        out.mkdir(parents=True, exist_ok=True)
        return host._write(out / rpms[-1].name, rpms[-1].read_bytes(), 0o644)


def build_deb(keys: list[bytes], version: str, base_url: str, out: Path, epoch: int) -> Path:
    tool = shutil.which("dpkg-deb")
    if tool is None:
        raise signing.ToolMissing("dpkg-deb not found")
    with tempfile.TemporaryDirectory(prefix="spot-keys-deb-") as tmp:
        root = Path(tmp) / "root"
        ring = signing.make_keyring(keys, Path(tmp) / "ring.gpg")
        host._write(root / KEYRING.lstrip("/"), ring.read_bytes(), 0o644)
        host._write(root / "etc/apt/sources.list.d/spot.sources", render_deb_sources(base_url).encode(), 0o644)
        host._write(root / "usr/share/doc/spot-repo-keys/changelog.gz",
                    gzip.compress(b"spot-repo-keys: repository keys and configuration.\n", mtime=epoch), 0o644)
        host._write(root / "usr/share/doc/spot-repo-keys/copyright",
                    b"Files: *\nCopyright: Spotledger contributors\nLicense: Apache-2.0\n", 0o644)
        control = (
            f"Package: {NAME}\nVersion: {host.deb_version(version)}\nArchitecture: all\n"
            "Maintainer: Spotledger Packaging <packaging@spotledger.invalid>\n"
            f"Installed-Size: {host._tree_size_kb(root)}\nSection: admin\nPriority: optional\n"
            "Description: Spotledger package repository keys and configuration\n"
            " The public archive keys and the apt source that pins them with Signed-By.\n")
        host._write(root / "DEBIAN/control", control.encode(), 0o644)
        host._write(root / "DEBIAN/conffiles", b"/etc/apt/sources.list.d/spot.sources\n", 0o644)
        host._set_tree_times(root, epoch)
        name = f"{NAME}_{host.deb_version(version)}_all.deb"
        target = Path(tmp) / name
        run = subprocess.run([tool, "--root-owner-group", "--build", str(root), str(target)],
                             capture_output=True, text=True, check=False,
                             env=dict(os.environ, SOURCE_DATE_EPOCH=str(epoch)))
        if run.returncode != 0:
            raise SystemExit(f"dpkg-deb failed with exit {run.returncode}")
        out.mkdir(parents=True, exist_ok=True)
        return host._write(out / name, target.read_bytes(), 0o644)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("kind", choices=("rpm", "deb"))
    parser.add_argument("--key", action="append", type=Path, required=True, help="public key (armoured), repeatable")
    parser.add_argument("--version", required=True)
    parser.add_argument("--base-url", default=DEFAULT_BASE_URL)
    parser.add_argument("--out", type=Path, default=Path("dist"))
    args = parser.parse_args(argv)
    keys = [p.read_bytes() for p in args.key]
    for path, key in zip(args.key, keys):
        check_public(key, path.name)
    if not re.fullmatch(r"https://[A-Za-z0-9.-]+(:[0-9]+)?(/[A-Za-z0-9._/-]*)?", args.base_url):
        raise SystemExit("--base-url must be an https:// URL")
    epoch = host.source_date_epoch()
    try:
        build = build_rpm if args.kind == "rpm" else build_deb
        print(build(keys, args.version, args.base_url, args.out, epoch))
    except signing.ToolMissing as exc:
        print(f"ERROR required tool missing: {exc}", file=sys.stderr)
        return 3
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
