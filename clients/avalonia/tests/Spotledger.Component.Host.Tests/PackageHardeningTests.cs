using System.Runtime.InteropServices;
using System.Text.RegularExpressions;
using Spotledger.Component.Host;
using Xunit;

namespace Spotledger.Component.Host.Tests;

/// <summary>
/// Recorded security findings: reading and loading a package never throws to the caller, links inside a
/// package are refused, and a hash mismatch names the file and prints no hash. The malformed packages
/// are built on disk and installed through the real service. This is a review aid for a package a
/// person was sent; a writer who can change the directory during the read can still race it.
/// </summary>
public sealed class PackageHardeningTests
{
    private const string Dll = "assemblies/Acme.RatingComponent.dll";

    public static TheoryData<string, string> MalformedPackages => new()
    {
        { "manifest-empty", ComponentRefusalCodes.ManifestInvalid },
        { "manifest-garbage", ComponentRefusalCodes.ManifestInvalid },
        { "manifest-truncated", ComponentRefusalCodes.ManifestInvalid },
        { "manifest-oversized", ComponentRefusalCodes.FileOversized },
        { "signature-empty", ComponentRefusalCodes.SignatureMalformed },
        { "signature-truncated", ComponentRefusalCodes.SignatureMalformed },
        { "assembly-missing", ComponentRefusalCodes.FileMissing },
        { "assembly-is-a-folder", ComponentRefusalCodes.FileMissing },
        { "assembly-truncated", ComponentRefusalCodes.HashMismatch },
        { "assembly-not-a-pe", ComponentRefusalCodes.AssemblyInvalid },
        { "assembly-unreadable", ComponentRefusalCodes.FileUnreadable },
        { "duplicate-assembly-name", ComponentRefusalCodes.DuplicateAssembly },
        { "too-many-files", ComponentRefusalCodes.TooManyFiles },
    };

    [Theory]
    [MemberData(nameof(MalformedPackages))]
    public void AMalformedPackageIsARefusalWithAStableCodeAndNeverAnException(string kind, string expectedCode)
    {
        using var harness = new HardeningHarness();
        var (package, _, spec) = harness.Build(s => Shape(kind, s));
        using var cleanup = package;
        if (!Damage(kind, package.Root, spec))
        {
            return;
        }

        var result = harness.Service.Install(package.Root, HardeningHarness.Now);

        Assert.False(result.IsLoaded);
        Assert.Equal(expectedCode, result.Refusal!.Code);
        AssertAccessible(result.Refusal.Message);
        Assert.Equal(0, harness.Service.Catalogue.Count);
        Assert.Contains(harness.Log.Refusals, r => r.Code == expectedCode);
        Restore(kind, package.Root);
    }

    [Fact]
    public void AHashMismatchNamesTheFileAndPrintsNoHash()
    {
        using var harness = new HardeningHarness();
        var (package, manifest, spec) = harness.Build();
        using var cleanup = package;
        var path = Path.Combine(package.Root, Dll.Replace('/', Path.DirectorySeparatorChar));
        var tampered = spec.Assembly.Concat(new byte[] { 1, 2, 3 }).ToArray();
        File.WriteAllBytes(path, tampered);

        var result = harness.Service.Install(package.Root, HardeningHarness.Now);

        Assert.Equal(ComponentRefusalCodes.HashMismatch, result.Refusal!.Code);
        var message = result.Refusal.Message;
        Assert.Contains("package file hash mismatch", message, StringComparison.Ordinal);
        Assert.Contains(Dll, message, StringComparison.Ordinal);
        Assert.DoesNotContain(ComponentPackage.HashFile(tampered), message, StringComparison.Ordinal);
        Assert.DoesNotContain(manifest.Assemblies[0].Sha256, message, StringComparison.Ordinal);
        Assert.DoesNotMatch(new Regex("[0-9a-fA-F]{16,}"), message);
    }

    [Fact]
    public void ALinkInsideThePackageIsRefused()
    {
        using var harness = new HardeningHarness();
        var (package, _, _) = harness.Build();
        using var cleanup = package;
        if (!TryLink(Path.Combine(package.Root, "assemblies", "escape"), Path.GetTempPath(), directory: true))
        {
            return;
        }

        var result = harness.Service.Install(package.Root, HardeningHarness.Now);

        Assert.Equal(ComponentRefusalCodes.SymlinkRefused, result.Refusal!.Code);
        Assert.Contains("assemblies", result.Refusal.Message, StringComparison.Ordinal);
        AssertAccessible(result.Refusal.Message);
        Assert.Equal(0, harness.Service.Catalogue.Count);
    }

    [Fact]
    public void AListedFileThatIsALinkToAFileOutsideThePackageIsRefused()
    {
        using var harness = new HardeningHarness();
        var (package, _, spec) = harness.Build();
        using var cleanup = package;
        using var outside = new TempPackage();
        var target = Path.Combine(outside.Root, "Acme.RatingComponent.dll");
        File.WriteAllBytes(target, spec.Assembly);
        var listed = Path.Combine(package.Root, Dll.Replace('/', Path.DirectorySeparatorChar));
        File.Delete(listed);
        if (!TryLink(listed, target, directory: false))
        {
            return;
        }

        var result = harness.Service.Install(package.Root, HardeningHarness.Now);

        Assert.Equal(ComponentRefusalCodes.SymlinkRefused, result.Refusal!.Code);
        Assert.Empty(harness.Log.LoadedIds);
    }

