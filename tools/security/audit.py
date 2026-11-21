#!/usr/bin/env python3
"""Dependency audit: cargo-deny plus dotnet, one verdict (WP-PRD-08.T2).

Runs, from the repository root:

* ``cargo-deny check advisories licenses bans sources`` — the Rust dependency
  audit against the settings in ``deny.toml`` (parses cargo-deny 0.14+
  diagnostic output: ``error[<code>]:`` blocks with ``├─ Key: value`` details).
* ``dotnet list <project> package --vulnerable --include-transitive`` for each
  project referenced by the Avalonia solution (default
  ``clients/avalonia/Spotledger.Client.sln``).

Findings are printed as a table (finding, package, version, severity,
fixed-in) and matched against the time-boxed exceptions in the allowlist. A
tool that is not installed is skipped with a clear message; ``--strict``
turns every skip into exit code 2. Severity comes from the tool when the
output states it and falls back to ``high`` for error-level and ``low`` for
warning-level cargo-deny findings (dotnet threat levels map: Moderate ->
medium, None -> low). An expired allowlist exception always fails the audit.

Exit codes:
  0  clean
  1  findings at or above --fail-on, or an expired exception in the allowlist
  2  a tool ran and failed, or a tool is missing under --strict
"""
from __future__ import annotations

import argparse
import datetime
import re
import subprocess
import sys
import tomllib
from dataclasses import dataclass
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
DEFAULT_SOLUTION = Path("clients/avalonia/Spotledger.Client.sln")
DEFAULT_ALLOWLIST = Path("tools/security/allowlist.toml")
TOOL_TIMEOUT = 900

SEVERITIES = ("low", "medium", "high", "critical")
FAIL_ON_CHOICES = (*SEVERITIES, "moderate")


def normalise_severity(raw: str) -> str:
    """Map tool wording to one of SEVERITIES; anything unknown degrades to low."""
    words = raw.strip().rstrip(",;").split()
    if words:
        word = words[-1].lower().strip("()")
        if word in SEVERITIES:
            return word
        if word == "moderate":
            return "medium"
        if word == "none":
            return "low"
        try:
            score = float(word)
        except ValueError:
            return "low"
        if score >= 9.0:
            return "critical"
        if score >= 7.0:
            return "high"
        if score >= 4.0:
            return "medium"
    return "low"


def severity_at_or_above(severity: str, threshold: str) -> bool:
    return (SEVERITIES.index(normalise_severity(severity))
            >= SEVERITIES.index(normalise_severity(threshold)))


@dataclass
class Finding:
    """One auditable item: a tool finding or an expired exception."""

    tool: str        # cargo-deny | dotnet | allowlist
    check: str       # advisory | ban | license | duplicate | source | vulnerable | expired-exception
    finding: str     # advisory/GHSA identifier or the tool's diagnostic code
    package: str = ""
    version: str = ""
    severity: str = "low"
    fixed_in: str = ""
    url: str = ""
    where: str = ""  # lock-file location, project name or expiry date
    suppressed: bool = False


@dataclass
class ToolResult:
    """Outcome of invoking one external tool."""

    name: str
    status: str  # "ran" | "missing" | "failed"
    returncode: int = 0
    text: str = ""


def run_tool(name: str, argv: list[str], cwd: Path) -> ToolResult:
    """Run a tool; an executable that is not on PATH reads as missing."""
    try:
        proc = subprocess.run(argv, cwd=str(cwd), capture_output=True, text=True,
                              timeout=TOOL_TIMEOUT)
    except FileNotFoundError:
        return ToolResult(name, "missing")
    except subprocess.TimeoutExpired:
        return ToolResult(name, "failed", 0, f"{name} timed out after {TOOL_TIMEOUT}s")
    except OSError as exc:
        return ToolResult(name, "failed", 0, f"{name} could not start: {exc}")
    return ToolResult(name, "ran", proc.returncode, (proc.stdout or "") + (proc.stderr or ""))


_CARGO_HEADING = re.compile(r"^(error|warning)\[(?P<code>[A-Za-z0-9]+)\]: (?P<title>.*)$")
_CARGO_LOCATION = re.compile(r"^\s*[┌╭]+─\s*(?P<loc>\S+)")
_CARGO_KEY = re.compile(r"^\s*[├└╰]+─\s*(?P<key>[^:]+?)\s*:\s*(?P<value>.+)$")
_SEMVER = re.compile(r"\d+\.\d+\.\d+[\w.+\-]*")
_SEVERITY_VALUE = re.compile(r"^(?P<num>\d+(?:\.\d+)?)\s*\((?P<word>[^)]+)\)$")
_CARGO_CHECKS = {"A": "advisory", "B": "ban", "D": "duplicate",
                 "L": "license", "S": "source", "G": "source"}


