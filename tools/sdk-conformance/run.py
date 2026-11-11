"""The guest-SDK conformance harness (WP-STR-07.T1).

Builds the same sample app from each language template in a pinned container image, then runs
the contract kit (``tools/sdk-conformance/runner``, a Rust binary over ``spot-wasm``) against the
component it produced. The result is one JSON matrix and one Markdown table. A language whose
toolchain cannot produce a component that links on the no-WASI host is recorded as
``BLOCKED(<step>, <reason>)`` — never faked.

    python tools/sdk-conformance/run.py --list              # the matrix, no containers
    python tools/sdk-conformance/run.py --run               # build and test every language
    python tools/sdk-conformance/run.py --run --lang python # one language
    python tools/sdk-conformance/run.py --run --engine local --lang rust
    python tools/sdk-conformance/run.py --run --dry-run     # print the commands only

The harness is standard-library-only; only the contract kit links ``spot-wasm`` (and therefore
``wasmtime``), so the Python side needs no containers and no Rust to be tested.
"""

from __future__ import annotations

import argparse
import json
import shutil
import subprocess
import sys
from pathlib import Path

import contract
import images
import matrix

#: The repository root, from this file's location (``<repo>/tools/sdk-conformance/run.py``).
REPO_ROOT = Path(__file__).resolve().parents[2]

#: This directory (``tools/sdk-conformance``).
HERE = Path(__file__).resolve().parent


def container_command(
    engine: str, repo: Path, template_rel: str, image: str, shell_command: str
) -> list[str]:
    """The command that runs ``shell_command`` in ``image`` with the repo at ``/work``.

    The working directory is the template, so the command can call ``./build.sh`` and the
    harness's scripts by their repository paths.
    """
    return [
        engine,
        "run",
        "--rm",
        "-v",
        f"{repo}:/work",
        "-w",
        f"/work/{template_rel}",
        image,
        "sh",
        "-lc",
        shell_command,
    ]


def local_command(shell_command: str) -> list[str]:
    """The command that runs ``shell_command`` on the host (``--engine local``)."""
    return ["sh", "-lc", shell_command]


def runner_command(
    runner: Path, component: Path, cases: Path, language: str, out: Path
) -> list[str]:
    """The contract kit invocation for one component."""
    return [
        str(runner),
        "--component",
        str(component),
        "--cases",
        str(cases),
        "--language",
        language,
        "--out",
        str(out),
    ]


def outcome_from_runner_report(
    language: str, image: str, toolchain: str, report: dict
) -> matrix.Outcome:
    """Maps the contract kit's JSON report onto a matrix row.

    A report with a ``blocked`` step is a ``BLOCKED`` row; a report whose cases all passed is
    ``SUPPORTED``; anything else (it linked but a case disagreed) is ``FAILED``.
    """
    blocked = report.get("blocked")
    if blocked:
        return matrix.blocked(
            language,
            str(blocked.get("step", "unknown")),
            str(blocked.get("reason", "no reason given")),
            image=image,
            toolchain=toolchain,
            binary_bytes=report.get("binary_bytes"),
            notes=list(report.get("notes", [])),
        )
    cases = list(report.get("cases", []))
    passed = bool(report.get("passed")) and bool(cases)
    return matrix.Outcome(
        language=language,
        status=matrix.SUPPORTED if passed else matrix.FAILED,
        image=image,
        toolchain=toolchain,
        binary_bytes=report.get("binary_bytes"),
        component_sha256=report.get("component_sha256"),
        imports=list(report.get("imports", [])),
        cases=cases,
        notes=list(report.get("notes", [])),
    )


def _last_line(text: str) -> str:
    """The last non-empty line of a command's output, for a one-line blocked reason."""
    lines = [line.strip() for line in text.splitlines() if line.strip()]
    return lines[-1] if lines else ""


