using System.Runtime.CompilerServices;
using Spotledger.AdminCenter;
using Spotledger.AdminCenter.Tenants;

namespace Spotledger.AdminCenter.Tenants.Tests;

internal sealed class FakeSigner(string id) : ISigner
{
    public string SignerId { get; } = id;

    public Task<byte[]> SignAsync(byte[] payload, CancellationToken cancellationToken) =>
        Task.FromResult(System.Text.Encoding.UTF8.GetBytes($"{SignerId}:{payload.Length}"));
}

/// <summary>Fixture responses, recorded calls and scripted streams for every board.</summary>
internal sealed class FakeTenantsApi : ITenantsApi
{
    public const long Now = 1_800_000_000;

    public List<string> Calls { get; } = [];

    public List<(AdminAction Action, IReadOnlyList<Approval> Approvals)> Executed { get; } = [];

    public string? RefuseExecute { get; set; }

    public List<TenantInfo> Tenants { get; } =
    [
        new("t1", "Acme Pakistan", "prod", ["pk", "retail"], TenantState.Active, "Enterprise", "valid", "srv1",
            [new QuotaUsage("users", 95, 100), new QuotaUsage("storage_gb", 10, 500), new QuotaUsage("api", 5, 0)]),
        new("t2", "Beta Traders", "prod", ["pk"], TenantState.Suspended, "Standard", "valid", "srv2", [new QuotaUsage("users", 120, 100)]),
        new("t3", "Gamma Labs", "test", ["lab"], TenantState.Active, "Standard", "expiring", "srv1", []),
    ];

    public AppChangePlan AppPlan { get; set; } = new(
        "crm", AppChangeKind.Install, "Install CRM 2.0.",
        [new CapabilityRequest("read:customers", "Read customers", false), new CapabilityRequest("send:email", "Send email", true)],
        new MigrationPlanView("p9", "dg9", MigrationRisk.Additive, [], [], "plan text", 30));

    public MigrationPlanView MigrationPlan { get; set; } = new(
        "p1", "dg1", MigrationRisk.NeedsReview,
        [new PlannedChange("schema/add_column", "Add status to Invoice", MigrationRisk.Additive), new PlannedChange("schema/backfill", "Backfill status", MigrationRisk.NeedsReview)],
        [], "Plan for p1", 120);

    public ApprovalPolicy Policy { get; set; } = new(2, false, "A review plan needs two approvers.");

    public RehearsalReport Rehearsal { get; set; } = new(true, [], [new FingerprintDiff("invoice", "ab12", "cd34", true)]);

    public List<MigrationProgress> ApplyScript { get; set; } =
    [
        new("Add column", 2_500),
        new("Backfill", 7_500),
        new("Finalise", 10_000, new ApplyOutcome(true, "Applied.", null)),
    ];

    public VerifyResult VerifyResultValue { get; set; } = new(true, ["fingerprints match", "app starts"]);

    public MigrationStage StageOfP1 { get; set; } = MigrationStage.Plan;

    public Dictionary<string, string> DesiredErrors { get; } = [];

    public List<DriftItem> Drift { get; } =
    [
        new("d1", "app1", "log_level", "info", "debug", false),
        new("d2", "app2", "log_level", "info", "debug", false),
        new("d3", "db1", "max_conn", "200", "300", true),
    ];

    public RolloutStatus? Rollout { get; set; } = new("r1", [new WaveConfig("canary", 1, 600, true), new WaveConfig("rest", 50, 600, false)], 1, null);

    public List<HealthRow> Health { get; } =
    [
        new("t1", "Acme Pakistan", true, HealthColor.Green, "ok", Now),
        new("srv1", "srv1", false, HealthColor.Amber, "disk 85%", Now),
        new("t3", "Gamma Labs", true, HealthColor.Green, "ok", Now),
    ];

    public uint PollInterval { get; set; } = 20;

    public Task ExecuteAsync(AdminAction action, IReadOnlyList<Approval> approvals, CancellationToken cancellationToken)
    {
        Calls.Add($"execute {action.OperationId}");
        if (RefuseExecute is { } reason)
        {
            throw new TenantApiException(reason);
        }

        Executed.Add((action, approvals));
        if (action.OperationId == TenantOps.MigrationApprove)
        {
            StageOfP1 = MigrationStage.Approve;
        }

        return Task.CompletedTask;
    }

    public Task<IReadOnlyList<TenantInfo>> GetTenantsAsync(CancellationToken cancellationToken) => Task.FromResult<IReadOnlyList<TenantInfo>>([.. Tenants]);

    public Task<IReadOnlyList<CompanyInfo>> GetCompaniesAsync(string tenantId, CancellationToken cancellationToken) =>
        Task.FromResult<IReadOnlyList<CompanyInfo>>([new CompanyInfo("c1", "Acme HQ", null), new CompanyInfo("c2", "Acme Lahore", "c1")]);

