using System.Text.Json;
using Spotledger.Client.Shell;
using Xunit;

namespace Spotledger.Client.Browser.Tests;

/// <summary>The head's start-up wiring over a fake browser storage and a fake clock.</summary>
public sealed class StartupWiringTests : IDisposable
{
    private readonly string _dir = Path.Combine(Path.GetTempPath(), "spot-startup-" + Guid.NewGuid().ToString("N"));
    private long _now = 1_000;

    private string RecentFile => Path.Combine(_dir, "recent-servers.json");

    private long Tick() => _now += 250;

    public void Dispose()
    {
        if (Directory.Exists(_dir))
        {
            Directory.Delete(_dir, recursive: true);
        }
    }

    private sealed class RecordingOpener : IWorkspaceOpener
    {
        public List<string> Opened { get; } = [];

        public bool Accept { get; set; } = true;

        public List<(string Url, string User, string Password)> SignIns { get; } = [];

        public Task<bool> OpenFormAsync(string doctype, string id)
        {
            Opened.Add($"form:{doctype}:{id}");
            return Task.FromResult(Accept);
        }

        public Task<bool> OpenListAsync(string doctype)
        {
            Opened.Add($"list:{doctype}");
            return Task.FromResult(Accept);
        }

        public Task<bool> SignInAsync(string url, string user, string password)
        {
            SignIns.Add((url, user, password));
            return Task.FromResult(Accept);
        }
    }

    [Fact]
    public void ComposeMarksStartAndLabelsTheActiveParts()
    {
        var state = BrowserHostState.Compose(new MemoryBrowserStorage(), Tick, RecentFile);
        Assert.Equal(1_250, state.Metrics.MarkOf("dotnetStart"));
        Assert.Equal("managed", state.Metrics.LabelOf("calc"));
        Assert.Equal("available", state.Metrics.LabelOf("storage"));
        Assert.False(state.Metrics.IsReady);
    }

    [Fact]
    public void ComposeWorksWhenTheBrowserRefusesStorage()
    {
        var state = BrowserHostState.Compose(new UnavailableBrowserStorage(), Tick, RecentFile);
        Assert.Equal("unavailable", state.Metrics.LabelOf("storage"));
        state.RecentServers.Persist();
        Assert.Null(state.Storage.Read(RecentServersBridge.StorageKey));
        Assert.Empty(new RecentServersStore(RecentFile).Load());
    }

    [Fact]
    public void RecentServersComeBackFromStorageIntoTheShellStore()
    {
        var storage = new MemoryBrowserStorage();
        storage.Write(RecentServersBridge.StorageKey, "[\"https://erp.example.test\",\"https://two.example.test\"]");
        var state = BrowserHostState.Compose(storage, Tick, RecentFile);
        Assert.Equal(["https://erp.example.test", "https://two.example.test"], new RecentServersStore(state.RecentServers.FilePath).Load());
    }

    [Fact]
    public void ARememberedServerIsPersistedBackToStorage()
    {
        var storage = new MemoryBrowserStorage();
        var state = BrowserHostState.Compose(storage, Tick, RecentFile);
        Directory.CreateDirectory(_dir);
        new RecentServersStore(RecentFile).Remember("https://erp.example.test");
        state.RecentServers.Persist();
        using var document = JsonDocument.Parse(storage.Read(RecentServersBridge.StorageKey)!);
        Assert.Equal("https://erp.example.test", document.RootElement[0].GetString());

        // A second page load starts from what the first one stored.
        var reload = BrowserHostState.Compose(storage, Tick, Path.Combine(_dir, "second", "recent.json"));
        Assert.Equal(["https://erp.example.test"], new RecentServersStore(reload.RecentServers.FilePath).Load());
    }

    [Fact]
    public void OnlyServerUrlsCrossIntoStorage()
    {
        var storage = new MemoryBrowserStorage();
        var state = BrowserHostState.Compose(storage, Tick, RecentFile);
        Directory.CreateDirectory(_dir);
        new RecentServersStore(RecentFile).Remember("https://erp.example.test");
        state.RecentServers.Persist();
        var text = storage.Read(RecentServersBridge.StorageKey)!;
        Assert.DoesNotContain("token", text, StringComparison.OrdinalIgnoreCase);
        Assert.DoesNotContain("password", text, StringComparison.OrdinalIgnoreCase);
    }

