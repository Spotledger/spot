#!/usr/bin/env python3
"""List the tests the nightly's --ignored pass must skip: the documented gaps.

A test marked `#[ignore = "gap: ..."]` documents behaviour that does not exist yet (or a harness
helper that is missing). It can never pass, so a nightly that runs every ignored test would turn
red on it. This script greps the Rust sources for those attributes and prints one `--skip <name>`
pair per test (libtest's `--skip` matches by substring), in sorted order, one per line.

Usage: ignored_gap_skips.py [ROOT ...]   (default roots: crates harness tools)
"""
import re
import sys
from pathlib import Path

ATTR = re.compile(r'^\s*#\[ignore\s*=\s*"gap:')
FN = re.compile(r"\bfn\s+([A-Za-z_][A-Za-z0-9_]*)")


def gap_tests(text):
    """Names of the functions that follow a `#[ignore = "gap: ..."]` attribute."""
    names = []
    lines = text.splitlines()
    for i, line in enumerate(lines):
        if not ATTR.match(line):
            continue
        for follow in lines[i + 1 :]:
            m = FN.search(follow)
            if m:
                names.append(m.group(1))
                break
    return names


def collect(roots):
    names = set()
    for root in roots:
        for path in Path(root).rglob("*.rs"):
            if "target" in path.parts:
                continue
            try:
                names.update(gap_tests(path.read_text(encoding="utf-8")))
            except (OSError, UnicodeDecodeError):
                continue
    return sorted(names)


def main(argv):
    roots = argv[1:] or ["crates", "harness", "tools"]
    for name in collect(roots):
        print("--skip")
        print(name)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
