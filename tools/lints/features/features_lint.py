#!/usr/bin/env python3
"""Keep the `testing` feature of the framework crates out of product builds.

A framework crate may expose a `testing` feature (fakes, helpers, `SecretCaller::Test`). It is for
tests: a dev-dependency or a self dev-dependency enables it, never a normal or build dependency,
and never a product binary. The gate fails when

1. a non-dev dependency edge of a workspace package enables `testing` on a framework crate, by
   naming the feature, by a feature that turns it on, or through that crate's default features
   (an edge listed in `rules.toml` is existing debt and passes; a listed edge that is gone fails,
   so the list can only shrink); or
2. the normal and build dependency closure of a product binary has `testing` enabled on a
   framework crate, however it got there (the check reads the features cargo resolves for that
   build, so it also catches a chain of forwarded features);
3. the product binaries, built together as one release build, unify `testing` onto a framework
   crate that one of them uses: cargo resolves one feature set per crate for the whole build, so a
   neighbour binary can switch the feature on for a binary that never asked for it; or
4. `non_product` or `[[allow_edge]]` of `rules.toml` grew compared with the committed
   `baseline.toml` (a bypass must show up as a visible, lead-reviewed baseline change), or an
   entry of `non_product` has no reason.

Fail closed: a manifest, a tree listing or a baseline that cannot be read is an error (exit 2) or
a finding (exit 1), never a pass.

Exit codes: 0 clean, 1 finding, 2 the input could not be read (no cargo, no workspace).
"""

from __future__ import annotations

import json
import re
import subprocess
import sys
import tomllib
from pathlib import Path
from typing import Callable

HERE = Path(__file__).resolve().parent


def load_rules(path: Path | None = None) -> dict:
    try:
        with (path or HERE / "rules.toml").open("rb") as stream:
            return tomllib.load(stream)
    except (OSError, tomllib.TOMLDecodeError) as exc:
        raise Unreadable(f"rules.toml cannot be read: {exc}") from exc


def relative(path: str, root: Path) -> str | None:
    try:
        return Path(path).resolve().relative_to(root.resolve()).as_posix()
    except ValueError:
        return None


def framework_packages(metadata: dict, root: Path, rules: dict) -> dict[str, dict]:
    """The workspace packages under the framework folder, by name."""
    prefix = rules["framework"]["prefix"]
    members = set(metadata.get("workspace_members", []))
    found = {}
    for package in metadata["packages"]:
        if members and package["id"] not in members:
            continue
        rel = relative(package["manifest_path"], root)
        if rel is not None and rel.startswith(prefix):
            found[package["name"]] = package
    return found


def enables(package: dict, feature: str, wanted: str) -> bool:
    """Whether turning on `feature` of `package` turns on its own `wanted` feature."""
    table = package.get("features", {})
    seen: set[str] = set()
    todo = [feature]
    while todo:
        current = todo.pop()
        if current == wanted:
            return True
        if current in seen:
            continue
        seen.add(current)
        for item in table.get(current, []):
            # "dep:x", "x/feature" and "x?/feature" name another package's feature.
            if "/" in item or item.startswith("dep:"):
                continue
            todo.append(item)
    return False


def edge_findings(metadata: dict, root: Path, rules: dict) -> list[tuple[str, str, str]]:
    """Non-dev dependency edges that enable the feature on a framework crate."""
    wanted = rules["feature"]["name"]
    frameworks = framework_packages(metadata, root, rules)
    members = set(metadata.get("workspace_members", []))
    found = []
    for package in metadata["packages"]:
        if members and package["id"] not in members:
            continue
        for dep in package["dependencies"]:
            if dep.get("kind") == "dev":
                continue
            target = frameworks.get(dep["name"])
            if target is None:
                continue
            requested = list(dep.get("features", []))
            if dep.get("uses_default_features", True):
                requested.append("default")
            if any(enables(target, f, wanted) for f in requested if f in target.get("features", {}) or f == wanted):
                found.append((package["name"], dep["name"], dep.get("kind") or "normal"))
    return sorted(set(found))


def product_binaries(metadata: dict, rules: dict) -> list[str]:
    """Workspace packages with a binary target that are not listed as non-product."""
    skip = set(rules.get("non_product", {}).get("packages", []))
    members = set(metadata.get("workspace_members", []))
    out = []
    for package in metadata["packages"]:
        if members and package["id"] not in members:
            continue
        if package["name"] in skip:
            continue
        if any("bin" in target["kind"] for target in package["targets"]):
            out.append(package["name"])
    return sorted(out)


TREE_LINE = re.compile(r"^(?P<name>[^\s|]+) v[^\s|]*[^|]*\|(?P<features>.*)$")


class Unreadable(Exception):
    """The workspace or cargo could not be read."""


