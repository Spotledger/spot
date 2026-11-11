using System.Text.Json;
using Spotledger.Client.Browser.Build;
using Xunit;

namespace Spotledger.Client.Browser.Tests;

/// <summary>assets.json: stable hashes, every published file listed, a changed file changes its hash.</summary>
public sealed class AssetManifestTests : IDisposable
{
    private readonly string _root = Path.Combine(Path.GetTempPath(), "spot-manifest-" + Guid.NewGuid().ToString("N"));

    public AssetManifestTests()
    {
        Directory.CreateDirectory(Path.Combine(_root, "_framework"));
        Directory.CreateDirectory(Path.Combine(_root, "sw"));
        File.WriteAllText(Path.Combine(_root, "index.html"), "<html></html>");
        File.WriteAllText(Path.Combine(_root, "_framework", "dotnet.js"), "export const dotnet = 1;");
        File.WriteAllBytes(Path.Combine(_root, "_framework", "dotnet.js.br"), [1, 2, 3, 4]);
        File.WriteAllBytes(Path.Combine(_root, "_framework", "app.wasm"), [0, 97, 115, 109]);
        File.WriteAllText(Path.Combine(_root, "sw", "README.md"), "reserved");
    }

    public void Dispose() => Directory.Delete(_root, recursive: true);

    private static string Sha256(string path) =>
        Convert.ToHexString(System.Security.Cryptography.SHA256.HashData(File.ReadAllBytes(path))).ToLowerInvariant();

    [Fact]
    public void EveryPublishedFileIsListedIncludingCompressedSiblings()
    {
        var paths = AssetManifest.Scan(_root).Select(e => e.Path).ToList();
        Assert.Equal(
            ["_framework/app.wasm", "_framework/dotnet.js", "_framework/dotnet.js.br", "index.html", "sw/README.md"],
            paths);
    }

    [Fact]
    public void HashesAndSizesAreTheFilesOwn()
    {
        foreach (var entry in AssetManifest.Scan(_root))
        {
            var full = Path.Combine(_root, entry.Path.Replace('/', Path.DirectorySeparatorChar));
            Assert.Equal(new FileInfo(full).Length, entry.Bytes);
            Assert.Equal(Sha256(full), entry.Sha256);
        }
    }

    [Fact]
    public void WritingTwiceGivesTheSameBytes()
    {
        var first = File.ReadAllText(AssetManifest.Write(_root));
        var second = File.ReadAllText(AssetManifest.Write(_root));
        Assert.Equal(first, second);
    }

    [Fact]
    public void TheManifestDoesNotListItself()
    {
        AssetManifest.Write(_root);
        using var document = JsonDocument.Parse(File.ReadAllText(Path.Combine(_root, AssetManifest.FileName)));
        var listed = document.RootElement.GetProperty("files").EnumerateArray().Select(f => f.GetProperty("path").GetString()).ToList();
        Assert.DoesNotContain(AssetManifest.FileName, listed);
        Assert.Equal(5, listed.Count);
        Assert.Equal(1, document.RootElement.GetProperty("version").GetInt32());
    }

    [Fact]
    public void AChangedFileChangesItsHashAndTheBuildId()
    {
        var before = AssetManifest.Scan(_root);
        var buildBefore = AssetManifest.BuildId(before);
        File.WriteAllText(Path.Combine(_root, "_framework", "dotnet.js"), "export const dotnet = 2;");
        var after = AssetManifest.Scan(_root);
        Assert.NotEqual(before.Single(e => e.Path == "_framework/dotnet.js").Sha256, after.Single(e => e.Path == "_framework/dotnet.js").Sha256);
        Assert.Equal(before.Single(e => e.Path == "index.html").Sha256, after.Single(e => e.Path == "index.html").Sha256);
        Assert.NotEqual(buildBefore, AssetManifest.BuildId(after));
    }

    [Fact]
    public void AnAddedFileAppearsAndChangesTheBuildId()
    {
        var buildBefore = AssetManifest.BuildId(AssetManifest.Scan(_root));
        File.WriteAllText(Path.Combine(_root, "extra.txt"), "x");
        var after = AssetManifest.Scan(_root);
        Assert.Contains(after, e => e.Path == "extra.txt");
        Assert.NotEqual(buildBefore, AssetManifest.BuildId(after));
    }

    [Fact]
    public void ManifestIsValidJsonWithOneEntryPerFile()
    {
        using var document = JsonDocument.Parse(AssetManifest.ToJson(AssetManifest.Scan(_root)));
        Assert.Equal(64, document.RootElement.GetProperty("build").GetString()!.Length);
        Assert.All(
            document.RootElement.GetProperty("files").EnumerateArray(),
            f => Assert.Equal(64, f.GetProperty("sha256").GetString()!.Length));
    }

    [Fact]
    public void AnEmptyFolderGivesAnEmptyList()
    {
        var empty = Path.Combine(_root, "empty");
        Directory.CreateDirectory(empty);
        using var document = JsonDocument.Parse(AssetManifest.ToJson(AssetManifest.Scan(empty)));
        Assert.Equal(0, document.RootElement.GetProperty("files").GetArrayLength());
    }
}
