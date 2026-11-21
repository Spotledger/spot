"""Generating a browser shell around a fixed, approved, signed component set.

Tier B on the web is not dynamic loading: the tenant's component set is chosen at build time, each
package's signature is verified before it is bundled, and the shell is generated with a static
reference to every component it ships. The published folder is therefore a complete record of one
build:

````
publish/
  Shell.csproj                  the generated project, one <Reference> per component
  Program.cs                    the host: reads the manifest, checks the bundled components, runs
  Components.g.cs               the component list compiled into the shell
  component-set-<hash>.json     the content-addressed manifest
  components/<id>/<version>/    each package, exactly as it was signed
````

The generated shell is emitted, not compiled, by this tool. Building it needs the Avalonia browser
workload and a browser-side host, which this task does not own; what the tool can promise — and what
the tests check — is that the project it writes references only verified packages and that the
manifest it writes is the document the C# host parses.
"""

from __future__ import annotations

import os
import re
import shutil
from dataclasses import dataclass
from pathlib import Path
from typing import Final, Sequence

from . import manifest as set_manifest
from .package import Package, Refused, hash_bytes

#: The generated project's file name.
PROJECT_FILE: Final = "Shell.csproj"

#: The generated host program's file name.
PROGRAM_FILE: Final = "Program.cs"

#: The generated component list's file name.
COMPONENTS_FILE: Final = "Components.g.cs"

#: The folder the packages are copied into.
COMPONENT_FOLDER: Final = "components"


#: The characters a bundled file's path may use: it is written into a generated MSBuild project.
_GENERATED_PATH: Final = re.compile(r"[A-Za-z0-9_./-]+")


@dataclass(frozen=True)
class Bundled:
    """One component that was verified and bundled: the package and where its payload was copied."""

    package: Package
    directory: Path

    @property
    def assembly_name(self) -> str:
        """The simple name of the assembly the shell references."""
        return Path(self.package.manifest.entry_assembly).stem


def _relative(from_directory: Path, target: Path) -> str:
    """A forward-slash relative path, because that is what MSBuild accepts on every platform."""
    return Path(os.path.relpath(target, from_directory)).as_posix()


def copy_package(package: Package, root: Path) -> Bundled:
    """Copies a package's listed files into ``root/components/<id>/<version>/``."""
    manifest = package.manifest
    directory = root / COMPONENT_FOLDER / manifest.component_id / manifest.component_version
    directory.mkdir(parents=True, exist_ok=True)
    for listed in manifest.files:
        destination = directory / listed.path
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_bytes(package.files[listed.path])
        if hash_bytes(destination.read_bytes()) != listed.sha256:
            raise OSError(f"'{destination}' did not survive the copy")
    return Bundled(package=package, directory=directory)


def entries_of(bundled: Sequence[Bundled]) -> list[set_manifest.Entry]:
    """The manifest entries for the bundled components."""
    return [
        set_manifest.Entry(
            component_id=item.package.manifest.component_id,
            component_version=item.package.manifest.component_version,
            publisher_id=item.package.manifest.publisher_id,
            key_id=item.package.manifest.key_id,
            entry_assembly_sha256=hash_bytes(item.package.entry_assembly),
            manifest_sha256=item.package.manifest.digest(),
        )
        for item in bundled
    ]


def project_file(shell: str, bundled: Sequence[Bundled], root: Path, host_project: Path) -> str:
    """The generated ``Shell.csproj``: a static reference per component, plus the host project."""
    references = []
    for item in bundled:
        assembly = item.directory / item.package.manifest.entry_assembly
        references.append(
            "  <ItemGroup>\n"
            f'    <Reference Include="{item.assembly_name}">\n'
            f"      <HintPath>{_relative(root, assembly)}</HintPath>\n"
            "      <Private>true</Private>\n"
            "    </Reference>\n"
            "  </ItemGroup>"
        )
    return (
        '<Project Sdk="Microsoft.NET.Sdk">\n'
        "  <PropertyGroup>\n"
        "    <OutputType>Exe</OutputType>\n"
        "    <TargetFramework>net10.0</TargetFramework>\n"
        "    <Nullable>enable</Nullable>\n"
        "    <ImplicitUsings>enable</ImplicitUsings>\n"
        f"    <AssemblyName>{shell}</AssemblyName>\n"
        "    <RootNamespace>Spotledger.Shell</RootNamespace>\n"
        "  </PropertyGroup>\n"
        "  <ItemGroup>\n"
        f'    <ProjectReference Include="{_relative(root, host_project)}" />\n'
        "  </ItemGroup>\n"
        + "\n".join(references)
        + "\n</Project>\n"
    )


def components_file(entries: Sequence[set_manifest.Entry]) -> str:
    """The generated component list: what this shell was compiled with."""
    lines = [
        "// Generated by tools/component-shell/build-shell.py. Do not edit.",
        "namespace Spotledger.Shell;",
        "",
        "/// <summary>The components this shell was built with, as the manifest listed them.</summary>",
        "internal static class BundledComponents",
        "{",
        "    /// <summary>One bundled component.</summary>",
        "    internal sealed record Bundled(string Id, string Version, string ManifestSha256);",
        "",
        "    /// <summary>The bundled components, in id order.</summary>",
        "    internal static IReadOnlyList<Bundled> All { get; } = new Bundled[]",
        "    {",
    ]
    for entry in entries:
        lines.append(
            f'        new("{entry.component_id}", "{entry.component_version}", "{entry.manifest_sha256}"),'
        )
    lines += ["    };", "}", ""]
    return "\n".join(lines)


