# Linux module conformance harness (WP-ADM-04.T3)

Proves WP-ADM-04's acceptance — **the module suite is green on Fedora, a RHEL-compatible
distribution and Ubuntu LTS** — by running the real `spot-agent-linux` and
`spot-agent-linux-sys` handlers against genuine distributions, not transcripts. T1 and T2
test against recorded command transcripts; this harness runs the modules inside a Fedora
44, an AlmaLinux 10 and an Ubuntu 24.04 LTS container (and, for the modules a container
cannot exercise, inside qemu cloud-image VMs).

The suite is defined by `matrix.toml`; the runner is `run.sh` (containers) and `vm/run.sh`
(VMs). Both are thin wrappers over the stdlib-only Python package `agent_linux_harness/`.

## What is where

| Path | What |
| --- | --- |
| `matrix.toml` | The single source of truth: the targets, the capabilities each gives, and the real tests, each with the capabilities it needs |
| `run.sh` | Container entry point: `run.sh [--list\|--dry-run\|...]` |
| `agent_linux_harness/` | The orchestrator (stdlib only): matrix loading, planning, container argv, JUnit XML, the CLI |
| `tests/` | Python unit tests for the orchestrator, driven by a stubbed runner |
| `tests-real/` | The standalone cargo crate `spot-agent-linux-real`: the `#[ignore]`d real-system tests |
| `docker/` | The per-distribution images the harness builds (base image + the tools the tests drive) |
| `vm/run.sh` | qemu cloud-image runner for the `vm_only` tests (not run by CI by default) |

## Running

The harness **refuses to run on a bare-metal host**: it needs `SPOT_HARNESS_DISPOSABLE=1`
and a root that is a container or a VM (`/.dockerenv`, `/run/.containerenv`,
`systemd-detect-virt`, or the init cgroup). That check is in
`agent_linux_harness/orchestrator.py` (`check_disposable`) and is unit-tested.

```console
$ ./run.sh --list          # show the targets and tests; no guard needed
$ ./run.sh --dry-run       # show what would run; no guard needed
$ SPOT_HARNESS_DISPOSABLE=1 ./run.sh                 # the whole container matrix
$ SPOT_HARNESS_DISPOSABLE=1 ./run.sh --target fedora-44 --target ubuntu-2404
$ SPOT_HARNESS_DISPOSABLE=1 ./run.sh --runtime docker
```

For each target the runner builds the image (`docker/<target>/Dockerfile`), starts the
container detached, waits for systemd, runs each runnable test with `exec` on its own
(`--ignored --exact`), and removes the container. Results are written as JUnit XML under
`junit/<target>.xml` and printed as a matrix:

```
test                                      fedora-44         almalinux-10      ubuntu-2404
----------------------------------------------------------------------------------------
distro_family_matches_target              PASS              PASS              PASS
...
mac_status_selinux                        SKIP-vm           SKIP-vm           SKIP-vm
```

Exit code is non-zero when any run test failed or a target could not be set up.

### Why the container is started detached and the test is run with `exec`

A test that needs `systemctl` needs a real systemd as PID 1, which podman gives with
`--systemd=always`. But then systemd must be the entry point, so the test binary cannot be
the entry point too. The harness therefore starts the container detached (systemd or, for a
target without systemd, a keep-alive shell), runs each test with `<runtime> exec`, and
removes the container at the end.

### The built test binary

`run.sh` builds `tests-real/` with `cargo test --no-run --message-format=json`, finds the
test executable, and mounts it read-only into the container at `/opt/spot/real-tests`. The
binary is built on the host and run in the container, so the host's glibc must be no newer
than the container's (all three targets ship a recent glibc; the CI runner is Ubuntu LTS).
Pass `--binary PATH` to reuse a binary you built yourself.

## The matrix

**Capabilities** are a fixed vocabulary (`agent_linux_harness/matrix.py`). A target gives
some; a test needs some. A test runs on a target when every capability it needs is present;
otherwise it is skipped with the missing capability named. The vocabulary:

| Capability | Meaning |
| --- | --- |
| `systemd` | systemd is PID 1 (`--systemd=always`), so `systemctl` is real |
| `privileged` | the container runs `--privileged` (mount, loop devices) |
| `packages` | the distribution package manager is present and usable |
| `network` | outbound package-manager network access |
| `btrfs_loop` | a btrfs filesystem can be made on a loop device |
| `lvm_loop` | an LVM volume group can be made on a loop device |
| `sysctl` | `/proc/sys` is writable for the allow-listed keys |
| `chrony` | chrony is installed |
| `selinux`, `apparmor`, `firewalld`, `ufw` | the LSM / firewall the module reads |

`selinux`, `apparmor`, `firewalld` and `ufw` are **vm-only**: a container shares the host's
LSM and firewall, so testing them in a container would test the host, not the distribution.
Every test that needs one is `vm_only = true` and is reported as `SKIPPED (needs vm)` in a
container run.

### The VM targets

`vm/run.sh <target>` boots a cloud image under qemu, copies the built test binary in over
ssh, runs the tests and destroys the VM. It needs `qemu-system-x86_64`, `qemu-img`,
`cloud-localds` (cloud-utils) and `ssh`, and it is **not run by CI by default**. It reads
the image URL and the target list from `matrix.toml`:

```console
$ SPOT_HARNESS_DISPOSABLE=1 ./vm/run.sh --all
$ SPOT_HARNESS_DISPOSABLE=1 ./vm/run.sh ubuntu-2404-vm
```

## The real tests

Every test in `tests-real/tests/real.rs` is `#[ignore]`d, so `cargo test` on a developer's
host compiles the crate and runs nothing. The harness runs them with `--ignored --exact`.
Each test exercises the module's public `plan`/`apply`/`verify` (part 1) or its typed
methods (part 2) against the genuine system, and cleans up anything it creates.

Two tests are deliberately lenient about a daemon that a minimal image may not run:
`time_status_is_read` accepts a typed `tool_missing`/`command_failed` error from `chronyc`
rather than panicking, and `network_facts_are_read` reads interfaces and routes (through
`ip`) without requiring `systemd-resolved`. Both still prove the real command path is
wired and returns typed results; they never assert a fake success.

## Tests of the harness itself

`tests/` holds 61 Python unit tests for the orchestrator, driven by `StubRunner`: the
capability decisions, the exact container argv (podman vs docker, systemd vs not,
privileged vs not), the safety gate, the cargo artifact parser, the libtest-to-JUnit
conversion, and a full execute loop with scripted pass/fail/skip results.

```console
$ python -m unittest discover -s harness/agent-linux/tests -v
$ bash -n harness/agent-linux/run.sh
```
