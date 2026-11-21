using System.Security.Cryptography;
using Spotledger.Component.Host;
using Xunit;

namespace Spotledger.Component.Host.Tests;

/// <summary>
/// Recorded security finding. The host reads each file of a package once, checks its digest on those bytes and loads
/// from the same bytes (<c>LoadFromStream</c>); the file on disk is not read again. A writer who
/// replaces a file between the check and the load therefore changes nothing the host loads.
/// </summary>
public sealed class VerifiedBytesLoadTests
{
    private const string Dll = "assemblies/Acme.RatingComponent.dll";

    [Fact]
    public void AFileSwappedBetweenVerificationAndLoadDoesNotChangeWhatIsLoaded()
    {
        using var harness = new HardeningHarness();
        var (package, manifest, spec) = harness.Build();
        using var cleanup = package;
        var onDisk = Path.Combine(package.Root, Dll.Replace('/', Path.DirectorySeparatorChar));
        var loader = new ComponentLoader(harness.Trust.Store, new ComponentCatalogue(), harness.Log);
        var swapped = false;

        // The admission callback runs after verification and before any code is loaded: the moment a
        // writer to the install directory would pick.
        var result = loader.Load(package.Root, HardeningHarness.Now, _ =>
        {
            File.WriteAllBytes(onDisk, Enumerable.Range(0, 4096).Select(static i => (byte)(i * 7)).ToArray());
            swapped = true;
            return null;
        });

        Assert.True(swapped);
        Assert.True(result.IsLoaded, result.Refusal?.ToString());
        var component = result.Component!;
        var heldBytes = component.Context.PackageAssemblies["Acme.RatingComponent"];
        Assert.Equal(manifest.Assemblies[0].Sha256, ComponentPackage.HashFile(heldBytes));
        Assert.Equal(ComponentPackage.HashFile(spec.Assembly), ComponentPackage.HashFile(heldBytes));
        var assembly = Assert.Single(component.Context.Assemblies, a => a.GetName().Name == "Acme.RatingComponent");
        Assert.Equal(string.Empty, assembly.Location);
        Assert.NotEqual(
            Convert.ToHexStringLower(SHA256.HashData(File.ReadAllBytes(onDisk))),
            manifest.Assemblies[0].Sha256);
        component.Dispose();
    }

    [Fact]
    public void AFileReplacedAfterThePackageWasReadIsNotReadAgain()
    {
        using var harness = new HardeningHarness();
        var (package, manifest, _) = harness.Build();
        using var cleanup = package;
        var read = ComponentPackage.Read(package.Root);
        Assert.True(read.IsAccepted, read.Refusal?.ToString());
        File.Delete(Path.Combine(package.Root, Dll.Replace('/', Path.DirectorySeparatorChar)));
        var loader = new ComponentLoader(harness.Trust.Store, new ComponentCatalogue(), harness.Log);

        var result = loader.Load(read.Package!, HardeningHarness.Now);

        Assert.True(result.IsLoaded, result.Refusal?.ToString());
        Assert.Equal(
            manifest.Assemblies[0].Sha256,
            ComponentPackage.HashFile(result.Component!.Context.PackageAssemblies["Acme.RatingComponent"]));
        result.Component.Dispose();
    }

    [Fact]
    public void ThePackageHoldsTheVerifiedBytesAndNotAPathToReadAgain()
    {
        using var harness = new HardeningHarness();
        var (package, manifest, spec) = harness.Build();
        using var cleanup = package;
        var path = Path.Combine(package.Root, Dll.Replace('/', Path.DirectorySeparatorChar));

        var read = ComponentPackage.Read(package.Root);

        Assert.True(read.IsAccepted, read.Refusal?.ToString());
        var file = read.Package!.EntryAssembly;
        Assert.Equal(manifest.Assemblies[0].Sha256, ComponentPackage.HashFile(file.Bytes));
        // The bytes the digest covers are the bytes the package holds; changing the disk afterwards
        // changes neither.
        File.WriteAllBytes(path, new byte[] { 1, 2, 3 });
        Assert.Equal(ComponentPackage.HashFile(spec.Assembly), ComponentPackage.HashFile(file.Bytes));
    }
}
