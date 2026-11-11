# Windows runner toolchain

The Windows side of WP-PRD-10: the definition of the Windows CI runner image,
matching the "Windows" section of the CI runner notes (a Windows Server
2022 evaluation VM, two runner services as the non-admin `ghrunner` user,
machine-wide toolchains).

| File | What it is |
| --- | --- |
| `setup.ps1` | Idempotent, machine-wide provisioning. Run once as Administrator. |
| `versions.psd1` | Every pinned version and hash, and the source of truth for `setup.ps1`. |
| `verify.ps1` | Checks the pinned versions and builds a fast subset. Runs as the runner user. |

## Provision

```powershell
# As Administrator, on the runner VM:
powershell -ExecutionPolicy Bypass -File dev-environment\windows\setup.ps1
powershell -ExecutionPolicy Bypass -File dev-environment\windows\setup.ps1 -DryRun    # show the plan
```

What it installs, and how each is pinned:

| Tool | Source | Pinned by |
| --- | --- | --- |
| Rust (from `rust-toolchain.toml`) | `rustup-init.exe` from static.rust-lang.org | SHA-256 in `versions.psd1`, checked with `Get-FileHash` |
| .NET SDK (from `clients/avalonia/global.json`) | `dotnet-install.ps1` at an immutable install-scripts commit | SHA-256 in `versions.psd1` |
| Git | winget `Git.Git` | exact version; winget validates the installer hash |
| Python 3.12 | winget `Python.Python.3.12` | exact version; winget validates the installer hash |
| PostgreSQL 18 | winget `PostgreSQL.PostgreSQL.18` | exact version; winget validates the installer hash |
| Visual Studio Build Tools (MSVC C++ workload) | winget `Microsoft.VisualStudio.2022.BuildTools` | exact version plus the VCTools workload override |

No other network use: the two direct downloads are the only unpinned endpoints,
and both are verified against a hash in `versions.psd1` before they run. The
script is idempotent — it checks each tool's version and leaves it alone when it
already matches — so re-running after a partial install is safe.

Prerequisite: `winget` (App Installer) must be present. Pass `-SkipWinget` to
provision only Rust and .NET (the two toolchains this repository pins) and
install Git, Python, PostgreSQL and Build Tools by hand.

`RUSTUP_HOME` and `CARGO_HOME` are set machine-wide to `C:\rust` subdirectories,
and `C:\rust\cargo\bin` plus `C:\dotnet` are added to the machine `PATH`. The
`rust` CI job still points `CARGO_HOME` at `C:\cargo-home\<runner name>` before
`rust-cache` runs, because the three runners share the machine (see
the CI runner notes); `setup.ps1` sets the runner image's baseline, not a
per-job override.

## Verify

```powershell
powershell -ExecutionPolicy Bypass -File dev-environment\windows\verify.ps1
```

It checks `rustc`/`cargo` against the `rust-toolchain.toml` channel, the .NET
SDK against `global.json`, Python 3.12, PostgreSQL 18 and Git, then runs
`cargo fmt --all -- --check` and the gate-matrix unit tests from the repository.
It exits non-zero if any check fails.

## Why not Chocolatey

the CI runner notes keeps the runner unprivileged and secret-free; winget
is already on the VM and validates installer hashes from its manifests, so the
image needs no third-party package manager or key.