def parse_cargo_deny(text: str) -> list[Finding]:
    """Parse cargo-deny 0.14+ diagnostic output into findings.

    A block starts with ``error[<code>]:`` or ``warning[<code>]:`` and carries
    ``├─ Key: value`` detail lines. Severity is taken from a ``Severity`` key
    when present (``7.5 (High)`` style) and otherwise graded by the diagnostic
    level: error -> high, warning -> low. ``Solution`` is searched for a
    version to fill fixed-in.
    """
    findings: list[Finding] = []
    level = code = location = ""
    keys: dict[str, str] = {}

    def flush() -> None:
        nonlocal level, code, location, keys
        if code:
            check = _CARGO_CHECKS.get(code[0].upper(), "policy")
            severity = "high" if level == "error" else "low"
            raw_severity = keys.get("Severity", "")
            if raw_severity:
                quoted = _SEVERITY_VALUE.match(raw_severity.strip())
                severity = normalise_severity(quoted.group("word") if quoted else raw_severity)
            version_hint = _SEMVER.search(keys.get("Solution", ""))
            findings.append(Finding(
                tool="cargo-deny", check=check,
                finding=keys.get("Advisory ID", "") or code,
                package=keys.get("Package", ""), version=keys.get("Version", ""),
                severity=severity,
                fixed_in=version_hint.group(0) if version_hint else "",
                url=keys.get("Advisory URL", ""),
                where=location or keys.get("Package", ""),
            ))
        level = code = location = ""
        keys = {}

    for line in text.splitlines():
        heading = _CARGO_HEADING.match(line)
        if heading:
            flush()
            level, code = heading.group(1), heading.group("code")
        elif location_match := _CARGO_LOCATION.match(line):
            location = location_match.group("loc")
        elif key := _CARGO_KEY.match(line):
            keys[key.group("key")] = key.group("value").strip()
    flush()
    return findings


_DOTNET_PROJECT = re.compile(r"^Project `(?P<project>.+)` has the following vulnerable packages\s*$")
_DOTNET_PACKAGE = re.compile(r"^\s*>\s*(?P<tfm>\[[^\]]+\]\s+)?(?P<package>\S+)"
                             r"(?:\s+\[[^\]]*\]?)?\s*$")
_DOTNET_SCOPE = re.compile(r"^\s*(?P<scope>Transitive|Top-level|Direct)\s+[Pp]ackage\s*$")
_DOTNET_ATTR = re.compile(r"^\s*(?P<key>Resolved|Threat Level|Advertised|Advisory"
                          r"|Recommended Version)\s*:\s*(?P<value>.+)$")
_GHSA = re.compile(r"GHSA(-[A-Za-z0-9]{4}){3}")


def parse_dotnet_vulnerable(text: str) -> list[Finding]:
    """Parse ``dotnet list package --vulnerable --include-transitive`` output.

    Each project section follows the pattern::

        Project `Name` has the following vulnerable packages
           > [net9.0] Package [advised range, )
              Transitive Package
                 Resolved: 1.2.3
                 Threat Level: High
                 Advertised: https://aka.ms/nugetclp

    dotnet does not print a fixed-in version; the finding id is the GHSA id
    from the advisory URL when present and the package name otherwise.
    """
    findings: list[Finding] = []
    project = tfm = package = ""
    attrs: dict[str, str] = {}
    transitive = False

    def flush() -> None:
        nonlocal project, tfm, package, attrs, transitive
        if package:
            url = attrs.get("Advisory") or attrs.get("Advertised") or ""
            ghsa = _GHSA.search(url)
            where = " ".join(part for part in (f"[{tfm}]", project) if part)
            findings.append(Finding(
                tool="dotnet", check="vulnerable",
                finding=ghsa.group(0) if ghsa else package,
                package=package, version=attrs.get("Resolved", ""),
                severity=normalise_severity(attrs.get("Threat Level", "")),
                fixed_in=attrs.get("Recommended Version", ""), url=url, where=where,
            ))
        tfm = package = ""
        attrs, transitive = {}, False

    for line in text.splitlines():
        if section := _DOTNET_PROJECT.match(line):
            flush()
            project = section.group("project")
        elif entry := _DOTNET_PACKAGE.match(line):
            flush()
            package = entry.group("package")
            tfm = (entry.group("tfm") or "").lstrip("[").split("]")[0].strip()
        elif scope := _DOTNET_SCOPE.match(line):
            transitive = scope.group("scope").lower() == "transitive"
        elif attr := _DOTNET_ATTR.match(line):
            attrs[attr.group("key")] = attr.group("value").strip()
    flush()
    return findings