def _failure_reason(result: subprocess.CompletedProcess) -> str:
    """A one-line reason for a failed command.

    Prefers the first line that looks like an error (it contains ``error`` or ``not found``) over
    the last line, because build tools end with a summary line ("Time Elapsed ...") that says
    nothing.
    """
    combined = (result.stderr or "") + "\n" + (result.stdout or "")
    for line in combined.splitlines():
        text = line.strip()
        if text and ("error" in text.lower() or "not found" in text.lower()):
            return text
    return _last_line(result.stderr) or _last_line(result.stdout) or f"exit {result.returncode}"


def _detect_engine(requested: str) -> str:
    """``local``, or the first of podman/docker that is installed."""
    if requested != "auto":
        return requested
    for candidate in ("podman", "docker"):
        if shutil.which(candidate):
            return candidate
    return "none"


def _run(
    command: list[str], *, cwd: Path | None, timeout: int
) -> subprocess.CompletedProcess:
    """Runs a command, capturing text output, never raising on a non-zero exit or a timeout.

    A timeout becomes a completed process with return code 124 and a ``timed out`` message, so a
    slow build is a ``BLOCKED(build, ...)`` row rather than a crash.
    """
    try:
        return subprocess.run(
            command, cwd=cwd, capture_output=True, text=True, timeout=timeout, check=False
        )
    except subprocess.TimeoutExpired:
        return subprocess.CompletedProcess(
            command, 124, stdout="", stderr=f"timed out after {timeout}s"
        )


