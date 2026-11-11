# Packaging verification on real distributions

`tools/packaging/build.py` proves what can be proved without root. This directory documents
the other half: installing, upgrading and removing the packages on a real Fedora 44, a real
RHEL-compatible AlmaLinux 10 and a real Ubuntu 24.04, inside throwaway systemd containers,
and running the shipped runtime image as a customer would.

The driver is `tools/packaging/verify/run.py` (standard library only). It **builds no
package**: it takes the artefacts `build.py` wrote.

```sh
python3 tools/packaging/build.py rpm       --binary target/release/spot --version 1.2.3 --out dist
python3 tools/packaging/build.py rpm       --binary target/release/spot --version 1.2.4 --out dist
python3 tools/packaging/build.py deb       --binary target/release/spot --version 1.2.3 --out dist
python3 tools/packaging/build.py deb       --binary target/release/spot --version 1.2.4 --out dist
python3 tools/packaging/build.py container --binary target/release/spot --version 1.2.4 --out dist

python3 tools/packaging/verify/run.py --distro all --package-dir dist --binary target/release/spot
```

Two versions are needed for the upgrade and downgrade scenarios: one version alone makes
them report `skip`, with the reason, rather than passing.

## Flags

| Flag | What it does |
| --- | --- |
| `--distro fedora-44\|almalinux-10\|ubuntu-2404\|all` | what to prove (default `all`) |
| `--package-dir DIR` | where `build.py` wrote the `.rpm`/`.deb` files (default `dist`) |
| `--binary PATH` | the release binary the container-image scenario builds with; without it that scenario skips |
| `--scenario NAME` | only this scenario, repeatable |
| `--keep` | keep the containers and the fixture directory for inspection |
| `--dry-run` | print the plan and every argument list, start nothing |
| `--list` | list the distributions, the scenarios and the per-distro expectations |
| `--report FILE` | write the JSON result report |

Exit codes: `0` everything passed or skipped, `1` a scenario failed or errored, `2` bad
usage, `77` podman is not installed (a skip, so CI without podman is not red).

## What you need

- **Rootless podman 4.4 or newer.** The CI runner user is not root and is not in the
  `docker` group; rootless podman is the supported path (the CI runner notes). The
  driver passes `--security-opt label=disable` and `z` mount labels so it works on an
  SELinux host.
- **cgroup v2.** A systemd inside a rootless container needs the unified hierarchy; the
  driver prints a note when `/sys/fs/cgroup/cgroup.controllers` is missing.
- **Network access for the first run.** The harness images install `systemd` and `curl` from
  the distribution's own repository. Nothing else is fetched: the packages under test come
  from `--package-dir`.
- About 1.5 GB of images (`harness/packaging-linux/*`), reused between runs.

## What each scenario proves

The table is data in `tools/packaging/verify/scenarios.py`; `--list` prints it with the
per-distro expectations. All of them run in order on one fresh container per distribution,
and a scenario whose prerequisite did not pass is reported as `skip` with that reason
instead of running against a half-installed system.

| Scenario | What a pass means |
| --- | --- |
| `clean_install` | the package installs; `spot` exists with no login shell; `/etc/spot/spot.toml` is `0640 root:spot` and marked `noreplace` (`%config(noreplace)`, or a conffile on DEB); the unit is installed `0644`, **not** enabled and **not** started; `systemd-analyze verify` accepts the installed unit; `/var/lib/spot` is `0750 spot:spot` |
| `start_and_health` | after an explicit `systemctl start` the health endpoint answers on loopback within 60 s; `Restart=on-failure` is what systemd reports, and a `kill -9` of the main process produces a new MainPID within 30 s; `systemctl stop` finishes inside the unit's own `TimeoutStopSec` (read with `systemctl show`, not hard-coded) |
| `upgrade` | install N, edit `node_id` in `/etc/spot/spot.toml`, write a marker into `/var/lib/spot`, start the service, install N+1: the installed version becomes N+1, the edit survives, the marker survives byte for byte, and the service is restarted exactly once — or not restarted at all where the packaging has no restart snippet (see the per-distro policy below). A `.rpmnew`/`.dpkg-dist` is reported, not required: it only appears when the *shipped template* changed between the two builds, which the driver cannot synthesise without editing `packaging/linux/etc` |
| `downgrade` | installing N over N+1 never silently downgrades: the installed version is still N+1 afterwards, and the exit code and first line of the package manager's answer are recorded. RPM's `dnf install` reports what is installed and leaves it; DEB's `apt-get install` refuses and exits non-zero |
| `removal` | `remove` keeps the config (DEB) or saves a modified one as `.rpmsave` (RPM) and always keeps the data; `purge` on DEB then deletes the conffiles; the binary, the unit and the package record are gone; the `spot` account is left behind (no scriptlet deletes an account that may own data); two canary files the package does **not** install survive both steps |
| `container_image` | `podman build` of `packaging/linux/container/Containerfile` with the release binary succeeds; the container runs as the image's non-root user with a read-only root filesystem; the image's own `HEALTHCHECK` turns `healthy`; `podman stop` ends it with exit code 0 inside the 30 s grace. This one runs once per invocation, not once per distro |

