#!/usr/bin/env python3
"""Client conformance runner (WP-WEB-10.T1, ADR-088 section 9, ADR-092 section 10.2 C1).

    python3 tools/client-conformance/run.py --leg rust      # corpus through spot-client-core, both feature sets
    python3 tools/client-conformance/run.py --leg csharp    # NOT RUN (exit 2) until the C# client exists
    python3 tools/client-conformance/run.py --check         # corpus files are well formed, no run

Exit status: 0 every case passed, 1 a case failed or the corpus is malformed, 2 the leg was not run.
The C# leg never reports success on its own: it stays "NOT RUN" with exit 2 until a runner exists,
so the gate is explicit.
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
CORPUS = HERE / "corpus"
# Features a corpus file may require (top-level "requires"). A runner without the feature
# skips the file and says how many it skipped; a file that requires one is run with it on.
KNOWN_FEATURES = {"storage"}


def check_corpus() -> list[str]:
    """Returns the problems found in the corpus files (empty when well formed)."""
    problems: list[str] = []
    files = sorted(CORPUS.glob("*.json"))
    if not files:
        return [f"{CORPUS}: no corpus files"]
    for path in files:
        try:
            doc = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, ValueError) as err:
            problems.append(f"{path.name}: {err}")
            continue
        if not isinstance(doc.get("topic"), str) or not isinstance(doc.get("cases"), list) or not doc["cases"]:
            problems.append(f"{path.name}: needs a string 'topic' and a non-empty 'cases' list")
            continue
        requires = doc.get("requires", [])
        if not isinstance(requires, list) or any(r not in KNOWN_FEATURES for r in requires):
            problems.append(f"{path.name}: 'requires' must be a list of {sorted(KNOWN_FEATURES)}")
            continue
        for case in doc["cases"]:
            name = case.get("name") if isinstance(case, dict) else None
            steps = case.get("steps") if isinstance(case, dict) else None
            if not isinstance(name, str) or not isinstance(steps, list) or not steps:
                problems.append(f"{path.name}: a case needs a 'name' and non-empty 'steps'")
                continue
            start = case.get("start")
            if start is not None and (
                not isinstance(start, dict) or not isinstance(start.get("site"), str) or not isinstance(start.get("out"), list)
            ):
                problems.append(f"{path.name}: case '{name}' 'start' needs a string 'site' and a list 'out'")
            for n, step in enumerate(steps):
                if not isinstance(step, dict) or "in" not in step or not isinstance(step.get("out"), list):
                    problems.append(f"{path.name}: case '{name}' step {n} needs 'in' and a list 'out'")
    return problems


def run_rust() -> int:
    """Runs the corpus under both feature sets; fails when either fails."""
    env = dict(os.environ)
    base = ["cargo", "test", "-p", "spot-client-core"]
    runs = [
        base + ["--test", "corpus", "--", "--nocapture"],
        base + ["--features", "storage", "--test", "corpus_storage", "--", "--nocapture"],
    ]
    status = 0
    for command in runs:
        code = subprocess.run(command, cwd=ROOT, env=env, check=False).returncode
        status = status or code
    return status


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--leg", choices=["rust", "csharp"])
    parser.add_argument("--check", action="store_true", help="validate the corpus files only")
    args = parser.parse_args()
    if not args.leg and not args.check:
        parser.error("give --leg or --check")
    problems = check_corpus()
    for problem in problems:
        print(f"corpus: {problem}", file=sys.stderr)
    if problems:
        return 1
    if args.check and not args.leg:
        print("corpus well formed")
        return 0
    if args.leg == "csharp":
        print("NOT RUN: the C# client leg does not exist yet (ADR-088 section 8.3 gate); this is not a pass.")
        return 2
    return run_rust()


if __name__ == "__main__":
    sys.exit(main())