def run_language(
    entry: images.LanguageImage,
    config: images.HarnessConfig,
    *,
    engine: str,
    cases_path: Path,
    runner_path: Path,
    timeout: int,
) -> matrix.Outcome:
    """Builds and tests one language, returning its matrix row.

    Every failure is a ``BLOCKED`` row naming the step: ``image``, ``engine``, ``pull``,
    ``build``, ``component``, ``runner`` or ``contract`` (plus the steps the contract kit itself
    reports: ``parse``, ``export``, ``instantiate``).
    """
    language = entry.language
    template_dir = REPO_ROOT / "sdk" / "templates" / language
    common = {"image": entry.image, "toolchain": entry.toolchain}

    if not entry.pinned:
        return matrix.blocked(language, "image", f"{entry.image!r} is not pinned", **common)
    if engine == "none":
        return matrix.blocked(
            language, "engine", "no podman or docker is installed for a container build", **common
        )
    if engine != "local" and shutil.which(engine) is None:
        return matrix.blocked(language, "engine", f"{engine} is not on PATH", **common)

    if engine != "local":
        pull = _run([engine, "pull", entry.image], cwd=None, timeout=timeout)
        if pull.returncode != 0:
            reason = _last_line(pull.stderr) or _last_line(pull.stdout) or "the pull failed"
            return matrix.blocked(language, "pull", reason, **common)

    if engine == "local":
        command = local_command(entry.build)
        cwd: Path | None = template_dir
    else:
        shell = f"{entry.provision} && {entry.build}" if entry.provision else entry.build
        command = container_command(
            engine, REPO_ROOT, f"sdk/templates/{language}", entry.image, shell
        )
        cwd = None

    # A component left by an earlier run must never be mistaken for this build's output.
    (template_dir / config.component).unlink(missing_ok=True)
    build = _run(command, cwd=cwd, timeout=timeout)
    if build.returncode != 0:
        return matrix.blocked(language, "build", _failure_reason(build), **common)

    component = template_dir / config.component
    if not component.is_file():
        return matrix.blocked(
            language, "component", f"the build produced no {config.component}", **common
        )

    if not runner_path.is_file():
        return matrix.blocked(
            language,
            "runner",
            f"the contract kit {runner_path} is not built; run `cargo build --release` in "
            "tools/sdk-conformance/runner",
            **common,
        )

    out_json = template_dir / "out" / "conformance.json"
    run = _run(
        runner_command(runner_path, component, cases_path, language, out_json),
        cwd=None,
        timeout=timeout,
    )
    if run.returncode != 0:
        reason = _last_line(run.stderr) or _last_line(run.stdout) or f"exit {run.returncode}"
        return matrix.blocked(language, "contract", reason, **common)
    try:
        report = json.loads(out_json.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        return matrix.blocked(language, "contract", f"the report was unreadable: {error}", **common)
    return outcome_from_runner_report(language, entry.image, entry.toolchain, report)


def _list_rows(
    entries: list[images.LanguageImage], cases: list[dict]
) -> list[matrix.Outcome]:
    """One ``NOT_RUN`` row per language, for ``--list``."""
    return [
        matrix.Outcome(
            language=entry.language,
            status=matrix.NOT_RUN,
            image=entry.image,
            toolchain=entry.toolchain,
            notes=[f"{len(cases)} contract cases; not built (run --run)"],
        )
        for entry in entries
    ]


def _print_list(
    config: images.HarnessConfig, entries: list[images.LanguageImage], cases: list[dict]
) -> None:
    """Prints the configured matrix without building anything."""
    print("Guest SDK conformance matrix (WP-STR-07.T1)")
    print()
    print(f"harness: runner={config.runner}")
    print(f"         cases={config.cases} ({len(cases)} contract cases)")
    print(f"         component={config.component}")
    print(f"         wasm-tools={config.wasm_tools_version}")
    print()
    print(matrix.markdown_table(_list_rows(entries, cases)))
    print()
    print("Supported languages are those a `--run` proves build and pass every case;")
    print("unsupported ones are recorded as BLOCKED(<step>, <reason>). Rust is the reference.")


def parse_args(argv: list[str]) -> argparse.Namespace:
    """Parses the command line."""
    parser = argparse.ArgumentParser(
        prog="run.py", description="Guest SDK conformance harness (WP-STR-07.T1)"
    )
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--list", action="store_true", help="print the configured matrix only")
    mode.add_argument("--run", action="store_true", help="build and test the languages")
    parser.add_argument("--lang", action="append", default=[], help="limit to one language")
    parser.add_argument(
        "--engine",
        default="auto",
        choices=("auto", "podman", "docker", "local"),
        help="the container engine, or `local` to build on the host",
    )
    parser.add_argument("--json", help="write the matrix JSON here")
    parser.add_argument("--markdown", help="write the matrix Markdown here")
    parser.add_argument("--timeout", type=int, default=3600, help="per-command timeout, seconds")
    parser.add_argument("--dry-run", action="store_true", help="print commands, run nothing")
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    """The entry point."""
    args = parse_args(sys.argv[1:] if argv is None else argv)
    config, entries = images.load_images(HERE / "images.toml")
    cases = contract.load_cases(HERE / config.cases)

    if args.lang:
        wanted = set(args.lang)
        unknown = wanted - {entry.language for entry in entries}
        if unknown:
            print(f"unknown language(s): {', '.join(sorted(unknown))}", file=sys.stderr)
            return 2
        entries = [entry for entry in entries if entry.language in wanted]

    if not args.run:
        _print_list(config, entries, cases)
        return 0

    engine = _detect_engine(args.engine)
    runner_path = REPO_ROOT / "tools" / "sdk-conformance" / config.runner
    cases_path = HERE / config.cases

    if args.dry_run:
        for entry in entries:
            template_rel = f"sdk/templates/{entry.language}"
            if engine == "local":
                print(f"[{entry.language}] local: {entry.build}")
            else:
                shell = (
                    f"{entry.provision} && {entry.build}" if entry.provision else entry.build
                )
                print(
                    " ".join(
                        container_command(engine, REPO_ROOT, template_rel, entry.image, shell)
                    )
                )
        return 0

    outcomes = [
        run_language(
            entry,
            config,
            engine=engine,
            cases_path=cases_path,
            runner_path=runner_path,
            timeout=args.timeout,
        )
        for entry in entries
    ]

    text = matrix.to_json(outcomes)
    if args.json:
        Path(args.json).write_text(text + "\n", encoding="utf-8")
    table = matrix.markdown_table(outcomes)
    if args.markdown:
        Path(args.markdown).write_text(table + "\n", encoding="utf-8")

    print(table)
    print()
    passed, problems = matrix.overall(outcomes)
    if passed:
        print("matrix passed: every supported language passed every case; every blocked row has a reason")
        return 0
    print("matrix failed:")
    for problem in problems:
        print(f"  - {problem}")
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
