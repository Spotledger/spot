"""GPG signing and verification for the Linux repositories.

Keys come from the environment, never from the repository:

* ``SPOT_GPG_KEY``: the ASCII-armoured private key (a CI secret).
* ``SPOT_GPG_PASSPHRASE_FILE``: path to a file holding the passphrase (optional).

The key is imported into a throwaway GNUPGHOME that is removed (and its agent
killed) when the :class:`Signer` is closed. The key text is passed to ``gpg`` on
stdin, never as an argument, and no message in this module contains key or
passphrase material.

Verification is done with ``gpgv`` against a keyring made only from public keys,
the way a client's package manager does it. Standard library only.
"""

from __future__ import annotations

import os
import shutil
import subprocess
import tempfile
from pathlib import Path
from typing import Iterable, Mapping

KEY_ENV = "SPOT_GPG_KEY"
PASSPHRASE_ENV = "SPOT_GPG_PASSPHRASE_FILE"
DIGEST = "SHA256"


class SigningError(Exception):
    """A signing or verification step failed. Never carries key or passphrase text."""


class ToolMissing(SigningError):
    """A required external tool is not installed."""


def need(tool: str) -> str:
    path = shutil.which(tool)
    if path is None:
        raise ToolMissing(f"{tool} not found")
    return path


def _run(cmd: list[str], *, stdin: bytes | None = None, check: bool = True) -> subprocess.CompletedProcess:
    run = subprocess.run(cmd, input=stdin, capture_output=True, check=False, timeout=120)
    if check and run.returncode != 0:
        # stderr of gpg never contains key material; it is cut to the first line anyway.
        first = run.stderr.decode("utf-8", "replace").strip().splitlines()[:1]
        raise SigningError(f"{Path(cmd[0]).name} failed ({run.returncode}): {first[0] if first else ''}")
    return run


class Signer:
    """A private key imported into a private, temporary GNUPGHOME."""

    def __init__(self, passphrase_file: Path | None = None) -> None:
        self._gpg = need("gpg")
        self._tmp = tempfile.mkdtemp(prefix="spot-gpg-")
        os.chmod(self._tmp, 0o700)
        self.home = Path(self._tmp)
        self.passphrase_file = passphrase_file
        self.fingerprint = ""

    # -- construction ---------------------------------------------------------

    @classmethod
    def from_env(cls, env: Mapping[str, str] | None = None) -> "Signer":
        env = os.environ if env is None else env
        key = env.get(KEY_ENV, "")
        if not key.strip():
            raise SigningError(f"{KEY_ENV} is not set: the signing key comes from the environment, never from the repository")
        passphrase = env.get(PASSPHRASE_ENV, "")
        signer = cls(Path(passphrase) if passphrase else None)
        try:
            signer.import_secret(key.encode("utf-8"))
        except BaseException:
            signer.close()
            raise
        return signer

    def import_secret(self, armored: bytes) -> None:
        if self.passphrase_file is not None and not self.passphrase_file.is_file():
            raise SigningError("the passphrase file does not exist")
        _run(self._base() + ["--import"], stdin=armored)
        out = _run(self._base() + ["--list-secret-keys", "--with-colons", "--fingerprint"]).stdout.decode()
        fprs = [l.split(":")[9] for l in out.splitlines() if l.startswith("fpr:")]
        if not fprs:
            raise SigningError("the imported material contains no secret key")
        self.fingerprint = fprs[0]

    def close(self) -> None:
        gpgconf = shutil.which("gpgconf")
        if gpgconf:
            subprocess.run([gpgconf, "--homedir", str(self.home), "--kill", "all"],
                           capture_output=True, check=False, timeout=30)
        shutil.rmtree(self._tmp, ignore_errors=True)

    def __enter__(self) -> "Signer":
        return self

    def __exit__(self, *exc: object) -> None:
        self.close()

    # -- operations -----------------------------------------------------------

    def _base(self) -> list[str]:
        cmd = [self._gpg, "--homedir", str(self.home), "--batch", "--no-tty", "--yes",
               "--pinentry-mode", "loopback", "--digest-algo", DIGEST]
        if self.passphrase_file is not None:
            cmd += ["--passphrase-file", str(self.passphrase_file)]
        return cmd

    def _key_args(self) -> list[str]:
        return ["--local-user", self.fingerprint]

    def detached(self, path: Path, out: Path, armor: bool = True) -> Path:
        """Detached signature of ``path`` written to ``out`` (armoured: ``.asc``; binary: ``.gpg``)."""
        cmd = self._base() + self._key_args() + ["--detach-sign"] + (["--armor"] if armor else [])
        _run(cmd + ["--output", str(out), str(path)])
        return out

    def clearsign(self, path: Path, out: Path) -> Path:
        """Clear-signed copy of ``path`` (an apt ``InRelease``)."""
        _run(self._base() + self._key_args() + ["--clearsign", "--output", str(out), str(path)])
        return out

    def public_armored(self) -> bytes:
        return _run(self._base() + ["--armor", "--export", self.fingerprint]).stdout

    def public_binary(self) -> bytes:
        return _run(self._base() + ["--export", self.fingerprint]).stdout


