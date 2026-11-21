# Windows module conformance suite

Proves WP-ADM-05's acceptance: **the Windows module suite is green on Windows Server,
Windows 10 and 11** (blueprint WS22, ADR-44). It runs the real `#[cfg(windows)]`
implementations of `spot-agent-windows` (services, updates, the firewall, users) and
`spot-agent-windows-sys` (Volume Shadow Copy, certificates) against real Windows
machines, on a disposable Windows VM runner.

The Linux CI host cannot run any of this, so the split is:

| Piece | Runs on | What it does |
| --- | --- | --- |
| `matrix.toml` | every host | The checked-in definition: the target operating systems, their required capabilities and the real-system test list. |
| `orchestrator.py` | every host | Reads and validates `matrix.toml`, gates each test on its target's capabilities, drives a `Runner`, aggregates JUnit XML and prints the matrix. |
| `tests/` | Linux CI | Unit tests for the orchestration with a **stubbed runner** (no Windows, no `pwsh`, no cargo), plus structural tests for `run.ps1`. |
| `run.ps1` | Windows VM | The safety guards, the build, the run, the JUnit collection. |
| `tests-real/` | Windows VM | The `spot-agent-windows-real` cargo crate: the real tests. |

## Running on the disposable Windows VM runner

The runner is the disposable Windows Server 2022 evaluation VM labelled `windows-vm` in
`matrix.toml`. Two things must be true before it will run:

1. **`SPOT_HARNESS_DISPOSABLE=1`** — the suite creates and deletes a service, a local
   account and a firewall rule (and a shadow copy), so it only runs on a machine that may
   be modified.
2. **The machine is a virtual machine** — checked through CIM
   `Win32_ComputerSystem.Model`/`Manufacturer` and `HypervisorPresent`. A physical host
   is refused.

Python 3.11 or newer (for `matrix.toml`), Rust from `rust-toolchain.toml` and an elevated
shell are also required.

```powershell
# See the matrix without touching the machine:
./run.ps1 -ListMatrix

# Run every test for this machine's target:
$env:SPOT_HARNESS_DISPOSABLE = '1'
./run.ps1 -Target windows-server-2022

# One test, as orchestrator.py drives it:
./run.ps1 -Target windows-11 -Test services -Json

# No reachable Windows Update source on this VM:
$env:SPOT_HARNESS_OFFLINE = '1'; ./run.ps1 -Target windows-10-22h2
```

JUnit XML lands under `artifacts/junit/<target>/<test>.xml`. `run.ps1` exits non-zero when
any test failed.

To run the whole matrix across the four targets, use `orchestrator.py` with a `pwsh` on
each machine (it invokes `run.ps1` per test):

```bash
python orchestrator.py matrix --json          # the definition, as JSON
python orchestrator.py run --target windows-11 --test services
```

## Throw-away objects only

Every object a test creates is named with a unique per-run prefix that `run.ps1`
generates and passes in `SPOT_HARNESS_PREFIX` (`spot-h-` plus eight hex digits, short
enough that a local account name stays inside Windows' 20-character limit). Each test:

- refuses to start if an object with its name already exists;
- asserts, before every change, that the name carries the prefix
  (`spot_agent_windows_real::assert_ours`);
- enumerates the machine and asserts that the only objects carrying the prefix are its
  own;
- removes its objects through a `Cleanup` guard whose `Drop` is the Rust equivalent of a
  `finally`: it runs on the success path and on a panic.

`run.ps1` additionally sweeps any object still carrying the prefix in its own `finally`,
and only prefixed ones.

## Adding a target or a test

Edit `matrix.toml`:

- a new `[[targets]]` entry lists the operating system, the image, the runner labels and
  the capabilities it offers (a capability a target does not offer makes its dependent
  tests **skip**, never silently pass);
- a new `[[tests]]` entry names a cargo integration-test target under `tests-real/tests/`;
  add `Test-Capability` support in `run.ps1` if it needs a capability that does not exist
  yet.

The Linux suite (`tests/test_matrix.py`) fails if a target or test is malformed, if a
capability is unknown, or if a listed test file does not exist.

## Not run on the Linux host

`tests-real/` is deliberately excluded from the repository workspace and fails to compile
off Windows, so a "green" run that built nothing is impossible. What the Linux CI host
verifies about it is structural: the matrix names real files, `run.ps1` has the guards
and the cleanup, and the orchestration gates and aggregates correctly.
