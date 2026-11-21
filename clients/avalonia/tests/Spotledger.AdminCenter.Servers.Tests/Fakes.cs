using System.Runtime.CompilerServices;
using Spotledger.AdminCenter;
using Spotledger.AdminCenter.Servers;

namespace Spotledger.AdminCenter.Servers.Tests;

internal sealed class FakeSigner(string id) : ISigner
{
    public string SignerId { get; } = id;

    public Task<byte[]> SignAsync(byte[] payload, CancellationToken cancellationToken) =>
        Task.FromResult(System.Text.Encoding.UTF8.GetBytes($"{SignerId}:{payload.Length}"));
}

/// <summary>Fixture responses and recorded calls: the screens run against typed data, never text.</summary>
internal sealed class FakeServerApi : IServerApi
{
    public ServerCapabilities Capabilities { get; set; } = Fixtures.Linux;

    public List<string> Calls { get; } = [];

    public List<AdminAction> Ran { get; } = [];

    public Dictionary<string, PlanPreview> Previews { get; } = [];

    public OperationResult RunResult { get; set; } = new(true, "Done.", new VerifyCheck(true, "State matches."), null);

    public string? RefuseRun { get; set; }

    public List<IReadOnlyList<Approval>> ApprovalsSeen { get; } = [];

    public List<RevertTicket> Confirmed { get; } = [];

    public Task<ServerCapabilities> GetCapabilitiesAsync(CancellationToken cancellationToken) => Task.FromResult(Capabilities);

    public Task<HostOverview> GetOverviewAsync(CancellationToken cancellationToken) => Task.FromResult(new HostOverview(
        "Ubuntu 24.04 LTS", "app1", 3 * 86_400UL + 4 * 3_600 + 5 * 60, 1_500, 800, 250,
        [new DiskUse("/", 100, 60), new DiskUse("/data", 500, 50)], true));

    public Task<IReadOnlyList<ServiceInfo>> GetServicesAsync(CancellationToken cancellationToken) =>
        Task.FromResult<IReadOnlyList<ServiceInfo>>(
        [
            new ServiceInfo("nginx", "Web server", ServiceState.Running, true),
            new ServiceInfo("postgresql", "Database", ServiceState.Running, true),
            new ServiceInfo("cups", "Printing", ServiceState.Stopped, false),
            new ServiceInfo("backup", "Nightly backup", ServiceState.Failed, true),
        ]);

    public Task<PackageReport> GetPackagesAsync(CancellationToken cancellationToken) => Task.FromResult(new PackageReport(
        [new PackageInfo("openssl", "3.0.13", null, false), new PackageInfo("linux-image", "6.8.0", null, true)],
        [new PackageInfo("openssl", "3.0.13", "3.0.14", false), new PackageInfo("linux-image", "6.8.0", "6.8.1", true)],
        false));

    public Task<IReadOnlyList<UserInfo>> GetUsersAsync(CancellationToken cancellationToken) =>
        Task.FromResult<IReadOnlyList<UserInfo>>([new UserInfo("ann", false, ["sudo"], true), new UserInfo("bob", true, [], false)]);

    public Task<IReadOnlyList<FirewallRule>> GetFirewallAsync(CancellationToken cancellationToken) =>
        Task.FromResult<IReadOnlyList<FirewallRule>>([new FirewallRule("r1", "SSH", 22, "tcp", true), new FirewallRule("r2", "HTTPS", 443, "tcp", true)]);

    public Task<StorageReport> GetStorageAsync(CancellationToken cancellationToken) => Task.FromResult(new StorageReport(
        [new VolumeInfo("data", "/data", 500, 450), new VolumeInfo("root", "/", 100, 40)],
        [new SnapshotInfo("s1", "data", "before upgrade", 1_000), new SnapshotInfo("s2", "data", "nightly", 2_000)]));

    public Task<IReadOnlyList<NetworkInterfaceInfo>> GetNetworkAsync(CancellationToken cancellationToken) =>
        Task.FromResult<IReadOnlyList<NetworkInterfaceInfo>>([new NetworkInterfaceInfo("eth0", false, ["10.0.0.5/24"], "10.0.0.1")]);