    [Fact]
    public async Task HooksRefuseUntilTheShellExistsThenForwardAndCount()
    {
        var state = BrowserHostState.Compose(new MemoryBrowserStorage(), Tick, RecentFile);
        Assert.False(await state.OpenFormAsync("Sales Invoice", "SINV-0001"));
        Assert.False(await state.OpenListAsync("Customer"));
        Assert.Equal(0, state.Metrics.CountOf("openForm"));

        var opener = new RecordingOpener();
        state.AttachOpener(opener);
        Assert.True(await state.OpenFormAsync("Sales Invoice", "SINV-0001"));
        Assert.True(await state.OpenListAsync("Customer"));
        Assert.Equal(["form:Sales Invoice:SINV-0001", "list:Customer"], opener.Opened);
        Assert.Equal(1, state.Metrics.CountOf("openForm"));
        Assert.Equal(1, state.Metrics.CountOf("openList"));
        Assert.NotNull(state.Metrics.MarkOf("lastFormOpened"));

        opener.Accept = false;
        Assert.False(await state.OpenListAsync("Customer"));
        Assert.NotNull(state.Metrics.MarkOf("lastListRefused"));
    }

    [Fact]
    public void ReadyFollowsFirstPaintAndTheFirstMarkWins()
    {
        var state = BrowserHostState.Compose(new MemoryBrowserStorage(), Tick, RecentFile);
        state.MarkFirstPaint();
        var firstPaint = state.Metrics.MarkOf("firstPaint");
        state.MarkReady();
        state.MarkFirstPaint();
        Assert.True(state.Metrics.IsReady);
        Assert.Equal(firstPaint, state.Metrics.MarkOf("firstPaint"));
        Assert.True(state.Metrics.MarkOf("ready") > firstPaint);
    }

    [Fact]
    public void SwitchingTheCalculationBackendIsVisibleInTheMetrics()
    {
        var state = BrowserHostState.Compose(new MemoryBrowserStorage(), Tick, RecentFile);
        state.Calculation.UseModule(new InProcessModule());
        Assert.Equal("wasm", state.Metrics.LabelOf("calc"));
        Assert.NotNull(state.Metrics.MarkOf("calcSwitched"));
    }

    [Fact]
    public void MetricsJsonHasTheDocumentedShape()
    {
        var state = BrowserHostState.Compose(new MemoryBrowserStorage(), Tick, RecentFile);
        state.MarkFirstPaint();
        state.MarkReady();
        using var document = JsonDocument.Parse(state.Metrics.ToJson());
        var root = document.RootElement;
        Assert.Equal(BrowserMetrics.SchemaVersion, root.GetProperty("schema").GetInt32());
        Assert.True(root.GetProperty("ready").GetBoolean());
        Assert.Equal(JsonValueKind.Number, root.GetProperty("marksUs").GetProperty("firstPaint").ValueKind);
        Assert.Equal("managed", root.GetProperty("labels").GetProperty("calc").GetString());
        Assert.Equal(JsonValueKind.Object, root.GetProperty("counters").ValueKind);
    }

    [Fact]
    public void MetricsAreIdenticalForTheSameSequence()
    {
        string Run()
        {
            var clock = 0L;
            var metrics = new BrowserMetrics(() => clock += 10);
            metrics.Mark("b");
            metrics.Mark("a");
            metrics.Count("n");
            metrics.Label("x", "y");
            return metrics.ToJson();
        }

        Assert.Equal(Run(), Run());
    }

    // ---- WP-UI-09.T8: the test sign-in hook. The refusals come first: this is a way into authentication. ----

    private const string TestUser = "test-user";
    private const string TestSecret = "example-secret-example-secret";

    private BrowserHostState ComposeWithOpener(out RecordingOpener opener, bool flag)
    {
        var state = BrowserHostState.Compose(new MemoryBrowserStorage(), Tick, RecentFile);
        opener = new RecordingOpener();
        state.AttachOpener(opener);
        if (flag)
        {
            state.EnableTestSignIn();
        }

        return state;
    }

