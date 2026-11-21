using Xunit;

namespace Spotledger.Client.Browser.Shell.Tests;

public sealed class InstallerTests
{
    private readonly FakeStorage _storage = new();
    private readonly FakeNetwork _network = new();

    private ShellInstaller Installer() => new(_storage, _network);

    private static int Cacheable(FakeBuild build) => build.Manifest.CacheableEntries.Count();

    [Fact]
    public async Task ColdStartDownloadsEverythingOnceAndActivates()
    {
        var build = FakeBuild.Sample();
        _network.Files = build.Files;
        var result = await Installer().StageAsync(build.Manifest, "c1");

        Assert.True(result.Activated);
        Assert.Equal(Cacheable(build), result.Downloaded);
        Assert.Equal(_network.Downloads.Distinct().Count(), _network.Downloads.Count);
        Assert.DoesNotContain(_network.Downloads, p => p.EndsWith(".br", StringComparison.Ordinal));
        Assert.Equal(build.Manifest.Build, (await Installer().ReadActiveAsync())!.Build);
        Assert.Equal(Cacheable(build), _storage.ShellKeys().Count);
    }

    [Fact]
    public async Task WarmStartDownloadsNothing()
    {
        var build = FakeBuild.Sample();
        _network.Files = build.Files;
        await Installer().StageAsync(build.Manifest, "c1");
        _network.Downloads.Clear();

        var result = await Installer().StageAsync(build.Manifest, "c1");
        Assert.Equal(0, result.Downloaded);
        Assert.Empty(_network.Downloads);
        Assert.Equal(ShellPlanReason.Unchanged, result.Plan.Reason);
    }

    [Fact]
    public async Task OneChangedFileDownloadsExactlyThatFile()
    {
        var one = FakeBuild.Sample();
        _network.Files = one.Files;
        await Installer().StageAsync(one.Manifest, "c1");
        var two = one.With("main.js", "start(2);");
        _network.Files = two.Files;
        _network.Downloads.Clear();

        var result = await Installer().StageAsync(two.Manifest, "c1");
        Assert.Equal(["main.js"], _network.Downloads);
        Assert.False(result.Activated);

        // The running build keeps every file until the new one activates.
        Assert.Equal(Cacheable(one), _storage.ShellKeys().Count - 1);
        Assert.NotNull(await Installer().ReadActiveFileAsync("main.js"));
        Assert.Equal(one.Manifest.Build, (await Installer().ReadActiveAsync())!.Build);

        Assert.True(await Installer().ActivateAsync());
        Assert.Equal(two.Manifest.Build, (await Installer().ReadActiveAsync())!.Build);
        Assert.Equal(Cacheable(two), _storage.ShellKeys().Count);
        Assert.Equal("start(2);", System.Text.Encoding.UTF8.GetString((await Installer().ReadActiveFileAsync("main.js"))!));
    }

    [Fact]
    public async Task ContractChangeClearsTheCache()
    {
        var build = FakeBuild.Sample();
        _network.Files = build.Files;
        await Installer().StageAsync(build.Manifest, "c1");
        // A file only the old contract's build had.
        var stale = build.With("old.js", "old");
        _network.Files = stale.Files;
        await Installer().StageAsync(stale.Manifest, "c1");
        await Installer().ActivateAsync();
        Assert.Contains(_storage.ShellKeys(), k => k.StartsWith("old.js#", StringComparison.Ordinal));

        _network.Files = build.Files;
        _network.Downloads.Clear();
        var result = await Installer().StageAsync(build.Manifest, "c2");

        Assert.Equal(ShellPlanReason.ContractChanged, result.Plan.Reason);
        Assert.Equal(Cacheable(build), _network.Downloads.Count);
        await Installer().ActivateAsync();
        Assert.Equal(build.Manifest.CacheableEntries.Select(e => e.Key).OrderBy(k => k, StringComparer.Ordinal), _storage.ShellKeys());
        Assert.Equal("c2", (await Installer().ReadActiveAsync())!.ContractVersion);
    }

    [Fact]
    public async Task ADownloadThatDoesNotMatchTheManifestCommitsNothing()
    {
        var one = FakeBuild.Sample();
        _network.Files = one.Files;
        await Installer().StageAsync(one.Manifest, "c1");
        var before = _storage.ShellKeys().ToList();
        var two = one.With("main.js", "start(2);").With("extra.js", "x");
        _network.Files = two.Files;
        _network.Corrupt.Add("main.js");

        await Assert.ThrowsAsync<ShellIntegrityException>(() => Installer().StageAsync(two.Manifest, "c1"));
        Assert.Equal(before, _storage.ShellKeys());
        Assert.Null(await Installer().ReadPendingAsync());
        Assert.Equal(one.Manifest.Build, (await Installer().ReadActiveAsync())!.Build);
    }

    [Fact]
    public async Task ColdStartThatFailsLeavesNothingActive()
    {
        var build = FakeBuild.Sample();
        _network.Files = build.Files;
        _network.Corrupt.Add("main.js");
        await Assert.ThrowsAsync<ShellIntegrityException>(() => Installer().StageAsync(build.Manifest, "c1"));
        Assert.Null(await Installer().ReadActiveAsync());
        Assert.Empty(_storage.ShellKeys());
    }

