using Spotledger.AdminCenter;
using Spotledger.AdminCenter.Servers;
using Xunit;

namespace Spotledger.AdminCenter.Servers.Tests;

public sealed class ScreenTests
{
    private static (T Screen, FakeServerApi Api) Make<T>(Func<ServerContext, T> create, ServerCapabilities? capabilities = null)
        where T : ServerScreen
    {
        var api = new FakeServerApi { Capabilities = capabilities ?? Fixtures.Linux };
        return (create(Fixtures.Context(api)), api);
    }

    [Fact]
    public async Task OverviewShowsUptimeLoadDiskAndTheRebootBanner()
    {
        var (screen, _) = Make(c => new OverviewScreen(c));
        Assert.True(await screen.LoadAsync(Fixtures.Now));
        Assert.Equal("3 days 4 hours 5 minutes", screen.UptimeText);
        Assert.Equal("1.50 / 0.80 / 0.25", screen.LoadText);
        Assert.Equal("/data: 90% used", screen.FullestDisk);
        Assert.NotNull(screen.RebootBanner);
    }

    [Fact]
    public async Task ServicesFilterBySearchAndState()
    {
        var (screen, _) = Make(c => new ServicesScreen(c));
        await screen.LoadAsync(Fixtures.Now);
        Assert.Equal(4, screen.Visible.Count);
        screen.Filter = ServiceFilter.Running;
        Assert.Equal(["nginx", "postgresql"], screen.Visible.Select(s => s.Name));
        screen.Search = "data";
        Assert.Equal("postgresql", Assert.Single(screen.Visible).Name);
        screen.Filter = ServiceFilter.Failed;
        screen.Search = string.Empty;
        Assert.Equal("backup", Assert.Single(screen.Visible).Name);
    }

    [Fact]
    public async Task ServiceButtonsFollowStateAndCapabilities()
    {
        var (screen, _) = Make(c => new ServicesScreen(c));
        await screen.LoadAsync(Fixtures.Now);
        var nginx = screen.Visible.Single(s => s.Name == "nginx");
        var cups = screen.Visible.Single(s => s.Name == "cups");
        Assert.False(screen.CanStart(nginx));
        Assert.True(screen.CanStop(nginx));
        Assert.True(screen.CanRestart(nginx));
        Assert.True(screen.CanStart(cups));
        Assert.False(screen.CanStop(cups));
    }

    [Fact]
    public async Task EnablingAtBootUsesEnableOnLinuxAndStartTypeOnWindows()
    {
        var (linux, linuxApi) = Make(c => new ServicesScreen(c));
        await linux.SetEnabledAsync("cups", true);
        Assert.Equal("os.service.enable", linux.Operation!.Action.OperationId);

        var (windows, _) = Make(c => new ServicesScreen(c), Fixtures.Windows);
        await windows.SetEnabledAsync("Spooler", false);
        Assert.Equal("os.service.set_start_type", windows.Operation!.Action.OperationId);
        Assert.Contains(windows.Operation.Action.Args, a => a is { Key: "start_type", Value: "disabled" });
        Assert.Empty(linuxApi.Ran);
    }

    [Fact]
    public async Task PackagesWarnAboutRebootsAndApplyWithTheRightOperationPerOs()
    {
        var (linux, _) = Make(c => new PackagesScreen(c));
        await linux.LoadAsync(Fixtures.Now);
        Assert.Equal(2, linux.Pending.Count);
        Assert.Contains("1 of the pending updates need a reboot", linux.RebootWarning, StringComparison.Ordinal);
        Assert.True(linux.CanApply);
        await linux.ApplyUpdatesAsync();
        Assert.Equal("os.update.apply", linux.Operation!.Action.OperationId);

        var (windows, _) = Make(c => new PackagesScreen(c), Fixtures.Windows);
        await windows.LoadAsync(Fixtures.Now);
        await windows.ApplyUpdatesAsync();
        Assert.Equal("os.update.install", windows.Operation!.Action.OperationId);
    }

    [Fact]
    public async Task UsersValidateTheNameAndPickLockOrDisablePerOs()
    {
        var (linux, _) = Make(c => new UsersScreen(c));
        await linux.LoadAsync(Fixtures.Now);
        Assert.False(await linux.CreateAsync(" "));
        Assert.False(await linux.CreateAsync("ANN"));
        Assert.Contains("already exists", linux.Error, StringComparison.Ordinal);
        Assert.True(await linux.CreateAsync("cy"));
        await linux.LockAsync("bob");
        Assert.Equal("os.user.lock", linux.Operation!.Action.OperationId);

        var (windows, _) = Make(c => new UsersScreen(c), Fixtures.Windows);
        await windows.LockAsync("bob");
        Assert.Equal("os.user.disable", windows.Operation!.Action.OperationId);
        Assert.False(await windows.UnlockAsync("bob"));
        Assert.Contains("does not support", windows.Error, StringComparison.Ordinal);
    }

