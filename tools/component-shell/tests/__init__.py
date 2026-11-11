"""The build tool's suite: one package, so both discovery commands find the same tests.

``python3 -m unittest discover -s tools/component-shell`` (the task's acceptance command) and
``python3 -m unittest discover -s tools/component-shell/tests`` (what ``tools/ci/run_python_suites.py``
runs, and what the README documents) have to agree on what the suite is. The second form imports the
test modules from the tests directory itself, so ``import support`` resolves there; the first form
imports them as ``tests.test_*`` from the tool directory, and this line puts the directory that holds
``support.py`` on the path so the same modules import unchanged. It is test-only scaffolding: nothing
in the tool imports this package.
"""

from __future__ import annotations

import sys
from pathlib import Path

_DIRECTORY = str(Path(__file__).resolve().parent)
if _DIRECTORY not in sys.path:
    sys.path.insert(0, _DIRECTORY)
