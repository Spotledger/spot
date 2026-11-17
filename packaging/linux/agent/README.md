# spot-agent packaging (RPM and DEB)

Packages the agent (`spot-agent`) and its privilege helper (`spot-agent-priv`) from prebuilt
binaries. Nothing compiles Rust. The host (`spot`) packages are in `../` (WP-PRD-09.T1-T6).

```sh
python3 packaging/linux/agent/build_agent.py rpm --binary target/release/spot-agent \
    --helper target/release/spot-agent-priv --version 1.2.3 --out dist
python3 packaging/linux/agent/build_agent.py deb ...   # same flags
bash packaging/linux/verify/verify-agent.sh target/release/spot-agent-priv
```

## Privilege design (decision)

The daemon runs as the unprivileged `spot-agent` account with **no ambient capability**. The
few privileged operations run as `sudo -n /usr/libexec/spot-agent/spot-agent-priv run <name>`.
sudo is setuid, so `NoNewPrivileges=true` would defeat it: it is **off in this unit only**.
In exchange: a short explicit `CapabilityBoundingSet` (the ceiling for what a root helper child
can do), `ProtectSystem=strict`, `ReadWritePaths` limited to the state directory and the one
directory `cert.install` writes, and the sudoers drop-in below. `systemd-analyze security`
measured 2.4 (systemd 259, 2026-11-12); the ceiling checked by the verify script and the test
is 3.0, the same as the host unit.

## The sudoers drop-in is generated, never written

`/etc/sudoers.d/spot-agent` (0440 root:root) is the output of
`spot-agent-priv --print-sudoers --user spot-agent`, produced at package build and checked by
`agent_sudoers.py` (one exact `<helper> run <template>` per template, no wildcard, no `ALL`
command, only the agent account, nothing else). No copy is committed. `verify-agent.sh` runs
`visudo -cf` on it.

## Lifecycle

- Install: creates the account (sysusers) and `/var/lib/spot-agent` (0700). It does **not**
  start, enable or enrol the agent and writes no secret. The unit has
  `ConditionPathExists=/var/lib/spot-agent/agent.key`, so before enrolment it simply does not start.
  Enrol explicitly: `spot-agent enrol --gateway URL --token-file FILE`, then
  `systemctl enable --now spot-agent`.
- Upgrade: `try-restart` once (RPM `%systemd_postun_with_restart`, DEB `postinst`); the key and
  enrolment live in the state directory and are kept.
- Downgrade: refused (RPM `%pre` with `rpm.vercmp`, DEB `preinst` with `dpkg --compare-versions`).
  Way back: remove the package (state is kept) and install the older one.
- Remove: state kept. DEB `purge`: removes `/var/lib/spot-agent`. The account is never deleted.

## Known limits

- `user.lock`/`user.unlock` run `usermod`, which writes `/etc/passwd` and `/etc/shadow`; under
  `ProtectSystem=strict` they fail with a read-only file system until an operator adds a drop-in
  `ReadWritePaths=/etc`. `cert.install` writes only the Fedora-style anchors directory.
- `MemoryDenyWriteExecute=true` is set (the agent has no JIT); re-test with sudo children on a real host.
- Install/upgrade/removal on real distributions need the `tools/packaging/verify` harness and
  podman; this task's tests check the files and run the maintainer scripts against stubs.
