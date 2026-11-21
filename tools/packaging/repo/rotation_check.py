#!/usr/bin/env python3
"""Prove the dual-key rotation window with throwaway keys.

Builds a DEB repository signed with an old key and another signed with a new key,
then checks what clients with different pinned keyrings accept:

    client trusting old only   accepts old-signed, REJECTS new-signed
    client trusting new only   REJECTS old-signed, accepts new-signed
    client trusting both       accepts both       (the rotation window)

Exit 0 when exactly that holds. The keys never leave a temporary directory.
"""

from __future__ import annotations

import os
import subprocess
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import debrepo  # noqa: E402
import signing  # noqa: E402

FIXED_EPOCH = 1767225600


def throwaway_key(home: Path, uid: str) -> bytes:
    """Generate a passphrase-less key in ``home``; return the armoured private key."""
    gpg = signing.need("gpg")
    os.chmod(home, 0o700)
    base = [gpg, "--homedir", str(home), "--batch", "--no-tty", "--pinentry-mode", "loopback"]
    subprocess.run(base + ["--passphrase", "", "--quick-generate-key", uid, "ed25519", "sign", "1d"],
                   capture_output=True, check=True, timeout=120)
    out = subprocess.run(base + ["--passphrase", "", "--armor", "--export-secret-keys"],
                         capture_output=True, check=True, timeout=120).stdout
    subprocess.run([signing.need("gpgconf"), "--homedir", str(home), "--kill", "all"],
                   capture_output=True, check=False, timeout=30)
    return out


def make_deb(path: Path, name: str, version: str) -> Path:
    """A minimal valid .deb (ar + control.tar.gz + data.tar.gz), enough for the repository tools."""
    import gzip, io, tarfile

    def tar_gz(files: dict[str, bytes]) -> bytes:
        buf = io.BytesIO()
        with tarfile.open(fileobj=buf, mode="w") as tar:
            for fname, data in files.items():
                info = tarfile.TarInfo(fname)
                info.size = len(data)
                info.mtime = 0
                tar.addfile(info, io.BytesIO(data))
        return gzip.compress(buf.getvalue(), mtime=0)

    control = (f"Package: {name}\nVersion: {version}\nArchitecture: amd64\nMaintainer: T <t@invalid>\n"
               "Description: test package\n").encode()
    members = [("debian-binary", b"2.0\n"), ("control.tar.gz", tar_gz({"./control": control})),
               ("data.tar.gz", tar_gz({}))]
    blob = bytearray(debrepo.AR_MAGIC)
    for mname, data in members:
        blob += f"{mname + '/':<16}{0:<12}{0:<6}{0:<6}{'100644':<8}{len(data):<10}`\n".encode("ascii")
        blob += data + (b"\n" if len(data) & 1 else b"")
    path.write_bytes(bytes(blob))
    return path


def run() -> int:
    with tempfile.TemporaryDirectory(prefix="spot-rotation-") as tmp:
        tmp = Path(tmp)
        homes = {}
        secrets = {}
        for label in ("old", "new"):
            homes[label] = tmp / f"gnupg-{label}"
            homes[label].mkdir()
            secrets[label] = throwaway_key(homes[label], f"Spot rotation {label} <{label}@invalid>")
        deb = make_deb(tmp / "spot-rot_1.0_amd64.deb", "spot-rot", "1.0")
        publics = {}
        roots = {}
        for label in ("old", "new"):
            with signing.Signer.from_env({signing.KEY_ENV: secrets[label].decode()}) as signer:
                publics[label] = signer.public_binary()
                roots[label] = debrepo.build_repo([deb], tmp / label, "stable", signer, FIXED_EPOCH)
        rings = {
            "old only": signing.make_keyring([publics["old"]], tmp / "r-old.gpg"),
            "new only": signing.make_keyring([publics["new"]], tmp / "r-new.gpg"),
            "both": signing.make_keyring([publics["old"], publics["new"]], tmp / "r-both.gpg"),
        }
        expected = {("old only", "old"): True, ("old only", "new"): False,
                    ("new only", "old"): False, ("new only", "new"): True,
                    ("both", "old"): True, ("both", "new"): True}
        failed = 0
        for (client, repo), want in expected.items():
            try:
                debrepo.verify_repo(roots[repo], "stable", rings[client])
                got = True
            except debrepo.RepoError:
                got = False
            status = "ok" if got == want else "FAIL"
            failed += got != want
            print(f"{status}: client trusting {client} {'accepts' if got else 'rejects'} the {repo}-signed repository")
        return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(run())
