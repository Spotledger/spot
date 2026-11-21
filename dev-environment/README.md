# Reproducible dev environments (WP-PRD-10.T1)

Two definitions of the environment the gates run in, one per CI OS, plus the
entry point that builds the Linux one. They exist so "it works on my machine"
and "it works on the runner" are the same environment, pinned by version and
hash.

| Path | What it is |
| --- | --- |
| `build.sh` | Builds the Fedora 44 image: downloads and verifies the pinned artefacts, then `podman`/`docker build`. |
| `fedora/Containerfile` | The Fedora 44 image: Rust, .NET SDK, Python 3.12, PostgreSQL 18, C/cmake toolchain. |
| `fedora/pins.env` | The pinned artefacts and hashes for the image. |
| `fedora/verify.sh` | Run inside the image: checks versions and builds a fast subset. |
| `fedora/README.md` | Build/run, toolbox, distrobox and podman usage. |
| `windows/setup.ps1` | Idempotent machine-wide provisioning of the Windows runner. |
| `windows/versions.psd1` | The pinned versions and hashes for Windows. |
| `windows/verify.ps1` | Checks versions and builds a fast subset. |
| `windows/README.md` | How the Windows runner is set up and why. |

## Versions come from the repository, not from here

- Rust: the `channel` in `rust-toolchain.toml`.
- .NET SDK: `sdk.version` in `clients/avalonia/global.json`.
- Python: 3.12, matching `actions/setup-python` and `tools/ci/skip.toml`.
- PostgreSQL: 18, matching `tools/ci/start-postgres.sh`.

Both setup scripts read those repository pins at run time, so a toolchain bump
in the repository is the only edit needed. Only the hashes and the third-party
versions (Git, Visual Studio Build Tools) live in `pins.env` and `versions.psd1`.

## Quick start

```sh
# Linux: build the image and smoke-test it
dev-environment/build.sh --verify

# Windows: provision once as Administrator, then verify as the runner user
powershell -ExecutionPolicy Bypass -File dev-environment\windows\setup.ps1
powershell -ExecutionPolicy Bypass -File dev-environment\windows\verify.ps1
```

The workflows stay owned by the lead; this directory is the environment those
workflows call, not the workflow wiring.
