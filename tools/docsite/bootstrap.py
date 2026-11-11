#!/usr/bin/env python3
"""Guarantee that the docsite's pinned dependencies are importable.

``ci.yml`` runs every Python suite (and the acceptance job every build
command) with a bare `setup-python` interpreter that never runs pip, while
this tool is written against the pins in ``requirements.txt`` (``PyYAML``,
``Jinja2``, ``markdown-it-py``). ensure_deps() returns as soon as the current
interpreter can import them; otherwise it walks a fallback ladder:

  (a) install the pins into a throwaway venv outside the repository (the
      convention ``tools/lints/native-api/run.sh`` uses for the contract
      checks) and re-exec the current command with that interpreter;
  (b) if the venv route fails, install with ``pip install --user`` into the
      current interpreter's user site and re-probe the imports in-process
      (no re-exec needed);
  (c) if both fail, exit with a message listing every attempted path and the
      captured pip output, so the CI log says WHY instead of hiding it.

Nothing weakens or skips a gate: without the pins the build and the tests
refuse to run.
"""
from __future__ import annotations

import importlib
import os
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
REQUIREMENTS = HERE / "requirements.txt"
MODULES = ("yaml", "jinja2", "markdown_it")
TAIL_LINES = 25  # pip output lines echoed per failed attempt


def missing() -> list[str]:
    """The pinned modules this interpreter cannot import, or an empty list."""
    absent = []
    for name in MODULES:
        try:
            __import__(name)
        except ImportError:
            absent.append(name)
    return absent


def venv_dir() -> Path:
    """Where the throwaway venv lives (never inside the repository)."""
    base = (os.environ.get("RUNNER_TEMP") or os.environ.get("TMPDIR")
            or os.environ.get("TEMP") or "/tmp")
    return Path(base) / "spot-docsite-venv"


def _python(venv: Path) -> Path:
    return venv / ("Scripts/python.exe" if os.name == "nt" else "bin/python")


def _target(python: Path) -> list[str]:
    """Rebuild the current command so it runs with the venv's interpreter."""
    argv0, rest = sys.argv[0], sys.argv[1:]
    if os.path.basename(argv0) == "__main__.py" and _parent(argv0) == "unittest":
        return [str(python), "-m", "unittest", *rest]  # `python -m unittest ...`
    if " -m " in argv0:  # 3.14: argv[0] is the command string "python -m unittest"
        return [str(python), "-m", argv0.split(" -m ", 1)[1].strip(), *rest]
    return [str(python), argv0, *rest]


def _parent(argv0: str) -> str:
    try:
        return Path(argv0).resolve().parent.name
    except OSError:  # pragma: no cover - argv[0] always resolves here
        return ""


def _tail(*chunks: str) -> str:
    """The last TAIL_LINES lines of the captured output, for the CI log."""
    lines: list[str] = []
    for chunk in chunks:
        lines.extend((chunk or "").splitlines())
    return "\n".join(lines[-TAIL_LINES:])


def _run(cmd: list[str]) -> subprocess.CompletedProcess:
    """Run a command quietly, echoing the output tail when it fails.

    No ``--quiet``: the failure reason (network, missing pip, resolver
    errors) must land in the CI log the moment it happens, not only in the
    final give-up message.
    """
    proc = subprocess.run(cmd, capture_output=True, text=True, errors="replace")
    if proc.returncode != 0:
        sys.stderr.write(f"docsite-bootstrap: `{' '.join(cmd)}` "
                         f"failed (exit {proc.returncode}):\n")
        sys.stderr.write(_tail(proc.stdout, proc.stderr) + "\n")
        sys.stderr.flush()
    return proc


def _pip_hint(proc: subprocess.CompletedProcess) -> str:
    """Name the actionable causes this run can recognise, else ''."""
    blob = (proc.stdout or "") + (proc.stderr or "")
    if "No module named pip" in blob:
        return " — this interpreter has no pip (`python -m pip` is unavailable here)"
    return ""


