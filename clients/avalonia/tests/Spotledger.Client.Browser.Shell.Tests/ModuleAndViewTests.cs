using Xunit;

namespace Spotledger.Client.Browser.Shell.Tests;

public sealed class ModuleAndViewTests
{
    private sealed class RecordingLoader : IModuleLoader
    {
        public List<string> Loads { get; } = [];

        public int Failures { get; set; }

        public TaskCompletionSource? Gate { get; set; }

        public async Task LoadAsync(IReadOnlyList<ShellEntry> assemblies, CancellationToken cancellationToken = default)
        {
            Loads.AddRange(assemblies.Select(a => a.Path));
            if (Gate is not null)
            {
                await Gate.Task;
            }

            if (Failures > 0)
            {
                Failures--;
                throw new ShellNetworkException("down");
            }
        }
    }

    private const string Map = """
        {"version":1,"modules":{"forms":["_framework/Spotledger.Client.Forms.dll"],"ghost":["_framework/Missing.dll"],"empty":[]}}
        """;

    private static (LazyModuleRegistry Registry, RecordingLoader Loader) Create()
    {
        var loader = new RecordingLoader();
        return (new LazyModuleRegistry(FakeBuild.Sample().Manifest, ModuleMap.Parse(Map), loader), loader);
    }

    [Fact]
    public async Task LoadsOnDemandAndOnlyOnce()
    {
        var (registry, loader) = Create();
        Assert.False(registry.IsLoaded("forms"));
        Assert.True((await registry.LoadAsync("forms")).Loaded);
        Assert.True((await registry.LoadAsync("forms")).Loaded);
        Assert.Equal(["_framework/Spotledger.Client.Forms.dll"], loader.Loads);
        Assert.True(registry.IsLoaded("forms"));
    }

    [Fact]
    public async Task ConcurrentCallersShareOneLoad()
    {
        var (registry, loader) = Create();
        loader.Gate = new TaskCompletionSource();
        var first = registry.LoadAsync("forms");
        var second = registry.LoadAsync("forms");
        loader.Gate.SetResult();
        Assert.True((await first).Loaded);
        Assert.True((await second).Loaded);
        Assert.Single(loader.Loads);
    }

    [Fact]
    public async Task AFailedLoadReportsAPlainErrorAndCanBeRetried()
    {
        var (registry, loader) = Create();
        loader.Failures = 1;
        var failed = await registry.LoadAsync("forms");
        Assert.False(failed.Loaded);
        Assert.Contains("try again", failed.Error);
        Assert.DoesNotContain("Exception", failed.Error);
        Assert.False(registry.IsLoaded("forms"));

        Assert.True((await registry.LoadAsync("forms")).Loaded);
        Assert.Equal(2, loader.Loads.Count);
    }

    [Fact]
    public async Task OnlyAssembliesOfTheShellManifestAreEverLoaded()
    {
        var (registry, loader) = Create();
        Assert.False((await registry.LoadAsync("ghost")).Loaded);       // listed in the map, absent from the manifest
        Assert.False((await registry.LoadAsync("unknown")).Loaded);     // not in the map
        Assert.False((await registry.LoadAsync("../forms")).Loaded);    // not a module id
        Assert.False((await registry.LoadAsync("Forms")).Loaded);
        Assert.Empty(loader.Loads);
    }

    [Fact]
    public void ModuleMapFailsClosed()
    {
        Assert.Throws<ShellIntegrityException>(() => ModuleMap.Parse("{"));
        Assert.Throws<ShellIntegrityException>(() => ModuleMap.Parse("{\"version\":2,\"modules\":{}}"));
        Assert.Throws<ShellIntegrityException>(() => ModuleMap.Parse("{\"version\":1,\"modules\":{\"Bad Id\":[]}}"));
        Assert.Throws<ShellIntegrityException>(() => ModuleMap.Parse("{\"version\":1,\"modules\":{\"a\":[\"../x.dll\"]}}"));
        Assert.Throws<ShellIntegrityException>(() => ModuleMap.Parse("{\"version\":1,\"modules\":{\"a\":[\"x.dll\",\"x.dll\"]}}"));
        Assert.Throws<ShellIntegrityException>(() => ModuleMap.Parse("{\"version\":1,\"modules\":{\"a\":[3]}}"));
        Assert.Equal(["empty", "forms", "ghost"], ModuleMap.Parse(Map).Ids);
    }

