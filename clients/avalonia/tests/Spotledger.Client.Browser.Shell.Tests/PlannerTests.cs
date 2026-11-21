using System.Text.Json;
using Xunit;

namespace Spotledger.Client.Browser.Shell.Tests;

public sealed class PlannerTests
{
    private static ShellManifest Manifest(string build, params string[] files) =>
        new(build, files.Select(f => { int i = f.LastIndexOf(':'); return new ShellEntry(f[..i], 1, f[(i + 1)..]); }).ToList());

    [Fact]
    public void ColdStartFetchesEverythingCacheable()
    {
        var plan = ShellCachePlanner.Plan(null, [], Manifest("B", "a.js:h1", "a.js.br:h2"), "c1");
        Assert.Equal(ShellPlanReason.ColdStart, plan.Reason);
        Assert.Equal(["a.js#h1"], plan.Fetch.Select(e => e.Key));
        Assert.True(plan.ClearAll);
    }

    [Fact]
    public void WarmStartFetchesNothing()
    {
        var record = new ShellCacheRecord("B", "c1", ["a.js#h1"]);
        var plan = ShellCachePlanner.Plan(record, ["a.js#h1"], Manifest("B", "a.js:h1"), "c1");
        Assert.Equal(ShellPlanReason.Unchanged, plan.Reason);
        Assert.Empty(plan.Fetch);
        Assert.Empty(plan.Delete);
    }

    [Fact]
    public void NeverDeletesAKeyTheNewBuildNeeds()
    {
        // Every combination of present/needed keys over a small universe: the delete set and the needed set never meet.
        string[] universe = ["a#1", "a#2", "b#1", "c#3"];
        for (int presentMask = 0; presentMask < 16; presentMask++)
        {
            for (int neededMask = 0; neededMask < 16; neededMask++)
            {
                var present = universe.Where((_, i) => (presentMask & (1 << i)) != 0).ToList();
                var needed = universe.Where((_, i) => (neededMask & (1 << i)) != 0)
                    .Select(k => new ShellEntry(k.Split('#')[0], 1, k.Split('#')[1])).DistinctBy(e => e.Path).ToList();
                var next = new ShellManifest("N", needed);
                foreach (var record in new ShellCacheRecord?[] { null, new("O", "c1", present), new("O", "c0", present) })
                {
                    var plan = ShellCachePlanner.Plan(record, present, next, "c1");
                    var neededKeys = needed.Select(e => e.Key).ToHashSet();
                    Assert.DoesNotContain(plan.Delete, neededKeys.Contains);
                    Assert.DoesNotContain(plan.Keep, k => !present.Contains(k));
                    Assert.True(neededKeys.SetEquals(plan.Fetch.Select(e => e.Key).Concat(plan.Keep)));
                }
            }
        }
    }

    [Fact]
    public void RecordRoundTripsAndABrokenRecordIsNone()
    {
        var record = new ShellCacheRecord("B", "c1", ["a#1", "b#2"]);
        Assert.Equal(record.Keys, ShellCacheRecord.TryParse(record.ToJson())!.Keys);
        Assert.Null(ShellCacheRecord.TryParse(null));
        Assert.Null(ShellCacheRecord.TryParse("{"));
        Assert.Null(ShellCacheRecord.TryParse("{\"build\":1}"));
        Assert.Null(ShellCacheRecord.TryParse("{\"build\":\"B\",\"contract\":\"c\",\"keys\":[1]}"));
    }

