#!/usr/bin/env python3
"""Say whether a `dotnet publish` log of the Browser head ran the WebAssembly AOT compile (WP-UI-09.T10).

    python3 tools/ci/browser/aot_evidence.py <publish.log> [--expect-aot]

An AOT publish runs the SDK targets that compile the assemblies to wasm (`_WasmAotCompileApp`,
`WasmAotCompileApp`, `PrepareForWasmBuildApp`'s AOT part, the `mono-aot-cross` compiler). The list below
is searched case-insensitively in the log; which names a given SDK prints is NOT measured yet (the first
real run fills in the project process notes), so the list is deliberately wide. Prints one line per
marker found and `aot-evidence: present|absent`. With --expect-aot exits 1 when none is found.
Pure standard library.
"""
from __future__ import annotations

import argparse
import sys
from pathlib import Path

MARKERS = (
    "_WasmAotCompileApp",
    "WasmAotCompileApp",
    "WasmAotCompile",
    "mono-aot-cross",
    "_WasmCompileAssemblies",
    "EmccCompile",
    "WasmBuildNative",
)
# Markers that also appear in an interpreted native relink; only the first four prove an AOT compile.
PROOF = MARKERS[:4]


def scan(text: str):
    lowered = text.lower()
    return [m for m in MARKERS if m.lower() in lowered]


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("log", type=Path)
    parser.add_argument("--expect-aot", action="store_true")
    args = parser.parse_args(argv)
    if not args.log.is_file():
        print(f"ERROR: log not found: {args.log.name}", file=sys.stderr)
        return 2
    found = scan(args.log.read_text(encoding="utf-8", errors="replace"))
    for marker in found:
        print(f"found: {marker}")
    proven = any(m in PROOF for m in found)
    print("aot-evidence: " + ("present" if proven else "absent"))
    if args.expect_aot and not proven:
        print("ERROR: AOT expected but the publish log shows no AOT compile target "
              "(see the AOT notes in the browser workflow)", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
