# Pinned artefacts for the Windows CI runner image (WP-PRD-10.T1).
#
# The source of truth for windows/setup.ps1, matching the CI runner notes
# (machine-wide Git, Python 3.12, VS Build Tools with the MSVC C++ workload,
# Rust in C:\rust, .NET in C:\dotnet). Every field is verified on install:
#   - Url/Sha256      downloaded by setup.ps1 and checked with Get-FileHash;
#   - WingetId/Version installed by winget, which validates the installer hash
#     in its manifest;
#   - Override        the winget --override a package needs (Build Tools).
#
# Refresh by downloading the artefact, recomputing the hash, and pasting it
# here. Never paste a hash you did not compute from the publisher's own bytes.

@{
    # Keep in sync with rust-toolchain.toml.
    Rust = @{
        Version = '1.98.1'
        Url     = 'https://static.rust-lang.org/rustup/dist/x86_64-pc-windows-msvc/rustup-init.exe'
        Sha256  = '6f4bef66261261fcb43131be8720bab817d403a09edec7455c371974b90bdb7e'
    }

    # Keep in sync with clients/avalonia/global.json. The installer script is
    # pinned to an immutable commit, not to dot.net/v1 (which Microsoft updates).
    DotNet = @{
        SdkVersion    = '10.0.401'
        ScriptUrl     = 'https://raw.githubusercontent.com/dotnet/install-scripts/4a37a9f9d1a061fc389d6515100336db4e51710e/src/dotnet-install.ps1'
        ScriptSha256  = '3bb07bc8025211836c1e4f9d3f6a044e55b1fb6eec518a6c78851d04e210442b'
        InstallDir    = 'C:\dotnet'
    }

    RustHome = @{
        RustupHome = 'C:\rust\rustup'
        CargoHome  = 'C:\rust\cargo'
    }

    Git = @{
        WingetId = 'Git.Git'
        Version  = '2.55.0.5'
    }

    Python = @{
        WingetId = 'Python.Python.3.12'
        Version  = '3.12.10'
    }

    PostgreSQL = @{
        WingetId = 'PostgreSQL.PostgreSQL.18'
        Version  = '18.6-5'
    }

    BuildTools = @{
        WingetId = 'Microsoft.VisualStudio.2022.BuildTools'
        Version  = '17.14.42'
        Override = '--quiet --wait --norestart --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended'
    }
}