    [Fact]
    public void SkeletonShowsUntilModuleMetadataAndDefinitionArrive()
    {
        var tracker = new ViewLoadTracker();
        Assert.True(tracker.ShowSkeleton);
        tracker.MetadataArrived();
        tracker.ModuleLoaded();
        Assert.True(tracker.ShowSkeleton);
        tracker.DefinitionArrived();
        Assert.Equal(ViewLoadState.Ready, tracker.State);
        Assert.False(tracker.ShowSkeleton);
    }

    [Fact]
    public void AFailedViewCanRetryAndKeepsWhatArrived()
    {
        var tracker = new ViewLoadTracker();
        tracker.MetadataArrived();
        tracker.Fail("The module could not be downloaded.");
        Assert.Equal(ViewLoadState.Failed, tracker.State);
        Assert.Equal("The module could not be downloaded.", tracker.Error);
        tracker.Retry();
        Assert.True(tracker.ShowSkeleton);
        Assert.Null(tracker.Error);
        tracker.ModuleLoaded();
        tracker.DefinitionArrived();
        Assert.Equal(ViewLoadState.Ready, tracker.State);
    }

    [Fact]
    public async Task HashedDocumentsAreStoredUnderTheirOwnHashAndCheckedOnRead()
    {
        var storage = new FakeStorage();
        var cache = new HashedDocumentCache(storage);
        byte[] doc = System.Text.Encoding.UTF8.GetBytes("{\"doctype\":\"Order\"}");
        string hash = await cache.PutAsync(HashedDocumentCache.Metadata, doc);
        Assert.Equal(doc, await cache.GetAsync(HashedDocumentCache.Metadata, hash));
        Assert.Null(await cache.GetAsync(HashedDocumentCache.View, hash));
        Assert.Null(await cache.GetAsync(HashedDocumentCache.Metadata, new string('0', 64)));
        Assert.Null(await cache.GetAsync(HashedDocumentCache.Metadata, "not-a-hash"));

        storage.Blobs["doc/metadata/" + hash] = [9, 9];
        Assert.Null(await cache.GetAsync(HashedDocumentCache.Metadata, hash));
        Assert.Empty(storage.Blobs);
    }

    [Fact]
    public async Task OnlyMetadataAndViewDocumentsAreCached()
    {
        var cache = new HashedDocumentCache(new FakeStorage());
        await Assert.ThrowsAsync<ArgumentException>(() => cache.PutAsync("data", [1]));
        await Assert.ThrowsAsync<ArgumentException>(() => cache.GetAsync("api/v1/doc/Order/1", new string('a', 64)));
    }

    [Fact]
    public async Task HashedDocumentsDoNotShowUpAsShellKeys()
    {
        var storage = new FakeStorage();
        await new HashedDocumentCache(storage).PutAsync(HashedDocumentCache.View, [1, 2, 3]);
        var build = FakeBuild.Sample();
        var network = new FakeNetwork { Files = build.Files };
        var installer = new ShellInstaller(storage, network);
        await installer.StageAsync(build.Manifest, "c1");
        var next = build.With("main.js", "2");
        network.Files = next.Files;
        await installer.StageAsync(next.Manifest, "c1");
        await installer.ActivateAsync();
        Assert.Single(storage.Blobs.Keys, k => k.StartsWith("doc/", StringComparison.Ordinal));
    }
}
