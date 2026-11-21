#!/usr/bin/env python3
"""Release tag gate (the project process notes, sections 2 and 4.2).

Exit 0: every gate passed. Exit 1: refusal (every reason printed, one per
line). Exit 2: usage or environment error. Fails closed: any input that
cannot be read or decided refuses the tag.  Standard library only.
"""
from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import urllib.error
import urllib.request

TAG_RE = re.compile(r"^v\d+\.\d+\.\d+(-[0-9A-Za-z]+(\.[0-9A-Za-z]+)*)?$", re.ASCII)


class EnvError(Exception):
    """Usage or environment problem (exit 2)."""


def git(repo_dir, *args, env=None, check=False):
    try:
        proc = subprocess.run(
            ["git", "-C", str(repo_dir), *args],
            capture_output=True, text=True, env=env,
        )
    except OSError as exc:
        raise EnvError(f"cannot run git: {exc}")
    if check and proc.returncode != 0:
        raise EnvError(f"git {' '.join(args)} failed: {(proc.stderr or proc.stdout).strip()}")
    return proc


def http_get_json(url, token):
    """Real HTTP layer; replaced by a fake in tests. Returns parsed JSON."""
    req = urllib.request.Request(url, headers={
        "Accept": "application/vnd.github+json",
        "User-Agent": "spot-tag-gate",
        **({"Authorization": f"Bearer {token}"} if token else {}),
    })
    with urllib.request.urlopen(req, timeout=30) as resp:  # noqa: S310
        return json.loads(resp.read().decode("utf-8"))


def check_shape(tag):
    if TAG_RE.fullmatch(tag or ""):
        return []
    return [f"tag '{tag}' is not a version tag (expected vX.Y.Z or vX.Y.Z-pre, e.g. v0.1.0-rc.1)"]


def check_annotated(repo_dir, tag):
    ref = f"refs/tags/{tag}"
    proc = git(repo_dir, "cat-file", "-t", ref)
    kind = proc.stdout.strip()
    if proc.returncode != 0:
        return None, [f"tag {tag} does not exist in {repo_dir}"]
    if kind != "tag":
        return None, [f"tag {tag} is lightweight; create an annotated tag with: "
                      f"git tag -a {tag} -m \"release {tag}\" <sha>"]
    rp = git(repo_dir, "rev-parse", f"{ref}^{{commit}}")
    sha = rp.stdout.strip()
    if rp.returncode or not sha:
        return None, [f"cannot resolve the commit of tag {tag}"]
    return sha, []


def check_on_main(repo_dir, remote, branch, sha):
    shallow = git(repo_dir, "rev-parse", "--is-shallow-repository").stdout.strip()
    if shallow != "false":
        return ["repository is shallow (or unreadable); cannot decide ancestry. "
                "Fetch full depth: git fetch --unshallow"]
    fetch = git(repo_dir, "fetch", "--no-tags", remote, branch)
    if fetch.returncode != 0:
        return [f"git fetch {remote} {branch} failed: {fetch.stderr.strip()}"]
    anc = git(repo_dir, "merge-base", "--is-ancestor", sha, f"{remote}/{branch}")
    if anc.returncode == 0:
        return []
    if anc.returncode == 1:
        return [f"commit {sha} is not contained in {remote}/{branch}"]
    return [f"cannot decide whether {sha} is on {remote}/{branch}: {anc.stderr.strip()}"]


def check_ci(repo, workflow, sha, http=http_get_json, token=None):
    if not repo or not workflow:
        return ["CI check needs --repo OWNER/NAME and --workflow (or pass --skip-ci)"]
    url = (f"https://api.github.com/repos/{repo}/actions/workflows/{workflow}/runs"
           f"?head_sha={sha}&per_page=100")
    try:
        data = http(url, token)
        runs = [r for r in data["workflow_runs"] if r.get("head_sha") == sha]
    except (urllib.error.URLError, OSError, ValueError, KeyError, TypeError) as exc:
        return [f"cannot read workflow runs for {sha}: {type(exc).__name__}"]
    if not runs:
        return [f"no {workflow} run exists for commit {sha}"]
    done = [r for r in runs if r.get("status") == "completed"]
    pending = [r for r in runs if r.get("status") != "completed"]
    if not done:
        r = pending[0]
        return [f"{workflow} run for {sha} is not finished ({r.get('status')}): {r.get('html_url')}"]
    newest = max(done, key=lambda r: (str(r.get("created_at", "")), r.get("id", 0)))
    if newest.get("conclusion") != "success":
        return [f"{workflow} run for {sha} concluded {newest.get('conclusion')}: {newest.get('html_url')}"]
    return []