    public Task<IReadOnlyList<InstalledApp>> GetAppsAsync(string tenantId, CancellationToken cancellationToken) =>
        Task.FromResult<IReadOnlyList<InstalledApp>>([new InstalledApp("crm", "CRM", "1.9", "2.0")]);

    public Task<AppChangePlan> PlanAppChangeAsync(string tenantId, string appId, AppChangeKind kind, CancellationToken cancellationToken)
    {
        Calls.Add($"plan-app {kind}");
        return Task.FromResult(AppPlan with { Kind = kind, AppId = appId });
    }

    public Task<IReadOnlyList<MigrationSummary>> GetMigrationsAsync(string tenantId, CancellationToken cancellationToken) =>
        Task.FromResult<IReadOnlyList<MigrationSummary>>([new MigrationSummary("p1", "CRM 2.0 schema", StageOfP1, MigrationRisk.NeedsReview)]);

    public Task<MigrationPlanView> GetMigrationPlanAsync(string planId, CancellationToken cancellationToken) => Task.FromResult(MigrationPlan);

    public Task<ApprovalPolicy> GetPolicyAsync(string planId, CancellationToken cancellationToken) => Task.FromResult(Policy);

    public Task<RehearsalReport> RehearseAsync(string planId, CancellationToken cancellationToken)
    {
        Calls.Add("rehearse");
        return Task.FromResult(Rehearsal);
    }

    public async IAsyncEnumerable<MigrationProgress> ApplyAsync(string planId, [EnumeratorCancellation] CancellationToken cancellationToken)
    {
        Calls.Add("apply");
        foreach (var update in ApplyScript)
        {
            await Task.Yield();
            yield return update;
        }
    }

    public Task<VerifyResult> VerifyAsync(string planId, CancellationToken cancellationToken)
    {
        Calls.Add("verify");
        return Task.FromResult(VerifyResultValue);
    }

    public Task<IReadOnlyList<AuditEntry>> GetHistoryAsync(string tenantId, CancellationToken cancellationToken) =>
        Task.FromResult<IReadOnlyList<AuditEntry>>([new AuditEntry(Now - 100, "ann", "plan approved", "p0")]);

    public Task<IReadOnlyList<SettingDescriptor>> GetSettingsSchemaAsync(CancellationToken cancellationToken) =>
        Task.FromResult<IReadOnlyList<SettingDescriptor>>(
        [
            new SettingDescriptor("log_level", "string", "info", "How much to log", false, SettingRisk.Low),
            new SettingDescriptor("max_conn", "int", "100", "Database connections", true, SettingRisk.Medium),
        ]);

    public Task<IReadOnlyDictionary<string, SettingValue>> GetDesiredAsync(string group, CancellationToken cancellationToken) =>
        Task.FromResult<IReadOnlyDictionary<string, SettingValue>>(new Dictionary<string, SettingValue> { ["max_conn"] = new("200") });

    public Task<IReadOnlyDictionary<string, string>> SetDesiredAsync(string group, IReadOnlyDictionary<string, string> changes, CancellationToken cancellationToken)
    {
        Calls.Add("set-desired");
        return Task.FromResult<IReadOnlyDictionary<string, string>>(
            changes.Keys.Where(DesiredErrors.ContainsKey).ToDictionary(k => k, k => DesiredErrors[k]));
    }

    public Task<IReadOnlyList<DriftItem>> GetDriftAsync(string group, CancellationToken cancellationToken) => Task.FromResult<IReadOnlyList<DriftItem>>([.. Drift]);

    public Task<RolloutStatus?> GetRolloutAsync(string group, CancellationToken cancellationToken) => Task.FromResult(Rollout);

    public Task<IReadOnlyList<HealthRow>> GetHealthAsync(CancellationToken cancellationToken)
    {
        Calls.Add("health");
        return Task.FromResult<IReadOnlyList<HealthRow>>([.. Health]);
    }

    public Task<uint> GetPollIntervalSecondsAsync(CancellationToken cancellationToken) => Task.FromResult(PollInterval);
}

internal static class Fixtures
{
    /// <summary>Approvals each operation needs: approve and move need two, destructive uninstall two, most one, suspend/resume none.</summary>
    public static EquivalentCommand Commands { get; } = new(
        new (string Id, int Approvals)[]
        {
            (TenantOps.TenantCreate, 1), (TenantOps.TenantSuspend, 1), (TenantOps.TenantResume, 0), (TenantOps.TenantMove, 2),
            (TenantOps.AppInstall, 1), (TenantOps.AppUpgrade, 1), (TenantOps.AppUninstall, 2), (TenantOps.MigrationApprove, 2),
            (TenantOps.DriftRemediate, 1), (TenantOps.RolloutNextWave, 1), (TenantOps.RolloutResume, 2), (TenantOps.RolloutConfigure, 1),
        }.Select(o => new OperationSpec(o.Id, [new ArgumentSpec("name")], o.Approvals)));

    public static (FakeTenantsApi Api, TenantContext Context) Make()
    {
        var api = new FakeTenantsApi();
        return (api, new TenantContext(api, Commands));
    }
}