def parse_tree(text: str) -> dict[str, set[str]]:
    """The features per package name in a `cargo tree -f '{p}|{f}' --prefix none` listing.

    A line that does not parse is an error, not something to skip: a skipped line could hide a
    framework crate with the feature on.
    """
    found: dict[str, set[str]] = {}
    for line in text.splitlines():
        line = line.strip()
        if not line:
            continue
        match = TREE_LINE.match(line)
        if not match:
            raise Unreadable(f"cannot read this line of the cargo tree listing: {line!r}")
        features = {f.strip() for f in match["features"].replace("(*)", "").split(",") if f.strip()}
        found.setdefault(match["name"], set()).update(features)
    return found


def tree_findings(text: str, frameworks: set[str], wanted: str) -> list[str]:
    """Framework crates that a `cargo tree -f '{p}|{f}'` listing shows with the feature on."""
    return sorted(
        name for name, features in parse_tree(text).items() if name in frameworks and wanted in features
    )


def unified_findings(
    unified: str, closures: dict[str, set[str]], frameworks: set[str], wanted: str
) -> list[tuple[str, str]]:
    """(binary, framework crate) pairs where the unified build has the feature on a crate the
    binary uses."""
    on = set(tree_findings(unified, frameworks, wanted))
    return sorted((binary, crate) for binary, used in closures.items() for crate in sorted(used & on))


def cargo_metadata(root: Path) -> dict:
    try:
        result = subprocess.run(
            ["cargo", "metadata", "--format-version", "1", "--no-deps", "--offline",
             "--manifest-path", str(root / "Cargo.toml")],
            check=True, capture_output=True, text=True,
        )
    except FileNotFoundError as exc:
        raise Unreadable("cargo is not installed, cannot check features") from exc
    except subprocess.CalledProcessError as exc:
        raise Unreadable(exc.stderr.strip() or "cargo metadata failed") from exc
    return json.loads(result.stdout)


def cargo_tree(root: Path, package: str | list[str]) -> str:
    """The features cargo resolves for a normal build of `package` (no dev-dependencies), or for
    one build of all the packages of a list (the unified product build).

    All target platforms are included, so a platform-specific dependency cannot hide the feature.
    Tried offline first (the registry cache of a CI runner or a developer machine is enough);
    if that fails, once more with the network.
    """
    packages = [package] if isinstance(package, str) else list(package)
    command = ["cargo", "tree"]
    for name in packages:
        command += ["-p", name]
    command += [
        "-e", "normal,build", "--target", "all", "-f", "{p}|{f}",
        "--prefix", "none", "--manifest-path", str(root / "Cargo.toml"),
    ]
    last = ""
    for extra in (["--offline"], []):
        try:
            result = subprocess.run(command + extra, check=True, capture_output=True, text=True)
        except FileNotFoundError as exc:
            raise Unreadable("cargo is not installed, cannot check features") from exc
        except subprocess.CalledProcessError as exc:
            last = exc.stderr.strip()
            continue
        return result.stdout
    raise Unreadable(last or f"cargo tree failed for {packages}")


def current_lists(rules: dict) -> tuple[list[str], list[str]]:
    """The names in `non_product` and the `package -> dependency` edges in `[[allow_edge]]`."""
    names = sorted(rules.get("non_product", {}).get("packages", []))
    edges = sorted(f"{a['package']} -> {a['dependency']}" for a in rules.get("allow_edge", []))
    return names, edges


def baseline_text(rules: dict) -> str:
    names, edges = current_lists(rules)

    def array(items: list[str]) -> str:
        return "[\n" + "".join(f'  "{item}",\n' for item in items) + "]" if items else "[]"

    return (
        "# Committed baseline of tools/lints/features/rules.toml. The gate fails when `non_product`\n"
        "# or `[[allow_edge]]` has an entry that is not listed here, so a new entry needs a visible\n"
        "# change to this file. A change to this file is a lead-reviewed change. Entries are removed\n"
        "# by `python3 tools/lints/features/features_lint.py --regenerate-baseline` after the\n"
        "# rules entry is gone.\n\n"
        f"[non_product]\ncount = {len(names)}\npackages = {array(names)}\n\n"
        f"[allow_edge]\ncount = {len(edges)}\nedges = {array(edges)}\n"
    )


def baseline_problems(rules: dict, baseline: dict | None) -> list[str]:
    """Findings when a bypass list grew compared with the baseline, or the baseline is unusable."""
    if baseline is None:
        return ["baseline.toml: missing; run --regenerate-baseline and have the lead review it"]
    names, edges = current_lists(rules)
    problems = []
    for label, current, key in (("non_product", names, "packages"), ("allow_edge", edges, "edges")):
        section = baseline.get(label)
        if not isinstance(section, dict) or not isinstance(section.get(key), list):
            problems.append(f"baseline.toml: the [{label}] section is missing or malformed")
            continue
        listed = section[key]
        if section.get("count") != len(listed):
            problems.append(
                f"baseline.toml: [{label}] count {section.get('count')!r} does not match its "
                f"{len(listed)} entries"
            )
        for entry in sorted(set(current) - set(listed)):
            problems.append(
                f"rules.toml: {label} entry `{entry}` is not in baseline.toml; a new entry is a "
                "bypass of this gate and needs a lead-reviewed baseline change"
            )
        if len(current) > len(listed):
            problems.append(
                f"rules.toml: {label} grew ({len(listed)} in the baseline, {len(current)} now)"
            )
    return problems