    [Fact]
    public async Task SignInDoesNothingWithoutTheFlag()
    {
        var state = ComposeWithOpener(out var opener, flag: false);
        Assert.False(state.TestSignInEnabled);
        Assert.False(await state.SignInAsync("http://127.0.0.1:8080", TestUser, TestSecret));
        Assert.Empty(opener.SignIns);
        Assert.Equal(0, state.Metrics.CountOf("signIn"));
    }

    [Theory]
    [InlineData("https://erp.example.test")]
    [InlineData("http://127.0.0.1.example.test:8080")]
    [InlineData("http://localhost.example.test")]
    [InlineData("http://localhost@evil.example.test")]
    [InlineData("http://user:pw@localhost:8080")]
    [InlineData("http://10.0.0.5:8080")]
    [InlineData("http://0.0.0.0:8080")]
    [InlineData("ftp://localhost")]
    [InlineData("localhost:8080")]
    [InlineData("")]
    public async Task SignInRefusesAnythingButTheLoopbackHost(string url)
    {
        var state = ComposeWithOpener(out var opener, flag: true);
        Assert.False(await state.SignInAsync(url, TestUser, TestSecret));
        Assert.Empty(opener.SignIns);
    }

    [Theory]
    [InlineData("http://127.0.0.1:8080")]
    [InlineData("http://localhost:8080")]
    [InlineData("http://[::1]:8080")]
    [InlineData("https://localhost")]
    public void LoopbackHostsAreRecognised(string url) => Assert.True(BrowserHostState.IsLoopbackUrl(url));

    [Fact]
    public async Task SignInWithNoOpenerIsFalse()
    {
        var state = BrowserHostState.Compose(new MemoryBrowserStorage(), Tick, RecentFile);
        state.EnableTestSignIn();
        Assert.False(await state.SignInAsync("http://127.0.0.1:8080", TestUser, TestSecret));
    }

    [Theory]
    [InlineData("", TestSecret)]
    [InlineData(TestUser, "")]
    public async Task SignInRefusesMissingCredentials(string user, string password)
    {
        var state = ComposeWithOpener(out var opener, flag: true);
        Assert.False(await state.SignInAsync("http://127.0.0.1:8080", user, password));
        Assert.Empty(opener.SignIns);
    }

    [Fact]
    public async Task SignInForwardsTheThreeArgumentsAndReportsTheResult()
    {
        var state = ComposeWithOpener(out var opener, flag: true);
        Assert.True(await state.SignInAsync("http://127.0.0.1:8080", TestUser, TestSecret));
        Assert.Equal([("http://127.0.0.1:8080", TestUser, TestSecret)], opener.SignIns);
        Assert.Equal(1, state.Metrics.CountOf("signIn"));
        Assert.NotNull(state.Metrics.MarkOf("lastSignedIn"));

        opener.Accept = false;
        Assert.False(await state.SignInAsync("http://localhost:8080", TestUser, TestSecret));
        Assert.NotNull(state.Metrics.MarkOf("lastSignInRefused"));
    }

    [Fact]
    public async Task TheOpenersExceptionIsSwallowedAndTheCredentialsDoNotEscape()
    {
        var state = BrowserHostState.Compose(new MemoryBrowserStorage(), Tick, RecentFile);
        state.EnableTestSignIn();
        state.AttachOpener(new ThrowingOpener());
        Assert.False(await state.SignInAsync("http://127.0.0.1:8080", TestUser, TestSecret));
    }

    [Fact]
    public async Task NeitherThePasswordNorTheUserIsInTheMetricsJson()
    {
        var state = ComposeWithOpener(out _, flag: true);
        await state.SignInAsync("http://127.0.0.1:8080", TestUser, TestSecret);
        await state.SignInAsync("http://127.0.0.1:8080", TestUser, TestSecret);
        var json = state.Metrics.ToJson();
        Assert.DoesNotContain(TestSecret, json, StringComparison.Ordinal);
        Assert.DoesNotContain(TestUser, json, StringComparison.Ordinal);
        Assert.Contains("\"signIn\"", json, StringComparison.Ordinal);
    }

    private sealed class ThrowingOpener : IWorkspaceOpener
    {
        public Task<bool> OpenFormAsync(string doctype, string id) => Task.FromResult(false);

        public Task<bool> OpenListAsync(string doctype) => Task.FromResult(false);

        public Task<bool> SignInAsync(string url, string user, string password) =>
            throw new InvalidOperationException("boom " + password);
    }
}
