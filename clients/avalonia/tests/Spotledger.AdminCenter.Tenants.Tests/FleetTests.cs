using Spotledger.AdminCenter;
using Spotledger.AdminCenter.Tenants;
using Xunit;

namespace Spotledger.AdminCenter.Tenants.Tests;

public sealed class FleetTests
{
    private const long Now = FakeTenantsApi.Now;

    private static async Task<(DesiredStateScreen Screen, FakeTenantsApi Api)> LoadedAsync()
    {
        var (api, context) = Fixtures.Make();
        var screen = new DesiredStateScreen(context, "prod");
        await screen.LoadAsync(Now);
        return (screen, api);
    }

    [Fact]
    public async Task TheEditorIsGeneratedFromTheSchemaAndSavesOnlyAfterValidation()
    {
        var (screen, api) = await LoadedAsync();
        Assert.Equal(["log_level", "max_conn"], screen.Editor.Rows().Select(r => r.Descriptor.Key));
        Assert.Equal("200", screen.Editor.Rows()[1].Value);
        Assert.False(await screen.SaveAsync(Now));
        screen.Set("nope", "x");
        Assert.Contains("not a setting", screen.Error, StringComparison.Ordinal);
        screen.Set("max_conn", "abc");
        api.DesiredErrors["max_conn"] = "Must be a whole number.";
        Assert.False(await screen.SaveAsync(Now));
        Assert.Equal("Must be a whole number.", screen.Editor.Rows().Single(r => r.Descriptor.Key == "max_conn").Error);
        Assert.NotEmpty(screen.Draft);

        api.DesiredErrors.Clear();
        screen.Set("max_conn", "250");
        Assert.True(await screen.SaveAsync(Now));
        Assert.Empty(screen.Draft);
    }

    [Fact]
    public async Task DriftCanBeRemediatedOneAtATimeButNeverWhenExempt()
    {
        var (screen, api) = await LoadedAsync();
        Assert.False(screen.BeginRemediate("d3"));
        Assert.Contains("exempt", screen.Error, StringComparison.Ordinal);
        Assert.True(screen.BeginRemediate("d1"));
        Assert.Contains("log_level on app1 back to info", screen.Gate!.Summary, StringComparison.Ordinal);
        await screen.Gate.ApproveAsync(new FakeSigner("ann"));
        Assert.True(await screen.ConfirmAsync(Now));
        Assert.Equal("d1", api.Executed[0].Action.Args.Single(a => a.Key == "items").Value);
    }

    [Fact]
    public async Task BulkRemediationTakesOnlyTickedNonExemptItemsInOrder()
    {
        var (screen, api) = await LoadedAsync();
        Assert.False(screen.BeginRemediateSelected());
        Assert.False(screen.ToggleDrift("d3"));
        Assert.True(screen.ToggleDrift("d2"));
        Assert.True(screen.ToggleDrift("d1"));
        Assert.True(screen.ToggleDrift("d1"));
        Assert.True(screen.ToggleDrift("d1"));
        Assert.True(screen.BeginRemediateSelected());
        Assert.Contains("2 drifted items", screen.Gate!.Summary, StringComparison.Ordinal);
        Assert.Contains(" on ", screen.Gate.Summary, StringComparison.Ordinal);
        await screen.Gate.ApproveAsync(new FakeSigner("ann"));
        await screen.ConfirmAsync(Now);
        Assert.Equal("d1,d2", api.Executed[0].Action.Args.Single(a => a.Key == "items").Value);
    }

    [Fact]
    public async Task WavesNeedUniqueNamesAndTheFirstIsTheCanary()
    {
        var (api, context) = Fixtures.Make();
        api.Rollout = null;
        var screen = new DesiredStateScreen(context, "prod");
        await screen.LoadAsync(Now);
        Assert.False(screen.BeginSaveWaves());
        Assert.False(screen.AddWave(" ", 1, 60));
        Assert.False(screen.AddWave("a", 0, 60));
        Assert.False(screen.AddWave("x:1:0:normal;y", 1, 60));
        Assert.False(screen.AddWave("line\nbreak", 1, 60));
        Assert.True(screen.AddWave("canary", 1, 600));
        Assert.True(screen.AddWave("rest", 50, 600));
        Assert.False(screen.AddWave("rest", 5, 60));
        Assert.Equal([true, false], screen.Waves.Select(w => w.Canary));
        Assert.True(screen.RemoveWave("canary"));
        Assert.True(Assert.Single(screen.Waves).Canary);
        Assert.True(screen.BeginSaveWaves());
        Assert.Equal("rest:50:600:canary", screen.Gate!.Action.Args.Single(a => a.Key == "waves").Value);
    }