    [Fact]
    public async Task StagingOfflineFailsAndLeavesTheRunningBuild()
    {
        var one = FakeBuild.Sample();
        _network.Files = one.Files;
        await Installer().StageAsync(one.Manifest, "c1");
        var two = one.With("main.js", "start(2);");
        _network.Files = two.Files;
        _network.Offline = true;

        await Assert.ThrowsAsync<ShellNetworkException>(() => Installer().StageAsync(two.Manifest, "c1"));
        Assert.Null(await Installer().ReadPendingAsync());
        Assert.NotNull(await Installer().ReadActiveFileAsync("main.js"));
    }

    [Fact]
    public async Task AnEvictedStagedFileIsNotActivated()
    {
        var one = FakeBuild.Sample();
        _network.Files = one.Files;
        await Installer().StageAsync(one.Manifest, "c1");
        var two = one.With("main.js", "start(2);");
        _network.Files = two.Files;
        await Installer().StageAsync(two.Manifest, "c1");
        var staged = (await Installer().ReadPendingAsync())!.Keys.First(k => k.StartsWith("main.js#", StringComparison.Ordinal));
        _storage.Blobs.Remove(ShellInstaller.KeyPrefix + staged);

        Assert.False(await Installer().ActivateAsync());
        Assert.Equal(one.Manifest.Build, (await Installer().ReadActiveAsync())!.Build);
    }

    [Fact]
    public async Task ACachedFileThatNoLongerMatchesItsHashIsNotServed()
    {
        var build = FakeBuild.Sample();
        _network.Files = build.Files;
        await Installer().StageAsync(build.Manifest, "c1");
        var key = _storage.ShellKeys().First(k => k.StartsWith("main.js#", StringComparison.Ordinal));
        _storage.Blobs[ShellInstaller.KeyPrefix + key] = [1, 2, 3];
        Assert.Null(await Installer().ReadActiveFileAsync("main.js"));
    }

    [Fact]
    public async Task OfflineServesTheShellButRefusesData()
    {
        var build = FakeBuild.Sample();
        _network.Files = build.Files;
        var installer = Installer();
        await installer.StageAsync(build.Manifest, "c1");
        _network.Offline = true;
        var active = (await installer.ReadActiveAsync())!;
        var shellPaths = active.Keys.Select(ShellEntry.PathOfKey).ToHashSet(StringComparer.Ordinal);

        // The shell page and a static file come from the cache with the network down.
        var page = OfflinePolicy.Decide("GET", "orders/42", true, true, shellPaths.Contains);
        Assert.Equal(OfflinePolicy.IndexPath, page.CachePath);
        Assert.Equal(ServeSource.Cache, OfflinePolicy.Resolve(page, await installer.ReadActiveFileAsync(page.CachePath!) is not null, false));
        var asset = OfflinePolicy.Decide("GET", "main.js", false, false, shellPaths.Contains);
        Assert.Equal(ServeSource.Cache, OfflinePolicy.Resolve(asset, await installer.ReadActiveFileAsync(asset.CachePath!) is not null, false));

        // A data request is refused offline: even if the cache held bytes under that name, it is not read.
        var data = OfflinePolicy.Decide("GET", "api/v1/doc/Order/1", false, false, shellPaths.Contains);
        Assert.Equal(ServeStrategy.NetworkOnly, data.Strategy);
        Assert.Null(data.CachePath);
        Assert.Equal(ServeSource.Refuse, OfflinePolicy.Resolve(data, cacheHasEntry: true, networkAvailable: false));
        Assert.Equal(ServeSource.Network, OfflinePolicy.Resolve(data, cacheHasEntry: true, networkAvailable: true));

        // Nothing a request could write lands in the shell cache: only manifest files are ever stored.
        Assert.All(_storage.ShellKeys(), k => Assert.NotNull(build.Manifest.Find(ShellEntry.PathOfKey(k))));
    }

    [Fact]
    public void OfflinePolicyAgreesWithTheGoldenVectors()
    {
        using var document = System.Text.Json.JsonDocument.Parse(File.ReadAllText(Path.Combine(AppContext.BaseDirectory, "Golden", "vectors.json")));
        foreach (var v in document.RootElement.GetProperty("classify").EnumerateArray())
        {
            string name = v.GetProperty("name").GetString()!;
            var shell = v.GetProperty("shell").EnumerateArray().Select(e => e.GetString()!).ToHashSet(StringComparer.Ordinal);
            var decision = OfflinePolicy.Decide(v.GetProperty("method").GetString()!, v.GetProperty("path").GetString()!,
                v.GetProperty("query").GetBoolean(), v.GetProperty("navigation").GetBoolean(), shell.Contains);
            var expect = v.GetProperty("expect");
            string strategy = decision.Strategy == ServeStrategy.CacheFirst ? "cache-first" : "network-only";
            Assert.True(expect.GetProperty("strategy").GetString() == strategy, name + ": strategy");
            Assert.True(expect.GetProperty("cachePath").GetString() == decision.CachePath, name + ": cachePath");
        }

        foreach (var v in document.RootElement.GetProperty("resolve").EnumerateArray())
        {
            var decision = new ServeDecision(v.GetProperty("strategy").GetString() == "cache-first" ? ServeStrategy.CacheFirst : ServeStrategy.NetworkOnly, "x");
            var source = OfflinePolicy.Resolve(decision, v.GetProperty("cached").GetBoolean(), v.GetProperty("network").GetBoolean());
            string text = source switch { ServeSource.Cache => "cache", ServeSource.Network => "network", _ => "refuse" };
            Assert.Equal(v.GetProperty("expect").GetString(), text);
        }
    }
}
