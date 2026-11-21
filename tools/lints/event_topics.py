#!/usr/bin/env python3
"""The event-topics lint (WP-CON-04.T5, decision B5).

Every platform event topic (`doc_*`, `sys_*`, `identity_*`) that a crate defines as a topic
constant, or that the events contract lists, must be an entry of the event class registry
(`contracts/events/src/registry.rs`), and the entry's `owner` crate must contain the topic as a
string literal. Stdlib only. Run from the repository root: `python3 tools/lints/event_topics.py`.
"""
from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
REGISTRY_FILE = Path("contracts/events/src/registry.rs")
PLATFORM = re.compile(r"^(doc|sys|identity)_[a-z0-9_]+$")
ENTRY = re.compile(r'^\s*entry(_retired)?\(\s*"([a-z0-9_]+)",.*?,\s*"([a-z0-9-]+)",\s*"[0-9.]+"\s*\),?\s*$')
TOPIC_CONST = re.compile(r'\bconst\s+[A-Z0-9_]*TOPIC[A-Z0-9_]*\s*:\s*&(?:\'static\s+)?str\s*=\s*"([^"]+)"')
TOPIC_ANY = re.compile(r'Topic::any\(\s*"([^"]+)"\s*\)')
LITERAL = re.compile(r'"((?:doc|sys|identity)_[a-z0-9_]+)"')


def parse_registry(text: str) -> dict[str, tuple[str, bool]]:
    """topic -> (owner crate, retired), read from the one-line `entry(...)` rows."""
    out: dict[str, tuple[str, bool]] = {}
    for line in text.splitlines():
        m = ENTRY.match(line)
        if m:
            out[m.group(2)] = (m.group(3), bool(m.group(1)))
    return out


def crate_dirs(root: Path) -> dict[str, Path]:
    """crate name -> directory, from every Cargo.toml with a [package] name."""
    out: dict[str, Path] = {}
    for manifest in root.glob("**/Cargo.toml"):
        if any(p in ("target", ".git", "node_modules") for p in manifest.parts):
            continue
        m = re.search(r'^\s*name\s*=\s*"([^"]+)"', _package_section(manifest.read_text("utf-8")), re.M)
        if m:
            out[m.group(1)] = manifest.parent
    return out


def _package_section(text: str) -> str:
    m = re.search(r"^\[package\]\s*$(.*?)(?=^\[|\Z)", text, re.M | re.S)
    return m.group(1) if m else ""


def is_test_file(path: Path) -> bool:
    return "tests" in path.parts or path.name in ("tests.rs", "registry_tests.rs") or path.name.endswith("_tests.rs")


def strip_comments(line: str) -> str:
    stripped = line.lstrip()
    return "" if stripped.startswith("//") else line


def found_topics(root: Path) -> dict[str, list[str]]:
    """Platform topic -> the files that define it as a constant or list it in the contract."""
    out: dict[str, list[str]] = {}
    registry = root / REGISTRY_FILE
    contract_src = root / "contracts" / "events" / "src"
    for path in sorted((root / "crates").glob("**/*.rs")) + sorted(contract_src.glob("*.rs")):
        if is_test_file(path.relative_to(root)) or path == registry:
            continue
        in_contract = contract_src in path.parents
        rel = str(path.relative_to(root))
        for raw in path.read_text("utf-8", errors="replace").splitlines():
            line = strip_comments(raw)
            if not line:
                continue
            names = TOPIC_CONST.findall(line) + TOPIC_ANY.findall(line)
            if in_contract:
                names += LITERAL.findall(line)
            for name in names:
                if PLATFORM.match(name):
                    out.setdefault(name, [])
                    if rel not in out[name]:
                        out[name].append(rel)
    return out


def check(root: Path) -> list[str]:
    registry_path = root / REGISTRY_FILE
    if not registry_path.is_file():
        return [f"{REGISTRY_FILE} is missing"]
    registry = parse_registry(registry_path.read_text("utf-8"))
    problems: list[str] = []
    for topic, files in found_topics(root).items():
        if topic not in registry:
            problems.append(f"{topic}: defined in {', '.join(files)} but not in the registry ({REGISTRY_FILE})")
    crates = crate_dirs(root)
    cache: dict[str, str] = {}
    for topic, (owner, _retired) in sorted(registry.items()):
        directory = crates.get(owner)
        if directory is None:
            problems.append(f"{topic}: owner crate `{owner}` does not exist")
            continue
        if owner not in cache:
            cache[owner] = "\n".join(
                p.read_text("utf-8", errors="replace") for p in sorted(directory.glob("**/*.rs"))
                if "target" not in p.parts
            )
        if f'"{topic}"' not in cache[owner]:
            problems.append(f"{topic}: owner crate `{owner}` does not contain the topic")
    return problems


def main(argv: list[str]) -> int:
    root = Path(argv[1]).resolve() if len(argv) > 1 else ROOT
    problems = check(root)
    for p in problems:
        print(f"event-topics: {p}", file=sys.stderr)
    if problems:
        return 1
    print("event-topics: ok")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
