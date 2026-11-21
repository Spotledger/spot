#!/usr/bin/env python3
"""Generate the typed TypeScript surface from the native API contract.

WP-WEB-10.T6, ADR-092 section 10.2 task C4. The third contract generator beside
progenitor (Rust) and Kiota (C#), with no third-party generator: one script
that reads contracts/native-api/openapi.yaml and writes TypeScript source text.
The output is never committed.

    generate.py --out DIR      write types.ts, operations.ts, client.ts,
                               index.ts, contract.ts and report.txt into DIR
    generate.py --check        generate into a temporary directory, run the
                               exactness self-test (and tsc if available), write
                               nothing else; non-zero exit on any finding
"""

from __future__ import annotations

import argparse
import hashlib
import os
import shutil
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

import yaml  # noqa: E402

import selftest  # noqa: E402
from tsclient import emit_client  # noqa: E402
from tsemit import emit_contract, emit_index, emit_operations, emit_types  # noqa: E402
from tsmodel import collect  # noqa: E402
from tsreport import Report  # noqa: E402

REPO_ROOT = HERE.parents[2]
DEFAULT_CONTRACT = REPO_ROOT / "contracts" / "native-api" / "openapi.yaml"


def read_version(contract: Path) -> str:
    version_file = contract.parent / "VERSION"
    if not version_file.is_file():
        raise SystemExit(f"generate.py: {version_file} not found; the contract version must travel with it")
    return version_file.read_text(encoding="utf-8").strip()


def generate(contract: Path) -> tuple[dict[str, str], Report]:
    """Return ``{file name: text}`` and the report for one contract."""
    raw = contract.read_bytes()
    root = yaml.safe_load(raw.decode("utf-8"))
    version = read_version(contract)
    report = Report()
    ops = collect(root, report)
    files = {
        "types.ts": emit_types(root, version, report),
        "operations.ts": emit_operations(ops, version),
        "client.ts": emit_client(ops, version),
        "index.ts": emit_index(version),
        "contract.ts": emit_contract(version, hashlib.sha256(raw).hexdigest()),
    }
    report.counts = selftest.contract_counts(contract)
    report.counts["generated_methods"] = len(ops)
    files["report.txt"] = report.text()
    return files, report


def write(files: dict[str, str], out_dir: Path) -> None:
    out_dir.mkdir(parents=True, exist_ok=True)
    for name, text in files.items():
        (out_dir / name).write_text(text, encoding="utf-8", newline="\n")


def verify(contract: Path, out_dir: Path, report: Report) -> list[str]:
    findings = sorted(report.unrepresentable | report.findings)
    findings += selftest.run(contract, read_version(contract), out_dir, report.counts)
    tsc = selftest.find_tsc(dict(os.environ))
    findings += selftest.type_check(out_dir, tsc, os.environ.get("SPOT_REQUIRE_TSC") == "1")
    return findings


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--contract", type=Path, default=DEFAULT_CONTRACT)
    parser.add_argument("--out", type=Path, help="directory to write the generated files into")
    parser.add_argument("--check", action="store_true", help="generate to a temp dir and run the self-test")
    args = parser.parse_args(argv)
    if not args.out and not args.check:
        parser.error("give --out DIR or --check")

    files, report = generate(args.contract)
    sys.stderr.write(files["report.txt"])
    status = 0
    targets: list[Path] = []
    temp: Path | None = None
    if args.check:
        temp = selftest.scratch()
        targets.append(temp)
    if args.out:
        targets.append(args.out)
    try:
        for target in targets:
            write(files, target)
            findings = verify(args.contract, target, report)
            for finding in findings:
                sys.stderr.write(f"FAIL {finding}\n")
            if findings:
                status = 1
        print(f"generate.py: {report.counts['operations']} operations, {report.counts['schemas']} schemas, "
              f"{'FAILED' if status else 'ok'}")
    finally:
        if temp is not None:
            shutil.rmtree(temp, ignore_errors=True)
    return status


if __name__ == "__main__":
    raise SystemExit(main())
