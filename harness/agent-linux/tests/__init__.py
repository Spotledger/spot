"""Tests for the agent_linux_harness package; runnable via python -m unittest discover."""

import sys
from pathlib import Path

HARNESS_DIR = Path(__file__).resolve().parents[1]
REAL_CRATE_DIR = HARNESS_DIR / "tests-real"
MATRIX_PATH = HARNESS_DIR / "matrix.toml"

sys.path.insert(0, str(HARNESS_DIR))
