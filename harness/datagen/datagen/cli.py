"""Command line for the dataset generator and verifier.

datagen.py --list-profiles
datagen.py --profile tiny --seed 42 --out DIR [--format jsonl|csv]
datagen.py --verify DIR

Generation refuses to touch a non-empty directory; verification reports
every violation on stderr and exits non-zero if any was found.
"""

import argparse
import sys
from pathlib import Path

from .generate import GeneratorError, generate_dataset
from .profile import ProfileError, load_profile, shipped_profiles
from .verify import VerifyError, verify_dataset


def main(argv=None):
    """Run one command; return a process exit code (argparse raises SystemExit)."""
    parser = argparse.ArgumentParser(
        prog="datagen",
        description="Generate and verify spot ledger sample datasets.",
    )
    exclusive = parser.add_mutually_exclusive_group()
    exclusive.add_argument(
        "--list-profiles",
        action="store_true",
        help="print the shipped profile names, one per line, and exit",
    )
    exclusive.add_argument(
        "--verify",
        metavar="DIR",
        help="verify a generated dataset; exit non-zero on any violation",
    )
    parser.add_argument("--profile", help="a shipped profile name or a TOML path")
    parser.add_argument("--seed", type=int, help="integer seed for reproducible data")
    parser.add_argument("--out", help="empty directory to write the dataset into")
    parser.add_argument(
        "--format",
        choices=("jsonl", "csv"),
        default="jsonl",
        help="storage format to write (default: jsonl)",
    )
    args = parser.parse_args(argv)
    if args.list_profiles:
        for path in shipped_profiles():
            print(path.stem)
        return 0
    if args.verify:
        return _run_verify(args.verify)
    return _run_generate(args, parser)


def _run_verify(directory) -> int:
    """Verify one dataset; every content violation goes to stderr."""
    try:
        errors = verify_dataset(directory)
    except VerifyError as exc:
        print(f"verify: FAILED: {exc}", file=sys.stderr)
        return 1
    if errors:
        print(f"verify: FAILED: {directory}", file=sys.stderr)
        for error in errors:
            print(f"  {error}", file=sys.stderr)
        return 1
    print(f"verify: OK: {directory}")
    return 0


def _run_generate(args, parser) -> int:
    """Generate one dataset; profile, seed and out are all required."""
    if args.profile is None or args.seed is None or args.out is None:
        parser.error("generation needs --profile, --seed and --out")
    try:
        profile = load_profile(args.profile)
        summary = generate_dataset(profile, args.seed, args.out, mode=args.format)
    except (ProfileError, GeneratorError, VerifyError, OSError) as exc:
        print(f"generate: FAILED: {exc}", file=sys.stderr)
        return 1
    rows = summary["rows"]
    total = 0
    for name, entry in sorted(rows.items()):
        count = entry["rows"]
        print(f"{name}: {count} rows")
        total += count
    print(f"manifest: {Path(args.out) / 'manifest.json'}")
    print(f"written: {total} rows in {summary['seconds']} s")
    return 0