def make_keyring(public_keys: Iterable[bytes], dest: Path) -> Path:
    """A binary keyring file holding only these public keys (what a client trusts)."""
    gpg = need("gpg")
    with tempfile.TemporaryDirectory(prefix="spot-keyring-") as tmp:
        os.chmod(tmp, 0o700)
        base = [gpg, "--homedir", tmp, "--batch", "--no-tty"]
        for key in public_keys:
            _run(base + ["--import"], stdin=key)
        data = _run(base + ["--export"]).stdout
        gpgconf = shutil.which("gpgconf")
        if gpgconf:
            subprocess.run([gpgconf, "--homedir", tmp, "--kill", "all"], capture_output=True, check=False, timeout=30)
    if not data:
        raise SigningError("no public key in the keyring")
    dest.write_bytes(data)
    return dest


def verify(keyring: Path, signed: Path, data: Path | None = None) -> bool:
    """True when ``signed`` (clear-signed file, or a detached signature of ``data``) is valid for ``keyring``.

    ``gpgv`` accepts only keys in the given keyring, like a package manager with a pinned key.
    """
    gpgv = need("gpgv")
    cmd = [gpgv, "--keyring", str(keyring)]
    cmd += [str(signed)] + ([str(data)] if data is not None else [])
    return _run(cmd, check=False).returncode == 0


def key_fingerprint(public_key: bytes) -> str:
    """The primary fingerprint of an armoured or binary public key."""
    gpg = need("gpg")
    with tempfile.TemporaryDirectory(prefix="spot-fpr-") as tmp:
        os.chmod(tmp, 0o700)
        out = _run([gpg, "--homedir", tmp, "--batch", "--no-tty", "--with-colons", "--show-keys", "--fingerprint"],
                   stdin=public_key).stdout.decode()
    fprs = [l.split(":")[9] for l in out.splitlines() if l.startswith("fpr:")]
    if not fprs:
        raise SigningError("no key found")
    return fprs[0]


def has_secret_material(path: Path) -> bool:
    """True when ``path`` holds an OpenPGP secret key (armoured or binary)."""
    data = path.read_bytes()
    if b"PRIVATE KEY BLOCK" in data:
        return True
    if path.suffix.lower() not in {".gpg", ".pgp", ".key", ".kbx"}:
        return False
    gpg = shutil.which("gpg")
    if gpg is None:
        return False
    run = subprocess.run([gpg, "--batch", "--no-tty", "--list-packets", str(path)],
                         capture_output=True, check=False, timeout=60)
    return b"secret key packet" in run.stdout or b"secret sub key packet" in run.stdout
