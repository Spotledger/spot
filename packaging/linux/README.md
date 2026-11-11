# Linux packaging

Everything here packages a **prebuilt release binary**. Nothing compiles Rust:
`tools/packaging/build.py` takes the binary as an input, so the same artefact
is packaged on every host.

## Build

```sh
# always buildable; standard library only
python3 tools/packaging/build.py tarball \
    --binary target/release/spot --version 1.2.3 --out dist

# only when the tool exists; otherwise prints "SKIP <what> <why>" and still
# exits 0 with a JSON summary, so this is safe on any host
python3 tools/packaging/build.py rpm       --binary target/release/spot --version 1.2.3
python3 tools/packaging/build.py deb       --binary target/release/spot --version 1.2.3
python3 tools/packaging/build.py container --binary target/release/spot --version 1.2.3
```

`SOURCE_DATE_EPOCH` is honoured; unset it falls back to a fixed epoch, archives
are byte-identical between runs, owners are `0/0`, modes are fixed and entries
are sorted. A pre-release maps to RPM `Version=2.0.0`, `Release=0.rc.1` and to
Debian `2.0.0~rc.1`, so it sorts before the final release.

## Check

```sh
python3 tools/packaging/check.py unit
python3 tools/packaging/check.py tarball dist/spot-1.2.3-linux-x86_64.tar.gz
python3 tools/packaging/check.py rpm  dist/spot-1.2.3-1.fc44.x86_64.rpm --version 1.2.3
python3 tools/packaging/check.py deb  dist/spot_1.2.3_amd64.deb --version 1.2.3
python3 tools/packaging/check.py containerfile
python3 tools/packaging/check.py container spot:1.2.3   # podman only
```

`unit` always runs a built-in lint (required sandbox directives, `User=root`,
`ProtectSystem=strict`, writable `/etc`, capability set, JIT-hostile
`MemoryDenyWriteExecute`, the 30 s shutdown budget). When `systemd-analyze` is
present it also runs `verify` on a copy whose `ExecStart` points at an existing
executable — the build host does not have `/usr/bin/spot` — and `security`,
failing above an exposure score of **3.0** (the shipped unit scores 1.5).
`rpm` and `deb` read real metadata and file lists with `rpm`/`dpkg-deb` and
skip when those tools are missing.

## Layout

| Path | Installed as |
| --- | --- |
| `systemd/spot.service` | `/usr/lib/systemd/system/spot.service` |
| `systemd/spot.sysusers` | `/usr/lib/sysusers.d/spot.conf` |
| `systemd/spot.tmpfiles` | `/usr/lib/tmpfiles.d/spot.conf` |
| `etc/spot.toml` | `/etc/spot/spot.toml` (config, `noreplace`) |
| `etc/environment` | `/etc/spot/environment` (config, `noreplace`) |
| `rpm/spot.spec` | template rendered by `build.py` |
| `debian/` | source-package metadata for `dpkg-buildpackage` |
| `container/Containerfile` | OCI image taking the binary from the context |

## Decisions and known gaps

- **`Type=simple`, not `notify`.** `spot-cli` (WP-PRD-01) does not implement
  `sd_notify`; it handles SIGINT/SIGTERM only. Readiness is the public
  `GET /api/health/ready` endpoint. Switch to `Type=notify` when the binary
  grows `sd_notify`.
- **No `spot@.service`.** The CLI has no per-instance config name to key a
  template unit on. Add one when it does.
- **No logrotate stanza.** The binary logs to the journal (`StandardOutput=
  journal`), not to files, so there is nothing to rotate.
- **`MemoryDenyWriteExecute` is off.** wasmtime and QuickJS compile to
  executable memory at runtime; W^X would break apps and scripts.
- **Configuration path (decision P1, 31 Oct 2026).** The CLI reads
  `<root>/common.toml`; the packages install `/etc/spot/spot.toml`. The unit
  bridges them with `ExecStartPre=`, which creates the symlink
  `/var/lib/spot/common.toml -> /etc/spot/spot.toml` as user `spot` before every
  start. If anything already exists at that path (a regular file written by the
  operator, or a link pointing elsewhere) it is left alone, wins, and the
  journal line says which file is used. No file moved and `spot-cli` did not
  change. A `--config` flag / `SPOT_CONFIG` in `spot-cli` is a follow-up and is
  **not** done.
- **Licence.** The repository carries the Apache-2.0 `LICENSE`; the
  tarball/RPM/DEB ship it, and `build.py` falls back to an Apache-2.0 notice
  only if the file is missing.

## What the second task verifies on real hosts

Clean install, upgrade and rollback on Fedora 44, a RHEL-compatible host and a
Debian-family host: package signatures and repository metadata, service
start/stop under SELinux/AppArmor, real PostgreSQL 18 supervision, an actual
container run and the health probes. Those need root and three virtual
machines; everything in this directory is the part that can be proved without
them.