    [Fact]
    public async Task AHealthyRolloutMayStartTheNextWave()
    {
        var (screen, api) = await LoadedAsync();
        Assert.Null(screen.HaltedBanner);
        Assert.True(screen.CanStartNextWave);
        Assert.False(screen.CanResume);
        Assert.True(screen.BeginNextWave());
        Assert.Contains("wave rest", screen.Gate!.Summary, StringComparison.Ordinal);
        await screen.Gate.ApproveAsync(new FakeSigner("ann"));
        await screen.ConfirmAsync(Now);
        Assert.Equal("rest", api.Executed[0].Action.Args.Single(a => a.Key == "wave").Value);
    }

    [Fact]
    public async Task AHaltedRolloutShowsTheBannerAndBlocksFurtherWavesUntilResumed()
    {
        var (api, context) = Fixtures.Make();
        api.Rollout = api.Rollout! with { Paused = new PauseInfo("canary_failure", "node7", "canary", "a canary node failed its migration") };
        var screen = new DesiredStateScreen(context, "prod");
        await screen.LoadAsync(Now);
        Assert.Contains("halted: a canary node failed its migration (node node7)", screen.HaltedBanner, StringComparison.Ordinal);
        Assert.False(screen.CanStartNextWave);
        Assert.False(screen.BeginNextWave());
        Assert.Contains("halted", screen.Error, StringComparison.Ordinal);
        Assert.Null(screen.Gate);
        Assert.Empty(api.Executed);

        Assert.True(screen.CanResume);
        Assert.True(screen.BeginResume());
        await screen.Gate!.ApproveAsync(new FakeSigner("ann"));
        Assert.False(screen.Gate.IsReady);
        await screen.Gate.ApproveAsync(new FakeSigner("bob"));
        api.Rollout = api.Rollout with { Paused = null };
        Assert.True(await screen.ConfirmAsync(Now));
        Assert.Null(screen.HaltedBanner);
        Assert.True(screen.CanStartNextWave);
        Assert.Equal("rollout.resume", api.Executed[0].Action.OperationId);
    }

    [Fact]
    public async Task ResumeIsRefusedWhenNothingIsHalted()
    {
        var (screen, _) = await LoadedAsync();
        Assert.False(screen.BeginResume());
    }

    [Fact]
    public async Task AFinishedRolloutHasNoNextWave()
    {
        var (api, context) = Fixtures.Make();
        api.Rollout = api.Rollout! with { CompletedWaves = 2 };
        var screen = new DesiredStateScreen(context, "prod");
        await screen.LoadAsync(Now);
        Assert.False(screen.CanStartNextWave);
        Assert.False(screen.BeginNextWave());
    }

    [Fact]
    public async Task TheHealthGridIsWorstFirstWithFiltersAndCounts()
    {
        var (_, context) = Fixtures.Make();
        var grid = new FleetHealthScreen(context);
        await grid.LoadAsync(Now);
        Assert.Equal("srv1", grid.Visible[0].Name);
        Assert.Equal("0 red, 1 amber, 2 green", grid.Summary);
        grid.TenantsOnly = true;
        Assert.Equal(["Acme Pakistan", "Gamma Labs"], grid.Visible.Select(r => r.Name));
        grid.TenantsOnly = null;
        grid.Color = HealthColor.Amber;
        Assert.Equal("srv1", Assert.Single(grid.Visible).Name);
        grid.Color = null;
        grid.Search = "gamma";
        Assert.Equal("t3", Assert.Single(grid.Visible).Id);
    }

    [Fact]
    public async Task ASeededFailureShowsOnTheNextPollWithinOneIntervalAndNoPollHappensEarlier()
    {
        var (api, context) = Fixtures.Make();
        api.PollInterval = 20;
        var grid = new FleetHealthScreen(context);
        Assert.True(await grid.PollAsync(Now));
        Assert.Equal(20u, grid.PollIntervalSeconds);
        Assert.Equal("0 red, 1 amber, 2 green", grid.Summary);

        // The failure is seeded 5 seconds after the first poll.
        api.Health[0] = api.Health[0] with { Color = HealthColor.Red, Reason = "database unreachable", ObservedUnixSeconds = Now + 5 };
        Assert.False(await grid.PollAsync(Now + 10));
        Assert.Equal("0 red, 1 amber, 2 green", grid.Summary);
        Assert.True(await grid.PollAsync(Now + 20));
        Assert.Equal("1 red, 1 amber, 1 green", grid.Summary);
        Assert.Equal("Acme Pakistan", grid.Visible[0].Name);
        Assert.InRange(grid.PollIntervalSeconds, 1u, 60u);
        Assert.Equal(2, api.Calls.Count(c => c == "health"));
    }

    [Fact]
    public async Task AFailedPollKeepsTheLastRowsAndRetriesAtTheNextInterval()
    {
        var api = new FlakyHealthApi();
        var grid = new FleetHealthScreen(new TenantContext(api, Fixtures.Commands));
        Assert.True(await grid.PollAsync(Now));
        Assert.Single(grid.Visible);
        api.Fail = true;
        Assert.False(await grid.PollAsync(Now + 30));
        Assert.Equal("The telemetry feed is not answering.", grid.Error);
        Assert.Single(grid.Visible);
        api.Fail = false;
        Assert.False(await grid.PollAsync(Now + 40));
        Assert.True(await grid.PollAsync(Now + 60));
        Assert.Null(grid.Error);
    }