    [Fact]
    public void ASoundPackageStillInstalls()
    {
        using var harness = new HardeningHarness();
        var (package, _, _) = harness.Build();
        using var cleanup = package;

        Assert.True(harness.Service.Install(package.Root, HardeningHarness.Now).IsLoaded);
    }

    [Fact]
    public void ReadingAPackageDirectoryThatDoesNotExistIsARefusal()
    {
        var read = ComponentPackage.Read(Path.Combine(Path.GetTempPath(), "spot-missing-" + Guid.NewGuid().ToString("N")));

        Assert.False(read.IsAccepted);
        Assert.Equal(ComponentRefusalCodes.FileMissing, read.Refusal!.Code);
    }

    private static void Shape(string kind, PackageSpec spec)
    {
        switch (kind)
        {
            case "assembly-not-a-pe":
                spec.Assembly = Enumerable.Range(0, 4096).Select(static i => (byte)(i * 31 % 251)).ToArray();
                break;
            case "duplicate-assembly-name":
                spec.ExtraAssemblies["assemblies/nested/Acme.RatingComponent.dll"] = spec.Assembly;
                break;
            case "too-many-files":
                for (var i = 0; i <= ComponentPackage.MaxFiles; i++)
                {
                    spec.Assets["assets/file" + i.ToString("D4", System.Globalization.CultureInfo.InvariantCulture) + ".txt"] = new byte[] { (byte)i };
                }

                break;
            default:
                break;
        }
    }

    // Returns false when the damage cannot be done on this platform (the test then has nothing to prove).
    private static bool Damage(string kind, string root, PackageSpec spec)
    {
        var manifest = Path.Combine(root, ComponentPackagePaths.ManifestFileName);
        var signature = Path.Combine(root, ComponentPackagePaths.SignatureFileName);
        var assembly = Path.Combine(root, Dll.Replace('/', Path.DirectorySeparatorChar));
        switch (kind)
        {
            case "manifest-empty":
                File.WriteAllBytes(manifest, Array.Empty<byte>());
                break;
            case "manifest-garbage":
                File.WriteAllBytes(manifest, Enumerable.Range(0, 300).Select(static i => (byte)(0xFF - (i % 7))).ToArray());
                break;
            case "manifest-truncated":
                var text = File.ReadAllText(manifest);
                File.WriteAllText(manifest, text[..(text.Length / 2)]);
                break;
            case "manifest-oversized":
                File.WriteAllText(manifest, new string(' ', (int)ComponentPackage.MaxManifestBytes + 1));
                break;
            case "signature-empty":
                File.WriteAllText(signature, string.Empty);
                break;
            case "signature-truncated":
                var sig = File.ReadAllText(signature);
                File.WriteAllText(signature, sig[..(sig.Length / 2)]);
                break;
            case "assembly-missing":
                File.Delete(assembly);
                break;
            case "assembly-is-a-folder":
                File.Delete(assembly);
                Directory.CreateDirectory(assembly);
                break;
            case "assembly-truncated":
                File.WriteAllBytes(assembly, spec.Assembly[..(spec.Assembly.Length / 2)]);
                break;
            case "assembly-unreadable":
                if (OperatingSystem.IsWindows())
                {
                    return false;
                }

                File.SetUnixFileMode(assembly, UnixFileMode.None);
                try
                {
                    using var probe = File.OpenRead(assembly);
                    File.SetUnixFileMode(assembly, UnixFileMode.UserRead | UnixFileMode.UserWrite);
                    return false; // running as a user the mode does not bind (root): nothing to prove
                }
                catch (UnauthorizedAccessException)
                {
                    // The expected case.
                }

                break;
            default:
                break;
        }

        return true;
    }

    private static void Restore(string kind, string root)
    {
        if (kind == "assembly-unreadable" && !OperatingSystem.IsWindows())
        {
            File.SetUnixFileMode(
                Path.Combine(root, Dll.Replace('/', Path.DirectorySeparatorChar)),
                UnixFileMode.UserRead | UnixFileMode.UserWrite);
        }
    }

    private static bool TryLink(string link, string target, bool directory)
    {
        try
        {
            if (directory)
            {
                Directory.CreateSymbolicLink(link, target);
            }
            else
            {
                File.CreateSymbolicLink(link, target);
            }

            return true;
        }
        catch (Exception error) when (error is IOException or UnauthorizedAccessException or PlatformNotSupportedException)
        {
            // Creating links needs a privilege on some platforms; nothing to prove there.
            return false;
        }
    }

    private static void AssertAccessible(string message)
    {
        Assert.False(string.IsNullOrWhiteSpace(message));
        Assert.DoesNotContain("   at ", message, StringComparison.Ordinal);
        Assert.DoesNotContain("Exception", message, StringComparison.Ordinal);
        Assert.DoesNotContain("\n", message, StringComparison.Ordinal);
        Assert.DoesNotContain(RuntimeInformation.FrameworkDescription, message, StringComparison.Ordinal);
    }
}
