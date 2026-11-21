using Spotledger.AdminCenter;
using Spotledger.AdminCenter.Tenants;
using Xunit;

namespace Spotledger.AdminCenter.Tenants.Tests;

public sealed class TenantAndAppTests
{
    private const long Now = FakeTenantsApi.Now;

    [Fact]
    public async Task TenantsFilterByGroupTagAndSearch()
    {
        var (_, context) = Fixtures.Make();
        var screen = new TenantsScreen(context);
        Assert.True(await screen.LoadAsync(Now));
        Assert.Equal(["prod", "test"], screen.Groups);
        Assert.Equal(["lab", "pk", "retail"], screen.Tags);
        Assert.Equal(3, screen.Visible.Count);
        screen.Group = "prod";
        Assert.Equal(2, screen.Visible.Count);
        screen.Tag = "retail";
        Assert.Equal("t1", Assert.Single(screen.Visible).Id);
        screen.Group = null;
        screen.Tag = null;
        screen.Search = "gamma";
        Assert.Equal("t3", Assert.Single(screen.Visible).Id);
    }

    [Fact]
    public async Task SelectingATenantShowsCompaniesEditionAndQuotaWarnings()
    {
        var (_, context) = Fixtures.Make();
        var screen = new TenantsScreen(context);
        await screen.LoadAsync(Now);
        Assert.False(await screen.SelectAsync("nope"));
        Assert.True(await screen.SelectAsync("t1"));
        Assert.Equal("Enterprise", screen.Selected!.Edition);
        Assert.Equal(2, screen.Companies.Count);
        var warning = Assert.Single(screen.QuotaWarnings);
        Assert.Equal("users", warning.Metric);
        Assert.Equal(95, warning.PercentUsed);
        Assert.False(warning.OverLimit);

        await screen.SelectAsync("t2");
        Assert.True(Assert.Single(screen.QuotaWarnings).OverLimit);
        Assert.Null(new QuotaUsage("api", 5, 0).PercentUsed);
    }

    [Fact]
    public async Task CreatingChecksTheNameAndNothingIsSentBeforeSigning()
    {
        var (api, context) = Fixtures.Make();
        var screen = new TenantsScreen(context);
        await screen.LoadAsync(Now);
        Assert.False(screen.Create(" ", "prod", "Standard"));
        Assert.False(screen.Create("acme pakistan", "prod", "Standard"));
        Assert.Contains("already exists", screen.Error, StringComparison.Ordinal);
        Assert.True(screen.Create("Delta", "prod", "Standard"));
        Assert.Empty(api.Executed);
        Assert.False(await screen.ConfirmAsync(Now));
        Assert.Contains("not complete", screen.Error, StringComparison.Ordinal);
        await screen.Gate!.ApproveAsync(new FakeSigner("ann"));
        Assert.True(await screen.ConfirmAsync(Now));
        Assert.Null(screen.Gate);
        Assert.Equal("tenant.create", Assert.Single(api.Executed).Action.OperationId);
    }

    [Fact]
    public async Task SuspendAndResumeFollowTheTenantsState()
    {
        var (api, context) = Fixtures.Make();
        var screen = new TenantsScreen(context);
        await screen.LoadAsync(Now);
        Assert.False(screen.Suspend("t2"));
        Assert.False(screen.Resume("t1"));
        Assert.True(screen.Suspend("t1"));
        await screen.Gate!.ApproveAsync(new FakeSigner("ann"));
        Assert.True(await screen.ConfirmAsync(Now));
        Assert.True(screen.Resume("t2"));
        Assert.True(screen.Gate!.IsReady);
        Assert.True(await screen.ConfirmAsync(Now));
        Assert.Equal(["execute tenant.suspend", "execute tenant.resume"], api.Calls.Where(c => c.StartsWith("execute", StringComparison.Ordinal)));
    }

    [Fact]
    public async Task MovingNeedsADifferentServerAndTwoSignatures()
    {
        var (api, context) = Fixtures.Make();
        var screen = new TenantsScreen(context);
        await screen.LoadAsync(Now);
        Assert.False(screen.Move("t1", "srv1"));
        Assert.True(screen.Move("t1", "srv3"));
        Assert.Contains("srv1 to srv3", screen.Gate!.Summary, StringComparison.Ordinal);
        await screen.Gate.ApproveAsync(new FakeSigner("ann"));
        Assert.False(await screen.Gate.ApproveAsync(new FakeSigner("ann")));
        Assert.False(await screen.ConfirmAsync(Now));
        await screen.Gate.ApproveAsync(new FakeSigner("bob"));
        Assert.True(await screen.ConfirmAsync(Now));
        Assert.Equal(2, Assert.Single(api.Executed).Approvals.Count);
    }

