#!/usr/bin/env python3
"""Workflow safety lint for the two-repository pipeline (WP-OSS-07.T3).

Usage: ops_workflow_lint.py <dir-or-file>... [--public-pair DIR]

Lints the workflows of the project process notes: the PUBLIC ones (`spot-pr.yml`,
`spot-verdict.yml`) installed in the public repository and the PRIVATE ones (files named
`ops-*.yml`) installed in the private tasks repository. It extends `workflow_lint.py`
(WP-OSS-03.T1): every rule of that lint applies except two that these workflows legitimately
trip (`pull-request`, a public workflow on pull_request; `secrets-trigger`, secrets on a
comment or PR trigger), which are replaced by the stricter rules below.

Public workflows (every file not named `ops-*`):
  public-trigger     a trigger other than pull_request and issue_comment
  public-secret      a secret other than SPOT_OPS_TOKEN (use `github.token`, which is not a
                     secret, for the public repository), `secrets: inherit`, `secrets[...]`,
                     `toJSON(secrets)`
  token-scope        SPOT_OPS_TOKEN anywhere but the `env` of one step (workflow or job env,
                     `with`, `run`, `if`), or in more than one step
  token-print        the token step prints the token: `set -x`, `printenv`, `env`, or a line that
                     starts with echo, printf, cat or tee and names the token, or writes it to
                     GITHUB_ENV or GITHUB_OUTPUT
  token-fork         the job of the token step has no guard against a fork or an unrelated
                     comment: on pull_request `github.event.pull_request.head.repo.full_name ==
                     github.repository`; on issue_comment `github.event.issue.pull_request` and
                     `startsWith(github.event.comment.body, '/verdict')`
  token-pr-code      a job that holds the token runs a local action (`uses: ./...`) or checks out
                     anything but `ref: main` (code from a pull request must never run with it)
  token-env-repos    the token step does not set SPOT_TASKS_REPO and SPOT_CODE_REPO

Private workflows (`ops-*.yml`):
  private-trigger    a trigger other than schedule, workflow_dispatch, issue_comment and issues
  private-secret     a secret other than GITHUB_TOKEN and SPOT_CODE_TOKEN (SPOT_OPS_TOKEN belongs
                     to the public repository only), `secrets: inherit`, `secrets[...]`
  code-token-scope   SPOT_CODE_TOKEN anywhere but the `env` of a step
  pipeline-env       a step that runs tools/pipeline/actions.py lacks SPOT_CODE_TOKEN,
                     SPOT_TASKS_REPO or SPOT_CODE_REPO in its env (two-repository mode fails
                     closed without them)

``--public-pair DIR`` (WP-OSS-07.T5) lints exactly the two public workflows (the names are
``workflow_lint.OPS_PUBLIC_WORKFLOWS``) in DIR and fails when either is missing. The public
repository's ci.yml runs ``workflow_lint.py .github/workflows --exclude spot-pr.yml --exclude
spot-verdict.yml`` and then ``ops_workflow_lint.py --public-pair .github/workflows``, so every file
in the directory is linted by exactly one of the two commands.
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path
from typing import Any, Iterator

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

import workflow_lint  # noqa: E402

yaml = workflow_lint.yaml

OPS_TOKEN = "SPOT_OPS_TOKEN"
CODE_TOKEN = "SPOT_CODE_TOKEN"
REPLACED = ("pull-request", "secrets-trigger")  # rules of workflow_lint that these files replace
PUBLIC_TRIGGERS = {"pull_request", "issue_comment"}
PRIVATE_TRIGGERS = {"schedule", "workflow_dispatch", "issue_comment", "issues"}
SECRET_RE = re.compile(r"\bsecrets\.([A-Za-z0-9_]+)")
BROAD_SECRETS_RE = re.compile(r"secrets:\s*inherit|\bsecrets\s*\[|toJSON\(\s*secrets\s*\)", re.I)
FORK_GUARD = "github.event.pull_request.head.repo.full_name == github.repository"
COMMENT_GUARDS = ("github.event.issue.pull_request", "startsWith(github.event.comment.body, '/verdict')")
PR_REF_RE = re.compile(r"pull|head|github\.sha|github\.event|refs/", re.I)
PRINT_LINE_RE = re.compile(r"^\s*(?:echo|printf|cat|tee)\b")
XTRACE_RE = re.compile(r"^\s*set\s+-\w*x|^\s*set\s+-o\s+xtrace|^\s*printenv\b|^\s*env\s*(?:$|\|)")
EXPORT_RE = re.compile(r"GITHUB_ENV|GITHUB_OUTPUT")


def is_private(name: str) -> bool:
    return Path(name).name.startswith("ops-")


def walk(node: Any, path: tuple = ()) -> Iterator[tuple[tuple, Any]]:
    """Every scalar string in a YAML document with its path (dict keys and list indexes)."""
    if isinstance(node, dict):
        for key, value in node.items():
            yield from walk(value, path + (key,))
    elif isinstance(node, list):
        for index, value in enumerate(node):
            yield from walk(value, path + (index,))
    elif isinstance(node, str):
        yield path, node


def is_step_env(path: tuple) -> bool:
    """``jobs.<job>.steps.<n>.env.<KEY>``"""
    return (len(path) == 6 and path[0] == "jobs" and path[2] == "steps"
            and isinstance(path[3], int) and path[4] == "env")


def secret_names(text: str) -> set[str]:
    return set(SECRET_RE.findall(workflow_lint.strip_comments(text)))


def step_has_secret(step: dict, name: str) -> bool:
    env = step.get("env")
    return isinstance(env, dict) and any(f"secrets.{name}" in str(v) for v in env.values())


def secret_sites(doc: dict, name: str) -> tuple[list[tuple], list[tuple]]:
    """(paths in a step env, paths anywhere else) that mention ``secrets.<name>``."""
    inside: list[tuple] = []
    outside: list[tuple] = []
    for path, value in walk(doc):
        if re.search(rf"\bsecrets\.{name}\b", value):
            (inside if is_step_env(path) else outside).append(path)
    return inside, outside


def where(path: tuple) -> str:
    return ".".join(str(p) for p in path)


def lint_public(doc: dict, name: str, text: str, events: set[str], add) -> None:
    for event in sorted(events - PUBLIC_TRIGGERS):
        add("public-trigger", f"trigger `{event}` is not allowed (only pull_request and issue_comment)")
    for secret in sorted(secret_names(text) - {OPS_TOKEN}):
        add("public-secret", f"secret `{secret}` is not allowed in a public workflow "
                             f"(the only secret is {OPS_TOKEN}; use `github.token` for this repository)")
    if BROAD_SECRETS_RE.search(workflow_lint.strip_comments(text)):
        add("public-secret", "`secrets: inherit`, `secrets[...]` and `toJSON(secrets)` are not allowed")

    inside, outside = secret_sites(doc, OPS_TOKEN)
    for path in outside:
        add("token-scope", f"{OPS_TOKEN} is used at `{where(path)}`; it may only be a step `env` value")

    token_steps: list[tuple[str, int, dict]] = []
    for job_id, job in (doc.get("jobs") or {}).items():
        if not isinstance(job, dict):
            continue
        for index, step in enumerate(job.get("steps") or []):
            if isinstance(step, dict) and step_has_secret(step, OPS_TOKEN):
                token_steps.append((str(job_id), index, step))
    if len(token_steps) > 1:
        add("token-scope", f"{OPS_TOKEN} is passed to {len(token_steps)} steps; it must reach one step only")

    for job_id, index, step in token_steps:
        job = doc["jobs"][job_id]
        label = f"job `{job_id}`, step {index + 1}"
        guard_text = f"{job.get('if', '')} {step.get('if', '')}"
        if "pull_request" in events and FORK_GUARD not in guard_text:
            add("token-fork", f"{label}: no `if: {FORK_GUARD}` (a fork must never reach the token)")
        if "issue_comment" in events and not all(g in guard_text for g in COMMENT_GUARDS):
            add("token-fork", f"{label}: the `if` must test `{COMMENT_GUARDS[0]}` and `{COMMENT_GUARDS[1]}`")

        for other in job.get("steps") or []:
            if not isinstance(other, dict):
                continue
            uses = str(other.get("uses", ""))
            if uses.startswith("./"):
                add("token-pr-code", f"{label}: local action `{uses}` runs code from the checkout")
            if uses.startswith("actions/checkout"):
                ref = str((other.get("with") or {}).get("ref", ""))
                if ref != "main" or PR_REF_RE.search(ref):
                    add("token-pr-code", f"{label}: a checkout in a job that holds {OPS_TOKEN} must set `ref: main`")

        env = step.get("env") or {}
        for key in ("SPOT_TASKS_REPO", "SPOT_CODE_REPO"):
            if key not in env:
                add("token-env-repos", f"{label}: env must set {key}")

        script = step.get("run")
        if isinstance(script, str):
            for line in workflow_lint.strip_comments(script).splitlines():
                mentions = OPS_TOKEN in line
                if XTRACE_RE.search(line):
                    add("token-print", f"{label}: `{line.strip()}` can print the token")
                elif mentions and (PRINT_LINE_RE.search(line) or EXPORT_RE.search(line)):
                    add("token-print", f"{label}: `{line.strip()[:60]}` prints or exports the token")


def lint_private(doc: dict, name: str, text: str, events: set[str], add) -> None:
    for event in sorted(events - PRIVATE_TRIGGERS):
        add("private-trigger", f"trigger `{event}` is not allowed "
                               f"(only {', '.join(sorted(PRIVATE_TRIGGERS))})")
    for secret in sorted(secret_names(text) - {"GITHUB_TOKEN", CODE_TOKEN}):
        add("private-secret", f"secret `{secret}` is not allowed in a private workflow "
                              f"(only GITHUB_TOKEN and {CODE_TOKEN}; {OPS_TOKEN} belongs to the public repository)")
    if BROAD_SECRETS_RE.search(workflow_lint.strip_comments(text)):
        add("private-secret", "`secrets: inherit`, `secrets[...]` and `toJSON(secrets)` are not allowed")
    _, outside = secret_sites(doc, CODE_TOKEN)
    for path in outside:
        add("code-token-scope", f"{CODE_TOKEN} is used at `{where(path)}`; it may only be a step `env` value")
    for job_id, job in (doc.get("jobs") or {}).items():
        if not isinstance(job, dict):
            continue
        for index, step in enumerate(job.get("steps") or []):
            if not isinstance(step, dict) or "tools/pipeline/actions.py" not in str(step.get("run", "")):
                continue
            env = step.get("env") or {}
            missing = [k for k in (CODE_TOKEN, "SPOT_TASKS_REPO", "SPOT_CODE_REPO") if k not in env]
            if missing or not step_has_secret(step, CODE_TOKEN):
                add("pipeline-env", f"job `{job_id}`, step {index + 1}: env must set "
                                    f"{CODE_TOKEN} (from secrets.{CODE_TOKEN}), SPOT_TASKS_REPO and SPOT_CODE_REPO"
                                    + (f" (missing {', '.join(missing)})" if missing else ""))


def lint_text(text: str, name: str) -> list[str]:
    """All findings for one workflow file, as ``name: [rule] message`` strings."""
    base = [f for f in workflow_lint.lint_text(text, name)
            if not any(f"[{rule}]" in f for rule in REPLACED)]
    if yaml is None or any("[parse]" in f for f in base):
        return base
    doc = yaml.safe_load(text)
    if not isinstance(doc, dict):
        return base
    findings: list[str] = []

    def add(rule: str, message: str) -> None:
        findings.append(f"{name}: [{rule}] {message}")

    events = workflow_lint.triggers_of(doc)
    if is_private(name):
        if "pull_request" in events:
            add("pull-request", "pull_request is not used by the private workflows")
        lint_private(doc, name, text, events, add)
    else:
        lint_public(doc, name, text, events, add)
    return base + findings


def lint_paths(paths: list[Path]) -> list[str]:
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
        findings += lint_text(file.read_text(encoding="utf-8"), str(file))
    return findings


def public_pair(directory: Path) -> list[Path]:
    return [directory / name for name in workflow_lint.OPS_PUBLIC_WORKFLOWS]


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("paths", nargs="*", type=Path)
    parser.add_argument(
        "--public-pair",
        type=Path,
        metavar="DIR",
        help="lint exactly the two public workflows (spot-pr.yml, spot-verdict.yml) in DIR",
    )
    args = parser.parse_args(argv)
    if not args.paths and args.public_pair is None:
        parser.error("give a path or --public-pair DIR")
    paths = list(args.paths)
    findings: list[str] = []
    if args.public_pair is not None:
        for file in public_pair(args.public_pair):
            if file.is_file():
                paths.append(file)
            else:
                findings.append(f"{file}: [public-pair] the public workflow is missing")
    findings += lint_paths(paths) if paths else []
    for finding in findings:
        print(finding)
    if findings:
        print(f"ops_workflow_lint: {len(findings)} finding(s)")
        return 1
    print("ops_workflow_lint: ok")
    return 0


if __name__ == "__main__":
    sys.exit(main())