_SLN_PROJECT = re.compile(r'^Project\("[^"]*"\)\s*=\s*"[^"]*",\s*"(?P<path>[^"]+\.csproj)"'
                          r',\s*"[^"]*"', re.IGNORECASE)


def parse_solution_text(text: str) -> list[Path]:
    """The .csproj paths referenced by a solution file, sorted and unique."""
    found = set()
    for line in text.splitlines():
        if match := _SLN_PROJECT.match(line.strip()):
            found.add(Path(match.group("path").replace("\\", "/")))
    return sorted(found)


def parse_solution(sln: Path) -> list[Path]:
    return parse_solution_text(sln.read_text(encoding="utf-8", errors="replace"))


REQUIRED_EXCEPTION_KEYS = ("id", "package", "reason", "owner", "expires")


class AllowlistError(ValueError):
    """The allowlist cannot be trusted; the audit must fail closed."""


def load_allowlist(path: Path) -> list[dict]:
    """Load time-boxed exceptions; a missing file is an empty allowlist."""
    if not path.is_file():
        return []
    try:
        data = tomllib.loads(path.read_text(encoding="utf-8"))
    except (tomllib.TOMLDecodeError, OSError, UnicodeDecodeError) as exc:
        raise AllowlistError(f"{path}: unreadable TOML: {exc}") from exc
    entries = data.get("exceptions", [])
    if not isinstance(entries, list) or any(not isinstance(e, dict) for e in entries):
        raise AllowlistError(f"{path}: 'exceptions' must be an array of tables")
    for index, entry in enumerate(entries, start=1):
        for key in REQUIRED_EXCEPTION_KEYS:
            if not str(entry.get(key, "")).strip():
                raise AllowlistError(f"{path}: exception {index} is missing '{key}'")
        try:
            datetime.date.fromisoformat(str(entry["expires"]))
        except ValueError as exc:
            raise AllowlistError(f"{path}: exception {index} has a bad 'expires' date") from exc
    return entries


def exception_expired(entry: dict, today: datetime.date | None = None) -> bool:
    """True once the expiry date has passed (the expiry day itself is valid)."""
    today = today or datetime.date.today()
    return today > datetime.date.fromisoformat(str(entry["expires"]))


def expiry_findings(entries: list[dict]) -> list[Finding]:
    """An expired exception is itself a blocking finding, even with no matches."""
    return [Finding(tool="allowlist", check="expired-exception", finding=str(entry["id"]),
                    package=str(entry["package"]), severity="high", where=str(entry["expires"]),
                    url="docs/security/dependency-policy.md")
            for entry in entries if exception_expired(entry)]


def suppress(findings: list[Finding], exceptions: list[dict],
             today: datetime.date | None = None) -> None:
    """Mark findings covered by an *active* exception as suppressed in place."""
    active = [entry for entry in exceptions if not exception_expired(entry, today)]
    for finding in findings:
        finding.suppressed = any(
            (finding.finding and str(entry["id"]).casefold() == finding.finding.casefold())
            or (finding.package and str(entry["package"]).casefold() == finding.package.casefold())
            for entry in active)


def audit_exit_code(*, tool_errors: int, missing: int, strict: bool,
                    failing: int, expired: int) -> int:
    """Tool errors (or missing tools under --strict) -> 2; failing findings or
    expired exceptions -> 1; otherwise clean. Tool errors take precedence."""
    if tool_errors or (missing and strict):
        return 2
    if expired or failing:
        return 1
    return 0


