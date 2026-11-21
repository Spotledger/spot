using Spotledger.Component.Host;
using Xunit;

namespace Spotledger.Component.Host.Tests;

/// <summary>
/// Recorded security finding. A refusal is shown in the UI and written to logs before the signature has been checked,
/// so it must not carry text the package chose: file names with control characters or ANSI escapes,
/// very long paths, or the directory the package was found in. One helper
/// (<see cref="ComponentRefusalText"/>) produces every file name a refusal shows.
/// </summary>
public sealed class RefusalTextHardeningTests
{
    private const string HostileTail = "evil\u001b[2J\u0007\r\nINJECTED‮.dll";

    private static readonly string HostilePath = "assemblies/" + HostileTail;

    private static readonly string LongPath = "assemblies/" + new string('a', 110) + "/" + new string('b', 110) + ".dll";

    [Theory]
    [InlineData(null, "")]
    [InlineData("", "")]
    [InlineData("assemblies/Acme.dll", "assemblies/Acme.dll")]
    [InlineData("a\u001b[2Jb", "a?[2Jb")]
    [InlineData("a\r\nb\tc", "a??b?c")]
    [InlineData("café/‮", "caf?/?")]
    public void TheHelperKeepsPrintableAsciiAndReplacesTheRest(string? input, string expected)
    {
        Assert.Equal(expected, ComponentRefusalText.SafePath(input));
    }

    [Fact]
    public void TheHelperCutsAnythingBeyondTheLimit()
    {
        var cut = ComponentRefusalText.SafePath(new string('x', 500));

        Assert.True(cut.Length <= ComponentRefusalText.MaxPathCharacters, cut.Length.ToString(System.Globalization.CultureInfo.InvariantCulture));
        Assert.StartsWith("xxx", cut, StringComparison.Ordinal);
        Assert.EndsWith("...", cut, StringComparison.Ordinal);
        Assert.Equal(new string('x', 20), ComponentRefusalText.SafePath(new string('x', 20)));
    }

    [Fact]
    public void AFileReferenceNamesTheZeroBasedIndexAndTheSafePath()
    {
        Assert.Equal("file #0 ('a/b.dll')", ComponentRefusalText.FileRef(0, "a/b.dll"));
        Assert.Equal("file #3 ('a?b')", ComponentRefusalText.FileRef(3, "a\u0001b"));
    }

    [Fact]
    public void AMissingFileWithAHostileNameIsRefusedWithoutThatName()
    {
        using var harness = new HardeningHarness();
        var (package, _, _) = harness.Build(s => Hostile(s, HostilePath));
        using var cleanup = package;
        File.Delete(Disk(package.Root, HostilePath));

        var result = harness.Service.Install(package.Root, HardeningHarness.Now);

        Assert.Equal(ComponentRefusalCodes.FileMissing, result.Refusal!.Code);
        AssertSafe(result.Refusal.Message, package.Root);
        Assert.Contains("file #0", result.Refusal.Message, StringComparison.Ordinal);
        Assert.DoesNotContain("INJECTED‮", result.Refusal.Message, StringComparison.Ordinal);
    }

    [Fact]
    public void AHashMismatchOnAHostileNameIsRefusedWithoutThatName()
    {
        using var harness = new HardeningHarness();
        var (package, _, spec) = harness.Build(s => Hostile(s, HostilePath));
        using var cleanup = package;
        File.WriteAllBytes(Disk(package.Root, HostilePath), spec.Assembly.Concat(new byte[] { 9 }).ToArray());

        var result = harness.Service.Install(package.Root, HardeningHarness.Now);

        Assert.Equal(ComponentRefusalCodes.HashMismatch, result.Refusal!.Code);
        AssertSafe(result.Refusal.Message, package.Root);
        Assert.Contains("file #0", result.Refusal.Message, StringComparison.Ordinal);
    }

    [Fact]
    public void AVeryLongPathIsCutInTheRefusal()
    {
        using var harness = new HardeningHarness();
        var (package, _, _) = harness.Build(s => Hostile(s, LongPath));
        using var cleanup = package;
        File.Delete(Disk(package.Root, LongPath));

        var result = harness.Service.Install(package.Root, HardeningHarness.Now);

        Assert.Equal(ComponentRefusalCodes.FileMissing, result.Refusal!.Code);
        AssertSafe(result.Refusal.Message, package.Root);
        Assert.DoesNotContain(LongPath, result.Refusal.Message, StringComparison.Ordinal);
    }

