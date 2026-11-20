"""Path bootstrap for the profile tests: the profile modules and the harness modules."""

from __future__ import annotations

import os
import sys

TESTS = os.path.dirname(os.path.abspath(__file__))
PROFILES = os.path.dirname(TESTS)
HARNESS = os.path.dirname(PROFILES)
for path in (PROFILES, HARNESS):
    if path not in sys.path:
        sys.path.insert(0, path)