def audit(root: Path, *, fail_on: str = "high", strict: bool = False,
          allowlist_path: Path | None = None, solution: Path | None = None,
          cargo_deny_bin: str = "cargo-deny", dotnet_bin: str = "dotnet",
          today: datetime.date | None = None) -> tuple[int, list[Finding]]:
    """Run both auditors and return (exit code, findings), printing a report."""
    root = Path(root)
    allowlist_path = Path(allowlist_path) if allowlist_path else root / DEFAULT_ALLOWLIST
    solution = Path(solution) if solution else root / DEFAULT_SOLUTION
    tool_errors: list[str] = []
    missing: list[str] = []
    findings: list[Finding] = []

    cargo = run_tool("cargo-deny", [cargo_deny_bin, "check", "advisories", "licenses",
                                    "bans", "sources"], root)
    if cargo.status == "ran":
        parsed = parse_cargo_deny(cargo.text)
        findings.extend(parsed)
        if cargo.returncode != 0 and not parsed:
            tail = " | ".join(cargo.text.strip().splitlines()[-3:][:3]) or "<no output>"
            tool_errors.append(f"cargo-deny exited {cargo.returncode} with no parseable "
                               f"findings ({tail})")
    elif cargo.status == "missing":
        missing.append("cargo-deny")
        print("SKIP: cargo-deny is not on PATH; Rust advisories, licences, bans "
              "and sources were not audited (install with: cargo install cargo-deny)")
    else:
        tool_errors.append(cargo.text)

    if not solution.is_file():
        tool_errors.append(f"solution not found: {solution}")
    else:
        relative_projects = parse_solution(solution)
        if not relative_projects:
            tool_errors.append(f"no .csproj project found in {solution}")
        projects = [solution.parent / relative for relative in relative_projects]
        for project in projects:
            result = run_tool("dotnet", [dotnet_bin, "list", str(project.resolve()),
                                         "package", "--vulnerable", "--include-transitive"], root)
            if result.status == "missing":
                missing.append("dotnet")
                print("SKIP: the dotnet CLI is not on PATH; the NuGet vulnerability "
                      "audit was skipped (install the .NET SDK)")
                break
            if result.status == "failed":
                tool_errors.append(result.text)
                break
            parsed = parse_dotnet_vulnerable(result.text)
            findings.extend(parsed)
            if result.returncode != 0 and not parsed:
                tail = " | ".join(result.text.strip().splitlines()[-3:][:3]) or "<no output>"
                tool_errors.append(f"dotnet list package exited {result.returncode} "
                                   f"for {project} with no parseable findings ({tail})")

    try:
        exceptions = load_allowlist(allowlist_path)
    except AllowlistError as exc:
        print(f"ERROR: {exc}", file=sys.stderr)
        return 2, []
    expired = expiry_findings(exceptions)
    suppress(findings, exceptions, today)
    findings.extend(expired)

    fail_on = normalise_severity(fail_on)
    print_table(findings)
    active = len(exceptions) - len(expired)
    print(f"allowlist: {active} active exception(s), {len(expired)} expired")
    if tool_errors:
        for message in tool_errors:
            print(f"ERROR: {message}", file=sys.stderr)
    failing = [f for f in findings
               if not f.suppressed and severity_at_or_above(f.severity, fail_on)]
    code = audit_exit_code(tool_errors=len(tool_errors), missing=len(missing), strict=strict,
                           failing=len(failing), expired=len(expired))
    if code == 2:
        print("verdict: FAIL — the audit could not complete")
    elif code == 1:
        reasons = []
        if failing:
            reasons.append(f"{len(failing)} finding(s) at or above {fail_on}")
        if expired:
            reasons.append(f"{len(expired)} expired exception(s)")
        print(f"verdict: FAIL — {' and '.join(reasons)}")
    else:
        print(f"verdict: clean — nothing at or above {fail_on}")
    return code, findings


def print_table(findings: list[Finding]) -> None:
    """Print findings as an aligned table with the required columns."""
    if not findings:
        print("No findings.")
        return
    headers = ("finding", "package", "version", "severity", "fixed-in", "tool", "where")
    rows = [(f.finding or f.check, f.package or "-", f.version or "-",
             f.severity + (" (suppressed)" if f.suppressed else ""),
             f.fixed_in or "-", f.tool, f.where or "-") for f in findings]
    widths = [max(len(headers[i]), max(len(row[i]) for row in rows)) for i in range(len(headers))]
    padded = lambda cells: "  ".join(cell.ljust(width) for cell, width in zip(cells, widths))
    print(padded(headers))
    print("  ".join("-" * width for width in widths))
    for row in rows:
        print(padded(row))


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Audit Rust and .NET dependencies for vulnerabilities and licence policy.")
    parser.add_argument("--fail-on", default="high", choices=FAIL_ON_CHOICES,
                        help="fail at or above this severity (default: high)")
    parser.add_argument("--strict", action="store_true",
                        help="a missing tool fails (exit 2) instead of being skipped")
    parser.add_argument("--allowlist", type=Path, default=None,
                        help=f"time-boxed exceptions file (default: {DEFAULT_ALLOWLIST})")
    parser.add_argument("--solution", type=Path, default=None,
                        help=f"solution to audit (default: {DEFAULT_SOLUTION})")
    parser.add_argument("--root", type=Path, default=REPO_ROOT,
                        help="repository root (default: the checkout containing this script)")
    args = parser.parse_args(argv)
    code, _ = audit(args.root, fail_on=normalise_severity(args.fail_on), strict=args.strict,
                    allowlist_path=args.allowlist, solution=args.solution)
    return code


if __name__ == "__main__":
    sys.exit(main())