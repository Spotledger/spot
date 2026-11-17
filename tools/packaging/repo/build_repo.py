#!/usr/bin/env python3
"""Assemble and sign the Linux package repositories.

    SPOT_GPG_KEY="$(cat key.asc)" [SPOT_GPG_PASSPHRASE_FILE=pass.txt] \\
      python3 tools/packaging/repo/build_repo.py all --channel stable \\
          --rpm dist/*.rpm --deb dist/*.deb --out repo

The private key is read from the environment only. The output tree contains the
public key (``spot-release.asc``) and nothing secret; the tree is scanned before
the command exits. A missing tool is an error (exit 3), never a silent skip, so a
publish step can not push an unsigned repository.
"""

from __future__ import annotations

import argparse
import os
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import debrepo  # noqa: E402
import rpmrepo  # noqa: E402
import signing  # noqa: E402


def assert_no_private_material(tree: Path) -> None:
    """Fail if any file under ``tree`` holds an OpenPGP secret key."""
    for path in sorted(tree.rglob("*")):
        if path.is_file() and signing.has_secret_material(path):
            raise signing.SigningError(f"private key material in the output tree: {path.relative_to(tree)}")
        if path.is_dir() and path.name in {"private-keys-v1.d", ".gnupg"}:
            raise signing.SigningError(f"keyring directory in the output tree: {path.relative_to(tree)}")


def epoch_from_env() -> int:
    raw = os.environ.get("SOURCE_DATE_EPOCH", "").strip()
    if not raw:
        raise SystemExit("SOURCE_DATE_EPOCH is not set: the Release date is an argument, never the clock")
    try:
        value = int(raw, 10)
    except ValueError as exc:
        raise SystemExit("SOURCE_DATE_EPOCH is not an integer") from exc
    if value < 0:
        raise SystemExit("SOURCE_DATE_EPOCH must not be negative")
    return value


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("kind", choices=("rpm", "deb", "all"))
    parser.add_argument("--channel", required=True, choices=debrepo.CHANNELS)
    parser.add_argument("--rpm", nargs="*", type=Path, default=[])
    parser.add_argument("--deb", nargs="*", type=Path, default=[])
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--valid-days", type=int, default=None, help="Valid-Until for the DEB Release")
    args = parser.parse_args(argv)
    try:
        with signing.Signer.from_env() as signer:
            args.out.mkdir(parents=True, exist_ok=True)
            if args.kind in ("rpm", "all"):
                rpmrepo.build_repo(args.rpm, args.out, args.channel, signer)
            if args.kind in ("deb", "all"):
                debrepo.build_repo(args.deb, args.out, args.channel, signer, epoch_from_env(),
                                   valid_days=args.valid_days)
            (args.out / "spot-release.asc").write_bytes(signer.public_armored())
        assert_no_private_material(args.out)
    except signing.ToolMissing as exc:
        print(f"ERROR required tool missing: {exc}", file=sys.stderr)
        return 3
    except (signing.SigningError, debrepo.RepoError, rpmrepo.RepoError) as exc:
        print(f"ERROR {exc}", file=sys.stderr)
        return 1
    print(f"repository written to {args.out}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