### Per-distro expectations

The RPM and the DEB do not behave the same, so the expectations are data
(`Policy` in `tools/packaging/verify/distros.py`) and the scenarios assert them:

| | fedora-44, almalinux-10 | ubuntu-2404 |
| --- | --- | --- |
| service restarted on upgrade | yes, `%postun` runs `%systemd_postun_with_restart` | **no**: the `.deb` from `build.py` runs no debhelper, so `prerm`/`postrm` keep only `#DEBHELPER#` |
| config after `remove` | gone, a modified one saved as `/etc/spot/spot.toml.rpmsave` | kept (conffile) |
| after `purge`/`erase` | erased with the package | deleted |
| service account after removal | kept | kept |
| installing an older version | `dnf install` leaves the newer one in place | `apt-get install` refuses, non-zero exit |

## The report

`--report FILE` writes JSON with `schema_version`, the distros that ran, a `summary` (total,
pass, fail, skip, error, exit code) and one row per scenario: `distro`, `scenario`, `status`,
`duration_ms` (a whole number, measured with `time.monotonic_ns`), `detail` and, for a
failure, the last 40 log lines. No timestamp and no absolute host path is written, so two
runs of the same tree produce the same file. The writer validates the document first and
refuses to write an invalid one.

## Adding a distribution

1. Add `harness/packaging-linux/<name>/Containerfile` that starts systemd as PID 1 and
   installs the package tooling (see the existing three).
2. Add one `Distro` entry to `tools/packaging/verify/distros.py`, with a `Policy` that says
   what *its* packages promise.
3. Nothing else: the driver reads both, the scenario table applies to every distro, and
   `--list` shows the new one. Add the name to the acceptance command in the task.

## Known limits

- **SELinux and AppArmor are not enforced.** A container shares the host's LSM, so a policy
  denial that a real host would produce cannot be reproduced here; the container also runs
  with `label=disable`. Policy-level behaviour needs a VM.
- **No real boot.** systemd is PID 1 in a container, not the host init: no kernel command
  line, no real `/boot`, no `network-online.target` that waits for a physical link, and
  `ProtectSystem=`/`ProtectClock=` are evaluated against the container's view.
- **No repository metadata.** The scenarios install a local file. Signatures, `dnf`/`apt`
  repository layout and dependency resolution against a real mirror are not covered (that is
  the repository work WP-PRD-09 leaves to the lead).
- **No PostgreSQL.** `Recommends: postgresql-18` is deliberately not installed, so the
  bundled database supervision is not exercised.
- **The `.rpmnew`/`.dpkg-dist` path is recorded, not forced.** It needs a genuinely changed
  shipped template, which the driver cannot produce without editing `packaging/linux/etc`.
- **A placeholder binary stops the service scenarios.** `clean_install`, `removal` and the
  image build work with any file; `start_and_health` and `upgrade` need a binary that
  actually serves `/api/health/live`. With a stub, those two fail honestly rather than pass.
- **Rootless podman cannot test `CapabilityBoundingSet=`** or anything that needs a real
  capability, which is why the unit needs none.

## Tests

```sh
python3 -m unittest discover -s tools/packaging/verify/tests -v
```

The suite needs no podman, no root and no network: the driver's runner and clock are
injected, so every scenario is driven against scripted answers, and the command line is run
as a subprocess with an empty `PATH` to prove the `--list`, `--dry-run` and exit-77 paths.
