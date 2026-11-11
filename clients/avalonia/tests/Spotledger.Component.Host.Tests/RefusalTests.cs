using System.Runtime.Loader;
using Spotledger.Component.Host;
using Xunit;

namespace Spotledger.Component.Host.Tests;

/// <summary>
/// The refusals: every one is a package the host must not load, asserted by its stable code, by the
/// log a support engineer would read, and — where the brief asks for it — by counting the collectible
/// load contexts that hold the package's assembly before and after the attempt.
/// <para>
/// That count is the proof that a refusal happens <em>before</em> a load: the loader only ever loads
/// into a collectible <see cref="AssemblyLoadContext"/> named <c>id@version</c>, so a count that does
/// not move means no package code entered the process.
/// </para>
/// </summary>
public sealed class RefusalTests
{
    /// <summary>The sample component's assembly name: the only assembly a sound package ships.</summary>
    private const string Sample = "Acme.RatingComponent";

    private static readonly Vectors VectorSet = Vectors.Load();

    [Fact]
    public void APackageWhoseAssemblyChangedAfterSigningIsRefusedBeforeAnyLoad()
    {
        using var trust = new TestHarness();
        using var package = new TempPackage();
        var spec = PackageBuilder.SampleSpec(trust.Trust);
        PackageBuilder.Build(package.Root, spec);

        // The publisher signed these bytes; someone edited the DLL afterwards and left the manifest
        // and the signature alone, which is exactly what a tampered release looks like.
        var assemblyPath = Path.Combine(package.Root, spec.AssemblyPath.Replace('/', Path.DirectorySeparatorChar));
        File.WriteAllBytes(assemblyPath, spec.Assembly.Concat(new byte[] { 0x00, 0x01, 0x02 }).ToArray());

        var before = ContextsHolding(Sample);
        var result = trust.Service.Install(package.Root, TestHarness.Now);

        AssertRefused(result, ComponentRefusalCodes.HashMismatch);
        Assert.Empty(trust.Log.LoadedIds);
        Assert.Equal(before, ContextsHolding(Sample));
    }

    [Fact]
    public void AnUnsignedPackageIsRefused()
    {
        using var trust = new TestHarness();
        using var package = new TempPackage();
        var spec = PackageBuilder.SampleSpec(trust.Trust);
        spec.WriteSignature = false;
        PackageBuilder.Build(package.Root, spec);

        var before = ContextsHolding(Sample);
        var result = trust.Service.Install(package.Root, TestHarness.Now);

        AssertRefused(result, ComponentRefusalCodes.SignatureMissing);
        Assert.Contains("unsigned", result.Refusal!.Message, StringComparison.OrdinalIgnoreCase);
        Assert.Equal(before, ContextsHolding(Sample));
    }

    [Fact]
    public void ASignatureOverDifferentBytesIsRefused()
    {
        using var trust = new TestHarness();
        using var package = new TempPackage();
        var spec = PackageBuilder.SampleSpec(trust.Trust);

        // A well-formed 64-byte signature over something else: the shape is right, the bytes are not.
        spec.SignatureOverride = Ed25519.Sign(trust.Trust.OutsiderSeed, "another manifest"u8.ToArray());
        PackageBuilder.Build(package.Root, spec);

        var before = ContextsHolding(Sample);
        var result = trust.Service.Install(package.Root, TestHarness.Now);

        AssertRefused(result, ComponentRefusalCodes.SignatureInvalid);
        Assert.Equal(before, ContextsHolding(Sample));
    }

    [Fact]
    public void ARevokedKeyIsRefused()
    {
        using var trust = new TestHarness();
        using var package = new TempPackage();
        PackageBuilder.Build(package.Root, PackageBuilder.SampleSpec(trust.Trust));

        // effective_from is before signed_at: the signature this key made is withdrawn.
        trust.Trust.Accept(trust.Trust.Revoke(
            generation: 2,
            keys: new[] { new RevokedKeyEntry(trust.Trust.PublisherKeyId, 1_750_000_000, false, "withdrawn key") }));

        var result = trust.Service.Install(package.Root, TestHarness.Now);

        AssertRefused(result, ComponentRefusalCodes.KeyRevoked);
    }