    [Fact]
    public void ToolsRegisterTenantToolsForErpAdministratorsAndFleetToolsBehindTheFleetCapability()
    {
        var registry = new ToolRegistry();
        TenantTools.Register(registry);
        var erp = new AdminPrincipal("fin", new HashSet<AdminRole> { AdminRole.ErpAdministrator });
        var infra = new AdminPrincipal("ops", new HashSet<AdminRole> { AdminRole.InfrastructureAdministrator });
        var withFleet = new Entitlements("Enterprise", new HashSet<string> { TenantTools.FleetCapability });
        var without = new Entitlements("Standard", new HashSet<string>());
        Assert.Equal(3, registry.Navigation(erp, without).Count);
        Assert.All(registry.Navigation(erp, without), n => Assert.True(n.Enabled));
        var fleet = registry.Navigation(infra, without);
        Assert.Equal(2, fleet.Count);
        Assert.All(fleet, n => Assert.False(n.Enabled));
        Assert.All(registry.Navigation(infra, withFleet), n => Assert.True(n.Enabled));
    }

    [Fact]
    public async Task EveryScreenHasAKeyboardOrder()
    {
        var (_, context) = Fixtures.Make();
        TenantScreen[] screens =
        [
            new TenantsScreen(context), new AppsScreen(context, "t1"), new MigrationsBoard(context, "t1"),
            new DesiredStateScreen(context, "prod"), new FleetHealthScreen(context),
        ];
        Assert.All(screens, s => Assert.NotEmpty(s.FocusOrder));
        Assert.Equal(5, screens.Select(s => s.Id).Distinct().Count());
        await Task.CompletedTask;
    }

    private sealed class FlakyHealthApi : ITenantsApi
    {
        public bool Fail { get; set; }

        public Task<IReadOnlyList<HealthRow>> GetHealthAsync(CancellationToken c) =>
            Fail ? throw new TenantApiException("The telemetry feed is not answering.") : Task.FromResult<IReadOnlyList<HealthRow>>([new HealthRow("t1", "Acme", true, HealthColor.Green, "ok", 0)]);

        public Task<uint> GetPollIntervalSecondsAsync(CancellationToken c) => Task.FromResult(30u);

        public Task ExecuteAsync(AdminAction a, IReadOnlyList<Approval> p, CancellationToken c) => throw new NotSupportedException();

        public Task<IReadOnlyList<TenantInfo>> GetTenantsAsync(CancellationToken c) => throw new NotSupportedException();

        public Task<IReadOnlyList<CompanyInfo>> GetCompaniesAsync(string t, CancellationToken c) => throw new NotSupportedException();

        public Task<IReadOnlyList<InstalledApp>> GetAppsAsync(string t, CancellationToken c) => throw new NotSupportedException();

        public Task<AppChangePlan> PlanAppChangeAsync(string t, string a, AppChangeKind k, CancellationToken c) => throw new NotSupportedException();

        public Task<IReadOnlyList<MigrationSummary>> GetMigrationsAsync(string t, CancellationToken c) => throw new NotSupportedException();

        public Task<MigrationPlanView> GetMigrationPlanAsync(string p, CancellationToken c) => throw new NotSupportedException();

        public Task<ApprovalPolicy> GetPolicyAsync(string p, CancellationToken c) => throw new NotSupportedException();

        public Task<RehearsalReport> RehearseAsync(string p, CancellationToken c) => throw new NotSupportedException();

        public IAsyncEnumerable<MigrationProgress> ApplyAsync(string p, CancellationToken c) => throw new NotSupportedException();

        public Task<VerifyResult> VerifyAsync(string p, CancellationToken c) => throw new NotSupportedException();

        public Task<IReadOnlyList<AuditEntry>> GetHistoryAsync(string t, CancellationToken c) => throw new NotSupportedException();

        public Task<IReadOnlyList<SettingDescriptor>> GetSettingsSchemaAsync(CancellationToken c) => throw new NotSupportedException();

        public Task<IReadOnlyDictionary<string, SettingValue>> GetDesiredAsync(string g, CancellationToken c) => throw new NotSupportedException();

        public Task<IReadOnlyDictionary<string, string>> SetDesiredAsync(string g, IReadOnlyDictionary<string, string> ch, CancellationToken c) => throw new NotSupportedException();

        public Task<IReadOnlyList<DriftItem>> GetDriftAsync(string g, CancellationToken c) => throw new NotSupportedException();

        public Task<RolloutStatus?> GetRolloutAsync(string g, CancellationToken c) => throw new NotSupportedException();
    }
}
