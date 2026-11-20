"""L9 nightly profiles (WP-TST-15.T8): load, validate and run the profile files in this directory.

One TOML file per L9 run (tier_s, tier_s_windows, tier_m, volume, soak, cluster3, faults) and the
PR-gate `smoke`. A file says which runner label it needs, its timeout, the command, the
environment and the artefacts; `run` executes it, tees the output to `target/l9/<name>.log` and
appends the run's measured budgets to the trend file (see trend.py).

    l9_profiles.py list
    l9_profiles.py run NAME [--commit SHA] [--run ID] [--trend FILE] [--dry-run]

Time is an argument: nothing here reads the clock. The run id and the commit come from the
caller (the workflow passes GITHUB_RUN_ID and GITHUB_SHA).
"""
from __future__ import annotations

import argparse
import os
import subprocess
import sys
import tomllib
from dataclasses import dataclass, field
from pathlib import Path
from typing import Mapping, Sequence

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[3]
HARNESS = REPO / "harness" / "rust" / "l9-scale"
LOG_DIR = "target/l9"
DEFAULT_TREND = f"{LOG_DIR}/l9-trend.jsonl"
KNOWN_KEYS = {
    "name", "description", "runner", "timeout_minutes", "pr_gate", "test", "l9_profile",
    "prebuild", "command", "artefacts", "env",
}
REQUIRED_KEYS = ("name", "description", "runner", "timeout_minutes", "pr_gate", "test", "l9_profile", "command")


class ProfileError(ValueError):
    """A profile file that cannot be used."""


@dataclass(frozen=True)
class L9Profile:
    name: str
    description: str
    runner: str
    timeout_minutes: int
    pr_gate: bool
    test: str
    l9_profile: str
    command: tuple[str, ...]
    prebuild: tuple[str, ...] = ()
    artefacts: tuple[str, ...] = ()
    env: Mapping[str, str] = field(default_factory=dict)
    path: Path | None = None

    @property
    def log(self) -> str:
        return f"{LOG_DIR}/{self.name}.log"


def _strings(value: object, what: str, path: Path) -> tuple[str, ...]:
    if not isinstance(value, list) or not all(isinstance(v, str) and v for v in value):
        raise ProfileError(f"{path.name}: {what} must be a list of non-empty strings")
    return tuple(value)


def parse(text: str, path: Path) -> L9Profile:
    """Parse and validate one profile file's text."""
    try:
        data = tomllib.loads(text)
    except tomllib.TOMLDecodeError as e:
        raise ProfileError(f"{path.name}: {e}") from e
    unknown = sorted(set(data) - KNOWN_KEYS)
    if unknown:
        raise ProfileError(f"{path.name}: unknown keys {unknown}")
    missing = [k for k in REQUIRED_KEYS if k not in data]
    if missing:
        raise ProfileError(f"{path.name}: missing keys {missing}")
    name = data["name"]
    if not isinstance(name, str) or path.stem != name:
        raise ProfileError(f"{path.name}: name must equal the file name ({path.stem!r})")
    minutes = data["timeout_minutes"]
    if not isinstance(minutes, int) or isinstance(minutes, bool) or not 1 <= minutes <= 720:
        raise ProfileError(f"{path.name}: timeout_minutes must be 1..720")
    if not isinstance(data["pr_gate"], bool):
        raise ProfileError(f"{path.name}: pr_gate must be true or false")
    for key in ("description", "runner", "test", "l9_profile"):
        if not isinstance(data[key], str) or not data[key]:
            raise ProfileError(f"{path.name}: {key} must be a non-empty string")
    env = data.get("env", {})
    if not isinstance(env, dict) or not all(isinstance(k, str) and isinstance(v, str) for k, v in env.items()):
        raise ProfileError(f"{path.name}: [env] values must be strings")
    return L9Profile(
        name=name,
        description=data["description"],
        runner=data["runner"],
        timeout_minutes=minutes,
        pr_gate=data["pr_gate"],
        test=data["test"],
        l9_profile=data["l9_profile"],
        command=_strings(data["command"], "command", path),
        prebuild=_strings(data.get("prebuild", []), "prebuild", path),
        artefacts=_strings(data.get("artefacts", []), "artefacts", path),
        env=dict(env),
        path=path,
    )