    public Task<IReadOnlyList<CertificateInfo>> GetCertificatesAsync(CancellationToken cancellationToken) =>
        Task.FromResult<IReadOnlyList<CertificateInfo>>(
        [
            new CertificateInfo("erp.example.com", "web", Fixtures.Now + (200 * 86_400L)),
            new CertificateInfo("old.example.com", "web", Fixtures.Now - 100),
            new CertificateInfo("db.example.com", "db", Fixtures.Now + (10 * 86_400L) + 5),
        ]);

    public Task<PlanPreview> PreviewAsync(AdminAction action, CancellationToken cancellationToken)
    {
        Calls.Add($"preview {action.OperationId}");
        return Task.FromResult(Previews.GetValueOrDefault(action.OperationId)
            ?? new PlanPreview(action.OperationId, "Would change one thing.", PreviewRisk.Low, [], [new PreviewChange("state", "old", "new")], [], true, []));
    }

    public async IAsyncEnumerable<OperationProgress> RunAsync(AdminAction action, IReadOnlyList<Approval> approvals, [EnumeratorCancellation] CancellationToken cancellationToken)
    {
        Calls.Add($"run {action.OperationId}");
        Ran.Add(action);
        ApprovalsSeen.Add(approvals);
        if (RefuseRun is { } reason)
        {
            throw new ServerApiException(reason);
        }

        yield return new OperationProgress("Applying...");
        await Task.Yield();
        yield return new OperationProgress("Verifying...", RunResult);
    }

    public Task ConfirmAsync(RevertTicket ticket, CancellationToken cancellationToken)
    {
        Calls.Add("confirm");
        Confirmed.Add(ticket);
        return Task.CompletedTask;
    }
}

internal static class Fixtures
{
    public const long Now = 1_800_000_000;

    public static ServerCapabilities Linux { get; } = new(OsFamily.Linux, new HashSet<string>
    {
        Ops.ServiceList, Ops.ServiceStatus, Ops.ServiceStart, Ops.ServiceStop, Ops.ServiceRestart, Ops.ServiceEnable, Ops.ServiceDisable,
        Ops.PackageList, Ops.UpdateCheck, Ops.UpdateApply,
        Ops.UserList, Ops.UserCreate, Ops.UserLock, Ops.UserUnlock,
        Ops.FirewallList, Ops.FirewallOpen, Ops.FirewallClose,
        Ops.StorageFacts, Ops.SnapshotCreate, Ops.SnapshotRestore, Ops.NetworkSet, Ops.CertInstall,
    });

    public static ServerCapabilities Windows { get; } = new(OsFamily.Windows, new HashSet<string>
    {
        Ops.ServiceList, Ops.ServiceStart, Ops.ServiceStop, Ops.ServiceRestart, Ops.ServiceSetStartType,
        Ops.UpdatePending, Ops.UpdateInstall,
        Ops.UserList, Ops.UserCreate, Ops.UserDisable,
        Ops.FirewallList, Ops.FirewallAddRule, Ops.FirewallRemoveRule,
    });

    /// <summary>Catalogue with the signatures each operation needs: restore and network need two, firewall one, reads none.</summary>
    public static EquivalentCommand Commands { get; } = new(
        new[]
        {
            Ops.ServiceStart, Ops.ServiceStop, Ops.ServiceRestart, Ops.ServiceEnable, Ops.ServiceDisable, Ops.ServiceSetStartType,
            Ops.UpdateApply, Ops.UpdateInstall, Ops.UserCreate, Ops.UserLock, Ops.UserUnlock, Ops.UserDisable,
            Ops.FirewallOpen, Ops.FirewallClose, Ops.FirewallAddRule, Ops.FirewallRemoveRule,
            Ops.SnapshotCreate, Ops.SnapshotRestore, Ops.NetworkSet, Ops.CertInstall,
        }.Select(id => new OperationSpec(
            id,
            [new ArgumentSpec("name"), new ArgumentSpec("pem")],
            id is Ops.SnapshotRestore or Ops.NetworkSet ? 2 : id is Ops.ServiceStart or Ops.ServiceStop or Ops.ServiceRestart ? 0 : 1)));

    public static ServerContext Context(FakeServerApi api) => new(api, Commands, api.Capabilities);
}
