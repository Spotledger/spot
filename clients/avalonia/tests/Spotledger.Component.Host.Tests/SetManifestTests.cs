using System.Text.Json;
using Spotledger.Component.Host;
using Xunit;

namespace Spotledger.Component.Host.Tests;

/// <summary>
/// The browser shell's component set manifest: the document a build writes for the component set it
/// bundled, and the check a shell makes at start-up so the components it loaded are the ones it was
/// built with.
/// <para>
/// The committed sample is also read by the Python build tool's tests
/// (<c>tools/component-shell/tests</c>), so the two languages agree on one file and not on two
/// opinions of the same file.
/// </para>
/// </summary>
public sealed class SetManifestTests
{
    private const string Sample = "component-set-eadd8795b07d255e.json";

    private static readonly Vectors VectorSet = Vectors.Load();

    [Fact]
    public void TheCommittedSampleIsAContentAddressedManifest()
    {
        var path = Path.Combine(AppContext.BaseDirectory, "Vectors", Sample);

        Assert.True(
            ComponentSetManifestReader.TryRead(path, out var manifest, out var refusal),
            refusal?.ToString() ?? "the sample set manifest was refused");
        Assert.NotNull(manifest);
        Assert.Equal(Sample, Path.GetFileName(path));
        Assert.Equal(Sample, manifest.FileName);
        Assert.Equal("acme-shell", manifest.Shell);
        var entry = Assert.Single(manifest.Components);
        Assert.Equal("acme.rating", entry.ComponentId);
        Assert.Equal("1.0.0", entry.ComponentVersion);
        Assert.Equal("acme", entry.PublisherId);
        Assert.Equal("03396219237f75a64f12aeb7f39723ab", entry.KeyId);
        Assert.Equal(64, entry.ManifestSha256.Length);
        Assert.Equal(64, entry.EntryAssemblySha256.Length);
    }

    [Fact]
    public void AFileRenamedAfterTheBuildIsRefused()
    {
        using var directory = new TempPackage();
        var path = Path.Combine(directory.Root, "component-set-0000000000000000.json");
        File.Copy(Path.Combine(AppContext.BaseDirectory, "Vectors", Sample), path);

        Assert.False(ComponentSetManifestReader.TryRead(path, out var manifest, out var refusal));
        Assert.Null(manifest);
        Assert.Equal(ComponentRefusalCodes.SetNameMismatch, refusal!.Code);
    }

    [Fact]
    public void AMissingManifestIsRefused()
    {
        using var directory = new TempPackage();

        Assert.False(
            ComponentSetManifestReader.TryRead(Path.Combine(directory.Root, Sample), out _, out var refusal));
        Assert.Equal(ComponentRefusalCodes.SetManifestMissing, refusal!.Code);
    }

    [Fact]
    public void AnEntryThatIsNotSortedIsRefused()
    {
        const string Unsorted = """
            {"schema":1,"shell":"s","components":[
              {"component_id":"b.one","component_version":"1.0.0","publisher_id":"acme","key_id":"03396219237f75a64f12aeb7f39723ab","entry_assembly_sha256":"1111111111111111111111111111111111111111111111111111111111111111","manifest_sha256":"2222222222222222222222222222222222222222222222222222222222222222"},
              {"component_id":"a.one","component_version":"1.0.0","publisher_id":"acme","key_id":"03396219237f75a64f12aeb7f39723ab","entry_assembly_sha256":"3333333333333333333333333333333333333333333333333333333333333333","manifest_sha256":"4444444444444444444444444444444444444444444444444444444444444444"}
            ]}
            """;

        Assert.False(ComponentSetManifest.TryParse(Unsorted, out var manifest, out var problems));
        Assert.Null(manifest);
        Assert.Contains(problems, problem => problem.Contains("sorted", StringComparison.Ordinal));
    }

    [Fact]
    public void AnUnknownMemberIsRefused()
    {
        const string Extra = """
            {"schema":1,"shell":"s","components":[],"extra":true}
            """;

        Assert.False(ComponentSetManifest.TryParse(Extra, out _, out var problems));
        Assert.Contains(problems, problem => problem.Contains("extra", StringComparison.Ordinal));
    }