def load_all(directory: Path = HERE) -> list[L9Profile]:
    """Every profile of `directory`, by name."""
    profiles = [parse(p.read_text(encoding="utf-8"), p) for p in sorted(directory.glob("*.toml"))]
    names = [p.name for p in profiles]
    dupes = sorted({n for n in names if names.count(n) > 1})
    if dupes:
        raise ProfileError(f"duplicate profile names {dupes}")
    return profiles


def missing_references(profile: L9Profile, harness: Path = HARNESS) -> list[str]:
    """Files the profile refers to that do not exist: its test file and its l9-scale profile."""
    problems = []
    if not (harness / "tests" / f"{profile.test}.rs").is_file():
        problems.append(f"{profile.name}: tests/{profile.test}.rs does not exist")
    if not (harness / "profiles" / f"{profile.l9_profile}.toml").is_file():
        problems.append(f"{profile.name}: profiles/{profile.l9_profile}.toml does not exist")
    return problems


def plan(profile: L9Profile, commit: str, run_id: str, trend: str) -> list[list[str]]:
    """The commands a run executes, in order (the test command is wrapped to tee its log)."""
    steps: list[list[str]] = []
    if profile.prebuild:
        steps.append(list(profile.prebuild))
    steps.append(list(profile.command))
    steps.append(
        [
            sys.executable, str(HERE / "trend.py"), "append",
            "--trend", trend, "--profile", profile.name, "--runner", profile.runner,
            "--commit", commit, "--run", run_id, "--report", profile.log,
        ]
    )
    return steps


def run(profile: L9Profile, commit: str, run_id: str, trend: str, environ: Mapping[str, str]) -> int:
    """Run a profile; the exit status is the test command's (a red test still gets its trend line)."""
    env = dict(environ)
    env.update(profile.env)
    env["SPOT_REQUIRE_PG"] = "1"
    log = REPO / profile.log
    log.parent.mkdir(parents=True, exist_ok=True)
    steps = plan(profile, commit, run_id, trend)
    status = 0
    for argv in steps[:-1]:
        print(f"== {profile.name}: {' '.join(argv)}", flush=True)
        with log.open("ab") as out:
            proc = subprocess.run(argv, env=env, cwd=REPO, stdout=out, stderr=subprocess.STDOUT, check=False)
        if proc.returncode != 0:
            status = proc.returncode
            break
    (REPO / trend).parent.mkdir(parents=True, exist_ok=True)
    subprocess.run([*steps[-1], "--status", str(status)], env=env, cwd=REPO, check=False)
    return status


def main(argv: Sequence[str] | None = None, environ: Mapping[str, str] | None = None) -> int:
    environ = os.environ if environ is None else environ
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = p.add_subparsers(dest="command", required=True)
    sub.add_parser("list", help="print the profiles")
    r = sub.add_parser("run", help="run one profile")
    r.add_argument("name")
    r.add_argument("--commit", default=environ.get("GITHUB_SHA", "unknown"))
    r.add_argument("--run", default=environ.get("GITHUB_RUN_ID", "0"))
    r.add_argument("--trend", default=DEFAULT_TREND)
    r.add_argument("--dry-run", action="store_true")
    args = p.parse_args(sys.argv[1:] if argv is None else argv)
    profiles = {x.name: x for x in load_all()}
    if args.command == "list":
        for x in profiles.values():
            gate = "pr-gate" if x.pr_gate else "nightly"
            print(f"{x.name}\t{x.runner}\t{x.timeout_minutes} min\t{gate}\t{x.description}")
        return 0
    profile = profiles.get(args.name)
    if profile is None:
        print(f"no L9 profile named {args.name!r} (have: {', '.join(profiles)})", file=sys.stderr)
        return 2
    if args.dry_run:
        for step in plan(profile, args.commit, args.run, args.trend):
            print(" ".join(step))
        return 0
    return run(profile, args.commit, args.run, args.trend, environ)


if __name__ == "__main__":
    sys.exit(main())