    [Fact]
    public void ACompromisedKeyIsRefusedHoweverOldTheSignatureIs()
    {
        using var trust = new TestHarness();
        using var package = new TempPackage();
        PackageBuilder.Build(package.Root, PackageBuilder.SampleSpec(trust.Trust));

        // effective_from is *after* signed_at and the key is still refused: a compromised key
        // invalidates every signature it ever made.
        trust.Trust.Accept(trust.Trust.Revoke(
            generation: 2,
            keys: new[] { new RevokedKeyEntry(trust.Trust.PublisherKeyId, 1_900_000_000, true, "leaked signing host") }));

        var result = trust.Service.Install(package.Root, TestHarness.Now);

        AssertRefused(result, ComponentRefusalCodes.KeyCompromised);
    }

    [Fact]
    public void ARevokedPublisherIsRefused()
    {
        using var trust = new TestHarness();
        using var package = new TempPackage();
        PackageBuilder.Build(package.Root, PackageBuilder.SampleSpec(trust.Trust));

        trust.Trust.Accept(trust.Trust.Revoke(
            generation: 2,
            publishers: new[] { new RevokedPublisherEntry("acme", 1_750_000_000, "publisher withdrawn") }));

        var result = trust.Service.Install(package.Root, TestHarness.Now);

        AssertRefused(result, ComponentRefusalCodes.PublisherRevoked);
    }

    [Fact]
    public void ARevokedPackageDigestIsRefused()
    {
        using var trust = new TestHarness();
        using var package = new TempPackage();
        var manifest = PackageBuilder.Build(package.Root, PackageBuilder.SampleSpec(trust.Trust));

        // The digest is what the signed manifest bytes hash to, which is what a revocation list names.
        trust.Trust.Accept(trust.Trust.Revoke(
            generation: 2,
            packages: new[] { new RevokedPackageEntry(manifest.SignedBytesSha256(), "withdrawn release") }));

        var result = trust.Service.Install(package.Root, TestHarness.Now);

        AssertRefused(result, ComponentRefusalCodes.PackageRevoked);
    }

    [Fact]
    public void APackageSignedByAKeyNoCertificateNamesIsRefused()
    {
        using var trust = new TestHarness();
        using var package = new TempPackage();
        var spec = PackageBuilder.SampleSpec(trust.Trust);
        spec.KeyId = trust.Trust.OutsiderKeyId;
        spec.SigningSeed = trust.Trust.OutsiderSeed;
        PackageBuilder.Build(package.Root, spec);

        var result = trust.Service.Install(package.Root, TestHarness.Now);

        AssertRefused(result, ComponentRefusalCodes.UnknownKey);
    }

    [Fact]
    public void ACertificateThatNamesAnotherPublisherIsRefused()
    {
        using var trust = new TestHarness();
        using var package = new TempPackage();

        // The outsider's key is trusted, but its certificate names 'other', while the manifest says
        // the package was published by 'acme'.
        var certificate = trust.Trust.Issue("other", trust.Trust.OutsiderSeed);
        Assert.Null(trust.Trust.Store.Trust(certificate, TestHarness.Now));

        var spec = PackageBuilder.SampleSpec(trust.Trust);
        spec.KeyId = trust.Trust.OutsiderKeyId;
        spec.SigningSeed = trust.Trust.OutsiderSeed;
        PackageBuilder.Build(package.Root, spec);

        var result = trust.Service.Install(package.Root, TestHarness.Now);

        AssertRefused(result, ComponentRefusalCodes.PublisherMismatch);
    }

    [Fact]
    public void AnExpiredCertificateIsRefused()
    {
        using var trust = new TestHarness();
        using var package = new TempPackage();
        PackageBuilder.Build(package.Root, PackageBuilder.SampleSpec(trust.Trust));

        // The certificate's window ends at 1_800_000_000; this is a year and a half past it.
        var result = trust.Service.Install(package.Root, TestHarness.Now + 50_000_000);

        AssertRefused(result, ComponentRefusalCodes.CertificateExpired);
    }

    [Fact]
    public void APackageForALaterContractIsRefused()
    {
        using var trust = new TestHarness();
        using var package = new TempPackage();
        var spec = PackageBuilder.SampleSpec(trust.Trust);
        spec.Minimum = "2.0";
        spec.MaximumExclusive = "3.0";
        PackageBuilder.Build(package.Root, spec);

        var before = ContextsHolding(Sample);
        var result = trust.Service.Install(package.Root, TestHarness.Now);

        AssertRefused(result, ComponentRefusalCodes.ContractTooNew);
        Assert.Equal(before, ContextsHolding(Sample));
    }