    [Fact]
    public async Task FirewallPicksOpenCloseOrAddRemovePerOs()
    {
        var (linux, _) = Make(c => new FirewallScreen(c));
        await linux.LoadAsync(Fixtures.Now);
        Assert.False(await linux.AddAsync(0, "tcp", "x"));
        await linux.AddAsync(8443, "tcp", "Edge");
        Assert.Equal("os.firewall.open", linux.Operation!.Action.OperationId);
        await linux.RemoveAsync(linux.Rules[0]);
        Assert.Equal("os.firewall.close", linux.Operation!.Action.OperationId);

        var (windows, _) = Make(c => new FirewallScreen(c), Fixtures.Windows);
        await windows.AddAsync(8443, "tcp", "Edge");
        Assert.Equal("os.firewall.add_rule", windows.Operation!.Action.OperationId);
        await windows.RemoveAsync(new FirewallRule("Edge", "Edge", 8443, "tcp", true));
        Assert.Equal("os.firewall.remove_rule", windows.Operation!.Action.OperationId);
    }

    [Fact]
    public async Task StorageListsNewestSnapshotsFirstAndPercentages()
    {
        var (screen, _) = Make(c => new StorageScreen(c));
        await screen.LoadAsync(Fixtures.Now);
        Assert.Equal(["s2", "s1"], screen.Snapshots.Select(s => s.Id));
        Assert.Equal(90, StorageScreen.PercentUsed(screen.Volumes[0]));
        Assert.Equal(40, StorageScreen.PercentUsed(screen.Volumes[1]));
        await screen.RestoreSnapshotAsync("s1");
        Assert.Equal(2, screen.Operation!.Signatures!.Required);
    }

    [Fact]
    public async Task CertificatesShowDaysLeftAndExpiryLevelFromTheCallersClock()
    {
        var (screen, _) = Make(c => new CertificatesScreen(c));
        await screen.LoadAsync(Fixtures.Now);
        var rows = screen.Rows;
        Assert.Equal(["old.example.com", "db.example.com", "erp.example.com"], rows.Select(r => r.Certificate.Subject));
        Assert.Equal(ExpiryLevel.Expired, rows[0].Level);
        Assert.Equal(-1, rows[0].DaysLeft);
        Assert.Equal(ExpiryLevel.Soon, rows[1].Level);
        Assert.Equal(10, rows[1].DaysLeft);
        Assert.Equal(ExpiryLevel.Fine, rows[2].Level);
    }

    [Fact]
    public async Task InstallingACertificateRefusesAPrivateKeyAndMissingPem()
    {
        var (screen, api) = Make(c => new CertificatesScreen(c));
        Assert.False(await screen.InstallAsync("web", "nonsense"));
        Assert.False(await screen.InstallAsync("web", "-----BEGIN CERTIFICATE-----\nx\n-----END CERTIFICATE-----\n-----BEGIN PRIVATE KEY-----\ny"));
        Assert.Contains("private key", screen.Error, StringComparison.Ordinal);
        Assert.Empty(api.Calls);
        Assert.True(await screen.InstallAsync("web", "-----BEGIN CERTIFICATE-----\nx\n-----END CERTIFICATE-----\n"));
    }

    [Fact]
    public async Task ALoadFailureShowsTheServersMessage()
    {
        var api = new ThrowingApi();
        var screen = new OverviewScreen(new ServerContext(api, Fixtures.Commands, Fixtures.Linux));
        Assert.False(await screen.LoadAsync(Fixtures.Now));
        Assert.Equal("The agent is not reachable.", screen.Error);
        Assert.False(screen.IsLoading);
    }

    [Fact]
    public void EveryScreenHasAKeyboardOrder()
    {
        var api = new FakeServerApi();
        var screens = ServerTools.CreateScreens(Fixtures.Context(api));
        Assert.Equal(8, screens.Count);
        Assert.All(screens, s => Assert.NotEmpty(s.FocusOrder));
        Assert.Equal(screens.Count, screens.Select(s => s.Id).Distinct().Count());
    }

    private sealed class ThrowingApi : IServerApi
    {
        public Task<ServerCapabilities> GetCapabilitiesAsync(CancellationToken c) => throw new ServerApiException("The agent is not reachable.");

        public Task<HostOverview> GetOverviewAsync(CancellationToken c) => throw new ServerApiException("The agent is not reachable.");

        public Task<IReadOnlyList<ServiceInfo>> GetServicesAsync(CancellationToken c) => throw new NotSupportedException();

        public Task<PackageReport> GetPackagesAsync(CancellationToken c) => throw new NotSupportedException();

        public Task<IReadOnlyList<UserInfo>> GetUsersAsync(CancellationToken c) => throw new NotSupportedException();

        public Task<IReadOnlyList<FirewallRule>> GetFirewallAsync(CancellationToken c) => throw new NotSupportedException();

        public Task<StorageReport> GetStorageAsync(CancellationToken c) => throw new NotSupportedException();

        public Task<IReadOnlyList<NetworkInterfaceInfo>> GetNetworkAsync(CancellationToken c) => throw new NotSupportedException();

        public Task<IReadOnlyList<CertificateInfo>> GetCertificatesAsync(CancellationToken c) => throw new NotSupportedException();

        public Task<PlanPreview> PreviewAsync(AdminAction a, CancellationToken c) => throw new NotSupportedException();

        public IAsyncEnumerable<OperationProgress> RunAsync(AdminAction a, IReadOnlyList<Approval> p, CancellationToken c) => throw new NotSupportedException();

        public Task ConfirmAsync(RevertTicket t, CancellationToken c) => throw new NotSupportedException();
    }
}