    [Fact]
    public void ManifestParsingFailsClosed()
    {
        var build = FakeBuild.Sample();
        Assert.Equal(build.Manifest.Build, ShellManifest.Parse(build.Json).Build);

        string json = build.Json;
        Assert.Throws<ShellIntegrityException>(() => ShellManifest.Parse(json.Replace("\"version\":1", "\"version\":2")));
        Assert.Throws<ShellIntegrityException>(() => ShellManifest.Parse(json.Replace("main.js", "../main.js")));
        Assert.Throws<ShellIntegrityException>(() => ShellManifest.Parse("not json"));
        Assert.Throws<ShellIntegrityException>(() => ShellManifest.Parse("{}"));
        // A changed hash no longer matches the build id.
        string hash = build.Manifest.Entries[0].Sha256;
        Assert.Throws<ShellIntegrityException>(() => ShellManifest.Parse(json.Replace(hash, new string('0', 64))));
        // A changed build id does not match the files.
        Assert.Throws<ShellIntegrityException>(() => ShellManifest.Parse(json.Replace(build.Manifest.Build, new string('1', 64))));
        // Upper-case hash, duplicate and unsorted lists are refused.
        Assert.Throws<ShellIntegrityException>(() => ShellManifest.Parse(json.Replace(hash, hash.ToUpperInvariant())));
        var entries = build.Manifest.Entries;
        var reversed = entries.Reverse().ToList();
        Assert.Throws<ShellIntegrityException>(() => ShellManifest.Parse(FakeBuild.ToJson(ShellManifest.BuildIdOf(reversed, new Sha256ContentHasher()), reversed)));
        var twice = entries.Concat(entries.Take(1)).ToList();
        Assert.Throws<ShellIntegrityException>(() => ShellManifest.Parse(FakeBuild.ToJson(ShellManifest.BuildIdOf(twice, new Sha256ContentHasher()), twice)));
    }

    [Theory]
    [InlineData("a/b.js", true)]
    [InlineData("", false)]
    [InlineData("/a", false)]
    [InlineData("a//b", false)]
    [InlineData("./a", false)]
    [InlineData("a/../b", false)]
    [InlineData("a\\b", false)]
    [InlineData("a?b", false)]
    [InlineData("a#b", false)]
    [InlineData("c:d", false)]
    [InlineData("a\nb", false)]
    public void PathSafety(string path, bool expected) => Assert.Equal(expected, ShellManifest.IsSafePath(path));

    [Fact]
    public void PlannerAgreesWithTheGoldenVectors()
    {
        using var document = JsonDocument.Parse(File.ReadAllText(Path.Combine(AppContext.BaseDirectory, "Golden", "vectors.json")));
        foreach (var v in document.RootElement.GetProperty("planner").EnumerateArray())
        {
            string name = v.GetProperty("name").GetString()!;
            var rec = v.GetProperty("record");
            var record = rec.ValueKind == JsonValueKind.Null
                ? null
                : new ShellCacheRecord(rec.GetProperty("build").GetString()!, rec.GetProperty("contract").GetString()!, []);
            var present = v.GetProperty("present").EnumerateArray().Select(e => e.GetString()!).ToList();
            var next = v.GetProperty("next");
            var entries = next.GetProperty("files").EnumerateArray().Select(e => e.GetString()!)
                .Select(f => { int i = f.LastIndexOf(':'); return new ShellEntry(f[..i], 1, f[(i + 1)..]); }).ToList();
            var plan = ShellCachePlanner.Plan(record, present, new ShellManifest(next.GetProperty("build").GetString()!, entries), v.GetProperty("contract").GetString()!);

            var expect = v.GetProperty("expect");
            Assert.True(Reason(expect.GetProperty("reason").GetString()!) == plan.Reason, name + ": reason");
            Assert.True(Strings(expect, "fetch").SequenceEqual(plan.Fetch.Select(e => e.Key)), name + ": fetch");
            Assert.True(Strings(expect, "keep").SequenceEqual(plan.Keep), name + ": keep");
            Assert.True(Strings(expect, "delete").SequenceEqual(plan.Delete), name + ": delete");
            Assert.True(expect.GetProperty("clearAll").GetBoolean() == plan.ClearAll, name + ": clearAll");
        }
    }

    private static List<string> Strings(JsonElement element, string name) =>
        element.GetProperty(name).EnumerateArray().Select(e => e.GetString()!).ToList();

    private static ShellPlanReason Reason(string text) => text switch
    {
        "cold-start" => ShellPlanReason.ColdStart,
        "contract-changed" => ShellPlanReason.ContractChanged,
        "update" => ShellPlanReason.Update,
        "repair" => ShellPlanReason.Repair,
        "unchanged" => ShellPlanReason.Unchanged,
        _ => throw new InvalidOperationException(text),
    };
}
