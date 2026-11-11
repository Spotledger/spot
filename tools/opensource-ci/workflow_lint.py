#!/usr/bin/env python3
"""Workflow safety lint for the public repository (WP-OSS-03.T1).

Usage: workflow_lint.py <dir-or-file>... [--allow-unpinned]

Reads every ``*.yml`` / ``*.yaml`` file under the given paths with ``yaml.safe_load`` and
reports every finding at once (exit 1 when there is any). Rules:

  self-hosted          a job runs on a self-hosted runner
  pull-request-target  the workflow triggers on pull_request_target
  pull-request         the workflow triggers on pull_request (the public repo takes none)
  unpinned-action      ``uses:`` is not pinned to a 40-hex commit SHA (off with --allow-unpinned)
  missing-permissions  no top-level ``permissions``
  write-permission     ``write-all`` or a ``write`` scope, unless the same line carries
                       ``# lint-allow: <scope> <reason>``
  missing-timeout      a job has no ``timeout-minutes``
  script-injection     ``${{ github.event.* }}`` (or github.head_ref) interpolated into ``run:``
  secrets-trigger      ``secrets.`` used in a workflow triggered by more than push, schedule
                       and workflow_dispatch
  postgres-missing     a job runs cargo test / cargo nextest / tools/ci/run_python_suites.py
                       with no earlier ``tools/ci/start-postgres.sh`` step (opt out with a
                       ``# lint-allow: no-postgres <reason>`` comment inside the job)
  postgres-service     a job declares a ``services:`` container whose image is a PostgreSQL
                       image (it lacks PostGIS and pgvector)
  require-pg-off       ``SPOT_REQUIRE_PG`` is set to anything but 1
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path
from typing import Any, Iterable

try:
    import yaml
except ImportError:  # pragma: no cover - exercised only on a bare interpreter
    yaml = None  # type: ignore[assignment]

SHA_RE = re.compile(r"^[0-9a-f]{40}$")
DOCKER_DIGEST_RE = re.compile(r"@sha256:[0-9a-f]{64}$")
INJECTION_RE = re.compile(r"\$\{\{[^}]*\b(?:github\.event\.|github\.head_ref)[^}]*\}\}")
TEST_CMD_RE = re.compile(
    r"(?:\bcargo\s+(?:test|nextest)\b|tools/ci/run_python_suites\.py)"
)
START_PG = "tools/ci/start-postgres.sh"
SAFE_TRIGGERS = {"push", "schedule", "workflow_dispatch"}
POSTGRES_IMAGE_RE = re.compile(r"postgres|postgis|pgvector", re.IGNORECASE)


def triggers_of(doc: dict[str, Any]) -> set[str]:
    """Event names in ``on:`` (PyYAML reads the bare key ``on`` as boolean True)."""
    raw = doc.get("on", doc.get(True))
    if raw is None:
        return set()
    if isinstance(raw, str):
        return {raw}
    if isinstance(raw, list):
        return {str(item) for item in raw}
    if isinstance(raw, dict):
        return {str(key) for key in raw}
    return set()


def strip_comments(script: str) -> str:
    return "\n".join(
        line for line in script.splitlines() if not line.lstrip().startswith("#")
    )


def job_text_range(lines: list[str], job_id: str) -> str:
    """The raw lines of one job (from ``  <id>:`` to the next two-space-indented key)."""
    start = None
    for index, line in enumerate(lines):
        if re.match(rf"^  {re.escape(str(job_id))}:\s*(#.*)?$", line):
            start = index
            break
    if start is None:
        return ""
    end = len(lines)
    for index in range(start + 1, len(lines)):
        if re.match(r"^  \S", lines[index]) or re.match(r"^\S", lines[index]):
            end = index
            break
    return "\n".join(lines[start:end])


def write_scopes(perms: Any) -> list[str]:
    """Scopes granted write access by a ``permissions`` value."""
    if perms == "write-all":
        return ["write-all"]
    if isinstance(perms, dict):
        return [str(k) for k, v in perms.items() if str(v) == "write"]
    return []


def write_allowed(lines: list[str], scope: str) -> bool:
    if scope == "write-all":
        pattern = re.compile(r"write-all.*#\s*lint-allow:\s*write-all\s+\S")
    else:
        pattern = re.compile(
            rf"\b{re.escape(scope)}\s*:\s*write\b.*#\s*lint-allow:\s*{re.escape(scope)}\s+\S"
        )
    return any(pattern.search(line) for line in lines)


def iter_env(doc: dict[str, Any]) -> Iterable[tuple[str, Any]]:
    """Every ``env`` mapping in the workflow, job and step levels."""
    if isinstance(doc.get("env"), dict):
        yield from doc["env"].items()
    for job in (doc.get("jobs") or {}).values():
        if not isinstance(job, dict):
            continue
        if isinstance(job.get("env"), dict):
            yield from job["env"].items()
        for step in job.get("steps") or []:
            if isinstance(step, dict) and isinstance(step.get("env"), dict):
                yield from step["env"].items()


def lint_text(text: str, name: str, allow_unpinned: bool = False) -> list[str]:
    """All findings for one workflow file, as ``name: [rule] message`` strings."""
    if yaml is None:
        return [f"{name}: [setup] PyYAML is not installed (python -m pip install PyYAML)"]
    findings: list[str] = []

    def add(rule: str, message: str) -> None:
        findings.append(f"{name}: [{rule}] {message}")

    try:
        doc = yaml.safe_load(text)
    except yaml.YAMLError as err:
        return [f"{name}: [parse] {err}"]
    if not isinstance(doc, dict):
        return [f"{name}: [parse] not a YAML mapping"]
    lines = text.splitlines()
    events = triggers_of(doc)

    if "pull_request_target" in events:
        add("pull-request-target", "pull_request_target is forbidden")
    if "pull_request" in events:
        add("pull-request", "the public repository takes no pull requests; remove pull_request")

    if "permissions" not in doc:
        add("missing-permissions", "no top-level `permissions:` (use `contents: read`)")
    for scope in write_scopes(doc.get("permissions")):
        if not write_allowed(lines, scope):
            add("write-permission", f"top-level permission `{scope}` without `# lint-allow: {scope} <reason>`")

    if re.search(r"\bsecrets\.|secrets:\s*inherit", strip_comments(text)):
        extra = sorted(events - SAFE_TRIGGERS)
        if extra:
            add("secrets-trigger", f"secrets used in a workflow also triggered by {', '.join(extra)}")

    for key, value in iter_env(doc):
        if key == "SPOT_REQUIRE_PG" and str(value) != "1":
            add("require-pg-off", f"SPOT_REQUIRE_PG is set to {value!r}; it must be 1")

    jobs = doc.get("jobs")
    if not isinstance(jobs, dict) or not jobs:
        add("parse", "no jobs")
        return findings

    for job_id, job in jobs.items():
        if not isinstance(job, dict):
            continue
        where = f"job `{job_id}`"
        job_text = job_text_range(lines, str(job_id))

        runs_on = job.get("runs-on")
        if "self-hosted" in repr(runs_on):
            add("self-hosted", f"{where} runs on a self-hosted runner")

        reusable = isinstance(job.get("uses"), str)
        if "timeout-minutes" not in job and not reusable:
            add("missing-timeout", f"{where} has no timeout-minutes")

        for scope in write_scopes(job.get("permissions")):
            if not write_allowed(job_text.splitlines(), scope):
                add("write-permission", f"{where} permission `{scope}` without `# lint-allow: {scope} <reason>`")

        uses_list = [job["uses"]] if reusable else []
        steps = [s for s in (job.get("steps") or []) if isinstance(s, dict)]
        uses_list += [s["uses"] for s in steps if isinstance(s.get("uses"), str)]
        for uses in uses_list:
            if uses.startswith("./"):
                continue
            if uses.startswith("docker://"):
                pinned = bool(DOCKER_DIGEST_RE.search(uses))
            else:
                _, _, ref = uses.partition("@")
                pinned = bool(SHA_RE.match(ref))
            if not pinned and not allow_unpinned:
                add("unpinned-action", f"{where} uses `{uses}`, not a full commit SHA")

        for container in (job.get("services") or {}).values():
            image = container.get("image", "") if isinstance(container, dict) else ""
            if POSTGRES_IMAGE_RE.search(str(image)):
                add("postgres-service", f"{where} starts a PostgreSQL service container ({image}); use {START_PG}")

        started = False
        runs_tests = False
        for index, step in enumerate(steps):
            script = step.get("run")
            label = step.get("name") or f"step {index + 1}"
            if isinstance(script, str):
                code = strip_comments(script)
                if INJECTION_RE.search(code):
                    add("script-injection", f"{where}, {label}: github.event expression interpolated into run:; pass it through env:")
                if START_PG in code:
                    started = True
                if TEST_CMD_RE.search(code) and not started:
                    runs_tests = True
        if runs_tests and not re.search(r"#\s*lint-allow:\s*no-postgres[ \t]+\S", job_text):
            add("postgres-missing", f"{where} runs tests without an earlier `bash {START_PG}` step")

    return findings


def lint_paths(paths: list[Path], allow_unpinned: bool = False) -> list[str]:
    files: list[Path] = []
    for path in paths:
        if path.is_dir():
            files += sorted(list(path.glob("*.yml")) + list(path.glob("*.yaml")))
        else:
            files.append(path)
    findings: list[str] = []
    if not files:
        findings.append("no workflow files found")
    for file in files:
        findings += lint_text(file.read_text(encoding="utf-8"), str(file), allow_unpinned)
    return findings


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("paths", nargs="+", type=Path)
    parser.add_argument(
        "--allow-unpinned",
        action="store_true",
        help="interim: accept version tags in uses: (off by default)",
    )
    args = parser.parse_args(argv)
    findings = lint_paths(args.paths, args.allow_unpinned)
    for finding in findings:
        print(finding)
    if findings:
        print(f"workflow_lint: {len(findings)} finding(s)")
        return 1
    print("workflow_lint: ok")
    return 0


if __name__ == "__main__":
    sys.exit(main())