    [Fact]
    public void AnUnsafeHostilePathInTheManifestIsRefusedWithoutThatPath()
    {
        using var harness = new HardeningHarness();
        var (package, _, _) = harness.Build();
        using var cleanup = package;
        var manifestPath = Path.Combine(package.Root, ComponentPackagePaths.ManifestFileName);
        var text = File.ReadAllText(manifestPath).Replace(
            "assemblies/Acme.RatingComponent.dll",
            "../\\u001b[31m\\u0007outside\\r\\nINJECTED",
            StringComparison.Ordinal);
        File.WriteAllText(manifestPath, text);

        var result = harness.Service.Install(package.Root, HardeningHarness.Now);

        Assert.Equal(ComponentRefusalCodes.ManifestInvalid, result.Refusal!.Code);
        AssertSafe(result.Refusal.Message, package.Root);
        Assert.Contains("outside", result.Refusal.Message, StringComparison.Ordinal);
    }

    [Fact]
    public void AHostileManifestMemberNameIsRefusedWithoutThatName()
    {
        using var harness = new HardeningHarness();
        var (package, _, _) = harness.Build();
        using var cleanup = package;
        var manifestPath = Path.Combine(package.Root, ComponentPackagePaths.ManifestFileName);
        var text = File.ReadAllText(manifestPath).TrimEnd();
        File.WriteAllText(manifestPath, text[..^1] + ",\"evil\\u001b[2J\\u0007\\nINJECTED\":1}");

        var result = harness.Service.Install(package.Root, HardeningHarness.Now);

        Assert.Equal(ComponentRefusalCodes.ManifestInvalid, result.Refusal!.Code);
        AssertSafe(result.Refusal.Message, package.Root);
    }

    [Fact]
    public void AMissingDirectoryIsRefusedWithoutNamingIt()
    {
        using var harness = new HardeningHarness();
        var hostile = Path.Combine(Path.GetTempPath(), "spot-gone-" + Guid.NewGuid().ToString("N") + "-\u001b[31mDIRNAME");

        var result = harness.Service.Install(hostile, HardeningHarness.Now);

        Assert.Equal(ComponentRefusalCodes.FileMissing, result.Refusal!.Code);
        AssertSafe(result.Refusal.Message, hostile);
        Assert.DoesNotContain("DIRNAME", result.Refusal.Message, StringComparison.Ordinal);
    }

    [Fact]
    public void AMissingManifestIsRefusedWithoutNamingTheDirectory()
    {
        using var harness = new HardeningHarness();
        using var empty = new TempPackage();

        var result = harness.Service.Install(empty.Root, HardeningHarness.Now);

        Assert.Equal(ComponentRefusalCodes.ManifestMissing, result.Refusal!.Code);
        AssertSafe(result.Refusal.Message, empty.Root);
        Assert.Contains(ComponentPackagePaths.ManifestFileName, result.Refusal.Message, StringComparison.Ordinal);
    }

    [Fact]
    public void EveryRefusalTheLogRecordedIsFreeOfControlCharactersAndTheDirectory()
    {
        using var harness = new HardeningHarness();
        var (package, _, _) = harness.Build(s => Hostile(s, HostilePath));
        using var cleanup = package;
        File.Delete(Disk(package.Root, HostilePath));

        harness.Service.Install(package.Root, HardeningHarness.Now);

        var logged = Assert.Single(harness.Log.Refusals);
        AssertSafe(logged.Message, package.Root);
    }

    private static void Hostile(PackageSpec spec, string path)
    {
        spec.AssemblyPath = path;
        spec.EntryAssembly = path;
    }

    private static string Disk(string root, string relative) =>
        Path.Combine(root, relative.Replace('/', Path.DirectorySeparatorChar));

    private static void AssertSafe(string message, string directory)
    {
        Assert.False(string.IsNullOrWhiteSpace(message));
        Assert.True(message.Length <= 700, "the message is " + message.Length.ToString(System.Globalization.CultureInfo.InvariantCulture) + " characters.");
        Assert.All(message, c => Assert.InRange(c, ' ', '~'));
        Assert.DoesNotContain(directory, message, StringComparison.Ordinal);
        Assert.DoesNotContain(Path.GetFileName(directory), message, StringComparison.Ordinal);
    }
}
