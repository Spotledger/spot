"""The Linux module conformance harness (WP-ADM-04.T3).

A small, stdlib-only toolkit that reads ``matrix.toml`` and runs the real-system
integration tests of ``spot-agent-linux`` and ``spot-agent-linux-sys`` inside Fedora, a
RHEL-compatible distribution and Ubuntu LTS containers (or qemu VMs). ``run.sh`` is the
entry point; ``vm/run.sh`` boots the VM targets. See ``README.md``.
"""

from .matrix import (
    CAPABILITIES,
    Matrix,
    Target,
    TestCase,
    load_matrix,
)
from .orchestrator import (
    RunPlan,
    TargetPlan,
    TestDecision,
    plan,
    select_targets,
)
from .runner import CommandResult, StubRunner, SubprocessRunner

__all__ = [
    "CAPABILITIES",
    "CommandResult",
    "Matrix",
    "RunPlan",
    "StubRunner",
    "SubprocessRunner",
    "Target",
    "TargetPlan",
    "TestCase",
    "TestDecision",
    "load_matrix",
    "plan",
    "select_targets",
]

__version__ = "0.1.0"