    [Fact]
    public async Task AServerRefusalLeavesTheGateOpenWithTheMessage()
    {
        var (api, context) = Fixtures.Make();
        api.RefuseExecute = "The target server is full.";
        var screen = new TenantsScreen(context);
        await screen.LoadAsync(Now);
        screen.Move("t1", "srv3");
        await screen.Gate!.ApproveAsync(new FakeSigner("ann"));
        await screen.Gate.ApproveAsync(new FakeSigner("bob"));
        Assert.False(await screen.ConfirmAsync(Now));
        Assert.Equal("The target server is full.", screen.Error);
        Assert.NotNull(screen.Gate);
        Assert.False(screen.Gate.IsDone);
    }

    [Fact]
    public async Task AnActionTheCatalogueDoesNotKnowFailsClosed()
    {
        var api = new FakeTenantsApi();
        var screen = new TenantsScreen(new TenantContext(api, new EquivalentCommand([])));
        await screen.LoadAsync(Now);
        screen.Suspend("t1");
        Assert.NotNull(screen.Error);
        Assert.False(screen.Gate!.IsReady);
        Assert.False(await screen.ConfirmAsync(Now));
        Assert.Empty(api.Executed);
    }

    [Fact]
    public async Task InstallingNeedsEveryCapabilityApprovedBeforeTheSignatures()
    {
        var (api, context) = Fixtures.Make();
        var screen = new AppsScreen(context, "t1");
        await screen.LoadAsync(Now);
        Assert.Equal("CRM", Assert.Single(screen.Apps).Name);
        Assert.True(await screen.InstallAsync("crm"));
        Assert.Contains(screen.Plan!.Capabilities, c => c.Widened);
        Assert.False(screen.Proceed());
        Assert.Contains("Approve every capability", screen.Error, StringComparison.Ordinal);
        Assert.True(screen.ApproveCapability("read:customers"));
        Assert.False(screen.ApproveCapability("made:up"));
        Assert.False(screen.CanProceed);
        screen.ApproveCapability("send:email");
        Assert.True(screen.CanProceed);
        Assert.True(screen.Proceed());
        Assert.Contains(screen.Gate!.Action.Args, a => a is { Key: "plan", Value: "dg9" });
        Assert.Empty(api.Executed);
        await screen.Gate.ApproveAsync(new FakeSigner("ann"));
        Assert.True(await screen.ConfirmAsync(Now));
        Assert.Equal("app.install", api.Executed[0].Action.OperationId);
    }

    [Fact]
    public async Task ABlockedMigrationPlanStopsAnUpgrade()
    {
        var (api, context) = Fixtures.Make();
        api.AppPlan = api.AppPlan with
        {
            Capabilities = [],
            Migration = api.AppPlan.Migration! with { Conflicts = [new MigrationConflict("mig.field_clash", "Field clashes with app X.", true)] },
        };
        var screen = new AppsScreen(context, "t1");
        await screen.UpgradeAsync("crm");
        Assert.True(screen.CapabilitiesApproved);
        Assert.False(screen.CanProceed);
        Assert.False(screen.Proceed());
        Assert.Contains("blocking conflict", screen.Error, StringComparison.Ordinal);
        Assert.Null(screen.Gate);
    }

    [Fact]
    public async Task UninstallUsesItsOwnOperationAndTwoSignatures()
    {
        var (api, context) = Fixtures.Make();
        api.AppPlan = api.AppPlan with { Capabilities = [], Migration = null };
        var screen = new AppsScreen(context, "t1");
        await screen.UninstallAsync("crm");
        Assert.True(screen.Proceed());
        Assert.Equal("app.uninstall", screen.Gate!.Action.OperationId);
        Assert.Equal(2, screen.Gate.Signing!.Required);
        Assert.DoesNotContain(screen.Gate.Action.Args, a => a.Key == "plan");
    }

    [Fact]
    public async Task APlanRefusalIsShown()
    {
        var api = new RefusingApi();
        var screen = new AppsScreen(new TenantContext(api, Fixtures.Commands), "t1");
        Assert.False(await screen.InstallAsync("crm"));
        Assert.Equal("The app catalogue is unavailable.", screen.Error);
        Assert.Null(screen.Plan);
    }

    private sealed class RefusingApi : ITenantsApi
    {
        public Task<AppChangePlan> PlanAppChangeAsync(string t, string a, AppChangeKind k, CancellationToken c) => throw new TenantApiException("The app catalogue is unavailable.");

        public Task ExecuteAsync(AdminAction a, IReadOnlyList<Approval> p, CancellationToken c) => throw new NotSupportedException();

        public Task<IReadOnlyList<TenantInfo>> GetTenantsAsync(CancellationToken c) => throw new NotSupportedException();

        public Task<IReadOnlyList<CompanyInfo>> GetCompaniesAsync(string t, CancellationToken c) => throw new NotSupportedException();

        public Task<IReadOnlyList<InstalledApp>> GetAppsAsync(string t, CancellationToken c) => throw new NotSupportedException();

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

        public Task<IReadOnlyList<HealthRow>> GetHealthAsync(CancellationToken c) => throw new NotSupportedException();

        public Task<uint> GetPollIntervalSecondsAsync(CancellationToken c) => throw new NotSupportedException();
    }
}