    [Fact]
    public void AnEmptySetIsSound()
    {
        Assert.True(ComponentSetManifest.TryParse("""{"schema":1,"shell":"s","components":[]}""", out var manifest, out _));

        Assert.NotNull(manifest);
        Assert.Empty(manifest.Components);
        Assert.StartsWith("component-set-", manifest.FileName, StringComparison.Ordinal);
    }

    [Fact]
    public void AComponentThatWasNotInstalledIsReported()
    {
        using var harness = new Harness();
        var set = Compose(Entries("acme.rating", "1.0.0", Digest("0")));

        var refusals = set.Compare(harness.Service.Catalogue);

        var refusal = Assert.Single(refusals);
        Assert.Equal(ComponentRefusalCodes.SetComponentMissing, refusal.Code);
        Assert.Contains("acme.rating", refusal.Message, StringComparison.Ordinal);
    }

    [Fact]
    public void AnInstalledComponentTheBuildDidNotListIsReported()
    {
        using var harness = new Harness();
        harness.Install();
        var set = Compose();

        var refusal = Assert.Single(set.Compare(harness.Service.Catalogue));

        Assert.Equal(ComponentRefusalCodes.SetComponentUnexpected, refusal.Code);
        Assert.Contains("acme.rating", refusal.Message, StringComparison.Ordinal);
    }

    [Fact]
    public void AVersionThatMovedIsReported()
    {
        using var harness = new Harness();
        var installed = harness.Install();
        var set = Compose(Entries(
            "acme.rating",
            "2.0.0",
            installed.Verified.SignedBytesSha256));

        var refusal = Assert.Single(set.Compare(harness.Service.Catalogue));

        Assert.Equal(ComponentRefusalCodes.SetVersionMismatch, refusal.Code);
    }

    [Fact]
    public void ADigestThatMovedIsReported()
    {
        using var harness = new Harness();
        harness.Install();
        var set = Compose(Entries("acme.rating", "1.0.0", Digest("f")));

        var refusal = Assert.Single(set.Compare(harness.Service.Catalogue));

        Assert.Equal(ComponentRefusalCodes.SetDigestMismatch, refusal.Code);
    }

    [Fact]
    public void AShellThatLoadedExactlyItsBuildHasNoComplaint()
    {
        using var harness = new Harness();
        var installed = harness.Install();
        var set = Compose(Entries("acme.rating", "1.0.0", installed.Verified.SignedBytesSha256));

        Assert.Empty(set.Compare(harness.Service.Catalogue));
    }

    private static string Digest(string digit) => new(digit[0], 64);

    private static ComponentSetEntry Entries(string id, string version, string manifestSha256) =>
        new(id, version, "acme", "03396219237f75a64f12aeb7f39723ab", Digest("1"), manifestSha256);

    private static ComponentSetManifest Compose(params ComponentSetEntry[] entries)
    {
        var json = JsonSerializer.Serialize(new
        {
            schema = 1,
            shell = "test-shell",
            components = entries.Select(e => new
            {
                component_id = e.ComponentId,
                component_version = e.ComponentVersion,
                publisher_id = e.PublisherId,
                key_id = e.KeyId,
                entry_assembly_sha256 = e.EntryAssemblySha256,
                manifest_sha256 = e.ManifestSha256,
            }).ToArray(),
        });

        Assert.True(ComponentSetManifest.TryParse(json, out var manifest, out var problems), string.Join(" ", problems));
        return manifest!;
    }

    /// <summary>The trust material, the install service and a package directory, disposed together.</summary>
    private sealed class Harness : IDisposable
    {
        private readonly TempPackage _package = new();

        public Harness()
        {
            Trust = new TestTrust(VectorSet);
            Trust.TrustPublisher();
            Service = new ComponentInstallationService(Trust.Store);
            PackageBuilder.Build(_package.Root, PackageBuilder.SampleSpec(Trust));
        }

        public TestTrust Trust { get; }

        public ComponentInstallationService Service { get; }

        public LoadedComponent Install()
        {
            var result = Service.Install(_package.Root, 1_760_000_000);
            Assert.True(result.IsLoaded, result.Refusal?.ToString());
            return result.Component!;
        }

        public void Dispose()
        {
            Service.UninstallAll();
            _package.Dispose();
        }
    }
}