def check_signature(repo_dir, tag, keys):
    if not keys:
        return ["--require-signed needs --keys FILE"], []
    if not os.path.isfile(keys):
        return [f"public key file {keys} not found"], []
    gpg = shutil.which("gpg")
    if not gpg:
        return ["gpg is not installed; cannot verify the tag signature"], []
    home = tempfile.mkdtemp(prefix="tag-gate-gnupg-")
    try:
        os.chmod(home, 0o700)
        env = {**os.environ, "GNUPGHOME": home}
        imp = subprocess.run([gpg, "--batch", "--import", keys], capture_output=True, text=True, env=env)
        if imp.returncode != 0:
            return [f"cannot import key file {keys}: {imp.stderr.strip()}"], []
        proc = subprocess.run(
            ["git", "-C", str(repo_dir), "verify-tag", "--raw", tag],
            capture_output=True, text=True, env=env)
        out = proc.stdout + proc.stderr
        if proc.returncode != 0:
            return [f"signature verification failed for {tag}: {out.strip()}"], []
        fpr = re.search(r"VALIDSIG\s+([0-9A-Fa-f]{16,})", out)
        if not fpr:
            return [f"no valid signature reported for {tag}"], []
        return [], [f"signed by key fingerprint {fpr.group(1)}"]
    finally:
        shutil.rmtree(home, ignore_errors=True)


def run_gate(args, http=http_get_json, token=None):
    """Returns (reasons, info lines)."""
    reasons, info = [], []
    shape = check_shape(args.tag)
    if shape:
        return shape, info
    sha, errs = check_annotated(args.repo_dir, args.tag)
    reasons += errs
    if sha:
        reasons += check_on_main(args.repo_dir, args.remote, args.branch, sha)
        if args.skip_ci:
            info.append("NOTICE CI check skipped (--skip-ci)")
        else:
            reasons += check_ci(args.repo, args.workflow, sha, http, token)
    if args.require_signed:
        if sha:
            r, i = check_signature(args.repo_dir, args.tag, args.keys)
            reasons += r
            info += i
        else:
            reasons.append("signature cannot be checked without an annotated tag")
    else:
        info.append("NOTICE tag signature not required")
    return reasons, info


def build_parser():
    p = argparse.ArgumentParser(description="Refuse a release tag unless it is annotated, on main, green in CI and (optionally) signed.")
    p.add_argument("--tag", required=True)
    p.add_argument("--repo-dir", default=".")
    p.add_argument("--remote", default="origin")
    p.add_argument("--branch", default="main")
    p.add_argument("--repo", help="OWNER/NAME for the CI check")
    p.add_argument("--workflow", default="ci.yml")
    p.add_argument("--require-signed", action="store_true")
    p.add_argument("--keys", help="public key file (with --require-signed)")
    p.add_argument("--skip-ci", action="store_true")
    return p


def main(argv=None, http=http_get_json):
    try:
        args = build_parser().parse_args(argv)
    except SystemExit as exc:
        return 0 if exc.code in (0, None) else 2
    if not os.path.isdir(args.repo_dir):
        print(f"ERROR repo dir {args.repo_dir} not found", file=sys.stderr)
        return 2
    token = os.environ.get("GITHUB_TOKEN") or None
    try:
        reasons, info = run_gate(args, http, token)
    except EnvError as exc:
        print(f"ERROR {exc}", file=sys.stderr)
        return 2
    for line in info:
        print(line)
    if reasons:
        for r in reasons:
            print(f"REFUSED {r.replace(token, '***') if token else r}")
        return 1
    print(f"OK tag {args.tag} passes every gate")
    return 0


if __name__ == "__main__":
    sys.exit(main())