def _pip_install(python: str, user: bool = False) -> list[str]:
    cmd = [python, "-m", "pip", "install", "--disable-pip-version-check"]
    if user:
        cmd.append("--user")
    return cmd + ["-r", str(REQUIREMENTS)]


def _probe_interpreter(python: str) -> bool:
    probe = subprocess.run([python, "-c", "import " + ", ".join(MODULES)],
                           capture_output=True, text=True, errors="replace")
    if probe.returncode != 0:
        sys.stderr.write("docsite-bootstrap: the pins are still not importable "
                         f"in {python}:\n")
        sys.stderr.write(_tail(probe.stdout, probe.stderr) + "\n")
        sys.stderr.flush()
    return probe.returncode == 0


def _install_into_venv(venv: Path, notes: list[str]) -> str | None:
    """Return the venv interpreter once it can import the pins, else None."""
    python = _python(venv)
    if not python.exists():
        proc = _run([sys.executable, "-m", "venv", str(venv)])
        if proc.returncode != 0:
            notes.append(f"creating a venv at {venv} with {sys.executable} "
                         f"failed{_pip_hint(proc)}")
            return None
    proc = _run(_pip_install(str(python)))
    if proc.returncode != 0:
        notes.append(f"`pip install -r {REQUIREMENTS}` into the venv {venv} "
                     f"failed (exit {proc.returncode}){_pip_hint(proc)}")
        return None
    if not _probe_interpreter(str(python)):
        notes.append(f"the pins are still not importable in {python} after "
                     "the venv install")
        return None
    return str(python)


def _install_user(notes: list[str]) -> bool:
    """pip install --user into the current interpreter; re-probe in-process."""
    if os.environ.get("PYTHONNOUSERSITE"):
        notes.append(f"skipped `pip install --user` into {sys.executable}: "
                     "PYTHONNOUSERSITE is set, user site is disabled")
        return False
    proc = _run(_pip_install(sys.executable, user=True))
    if proc.returncode != 0:
        notes.append(f"`pip install --user -r {REQUIREMENTS}` into "
                     f"{sys.executable} failed (exit {proc.returncode})"
                     f"{_pip_hint(proc)}")
        return False
    importlib.invalidate_caches()
    still = missing()
    if still:
        notes.append(f"the pins are still not importable in {sys.executable} "
                     f"after the --user install (missing: {', '.join(still)})")
        return False
    sys.stderr.write(f"docsite-bootstrap: pinned dependencies installed into "
                     f"the user site of {sys.executable}\n")
    sys.stderr.flush()
    return True


def ensure_deps() -> None:
    """Return when this interpreter has the pins; otherwise install via the
    fallback ladder (venv + re-exec, then pip --user) or exit with a full
    diagnostic of every attempt."""
    if not missing():
        return
    if os.environ.get("SPOT_DOCSITE_REEXEC") == "1":
        sys.exit("docsite: the pinned dependencies (" + ", ".join(missing())
                 + ") are still missing after the bootstrap re-exec, "
                 "refusing to loop")
    if not REQUIREMENTS.is_file():
        sys.exit(f"docsite: {REQUIREMENTS} not found — the pinned dependencies "
                 "cannot be installed from a checkout that lacks it")
    notes: list[str] = []
    python = _install_into_venv(venv_dir(), notes)
    if python is not None:
        os.environ["SPOT_DOCSITE_REEXEC"] = "1"
        os.execv(python, _target(Path(python)))
        return  # unreachable: execv only returns if it failed
    if _install_user(notes):
        return
    lines = ["docsite: could not make the pinned dependencies (PyYAML, "
             "Jinja2, markdown-it-py) importable. Attempts:"]
    lines += [f"  {i}. {note}" for i, note in enumerate(notes, 1)]
    lines.append("Fix the runner (network access for pip, or a python with "
                 "pip+venv) and re-run. See the docsite-bootstrap: lines "
                 "above for the captured output of each attempt.")
    sys.exit("\n".join(lines))