def program_file(file_name: str) -> str:
    """The generated host: it reads the manifest and refuses to run a set that is not the build's."""
    body = """
/// <summary>
/// A tier B browser shell: it ships a fixed, signed component set and checks, before it shows
/// anything, that the set it carries is the set the build approved.
/// <para>
/// The check is the host's, not a second opinion of it: the manifest is content-addressed (its file
/// name carries the SHA-256 of its own bytes), the components compiled into this shell are exactly the
/// ones it lists, and each bundled assembly still hashes to the digest the publisher signed.
/// </para>
/// </summary>
public static class Program
{
    /// <summary>The manifest this shell was built with.</summary>
    private const string SetManifest = "%(file)s";

    /// <summary>Runs the shell's start-up check and reports what it found.</summary>
    /// <param name="args">Unused: a browser shell takes no arguments.</param>
    /// <returns>0 when the loaded set equals the manifest.</returns>
    public static int Main(string[] args)
    {
        _ = args;
        var root = AppContext.BaseDirectory;
        var path = Path.Combine(root, SetManifest);
        if (!ComponentSetManifestReader.TryRead(path, out var manifest, out var refusal) || manifest is null)
        {
            Console.Error.WriteLine("the component set manifest was refused: " + refusal);
            return 1;
        }

        var complaints = new List<string>();
        var bundled = BundledComponents.All;
        foreach (var entry in manifest.Components)
        {
            var match = bundled.FirstOrDefault(b => string.Equals(b.Id, entry.ComponentId, StringComparison.Ordinal));
            if (match is null)
            {
                complaints.Add("'" + entry.ComponentId + "' is listed but not compiled into this shell");
                continue;
            }

            if (!string.Equals(match.Version, entry.ComponentVersion, StringComparison.Ordinal))
            {
                complaints.Add("'" + entry.ComponentId + "' is " + match.Version + " here but "
                    + entry.ComponentVersion + " in the manifest");
            }

            if (!string.Equals(match.ManifestSha256, entry.ManifestSha256, StringComparison.Ordinal))
            {
                complaints.Add("'" + entry.ComponentId + "' was signed as " + entry.ManifestSha256
                    + " but the manifest lists " + match.ManifestSha256);
            }

            var assembly = Path.Combine(root, "components", entry.ComponentId, entry.ComponentVersion,
                "assemblies", entry.ComponentId + ".dll");
            if (File.Exists(assembly)
                && !string.Equals(Ed25519.Sha256Hex(File.ReadAllBytes(assembly)), entry.EntryAssemblySha256, StringComparison.Ordinal))
            {
                complaints.Add("the bundled assembly of '" + entry.ComponentId + "' is not the bytes that were signed");
            }
        }

        foreach (var extra in bundled.Where(b => manifest.Components.All(
            e => !string.Equals(e.ComponentId, b.Id, StringComparison.Ordinal))))
        {
            complaints.Add("'" + extra.Id + "' is compiled into this shell but the manifest does not list it");
        }

        if (complaints.Count > 0)
        {
            foreach (var complaint in complaints)
            {
                Console.Error.WriteLine(complaint);
            }

            return 1;
        }

        Console.Out.WriteLine("the loaded component list equals the manifest ("
            + manifest.Components.Count.ToString(CultureInfo.InvariantCulture) + " component(s), content "
            + ComponentSetManifest.HashPrefix(manifest.ContentSha256) + ")");
        return 0;
    }
}
""" % {"file": file_name}
    return (
        "// Generated by tools/component-shell/build-shell.py. Do not edit.\n"
        "using System.Globalization;\n"
        "using Spotledger.Component.Host;\n\n"
        "namespace Spotledger.Shell;\n"
        + body
    )


def generate(
    *,
    shell: str,
    packages: Sequence[Package],
    output: Path,
    host_project: Path,
) -> set_manifest.SetManifest:
    """Writes the whole published folder and returns the manifest it wrote."""
    for package in packages:
        for listed in package.manifest.files:
            # These paths end up in a generated MSBuild project, where '$(...)', '%', ';' and '@' are
            # evaluated. A signed package may carry them in a file name; the build does not follow.
            if _GENERATED_PATH.fullmatch(listed.path) is None:
                raise Refused(
                    "component.package.path_invalid",
                    f"'{listed.path}' in {package.manifest.component_id} uses characters a generated project "
                    "does not accept (letters, digits, '.', '_', '-' and '/' only)",
                )
    if output.exists():
        # The output folder is rebuilt from scratch, but only a folder this tool wrote is deleted: a
        # mistyped --output must never wipe a directory that holds anything else.
        if not output.is_dir() or output.is_symlink():
            raise OSError(f"'{output}' exists and is not a directory this tool wrote")
        if any(output.iterdir()) and not (output / COMPONENTS_FILE).is_file():
            raise OSError(f"'{output}' is not empty and was not written by this tool; refusing to delete it")
        shutil.rmtree(output)
    output.mkdir(parents=True)
    bundled = [copy_package(package, output) for package in packages]
    document = set_manifest.build(shell, entries_of(bundled))

    (output / PROJECT_FILE).write_text(project_file(shell, bundled, output, host_project), encoding="utf-8")
    (output / COMPONENTS_FILE).write_text(components_file(document.entries), encoding="utf-8")
    (output / PROGRAM_FILE).write_text(program_file(document.file_name), encoding="utf-8")
    document.write(str(output))
    return document