    [Fact]
    public void APackageForAnEarlierContractIsRefused()
    {
        using var trust = new TestHarness();
        using var package = new TempPackage();
        var spec = PackageBuilder.SampleSpec(trust.Trust);
        spec.Minimum = "0.9";
        spec.MaximumExclusive = "1.0";
        PackageBuilder.Build(package.Root, spec);

        var result = trust.Service.Install(package.Root, TestHarness.Now);

        AssertRefused(result, ComponentRefusalCodes.ContractUnsupported);
    }

    [Fact]
    public void ADescriptorThatDisagreesWithTheSignedManifestIsRefused()
    {
        using var trust = new TestHarness();
        using var package = new TempPackage();
        var spec = PackageBuilder.SampleSpec(trust.Trust);

        // The signature covers 2.0.0; the assembly's descriptor says 1.0.0. The signed fields must be
        // the fields that run, so this is a refusal and not a second, unsigned version.
        spec.ComponentVersion = "2.0.0";
        PackageBuilder.Build(package.Root, spec);

        var result = trust.Service.Install(package.Root, TestHarness.Now);

        AssertRefused(result, ComponentRefusalCodes.DescriptorMismatch);
        Assert.Contains("2.0.0", result.Refusal!.Message, StringComparison.Ordinal);
        Assert.Empty(trust.Log.LoadedIds);
    }

    [Fact]
    public void EveryRefusalIsLoggedWithItsCode()
    {
        using var trust = new TestHarness();
        using var package = new TempPackage();
        var spec = PackageBuilder.SampleSpec(trust.Trust);
        spec.WriteSignature = false;
        PackageBuilder.Build(package.Root, spec);

        _ = trust.Service.Install(package.Root, TestHarness.Now);

        var refusal = Assert.Single(trust.Log.Refusals);
        Assert.Equal(ComponentRefusalCodes.SignatureMissing, refusal.Code);
    }

    [Theory]
    [InlineData("../escape.dll")]
    [InlineData("/etc/passwd")]
    [InlineData("C:/windows/system32/x.dll")]
    [InlineData("assemblies\\x.dll")]
    [InlineData("assemblies//x.dll")]
    [InlineData("./x.dll")]
    public void APathThatLeavesThePackageIsRefused(string path)
    {
        Assert.False(ComponentPackagePaths.IsSafeRelativePath(path, out var problem));
        Assert.NotEmpty(problem);
    }

    [Fact]
    public void APathInsideThePackageIsAccepted()
    {
        Assert.True(ComponentPackagePaths.IsSafeRelativePath("assemblies/Acme.RatingComponent.dll", out var problem));
        Assert.Empty(problem);
    }

    /// <summary>Asserts a refusal and that it carries the code the caller expects.</summary>
    /// <param name="result">The install outcome.</param>
    /// <param name="code">The expected stable code.</param>
    private static void AssertRefused(ComponentLoadResult result, string code)
    {
        Assert.False(result.IsLoaded, result.Refusal?.ToString() ?? "the package loaded and should not have");
        Assert.Equal(code, result.Refusal!.Code);
    }

    /// <summary>
    /// How many collectible load contexts hold an assembly named <paramref name="assemblyName"/>. The
    /// loader never loads into the default context, so a count that does not move across a refused
    /// install is the proof that no package code entered the process.
    /// </summary>
    /// <param name="assemblyName">The simple assembly name to look for.</param>
    private static int ContextsHolding(string assemblyName) =>
        AppDomain.CurrentDomain.GetAssemblies()
            .Where(a => string.Equals(a.GetName().Name, assemblyName, StringComparison.Ordinal))
            .Select(AssemblyLoadContext.GetLoadContext)
            .Count(static context => context is { IsCollectible: true });

    /// <summary>The trust material and the install service one refusal test needs, disposed together.</summary>
    private sealed class TestHarness : IDisposable
    {
        public TestHarness()
        {
            Trust = new TestTrust(VectorSet);
            Trust.TrustPublisher();
            Log = new RecordingLog();
            Service = new ComponentInstallationService(Trust.Store, Log);
        }

        /// <summary>The instant every test verifies at: inside the certificate's window.</summary>
        public static long Now => 1_760_000_000;

        public TestTrust Trust { get; }

        public RecordingLog Log { get; }

        public ComponentInstallationService Service { get; }

        public void Dispose() => Service.UninstallAll();
    }
}