def reason_problems(rules: dict) -> list[str]:
    """Every `non_product` entry needs a one-line reason."""
    packages = rules.get("non_product", {}).get("packages", {})
    if not isinstance(packages, dict):
        return ['rules.toml: [non_product] packages must be a table of name = "reason"']
    return [
        f"rules.toml: non_product entry `{name}` has no reason"
        for name, reason in sorted(packages.items())
        if not isinstance(reason, str) or not reason.strip()
    ]


def load_baseline(path: Path | None = None) -> dict | None:
    path = path or HERE / "baseline.toml"
    if not path.is_file():
        return None
    try:
        with path.open("rb") as stream:
            return tomllib.load(stream)
    except tomllib.TOMLDecodeError as exc:
        raise Unreadable(f"baseline.toml cannot be parsed: {exc}") from exc


def run(
    root: Path,
    rules: dict | None = None,
    metadata: Callable[[Path], dict] = cargo_metadata,
    tree: Callable[[Path, str], str] = cargo_tree,
    unified: Callable[[Path, list[str]], str] = cargo_tree,
    baseline: dict | None | str = "load",
) -> int:
    if not (root / "Cargo.toml").is_file():
        print("features: no Cargo workspace yet, nothing to check")
        return 0
    try:
        rules = rules or load_rules()
        wanted = rules["feature"]["name"]
        if baseline == "load":
            baseline = load_baseline()
        data = metadata(root)
        frameworks = framework_packages(data, root, rules)
        edges = edge_findings(data, root, rules)
        binaries = product_binaries(data, rules)
        problems: list[str] = reason_problems(rules) + baseline_problems(rules, baseline)

        allowed = {(a["package"], a["dependency"]) for a in rules.get("allow_edge", [])}
        for package, dependency, kind in edges:
            if (package, dependency) in allowed:
                continue
            problems.append(
                f"{package}: a {kind} dependency on {dependency} enables its `{wanted}` feature; "
                f"move it to [dev-dependencies] ({dependency} is a framework crate)"
            )
        present = {(p, d) for p, d, _ in edges}
        for package, dependency in sorted(allowed - present):
            problems.append(
                f"rules.toml: the allowed edge {package} -> {dependency} no longer exists; "
                "remove it from [[allow_edge]]"
            )
        closures: dict[str, set[str]] = {}
        alone: dict[str, set[str]] = {}
        for binary in binaries:
            parsed = parse_tree(tree(root, binary))
            closures[binary] = set(parsed) & set(frameworks)
            alone[binary] = {c for c in closures[binary] if wanted in parsed[c]}
            for crate in sorted(alone[binary]):
                problems.append(
                    f"{binary}: a product binary build enables `{wanted}` on the framework crate {crate}"
                )
        if binaries:
            for binary, crate in unified_findings(unified(root, binaries), closures, set(frameworks), wanted):
                if crate in alone[binary]:
                    continue
                others = sorted(b for b in binaries if crate in alone[b]) or ["another package of the build"]
                problems.append(
                    f"{binary}: in the unified product build (all product binaries built together) "
                    f"`{wanted}` is enabled on the framework crate {crate} that it uses, by "
                    f"{', '.join(others)}"
                )
    except Unreadable as exc:
        print(f"features: {exc}", file=sys.stderr)
        return 2

    if problems:
        for problem in problems:
            print(f"FEATURES: {problem}")
        print(f"FEATURES: {len(problems)} finding(s)")
        return 1
    print(
        f"FEATURES: no product build enables `{wanted}` on a framework crate "
        f"({len(binaries)} product binaries, {len(frameworks)} framework crates, "
        f"{len(allowed)} allowed legacy edges)"
    )
    names, edges = current_lists(rules)
    if baseline and (
        len(baseline.get("non_product", {}).get("packages", [])) > len(names)
        or len(baseline.get("allow_edge", {}).get("edges", [])) > len(edges)
    ):
        print("FEATURES: note: the lists shrank; run --regenerate-baseline in the same change")
    return 0


def main(argv: list[str]) -> int:
    args = argv[1:]
    if args == ["--regenerate-baseline"]:
        try:
            text = baseline_text(load_rules())
        except Unreadable as exc:
            print(f"features: {exc}", file=sys.stderr)
            return 2
        (HERE / "baseline.toml").write_text(text, encoding="utf-8")
        print("features: baseline.toml rewritten from rules.toml; the lead reviews the diff")
        return 0
    root = Path(args[0]) if args else HERE.parents[2]
    return run(root.resolve())


if __name__ == "__main__":
    sys.exit(main(sys.argv))
